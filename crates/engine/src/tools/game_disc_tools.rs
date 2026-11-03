//! Game disc images: listing the files on an ISO 9660 disc (a cooked `.iso`, a raw `.bin` track from
//! a `.cue`, a Dreamcast GD-ROM track) and taking them out. The image is read through a file handle,
//! never into memory, so a disc of several gigabytes is fine: only the descriptors and directories
//! are read to list it, and each file is streamed to disk when extracted.

use super::artifact_tools::{UnlessWritten, abs_path, create_absent_dir, safe_components};
use super::native_common::{params_json, spec};
use super::static_input::builder;
use crate::tool::{Ctx, Effects, Requires, ToolOutput, ToolSpec, Toolset, parse_params, schema_of};
use craft_anything_evidence::{Confidence, Coverage, CoverageStatus, ErrorCode, Limitation, Location, Subject, ToolError};
use craft_anything_formats::cue::{self, CueSheet, CueTrack};
use craft_anything_formats::fsread::Limits;
use craft_anything_formats::fsread::iso9660::{self, DiscEntry, Listing, Source, Volume};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;
use std::cell::RefCell;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

/// The most entries listed from one disc.
const MAX_ENTRIES: usize = 200_000;
/// Bytes copied at a time when extracting.
const CHUNK: usize = 1 << 20;

pub fn specs() -> Vec<ToolSpec> {
    vec![
        spec(
            "game_disc_files_list",
            "List a game disc's files",
            "List the files on a game disc image: a cooked .iso, a raw .bin track, or a .cue sheet (the data track that holds an ISO 9660 volume is used; track picks another). Handles 2048-byte and raw 2352-byte sectors (Mode 1 and Mode 2 Form 1), Joliet names and Dreamcast GD-ROM high-density tracks. Reports the volume id and system id (for example PSP GAME), the sector layout and every file with its size and block, paged and optionally limited to a folder (prefix). Only the descriptors and directories are read, so a multi-gigabyte image is fine. Rock Ridge names and files split over several extents are not read.",
            "See which files are on a game disc image (PlayStation, PSP, Dreamcast…).",
            Toolset::Formats,
            Effects::EVIDENCE,
            Requires::Nothing,
            schema_of::<DiscListParams>,
            game_disc_files_list,
        ),
        spec(
            "game_disc_files_extract",
            "Take a game disc's files out",
            "Extract the files of a game disc image (see game_disc_files_list for what is read) into a new folder (output_directory must not exist yet), keeping its folders, streaming each file to disk so large discs are fine. Optionally only a folder of the disc (prefix). Bounded by max_files and max_bytes; never overwrites; names that would leave the folder are refused. Returns each file's SHA-256 and what was skipped and why (a file that runs past the end of a cut-short image, a name clash, a limit).",
            "Copy the files off a game disc image into a folder so you can look at them.",
            Toolset::Formats,
            Effects::MUTATES_SESSION.union(Effects::WRITES_FILESYSTEM),
            Requires::Nothing,
            schema_of::<DiscExtractParams>,
            game_disc_files_extract,
        ),
    ]
}

fn d_offset() -> usize {
    0
}
fn d_limit() -> usize {
    500
}
fn d_files() -> usize {
    100_000
}
fn d_bytes() -> u64 {
    8 << 30
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscListParams {
    /// Absolute path of the .iso, .bin or .cue; the active target when omitted.
    #[serde(default)]
    pub path: Option<String>,
    /// For a .cue sheet: the track number to read (default: the first data track that holds a disc volume).
    #[serde(default)]
    pub track: Option<u32>,
    /// Only files under this folder of the disc (for example `PSP_GAME/SYSDIR`).
    #[serde(default)]
    pub prefix: Option<String>,
    /// First file of the page.
    #[serde(default = "d_offset")]
    pub offset: usize,
    /// Files per page (1..=20000, default 500).
    #[serde(default = "d_limit")]
    pub limit: usize,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscExtractParams {
    /// Absolute path of the .iso, .bin or .cue; the active target when omitted.
    #[serde(default)]
    pub path: Option<String>,
    /// For a .cue sheet: the track number to read.
    #[serde(default)]
    pub track: Option<u32>,
    /// Absolute path of a folder that does not exist yet; it is created (its parent must exist).
    pub output_directory: String,
    /// Only take files under this folder of the disc.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Most files written (1..=1000000, default 100000).
    #[serde(default = "d_files")]
    pub max_files: usize,
    /// Most bytes written in total (default 8 GiB).
    #[serde(default = "d_bytes")]
    pub max_bytes: u64,
}

/// A file opened for random reads. Used from one thread at a time.
struct FileSource {
    file: RefCell<File>,
    len: u64,
}

impl Source for FileSource {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> bool {
        let Ok(mut f) = self.file.try_borrow_mut() else { return false };
        f.seek(SeekFrom::Start(offset)).is_ok() && f.read_exact(buf).is_ok()
    }
}

/// The image a tool call resolved to.
struct Disc {
    /// The file holding the volume.
    image: PathBuf,
    name: String,
    source: FileSource,
    /// The cue track it came from, when the path was a cue sheet.
    track: Option<CueTrack>,
    /// The sheet's tracks that also hold a volume and weren't chosen.
    others: Vec<Value>,
    /// Tracks the sheet lists, with whether each could be found.
    sheet: Option<Value>,
}

fn open_file(path: &Path) -> Result<FileSource, ToolError> {
    let meta = std::fs::metadata(path).map_err(|e| ToolError::io(path.display(), e))?;
    if !meta.is_file() {
        return Err(ToolError::invalid(format!("{} is not a file", path.display())));
    }
    let file = File::open(path).map_err(|e| ToolError::io(path.display(), e))?;
    Ok(FileSource { file: RefCell::new(file), len: meta.len() })
}

/// A path in a cue sheet, joined to the sheet's folder; names that climb out of it or are absolute are refused.
fn beside(sheet: &Path, name: &str) -> Option<PathBuf> {
    let rel = Path::new(&name.replace('\\', "/")).to_path_buf();
    if rel.as_os_str().is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(sheet.parent()?.join(rel))
}

fn is_cue(path: &Path) -> bool {
    if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("cue")) {
        return true;
    }
    let Ok(f) = File::open(path) else { return false };
    let mut head = Vec::new();
    f.take(64 << 10).read_to_end(&mut head).is_ok_and(|_| !head.contains(&0) && cue::looks_like_cue(&String::from_utf8_lossy(&head)))
}

fn read_cue(path: &Path) -> Result<CueSheet, ToolError> {
    let mut text = Vec::new();
    File::open(path).and_then(|f| f.take(cue::MAX_BYTES as u64).read_to_end(&mut text)).map_err(|e| ToolError::io(path.display(), e))?;
    Ok(cue::parse(&String::from_utf8_lossy(&text)))
}

fn volume_summary(src: &FileSource) -> Option<Value> {
    let v = Volume::open(src).ok()?;
    Some(json!({"volume_id": v.volume_id, "system_id": v.system_id, "layout": v.layout.describe()}))
}

fn open_disc(ctx: &Ctx<'_>, path: Option<&str>, track: Option<u32>) -> Result<Disc, ToolError> {
    let p = match path {
        Some(p) => abs_path(p, "path")?,
        None => ctx.session.active()?.target.path.clone(),
    };
    if p.is_dir() {
        return Err(ToolError::invalid(format!("{} is a folder; pass a disc image (.iso, .bin or .cue)", p.display())));
    }
    let name_of = |q: &Path| q.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !is_cue(&p) {
        if track.is_some() {
            return Err(ToolError::invalid("track only applies to a .cue sheet"));
        }
        let source = open_file(&p)?;
        if iso9660::detect(&source).is_none() {
            return Err(ToolError::unsupported(format!("{} doesn't hold an ISO 9660 disc volume", name_of(&p)))
                .with_remediation("This reads ISO 9660 images: a cooked .iso, or a raw .bin track from a .cue sheet (pass the .cue). firmware_regions_list can look for other filesystems."));
        }
        return Ok(Disc { name: name_of(&p), image: p, source, track: None, others: Vec::new(), sheet: None });
    }
    let sheet = read_cue(&p)?;
    if sheet.tracks.is_empty() {
        return Err(ToolError::unsupported(format!("{} lists no tracks", name_of(&p))));
    }
    let mut report = Vec::new();
    let mut found: Vec<(CueTrack, PathBuf, FileSource, Value)> = Vec::new();
    for t in &sheet.tracks {
        let located = beside(&p, &t.file).filter(|f| f.is_file());
        let mut row = json!({"track": t.number, "file": t.file, "mode": t.mode, "data": t.data, "found": located.is_some()});
        if let Some(file) = located
            && t.data
            && let Ok(src) = open_file(&file)
            && let Some(info) = volume_summary(&src)
        {
            row["volume"] = info.clone();
            found.push((t.clone(), file, src, info));
        }
        report.push(row);
    }
    let pick = match track {
        Some(n) => found.iter().position(|(t, ..)| t.number == n).ok_or_else(|| {
            ToolError::new(ErrorCode::NotFound, format!("Track {n} of {} isn't a data track holding a disc volume", name_of(&p)))
                .with_remediation("The sheet's tracks are listed by core_target_triage.")
        })?,
        None if found.is_empty() => {
            return Err(ToolError::unsupported(format!(
                "No track of {} holds an ISO 9660 volume (or its .bin files weren't found beside the sheet)",
                name_of(&p)
            ))
            .with_remediation("Keep the .bin files in the same folder as the .cue, with the names the sheet gives."));
        }
        // A Dreamcast disc keeps the game in the high-density area: prefer it, else the first.
        None => found.iter().position(|(t, ..)| t.area.as_deref() == Some("high-density")).unwrap_or(0),
    };
    let others = found.iter().enumerate().filter(|(i, _)| *i != pick).map(|(_, (t, _, _, info))| json!({"track": t.number, "volume": info})).collect();
    let (t, file, source, _) = found.swap_remove(pick);
    Ok(Disc { name: name_of(&file), image: file, source, track: Some(t), others, sheet: Some(json!(report)) })
}

fn norm_prefix(p: Option<&str>) -> Option<String> {
    let s = p?.replace('\\', "/");
    let s = s.trim_matches('/').to_lowercase();
    (!s.is_empty()).then_some(s)
}

fn under(path: &str, prefix: &Option<String>) -> bool {
    match prefix {
        None => true,
        Some(p) => {
            let l = path.to_lowercase();
            l == *p || l.strip_prefix(p.as_str()).is_some_and(|rest| rest.starts_with('/'))
        }
    }
}

fn describe(disc: &Disc, vol: &Volume<'_, FileSource>) -> Value {
    json!({
        "image": disc.image.display().to_string(),
        "track": disc.track.as_ref().map(|t| json!({"number": t.number, "mode": t.mode, "area": t.area})),
        "volume_id": vol.volume_id,
        "system_id": vol.system_id,
        "layout": vol.layout.describe(),
        "sector_size": vol.layout.sector_size,
        "first_block": vol.lba_base,
        "names": if vol.joliet { "Joliet (long names)" } else { "ISO 9660" },
        "declared_blocks": vol.declared_blocks,
        "file_size": disc.source.len,
    })
}

fn limitations(list: &Listing, joliet: bool) -> Vec<Limitation> {
    let mut l = vec![
        Limitation::new("not_hashed", "The image as a whole isn't hashed (it can be gigabytes); each extracted file gets its own SHA-256."),
        Limitation::new("no_rock_ridge", "Rock Ridge extensions (Unix names and links) aren't read; names are the ISO 9660 or Joliet ones."),
    ];
    if !joliet {
        l.push(Limitation::new("short_names", "Without Joliet, names are the disc's 8.3-style ISO 9660 names."));
    }
    if !list.complete || !list.warnings.is_empty() {
        l.push(Limitation::new(
            "listing_incomplete",
            format!("The listing is not clean: {}", list.warnings.iter().take(5).cloned().collect::<Vec<_>>().join("; ")),
        ));
    }
    l
}

fn list_all(vol: &Volume<'_, FileSource>) -> Listing {
    vol.list(&Limits { max_entries: MAX_ENTRIES, ..Limits::default() })
}

fn game_disc_files_list(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: DiscListParams = parse_params(params)?;
    let disc = open_disc(ctx, p.path.as_deref(), p.track)?;
    ctx.report(None, format!("Reading the directories of {}", disc.name));
    let vol = Volume::open(&disc.source).map_err(|why| ToolError::unsupported(format!("{}: {why}", disc.name)))?;
    let list = list_all(&vol);
    ctx.cancel.check()?;
    let prefix = norm_prefix(p.prefix.as_deref());
    let files: Vec<&DiscEntry> = list.entries.iter().filter(|e| !e.is_dir && under(&e.path, &prefix)).collect();
    let folders = list.entries.iter().filter(|e| e.is_dir).count();
    let total_files = list.entries.iter().filter(|e| !e.is_dir).count();
    let bytes: u64 = list.entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
    let limit = p.limit.clamp(1, 20_000);
    let page: Vec<Value> =
        files.iter().skip(p.offset).take(limit).map(|e| json!({"path": e.path, "size": e.size, "block": e.lba, "present": e.available == e.size})).collect();
    let mut result = describe(&disc, &vol);
    result["files"] = json!(total_files);
    result["folders"] = json!(folders);
    result["bytes"] = json!(bytes);
    result["matching"] = json!(files.len());
    result["offset"] = json!(p.offset);
    result["complete"] = json!(list.complete);
    result["warnings"] = json!(list.warnings.iter().take(50).collect::<Vec<_>>());
    result["entries"] = json!(page);
    result["other_volumes"] = json!(disc.others);
    result["cue_tracks"] = disc.sheet.clone().unwrap_or(Value::Null);
    let summary = format!(
        "{} is a game disc image ({}; volume {:?}, system {:?}) with {total_files} files in {folders} folders ({bytes} bytes){}.",
        disc.name,
        vol.layout.describe(),
        vol.volume_id,
        vol.system_id,
        if list.complete { "" } else { "; the listing is incomplete" }
    );
    let shown = page.len() as u64;
    let b = builder(ctx, "game_disc_files_list", Subject { kind: "disc-image".into(), sha256: None, name: Some(disc.name.clone()) }, params_json(&p))
        .result(result)
        .summary(summary)
        .confidence(Confidence::Observed)
        .limitations(limitations(&list, vol.joliet))
        .coverage(Coverage::page(shown, files.len() as u64, p.offset as u64))
        .location(Location::new("path", disc.image.display().to_string()));
    Ok(ToolOutput::evidence(b.build()?))
}

/// Copy one file out of the volume into `dest` (which must not exist); returns its SHA-256.
fn copy_out(ctx: &Ctx<'_>, vol: &Volume<'_, FileSource>, e: &DiscEntry, dest: &Path) -> Result<String, ToolError> {
    let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(dest).map_err(|err| ToolError::io(dest.display(), err))?;
    let mut hash = sha2::Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    let result = (|| -> Result<(), ToolError> {
        while done < e.size {
            ctx.cancel.check()?;
            let want = usize::try_from((e.size - done).min(CHUNK as u64)).unwrap_or(CHUNK);
            let chunk = buf.get_mut(..want).ok_or_else(|| ToolError::invalid("chunk out of range"))?;
            let got = vol.read_at(e.lba, done, chunk);
            if got < want {
                return Err(ToolError::new(ErrorCode::GarbledOutput, format!("{} ends early at byte {} of {}", e.path, done + got as u64, e.size)));
            }
            hash.update(&*chunk);
            out.write_all(chunk).map_err(|err| ToolError::io(dest.display(), err))?;
            done += want as u64;
        }
        Ok(())
    })();
    drop(out);
    match result {
        Ok(()) => Ok(hex::encode(hash.finalize())),
        Err(err) => {
            let _ = std::fs::remove_file(dest);
            Err(err)
        }
    }
}

fn game_disc_files_extract(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: DiscExtractParams = parse_params(params)?;
    if !(1..=1_000_000).contains(&p.max_files) {
        return Err(ToolError::invalid("max_files must be 1..=1000000"));
    }
    if p.max_bytes == 0 || p.max_bytes > 64 << 30 {
        return Err(ToolError::invalid("max_bytes must be 1..=68719476736 (64 GiB)"));
    }
    let disc = open_disc(ctx, p.path.as_deref(), p.track)?;
    let vol = Volume::open(&disc.source).map_err(|why| ToolError::unsupported(format!("{}: {why}", disc.name)))?;
    ctx.report(None, format!("Reading the directories of {}", disc.name));
    let list = list_all(&vol);
    let prefix = norm_prefix(p.prefix.as_deref());
    let chosen: Vec<&DiscEntry> = list.entries.iter().filter(|e| under(&e.path, &prefix)).collect();
    let wanted_files = chosen.iter().filter(|e| !e.is_dir).count();
    if chosen.is_empty() {
        return Err(ToolError::new(ErrorCode::NotFound, format!("Nothing under {:?} on {}", p.prefix.unwrap_or_default(), disc.name))
            .with_remediation("game_disc_files_list shows the paths on the disc."));
    }
    let root = create_absent_dir(&p.output_directory)?;
    let guard = UnlessWritten::new(&root);
    let (mut written, mut bytes) = (0usize, 0u64);
    let mut skipped: Vec<Value> = Vec::new();
    let mut skipped_count = 0usize;
    let mut manifest: Vec<Value> = Vec::new();
    let mut note = |skipped: &mut Vec<Value>, path: &str, why: String| {
        skipped_count += 1;
        if skipped.len() < 200 {
            skipped.push(json!({"path": path, "reason": why}));
        }
    };
    for (i, e) in chosen.iter().enumerate() {
        ctx.cancel.check()?;
        let comps = match safe_components(&e.path) {
            Ok(c) => c,
            Err(why) => {
                note(&mut skipped, &e.path, format!("refused: {why}"));
                continue;
            }
        };
        let dest = comps.iter().fold(root.clone(), |acc, c| acc.join(c));
        if e.is_dir {
            if let Err(err) = std::fs::create_dir_all(&dest) {
                note(&mut skipped, &e.path, err.to_string());
            }
            continue;
        }
        if written >= p.max_files || bytes.saturating_add(e.size) > p.max_bytes {
            note(&mut skipped, &e.path, "file count or byte limit reached".into());
            continue;
        }
        if e.available < e.size {
            note(&mut skipped, &e.path, format!("runs past the end of the image ({} of {} bytes present)", e.available, e.size));
            continue;
        }
        if dest.exists() {
            note(&mut skipped, &e.path, "another file on the disc has the same name".into());
            continue;
        }
        if let Some(parent) = dest.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            note(&mut skipped, &e.path, err.to_string());
            continue;
        }
        if i % 25 == 0 {
            ctx.report(Some(i as f32 / chosen.len().max(1) as f32), format!("Taking {} off {}", e.path, disc.name));
        }
        match copy_out(ctx, &vol, e, &dest) {
            Ok(sha) => {
                written += 1;
                bytes += e.size;
                if manifest.len() < 1000 {
                    manifest.push(json!({"path": e.path, "size": e.size, "sha256": sha}));
                }
            }
            Err(err) if err.code == ErrorCode::Cancelled => return Err(err),
            Err(err) => note(&mut skipped, &e.path, err.message),
        }
    }
    if written > 0 {
        guard.keep();
    }
    let summary = format!(
        "Took {written} of {wanted_files} files ({bytes} bytes) off {} into {}{}.",
        disc.name,
        root.display(),
        if skipped_count == 0 { String::new() } else { format!("; {skipped_count} skipped") }
    );
    let mut result = describe(&disc, &vol);
    result["output_directory"] = json!(root.display().to_string());
    result["files_written"] = json!(written);
    result["bytes_written"] = json!(bytes);
    result["files_wanted"] = json!(wanted_files);
    result["skipped_count"] = json!(skipped_count);
    result["skipped"] = json!(skipped);
    result["files"] = json!(manifest);
    result["files_listed"] = json!(manifest.len());
    let partial = skipped_count > 0 || !list.complete;
    let mut lims = limitations(&list, vol.joliet);
    if manifest.len() < written {
        lims.push(Limitation::new("manifest_cut", "Only the first 1000 files are listed with their SHA-256 in this record."));
    }
    let mut cov = Coverage::complete(written as u64);
    cov.status = if partial { CoverageStatus::Partial } else { CoverageStatus::Complete };
    cov.total = Some(wanted_files as u64);
    let b = builder(ctx, "game_disc_files_extract", Subject { kind: "disc-image".into(), sha256: None, name: Some(disc.name.clone()) }, params_json(&p))
        .result(result)
        .summary(summary)
        .confidence(Confidence::Observed)
        .limitations(lims)
        .coverage(cov)
        .location(Location::new("path", root.display().to_string()).with_label("output folder"));
    Ok(ToolOutput::evidence(b.build()?))
}

/// What triage says about a disc image held in memory: a plain-English line and the facts behind it.
pub(crate) fn triage_disc(name: &str, bytes: &[u8]) -> (String, Value) {
    let vol = match Volume::open(bytes) {
        Ok(v) => v,
        Err(why) => {
            return (
                format!("{name} starts like a game disc image, but we couldn't read its volume: {why}. Check that the file is complete."),
                json!({"readable": false, "problem": why}),
            );
        }
    };
    let list = vol.list(&Limits { max_entries: MAX_ENTRIES, ..Limits::default() });
    let files = list.entries.iter().filter(|e| !e.is_dir).count();
    let folders = list.entries.len() - files;
    let size: u64 = list.entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
    let gd = if vol.lba_base == iso9660::GDROM_HIGH_DENSITY { " It is the high-density track of a Dreamcast GD-ROM." } else { "" };
    let system = if vol.system_id.is_empty() { String::new() } else { format!(", system {:?}", vol.system_id) };
    let plain = format!(
        "{name} is a game disc image ({}): volume {:?}{system}, with {files} files in {folders} folders ({} bytes).{gd}{} Take the files out with game_disc_files_extract.",
        vol.layout.describe(),
        vol.volume_id,
        size,
        if list.complete { "" } else { " The listing is incomplete (see warnings)." }
    );
    let facts = json!({
        "readable": true, "volume_id": vol.volume_id, "system_id": vol.system_id, "layout": vol.layout.describe(),
        "sector_size": vol.layout.sector_size, "first_block": vol.lba_base, "names": if vol.joliet { "joliet" } else { "iso9660" },
        "files": files, "folders": folders, "bytes": size, "complete": list.complete, "warnings": list.warnings.iter().take(20).collect::<Vec<_>>(),
        "examples": list.entries.iter().filter(|e| !e.is_dir).take(12).map(|e| &e.path).collect::<Vec<_>>(),
    });
    (plain, facts)
}

/// What triage says about a CUE sheet: its tracks, and whether their files are beside it.
pub(crate) fn triage_cue(name: &str, path: &Path, bytes: &[u8]) -> (String, Value) {
    let sheet = cue::parse(&String::from_utf8_lossy(bytes));
    let tracks: Vec<Value> = sheet
        .tracks
        .iter()
        .map(|t| {
            let file = beside(path, &t.file);
            let found = file.as_ref().and_then(|f| std::fs::metadata(f).ok()).filter(|m| m.is_file());
            json!({
                "track": t.number, "file": t.file, "mode": t.mode, "data": t.data, "area": t.area,
                "indexes": t.indexes.iter().map(|i| json!({"index": i.number, "time": i.time})).collect::<Vec<_>>(),
                "found": found.is_some(), "file_size": found.map(|m| m.len()),
            })
        })
        .collect();
    let data = sheet.tracks.iter().filter(|t| t.data).count();
    let missing = tracks.iter().filter(|t| t["found"] == false).count();
    let list = sheet.tracks.iter().map(|t| format!("track {} {} ({})", t.number, t.mode, t.file)).take(8).collect::<Vec<_>>().join(", ");
    let plain = format!(
        "{name} is a CUE sheet for a disc with {} tracks ({data} data): {list}.{} Read the disc's files with game_disc_files_list on this sheet.",
        sheet.tracks.len(),
        if missing > 0 { format!(" {missing} track file(s) weren't found beside it.") } else { String::new() }
    );
    (plain, json!({"tracks": tracks, "warnings": sheet.warnings}))
}

/// A disc image or CUE sheet is not an archive the archive tools open: say so and point at the disc tools
/// (reads about 38 KB of a big file, never all of it).
pub(crate) fn not_an_archive(path: &Path) -> Option<ToolError> {
    let mut head = Vec::new();
    let image =
        File::open(path).and_then(|f| f.take(iso9660::DETECT_PREFIX as u64).read_to_end(&mut head)).is_ok() && iso9660::detect(head.as_slice()).is_some();
    (image || is_cue(path)).then(|| {
        ToolError::unsupported(format!(
            "{} is a game disc image or disc sheet, not an archive",
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        ))
        .with_remediation("game_disc_files_list shows the files on a disc image (.iso, .bin or .cue) and game_disc_files_extract takes them out.")
    })
}

#[cfg(test)]
mod tests {
    use crate::{CallOptions, Caller, Engine};
    use craft_anything_testkit::disc::{Node, Wrap, iso_image};
    use serde_json::{Value, json};

    fn tree() -> Vec<Node> {
        vec![
            Node::File("readme.txt", b"hello disc".to_vec()),
            Node::Dir("PSP_GAME", vec![Node::File("param.sfo", vec![7u8; 5000]), Node::Dir("SYSDIR", vec![Node::File("eboot.bin", vec![9u8; 3000])])]),
        ]
    }

    fn engine() -> Engine {
        Engine::new(crate::testing::session())
    }

    fn call(e: &Engine, tool: &str, params: Value) -> Result<Value, craft_anything_evidence::ToolError> {
        e.call(tool, params, CallOptions::new(Caller::Mcp)).map(|o| o.to_json())
    }

    #[test]
    fn lists_and_extracts_a_cooked_and_a_raw_image() {
        let dir = tempfile::tempdir().expect("temp dir");
        for (name, wrap) in [("a.iso", Wrap::Cooked), ("b.bin", Wrap::RawMode1), ("c.bin", Wrap::RawMode2Form1), ("d.bin", Wrap::GdRom)] {
            let img = dir.path().join(name);
            std::fs::write(&img, iso_image(&tree(), wrap, "PSP GAME", "TESTDISC", false)).expect("write");
            let e = engine();
            let ev = call(&e, "game_disc_files_list", json!({"path": img})).expect("lists");
            assert_eq!(ev["result"]["files"], 3, "{name}");
            assert_eq!(ev["result"]["system_id"], "PSP GAME");
            let out = dir.path().join(format!("out-{name}"));
            let ev = call(&e, "game_disc_files_extract", json!({"path": img, "output_directory": out})).expect("extracts");
            assert_eq!(ev["result"]["files_written"], 3, "{name}: {:?}", ev["result"]);
            assert_eq!(std::fs::read(out.join("PSP_GAME/SYSDIR/EBOOT.BIN")).expect("file"), vec![9u8; 3000]);
            assert_eq!(std::fs::read(out.join("README.TXT")).expect("file"), b"hello disc");
            // A folder prefix limits what is taken.
            let part = dir.path().join(format!("part-{name}"));
            let ev = call(&e, "game_disc_files_extract", json!({"path": img, "output_directory": part, "prefix": "psp_game/sysdir"})).expect("extracts");
            assert_eq!(ev["result"]["files_written"], 1);
            assert!(!part.join("README.TXT").exists());
        }
    }

    #[test]
    fn a_cue_sheet_picks_its_data_track() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("g (Track 1).bin"), vec![0u8; 2352 * 4]).expect("write");
        std::fs::write(dir.path().join("g (Track 2).bin"), iso_image(&tree(), Wrap::GdRom, "SEGA", "DC", false)).expect("write");
        let cue = "REM SINGLE-DENSITY AREA\nFILE \"g (Track 1).bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\nREM HIGH-DENSITY AREA\nFILE \"g (Track 2).bin\" BINARY\n  TRACK 02 MODE1/2352\n    INDEX 01 00:00:00\n";
        let sheet = dir.path().join("g.cue");
        std::fs::write(&sheet, cue).expect("write");
        let e = engine();
        let ev = call(&e, "game_disc_files_list", json!({"path": sheet})).expect("lists");
        assert_eq!(ev["result"]["files"], 3);
        assert_eq!(ev["result"]["track"]["number"], 2);
        assert_eq!(ev["result"]["first_block"], 45000);
        let err = call(&e, "game_disc_files_list", json!({"path": sheet, "track": 1})).expect_err("track 1 has no volume");
        assert_eq!(err.code, craft_anything_evidence::ErrorCode::NotFound);
    }

    #[test]
    fn refuses_what_is_not_a_disc_and_unsafe_cue_names() {
        let dir = tempfile::tempdir().expect("temp dir");
        let junk = dir.path().join("x.iso");
        std::fs::write(&junk, vec![1u8; 100_000]).expect("write");
        let e = engine();
        assert!(call(&e, "game_disc_files_list", json!({"path": junk})).is_err());
        let sheet = dir.path().join("x.cue");
        std::fs::write(&sheet, "FILE \"../x.iso\" BINARY\n TRACK 01 MODE1/2352\n  INDEX 01 00:00:00\n").expect("write");
        assert!(call(&e, "game_disc_files_list", json!({"path": sheet})).is_err());
        // The output folder must be new.
        let img = dir.path().join("a.iso");
        std::fs::write(&img, iso_image(&tree(), Wrap::Cooked, "S", "V", false)).expect("write");
        assert!(call(&e, "game_disc_files_extract", json!({"path": img, "output_directory": dir.path()})).is_err());
    }

    #[test]
    fn archive_tools_point_at_the_disc_tools_and_firmware_extract_unpacks_a_disc() {
        let dir = tempfile::tempdir().expect("temp dir");
        let img = dir.path().join("a.iso");
        std::fs::write(&img, iso_image(&tree(), Wrap::RawMode1, "S", "V", false)).expect("write");
        let e = engine();
        for tool in ["core_archive_show", "core_archive_extract"] {
            let err = call(&e, tool, json!({"path": img})).expect_err("not an archive");
            assert!(err.remediation.as_deref().unwrap_or_default().contains("game_disc_files_list"), "{tool}: {err:?}");
        }
        let out = dir.path().join("fw");
        let v = call(&e, "firmware_image_extract", json!({"path": img, "output_directory": out})).expect("extracts");
        assert!(v["summary"].is_string());
        fn find(dir: &std::path::Path, name: &str) -> Option<Vec<u8>> {
            for entry in std::fs::read_dir(dir).ok()?.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    if let Some(b) = find(&p, name) {
                        return Some(b);
                    }
                } else if p.file_name().is_some_and(|n| n == name) {
                    return std::fs::read(&p).ok();
                }
            }
            None
        }
        assert_eq!(find(&out, "EBOOT.BIN"), Some(vec![9u8; 3000]));
    }
    #[test]
    fn a_cut_short_image_extracts_what_is_there() {
        let dir = tempfile::tempdir().expect("temp dir");
        let img = dir.path().join("cut.iso");
        let full = iso_image(&tree(), Wrap::Cooked, "S", "V", false);
        std::fs::write(&img, full.get(..full.len() - 2500).unwrap_or_default()).expect("write");
        let e = engine();
        let out = dir.path().join("out");
        let ev = call(&e, "game_disc_files_extract", json!({"path": img, "output_directory": out})).expect("extracts");
        assert!(ev["result"]["skipped_count"].as_u64().unwrap_or(0) >= 1, "{:?}", ev["result"]);
        assert_eq!(std::fs::read(out.join("README.TXT")).expect("file"), b"hello disc");
    }
}

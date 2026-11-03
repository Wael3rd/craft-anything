//! Game disc images: listing the files on an ISO 9660 disc (a cooked `.iso`, a raw `.bin` track from
//! a `.cue`, a Dreamcast GD-ROM track) and taking them out. The image is read through a file handle,
//! never into memory, so a disc of several gigabytes is fine: only the descriptors and directories
//! are read to list it, and each file is streamed to disk when extracted.

use super::artifact_tools::{UnlessWritten, abs_path, create_absent_dir, join_under, safe_components};
use super::native_common::{params_json, spec};
use super::static_input::builder;
use crate::tool::{CancelToken, Ctx, Effects, Requires, ToolOutput, ToolSpec, Toolset, parse_params, schema_of};
use craft_anything_evidence::{Confidence, Coverage, CoverageStatus, ErrorCode, Limitation, Location, Subject, ToolError};
use craft_anything_formats::cue::{self, CueSheet, CueTrack};
use craft_anything_formats::fsread::Limits;
use craft_anything_formats::fsread::iso9660::{self, DiscEntry, Listing, Source, Volume};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;
use std::cell::RefCell;
use std::collections::HashMap;
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
            "Extract the files of a game disc image (see game_disc_files_list for what is read) into a new folder (output_directory must not exist yet), keeping its folders, streaming each file to disk so large discs are fine. Optionally only a folder of the disc (prefix). Bounded by max_files and max_bytes (by default twice the image's size); never overwrites; names that would leave the folder or that Windows reserves (C:x, NUL, COM1…) are refused, and a file whose data was already written under another name is written once. Returns each file's SHA-256 and what was skipped and why (a file that runs past the end of a cut-short image or lies in another track, a Mode 2 Form 2 stream or CD audio, a name clash, a limit).",
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
/// What a real disc's files can add up to at most, when max_bytes isn't given: overlapping
/// extents could otherwise turn a small image into gigabytes of output.
fn default_max_bytes(image_bytes: u64) -> u64 {
    image_bytes.saturating_mul(2).max(1 << 20)
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
    /// Most bytes written in total (1..=68719476736; default: twice the image's size, more than a
    /// real disc's files add up to).
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

/// A file (or the part of it one track takes) opened for random reads. Used from one thread at a time.
struct FileSource {
    file: RefCell<File>,
    /// Where the track starts in the file.
    start: u64,
    /// Bytes from `start` that belong to it.
    len: u64,
    /// Stops a directory walk when the tool call is cancelled.
    cancel: Option<CancelToken>,
}

impl Source for FileSource {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> bool {
        let end = offset.checked_add(buf.len() as u64);
        let Some(at) = self.start.checked_add(offset).filter(|_| end.is_some_and(|e| e <= self.len)) else { return false };
        let Ok(mut f) = self.file.try_borrow_mut() else { return false };
        f.seek(SeekFrom::Start(at)).is_ok() && f.read_exact(buf).is_ok()
    }
    fn cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(CancelToken::is_cancelled)
    }
}

/// The image a tool call resolved to.
struct Disc {
    /// The file holding the volume.
    image: PathBuf,
    name: String,
    source: FileSource,
    /// The disc block the track starts at, when the sheet tells (see `Volume::open_numbered`).
    first_block: Option<u32>,
    /// The cue track it came from, when the path was a cue sheet.
    track: Option<CueTrack>,
    /// The sheet's tracks that also hold a volume and weren't chosen.
    others: Vec<Value>,
    /// Tracks the sheet lists, with whether each could be found.
    sheet: Option<Value>,
    /// Data tracks after the chosen one, in other files: where files past the end of this one are.
    later_data: Vec<CueTrack>,
}

impl Disc {
    fn volume(&self) -> Result<Volume<'_, FileSource>, ToolError> {
        Volume::open_numbered(&self.source, self.first_block).map_err(|why| ToolError::unsupported(format!("{}: {why}", self.name)))
    }

    /// Why a file past the end of this track's file is missing: it is probably in a later data track.
    fn elsewhere(&self) -> Option<String> {
        let t = self.later_data.last()?;
        Some(format!(
            "probably in track {} ({}): the disc's file system spans several tracks (as on a Dreamcast disc with CD audio), and reading across tracks isn't supported yet",
            t.number, t.file
        ))
    }
}

fn open_file(path: &Path, cancel: &CancelToken) -> Result<FileSource, ToolError> {
    let meta = std::fs::metadata(path).map_err(|e| ToolError::io(path.display(), e))?;
    if !meta.is_file() {
        return Err(ToolError::invalid(format!("{} is not a file", path.display())));
    }
    let file = File::open(path).map_err(|e| ToolError::io(path.display(), e))?;
    Ok(FileSource { file: RefCell::new(file), start: 0, len: meta.len(), cancel: Some(cancel.clone()) })
}

/// Bytes per sector of a track in its file (2352 unless the sheet says otherwise).
fn sector_bytes(t: &CueTrack) -> u64 {
    u64::from(t.sector_size.filter(|s| *s > 0).unwrap_or(2352))
}

/// Where track `i` lies in its file: from its INDEX 01 (or first index) to the first index of the
/// next track in the same file, or the file's end. Several tracks can share one `.bin`.
fn track_span(tracks: &[CueTrack], i: usize, file_len: u64) -> (u64, u64) {
    let Some(t) = tracks.get(i) else { return (0, file_len) };
    let frames = |t: &CueTrack| t.indexes.iter().filter_map(|x| x.frames).min();
    let start_frames = t.indexes.iter().find(|x| x.number == 1).and_then(|x| x.frames).or_else(|| frames(t)).unwrap_or(0);
    let start = start_frames.saturating_mul(sector_bytes(t)).min(file_len);
    let end = tracks
        .iter()
        .skip(i.saturating_add(1))
        .find(|n| n.file == t.file)
        .and_then(|n| frames(n)?.checked_mul(sector_bytes(n)))
        .unwrap_or(file_len)
        .clamp(start, file_len);
    (start, end)
}

/// A path in a cue sheet, joined to the sheet's folder; names that climb out of it or are absolute are refused.
fn beside(sheet: &Path, name: &str) -> Option<PathBuf> {
    let rel = Path::new(&name.replace('\\', "/")).to_path_buf();
    if rel.as_os_str().is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(sheet.parent()?.join(rel))
}

/// True for a regular file. Checked before opening anything a person names: opening a FIFO (or a
/// device) for reading can wait forever for a writer.
fn is_regular_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

fn is_cue(path: &Path) -> bool {
    if !is_regular_file(path) {
        return false;
    }
    if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("cue")) {
        return true;
    }
    let Ok(f) = File::open(path) else { return false };
    let mut head = Vec::new();
    f.take(64 << 10).read_to_end(&mut head).is_ok_and(|_| !head.contains(&0) && cue::looks_like_cue(&String::from_utf8_lossy(&head)))
}

fn read_cue(path: &Path) -> Result<CueSheet, ToolError> {
    if !is_regular_file(path) {
        return Err(ToolError::invalid(format!("{} is not a file", path.display())));
    }
    let mut text = Vec::new();
    File::open(path).and_then(|f| f.take(cue::MAX_BYTES as u64).read_to_end(&mut text)).map_err(|e| ToolError::io(path.display(), e))?;
    Ok(cue::parse(&String::from_utf8_lossy(&text)))
}

fn volume_summary(src: &FileSource, first_block: Option<u32>) -> Option<Value> {
    let v = Volume::open_numbered(src, first_block).ok()?;
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
        let source = open_file(&p, &ctx.cancel)?;
        if iso9660::detect(&source).is_none() {
            return Err(ToolError::unsupported(format!("{} doesn't hold an ISO 9660 disc volume", name_of(&p)))
                .with_remediation("This reads ISO 9660 images: a cooked .iso, or a raw .bin track from a .cue sheet (pass the .cue). firmware_regions_list can look for other filesystems."));
        }
        return Ok(Disc { name: name_of(&p), image: p, source, first_block: None, track: None, others: Vec::new(), sheet: None, later_data: Vec::new() });
    }
    let sheet = read_cue(&p)?;
    if sheet.tracks.is_empty() {
        return Err(ToolError::unsupported(format!("{} lists no tracks", name_of(&p))));
    }
    let mut report = Vec::new();
    let mut found: Vec<(CueTrack, PathBuf, FileSource, Option<u32>, Value)> = Vec::new();
    // The file being walked and the disc block it starts at (known while every file before it was
    // found: their sizes add up); a track's first block is then its file's plus its INDEX 01.
    let mut current: Option<(&str, Option<u64>)> = None;
    let mut next_block: Option<u64> = Some(0);
    for (i, t) in sheet.tracks.iter().enumerate() {
        let located = beside(&p, &t.file).filter(|f| is_regular_file(f));
        let file_len = located.as_ref().and_then(|f| std::fs::metadata(f).ok()).map(|m| m.len());
        if current.is_none_or(|(f, _)| f != t.file) {
            current = Some((t.file.as_str(), next_block));
            next_block = next_block.zip(file_len).and_then(|(s, len)| s.checked_add(len / sector_bytes(t)));
        }
        let (start, end) = track_span(&sheet.tracks, i, file_len.unwrap_or(0));
        let first_block = current.and_then(|(_, b)| b).and_then(|b| b.checked_add(start / sector_bytes(t))).and_then(|b| u32::try_from(b).ok());
        let mut row = json!({"track": t.number, "file": t.file, "mode": t.mode, "data": t.data, "found": located.is_some(), "offset_in_file": start});
        if let Some(file) = located
            && t.data
            && let Ok(mut src) = open_file(&file, &ctx.cancel)
        {
            src.start = start;
            src.len = end.saturating_sub(start);
            if let Some(info) = volume_summary(&src, first_block) {
                row["volume"] = info.clone();
                found.push((t.clone(), file, src, first_block, info));
            }
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
    let others = found.iter().enumerate().filter(|(i, _)| *i != pick).map(|(_, (t, _, _, _, info))| json!({"track": t.number, "volume": info})).collect();
    let (t, file, source, first_block, _) = found.swap_remove(pick);
    let later_data = sheet.tracks.iter().filter(|x| x.data && x.number > t.number && x.file != t.file).cloned().collect();
    Ok(Disc { name: name_of(&file), image: file, source, first_block, track: Some(t), others, sheet: Some(json!(report)), later_data })
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
        "offset_in_file": disc.source.start,
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

fn list_all(disc: &Disc, vol: &Volume<'_, FileSource>) -> Listing {
    let mut list = vol.list(&Limits { max_entries: MAX_ENTRIES, ..Limits::default() });
    let past_end = list.entries.iter().filter(|e| !e.is_dir && e.available < e.size).count();
    if past_end > 0
        && let Some(why) = disc.elsewhere()
    {
        list.warnings.insert(0, format!("{past_end} file(s) lie past the end of {}: {why}", disc.name));
        list.complete = false;
    }
    list
}

/// What the listing says about one file, for the page of entries.
fn entry_json(e: &DiscEntry) -> Value {
    let mut v = json!({"path": e.path, "size": e.size, "block": e.lba, "present": e.available == e.size});
    if let Some(why) = e.unreadable {
        v["unreadable"] = json!(why);
        v["why_not_read"] = json!(why.describe());
    }
    v
}

fn game_disc_files_list(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: DiscListParams = parse_params(params)?;
    let disc = open_disc(ctx, p.path.as_deref(), p.track)?;
    ctx.report(None, format!("Reading the directories of {}", disc.name));
    let vol = disc.volume()?;
    let list = list_all(&disc, &vol);
    ctx.cancel.check()?;
    let prefix = norm_prefix(p.prefix.as_deref());
    let files: Vec<&DiscEntry> = list.entries.iter().filter(|e| !e.is_dir && under(&e.path, &prefix)).collect();
    let folders = list.entries.iter().filter(|e| e.is_dir).count();
    let total_files = list.entries.iter().filter(|e| !e.is_dir).count();
    let bytes: u64 = list.entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
    let limit = p.limit.clamp(1, 20_000);
    let page: Vec<Value> = files.iter().skip(p.offset).take(limit).map(|e| entry_json(e)).collect();
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
    if p.max_bytes.is_some_and(|b| b == 0 || b > 64 << 30) {
        return Err(ToolError::invalid("max_bytes must be 1..=68719476736 (64 GiB)"));
    }
    let disc = open_disc(ctx, p.path.as_deref(), p.track)?;
    let max_bytes = p.max_bytes.unwrap_or_else(|| default_max_bytes(disc.source.len));
    let vol = disc.volume()?;
    ctx.report(None, format!("Reading the directories of {}", disc.name));
    let list = list_all(&disc, &vol);
    ctx.cancel.check()?;
    let prefix = norm_prefix(p.prefix.as_deref());
    let chosen: Vec<&DiscEntry> = list.entries.iter().filter(|e| under(&e.path, &prefix)).collect();
    let wanted_files = chosen.iter().filter(|e| !e.is_dir).count();
    if chosen.is_empty() {
        return Err(ToolError::new(ErrorCode::NotFound, format!("Nothing under {:?} on {}", p.prefix.unwrap_or_default(), disc.name))
            .with_remediation("game_disc_files_list shows the paths on the disc."));
    }
    let root = create_absent_dir(&p.output_directory)?;
    let guard = UnlessWritten::new(&root);
    let (mut written, mut bytes, mut folders) = (0usize, 0u64, 0usize);
    let mut skipped: Vec<Value> = Vec::new();
    let mut skipped_count = 0usize;
    let mut manifest: Vec<Value> = Vec::new();
    // Extents already written, with the first path written from each: overlapping directory
    // records could otherwise write the same bytes under thousands of names.
    let mut extents: HashMap<(u32, u64), &str> = HashMap::new();
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
        let Some(dest) = join_under(&root, &comps) else {
            note(&mut skipped, &e.path, "refused: the name would leave the output folder".into());
            continue;
        };
        if e.is_dir {
            // Folders a file needs are made with it; this keeps empty ones, as many as max_files.
            if folders >= p.max_files {
                note(&mut skipped, &e.path, "folder limit reached (max_files)".into());
            } else if let Err(err) = std::fs::create_dir_all(&dest) {
                note(&mut skipped, &e.path, err.to_string());
            } else {
                folders += 1;
            }
            continue;
        }
        if let Some(why) = e.unreadable {
            note(&mut skipped, &e.path, why.describe().into());
            continue;
        }
        if e.available < e.size {
            let why = match disc.elsewhere() {
                Some(track) => format!("not in this track's file ({} of {} bytes present); {track}", e.available, e.size),
                None => format!("runs past the end of the image ({} of {} bytes present)", e.available, e.size),
            };
            note(&mut skipped, &e.path, why);
            continue;
        }
        if e.size > 0
            && let Some(first) = extents.get(&(e.lba, e.size))
        {
            note(&mut skipped, &e.path, format!("same data as {first} (the directory points both at one extent); written once"));
            continue;
        }
        if written >= p.max_files || bytes.saturating_add(e.size) > max_bytes {
            note(&mut skipped, &e.path, format!("file count or byte limit reached (max_files {}, max_bytes {max_bytes})", p.max_files));
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
                if e.size > 0 {
                    extents.insert((e.lba, e.size), e.path.as_str());
                }
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
    result["max_bytes"] = json!(max_bytes);
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
    if !is_regular_file(path) {
        return None;
    }
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

    /// Run on a thread with a time limit, so a walk or an extraction that runs away fails the test
    /// instead of hanging it.
    fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_secs(secs)).expect("it finished in time")
    }

    fn skipped_reason(ev: &Value, path: &str) -> String {
        ev["result"]["skipped"].as_array().and_then(|s| s.iter().find(|x| x["path"] == path)).and_then(|x| x["reason"].as_str()).unwrap_or_default().to_string()
    }

    fn files_under(dir: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).expect("lists").flatten() {
            let p = entry.path();
            if p.is_dir() {
                out.extend(files_under(&p));
            } else {
                out.push(p.display().to_string());
            }
        }
        out
    }

    /// On Windows `root.join("C:x")` is "x in drive C's current folder", so `C:ESCAPE…` wrote into
    /// the process's folder and `D:EVIL.BAT` onto drive D; device names open devices.
    #[test]
    fn names_that_would_leave_the_folder_are_refused_and_nothing_is_written_outside() {
        let dir = tempfile::tempdir().expect("temp dir");
        let img = dir.path().join("names.iso");
        let nodes = vec![
            Node::File("ok.txt", b"fine".to_vec()),
            Node::File("C:ESCAPE_ISOFIX_7F3A", b"escaped".to_vec()),
            Node::File("D:EVIL.BAT", b"@echo off".to_vec()),
            Node::File("NUL", b"device".to_vec()),
            Node::File("nul.txt", b"device".to_vec()),
            Node::File("COM1", b"device".to_vec()),
            Node::Dir("X:Y", vec![Node::File("z", b"zz".to_vec())]),
        ];
        std::fs::write(&img, iso_image(&nodes, Wrap::Cooked, "S", "V", false)).expect("write");
        let e = engine();
        let out = dir.path().join("out");
        let ev = call(&e, "game_disc_files_extract", json!({"path": img, "output_directory": out})).expect("extracts");
        assert_eq!(ev["result"]["files_written"], 1, "{:?}", ev["result"]);
        for bad in ["C:ESCAPE_ISOFIX_7F3A", "D:EVIL.BAT", "NUL", "NUL.TXT", "COM1", "X:Y", "X:Y/Z"] {
            assert!(skipped_reason(&ev, bad).starts_with("refused"), "{bad}: {:?}", ev["result"]["skipped"]);
        }
        assert_eq!(files_under(&out), vec![out.join("OK.TXT").display().to_string()]);
        let here = std::env::current_dir().expect("cwd");
        assert!(!here.join("ESCAPE_ISOFIX_7F3A").exists(), "a file was written outside the output folder");
    }

    /// Overlapping extents: thousands of records naming the same block (or neighbouring ones) once
    /// wrote 8 GiB out of a 1 MiB image.
    #[test]
    fn overlapping_extents_are_written_once_and_output_is_bounded_by_the_image() {
        use craft_anything_testkit::disc::{FIRST_BLOCK, SECTOR, cooked_volume, pack_records, record};
        let names: Vec<String> = (0..20_000).map(|i| format!("F{i:05}")).collect();
        let size = 256 << 10;
        let root_blocks = (pack_records(&names.iter().map(|n| record(0, size, false, n.as_bytes())).collect::<Vec<_>>()).len() + 1) as u32;
        let data = FIRST_BLOCK + root_blocks;
        let mut recs = vec![record(FIRST_BLOCK, root_blocks * SECTOR as u32, true, &[0])];
        for (i, n) in names.iter().enumerate() {
            // Half name one extent; the others start one block apart, each overlapping the next.
            let lba = if i.is_multiple_of(2) { data } else { data + (i as u32 / 2) % 128 };
            recs.push(record(lba, size, false, n.as_bytes()));
        }
        let mut blocks = pack_records(&recs);
        blocks.resize(root_blocks as usize + 256, [7u8; SECTOR]);
        let image = cooked_volume(&blocks, root_blocks * SECTOR as u32);
        let image_len = image.len() as u64;
        let dir = tempfile::tempdir().expect("temp dir");
        let img = dir.path().join("overlap.iso");
        std::fs::write(&img, image).expect("write");
        let out = dir.path().join("out");
        let ev = within(30, move || {
            let e = engine();
            call(&e, "game_disc_files_extract", json!({"path": img, "output_directory": out})).expect("extracts")
        });
        let written = ev["result"]["bytes_written"].as_u64().unwrap_or(u64::MAX);
        assert!(written <= 2 * image_len, "{written} bytes written from a {image_len}-byte image");
        assert!(skipped_reason(&ev, "F00002").contains("same data as F00000"), "{:?}", skipped_reason(&ev, "F00002"));
        assert!(ev["result"]["skipped"].as_array().is_some_and(|s| s.iter().any(|x| x["reason"].as_str().is_some_and(|r| r.contains("byte limit")))));
        assert_eq!(ev["coverage"]["status"], "partial");
    }

    /// Folders that overlap, list each other under ever longer paths and claim 64 MiB each: the
    /// tools finish quickly, bounded, and say the listing is incomplete.
    #[test]
    fn hostile_directories_finish_in_time_and_stay_bounded() {
        let dir = tempfile::tempdir().expect("temp dir");
        let img = dir.path().join("hostile.iso");
        std::fs::write(&img, craft_anything_testkit::disc::overlapping_dirs(512)).expect("write");
        let out = dir.path().join("out");
        let started = std::time::Instant::now();
        let (list, extract) = within(60, move || {
            let e = engine();
            let list = call(&e, "game_disc_files_list", json!({"path": img})).expect("lists");
            let extract = call(&e, "game_disc_files_extract", json!({"path": img, "output_directory": out, "max_files": 50}));
            (list, extract)
        });
        assert!(started.elapsed() < std::time::Duration::from_secs(20), "took {:?}", started.elapsed());
        assert_eq!(list["result"]["complete"], false);
        let warnings = list["result"]["warnings"].to_string();
        assert!(warnings.contains("add up to more than"), "{warnings}");
        assert!(list["result"]["folders"].as_u64().is_some_and(|n| n > 1000));
        // Folders only: nothing to write, but it ran to the end within the budgets.
        assert!(extract.is_ok(), "{extract:?}");
    }

    #[test]
    fn form_2_streams_are_not_extracted_as_data() {
        let dir = tempfile::tempdir().expect("temp dir");
        let img = dir.path().join("ps1.bin");
        let nodes = vec![Node::File("system.cnf", b"BOOT = cdrom:\\SLUS_000.01;1".to_vec()), Node::XaFile("movie.str", vec![5u8; 4096], 0x1000)];
        std::fs::write(&img, iso_image(&nodes, Wrap::RawMode2Form1, "PLAYSTATION", "PS1", false)).expect("write");
        let e = engine();
        let ev = call(&e, "game_disc_files_list", json!({"path": img})).expect("lists");
        let movie = ev["result"]["entries"].as_array().and_then(|a| a.iter().find(|x| x["path"] == "MOVIE.STR").cloned()).unwrap_or_default();
        assert_eq!(movie["unreadable"], "form-2", "{movie}");
        let out = dir.path().join("out");
        let ev = call(&e, "game_disc_files_extract", json!({"path": img, "output_directory": out})).expect("extracts");
        assert_eq!(ev["result"]["files_written"], 1);
        assert!(skipped_reason(&ev, "MOVIE.STR").contains("Mode 2 Form 2"), "{:?}", ev["result"]["skipped"]);
        assert!(!out.join("MOVIE.STR").exists());
    }

    /// Two data tracks in one `.bin`: the second starts at its INDEX 01, and its blocks are
    /// numbered from there.
    #[test]
    fn a_track_in_a_shared_bin_starts_at_its_index() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut bin = iso_image(&tree(), Wrap::RawMode1, "FIRST", "ONE", false);
        assert!(bin.len() <= 40 * 2352);
        bin.resize(40 * 2352, 0);
        let second = vec![Node::File("second.txt", b"track two".to_vec())];
        bin.extend(craft_anything_testkit::disc::iso_image_numbered(&second, Wrap::RawMode1, "SECOND", "TWO", 40));
        std::fs::write(dir.path().join("disc.bin"), &bin).expect("write");
        let sheet = dir.path().join("disc.cue");
        std::fs::write(&sheet, "FILE \"disc.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n  TRACK 02 MODE1/2352\n    INDEX 01 00:00:40\n")
            .expect("write");
        let e = engine();
        let one = call(&e, "game_disc_files_list", json!({"path": sheet})).expect("lists");
        assert_eq!(one["result"]["track"]["number"], 1);
        assert_eq!(one["result"]["files"], 3);
        let two = call(&e, "game_disc_files_list", json!({"path": sheet, "track": 2})).expect("lists track 2");
        assert_eq!(two["result"]["system_id"], "SECOND");
        assert_eq!(two["result"]["first_block"], 40);
        assert_eq!(two["result"]["offset_in_file"], 40 * 2352);
        assert_eq!(two["result"]["entries"][0]["path"], "SECOND.TXT");
        let out = dir.path().join("out");
        call(&e, "game_disc_files_extract", json!({"path": sheet, "track": 2, "output_directory": out})).expect("extracts");
        assert_eq!(std::fs::read(out.join("SECOND.TXT")).expect("file"), b"track two");
    }

    /// A Dreamcast disc with CD audio: the file system spans track 3 and the last data track, and
    /// only track 3's file is read, so files in the last track are missing; the listing says so.
    #[test]
    fn files_in_a_later_track_make_the_listing_incomplete_and_name_the_track() {
        let dir = tempfile::tempdir().expect("temp dir");
        let full = iso_image(&tree(), Wrap::GdRom, "SEGA SEGAKATANA", "DC", false);
        std::fs::write(dir.path().join("g (Track 1).bin"), vec![0u8; 2352 * 4]).expect("write");
        std::fs::write(dir.path().join("g (Track 2).bin"), vec![0u8; 2352 * 4]).expect("write");
        std::fs::write(dir.path().join("g (Track 3).bin"), full.get(..full.len() - 2352).unwrap_or_default()).expect("write");
        std::fs::write(dir.path().join("g (Track 4).bin"), vec![0u8; 2352 * 4]).expect("write");
        std::fs::write(dir.path().join("g (Track 5).bin"), vec![0u8; 2352 * 4]).expect("write");
        let cue = "REM SINGLE-DENSITY AREA\nFILE \"g (Track 1).bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\nFILE \"g (Track 2).bin\" BINARY\n  TRACK 02 AUDIO\n    INDEX 00 00:00:00\n    INDEX 01 00:02:00\nREM HIGH-DENSITY AREA\nFILE \"g (Track 3).bin\" BINARY\n  TRACK 03 MODE1/2352\n    INDEX 01 00:00:00\nFILE \"g (Track 4).bin\" BINARY\n  TRACK 04 AUDIO\n    INDEX 00 00:00:00\n    INDEX 01 00:02:00\nFILE \"g (Track 5).bin\" BINARY\n  TRACK 05 MODE1/2352\n    INDEX 00 00:00:00\n    INDEX 01 00:03:00\n";
        let sheet = dir.path().join("g.cue");
        std::fs::write(&sheet, cue).expect("write");
        let e = engine();
        let ev = call(&e, "game_disc_files_list", json!({"path": sheet})).expect("lists");
        assert_eq!(ev["result"]["track"]["number"], 3);
        assert_eq!(ev["result"]["complete"], false);
        let first = ev["result"]["warnings"][0].as_str().unwrap_or_default().to_string();
        assert!(first.contains("1 file(s) lie past the end") && first.contains("track 5 (g (Track 5).bin)"), "{first}");
        let out = dir.path().join("out");
        let ev = call(&e, "game_disc_files_extract", json!({"path": sheet, "output_directory": out})).expect("extracts");
        assert!(skipped_reason(&ev, "PSP_GAME/SYSDIR/EBOOT.BIN").contains("track 5"), "{:?}", ev["result"]["skipped"]);
        assert_eq!(std::fs::read(out.join("README.TXT")).expect("file"), b"hello disc");
    }

    #[test]
    fn a_folder_or_missing_path_is_never_opened_as_a_sheet() {
        let dir = tempfile::tempdir().expect("temp dir");
        let folder = dir.path().join("x.cue");
        std::fs::create_dir(&folder).expect("dir");
        assert!(!super::is_cue(&folder));
        assert!(super::read_cue(&folder).is_err());
        assert!(super::not_an_archive(&folder).is_none());
        assert!(super::not_an_archive(&dir.path().join("missing.iso")).is_none());
    }

    /// Opening a FIFO for reading waits for a writer; the disc tools check it is a file first.
    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_without_waiting() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fifo = dir.path().join("pipe.cue");
        let made = craft_anything_platform::process::Process::new("mkfifo").arg(&fifo).timeout(std::time::Duration::from_secs(10)).run();
        if made.is_err() || !fifo.exists() {
            return; // no mkfifo here
        }
        let p = fifo.clone();
        let (cue, archive) = within(10, move || (super::is_cue(&p), super::not_an_archive(&p).is_none()));
        assert!(!cue && archive);
        let err = within(10, move || {
            let e = engine();
            call(&e, "game_disc_files_list", json!({"path": fifo}))
        });
        assert!(err.is_err());
    }
}

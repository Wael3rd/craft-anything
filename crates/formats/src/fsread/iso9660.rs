//! ISO 9660 (ECMA-119) disc images, read-only. A game disc is one of:
//!
//! - a cooked image (`.iso`): 2048-byte sectors holding only user data;
//! - a raw track (`.bin` from a `.cue`): 2352-byte sectors, user data at +16 (Mode 1) or +24
//!   (Mode 2 Form 1, after the sub-header);
//! - a Dreamcast GD-ROM high-density track: the volume's block numbers count from 45000, so
//!   directory extents point past the start of the track file.
//!
//! The reader works on a [`Source`] (a byte slice here, a file in the engine) so a multi-gigabyte
//! image is never read whole: only the volume descriptors and the directories are read to list it,
//! and a file's bytes are read when asked for. Joliet names (UCS-2) are used when the disc has them.
//! Everything is bounded (entries, depth, the bytes of one directory and of all of them, the length
//! of a path and of all paths) and only one directory's bytes are held at a time; a corrupt or
//! looping directory becomes a warning, never a panic or a hang, and a [`Source`] can stop a walk.
//!
//! Not read: Rock Ridge extensions, multi-extent files (joined), and files whose CD-XA attributes
//! say they are Mode 2 Form 2 (2324-byte sectors: PlayStation video and streamed audio) or CD
//! audio: they are listed with [`DiscEntry::unreadable`] set, never read as if they were data.

use super::{FsEntry, FsImage, Limits, is_dot, join};
use serde::Serialize;
use std::collections::HashSet;

const SECTOR: usize = 2048;
const SECTOR_U64: u64 = 2048;
/// Where the first volume descriptor sits.
const FIRST_DESCRIPTOR: u64 = 16;
/// Most descriptors walked before giving up on finding a terminator.
const MAX_DESCRIPTORS: u64 = 32;
/// Largest directory read into memory.
const MAX_DIR_BYTES: u64 = 64 << 20;
/// Most directory bytes read in one walk (a real disc's directories take a few megabytes; without
/// this, thousands of folders each claiming 64 MiB would be read one after another).
pub const MAX_DIR_TOTAL: u64 = 64 << 20;
/// Longest path listed, in bytes; a name that would make one longer is skipped with a warning.
pub const MAX_PATH_BYTES: usize = 4096;
/// Most bytes of paths kept for one walk (folders that overlap can list the same records under
/// ever longer paths).
pub const MAX_PATH_TOTAL: usize = 32 << 20;
/// The first block of a Dreamcast GD-ROM high-density area.
pub const GDROM_HIGH_DENSITY: u32 = 45_000;

/// Random access to the bytes of an image.
pub trait Source {
    /// Length in bytes.
    fn len(&self) -> u64;
    /// Fill `buf` from `offset`; false when any of it is out of range or unreadable.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> bool;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// True when whoever asked has given up: a walk stops at the next directory (or every few
    /// thousand records) and says it was stopped.
    fn cancelled(&self) -> bool {
        false
    }
}

impl Source for [u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> bool {
        let Ok(start) = usize::try_from(offset) else { return false };
        let Some(end) = start.checked_add(buf.len()) else { return false };
        match self.get(start..end) {
            Some(s) => {
                buf.copy_from_slice(s);
                true
            }
            None => false,
        }
    }
}

/// How sectors are laid out in the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Layout {
    /// Bytes per sector in the file: 2048 (cooked) or 2352 (raw).
    pub sector_size: u32,
    /// Where the 2048 bytes of user data start in a sector: 0 (cooked), 16 (Mode 1) or 24 (Mode 2 Form 1).
    pub data_offset: u32,
}

impl Layout {
    /// Plain-English name of the layout.
    pub fn describe(&self) -> &'static str {
        match (self.sector_size, self.data_offset) {
            (2048, _) => "cooked 2048-byte sectors",
            (_, 16) => "raw 2352-byte sectors (Mode 1)",
            _ => "raw 2352-byte sectors (Mode 2 Form 1)",
        }
    }
}

const LAYOUTS: [Layout; 3] =
    [Layout { sector_size: 2048, data_offset: 0 }, Layout { sector_size: 2352, data_offset: 16 }, Layout { sector_size: 2352, data_offset: 24 }];

/// Bytes from the start of a file that [`detect`] needs to decide its layout.
pub const DETECT_PREFIX: usize = 16 * 2352 + 24 + 8;

/// The layout whose sector 16 starts with a volume descriptor (`CD001`), if any.
pub fn detect<S: Source + ?Sized>(src: &S) -> Option<Layout> {
    LAYOUTS.iter().copied().find(|l| {
        let mut h = [0u8; 8];
        let Some(off) = FIRST_DESCRIPTOR.checked_mul(u64::from(l.sector_size)).and_then(|v| v.checked_add(u64::from(l.data_offset))) else {
            return false;
        };
        src.read_at(off, &mut h) && h.get(1..6) == Some(b"CD001") && h.first().is_some_and(|t| matches!(t, 0..=3 | 255))
    })
}

fn le16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o.checked_add(2)?)?.try_into().ok()?))
}
fn le32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o.checked_add(4)?)?.try_into().ok()?))
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim_end_matches([' ', '\0']).to_string()
}

fn ucs2(b: &[u8]) -> String {
    let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect();
    String::from_utf16_lossy(&units)
}

/// A file or folder found in a volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscEntry {
    pub path: String,
    pub is_dir: bool,
    /// Size in bytes the directory record states.
    pub size: u64,
    /// First block of the file's data, in the volume's numbering.
    pub lba: u32,
    /// Bytes of the file actually present in the image (less than `size` when the image is cut short).
    pub available: u64,
    /// Set when the file's sectors aren't 2048-byte data sectors, so its bytes can't be read here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unreadable: Option<Unreadable>,
}

/// Why a listed file's bytes aren't read: its CD-XA attributes say its sectors aren't data sectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unreadable {
    /// Mode 2 Form 2 sectors (2324 bytes each, no error correction): PlayStation video and streamed audio.
    #[serde(rename = "form-2")]
    Form2,
    /// CD audio sectors.
    CdAudio,
}

impl Unreadable {
    /// Plain-English reason.
    pub fn describe(self) -> &'static str {
        match self {
            Unreadable::Form2 => {
                "stored in Mode 2 Form 2 sectors (2324 bytes each: video or streamed audio), which this reader doesn't take out; reading them as 2048-byte data would give wrong bytes"
            }
            Unreadable::CdAudio => "CD audio, not data sectors; it isn't read",
        }
    }
}

/// The CD-XA attributes in a directory record's system use field, when it has them.
fn xa_kind(rec: &[u8], name_len: usize) -> Option<Unreadable> {
    let after_name = 33usize.checked_add(name_len)?;
    // The field follows a padding byte when the name's length is even; some mastering tools leave it out.
    let padded = after_name.checked_add(usize::from(name_len.is_multiple_of(2)))?;
    let xa = [padded, after_name].into_iter().filter_map(|s| rec.get(s..)).find(|su| su.get(6..8) == Some(b"XA"))?;
    let attributes = u16::from_be_bytes([*xa.get(4)?, *xa.get(5)?]);
    if attributes & 0x4000 != 0 {
        Some(Unreadable::CdAudio)
    } else if attributes & 0x1000 != 0 {
        Some(Unreadable::Form2)
    } else {
        None
    }
}

/// What a walk of the directory tree found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Listing {
    pub entries: Vec<DiscEntry>,
    pub warnings: Vec<String>,
    /// The whole tree was walked within the limits.
    pub complete: bool,
}

impl Listing {
    fn warn(&mut self, msg: impl Into<String>) {
        if self.warnings.len() < 256 {
            self.warnings.push(msg.into());
        }
    }

    /// Why the walk stopped early: put first and kept even past the warning cap (it happens once a
    /// walk), so a listing cut short always says why.
    fn stopped(&mut self, msg: impl Into<String>) {
        self.headline(msg);
        self.complete = false;
    }

    /// A warning that goes first, ahead of the per-file ones, whatever the cap.
    fn headline(&mut self, msg: impl Into<String>) {
        self.warnings.insert(0, msg.into());
    }
}

/// An opened ISO 9660 volume.
pub struct Volume<'a, S: Source + ?Sized> {
    src: &'a S,
    pub layout: Layout,
    /// What to subtract from a block number to get the sector index in the file (45000 for a GD-ROM
    /// high-density track, otherwise 0).
    pub lba_base: u32,
    pub volume_id: String,
    pub system_id: String,
    /// Names come from a Joliet (UCS-2) tree.
    pub joliet: bool,
    /// Blocks the volume says it spans.
    pub declared_blocks: u32,
    root_lba: u32,
    root_size: u32,
    total_sectors: u64,
}

impl<'a, S: Source + ?Sized> Volume<'a, S> {
    /// Open the volume at the start of `src`. The error says in plain English why it isn't one.
    pub fn open(src: &'a S) -> Result<Volume<'a, S>, String> {
        Self::open_numbered(src, None)
    }

    /// As [`Volume::open`], trying `first_block` first as the number of the image's first block: a
    /// track that starts part-way into a disc (a second track in a shared `.bin`) numbers its blocks
    /// from where it starts on the disc. Block 0 and the GD-ROM high-density area are tried after it.
    pub fn open_numbered(src: &'a S, first_block: Option<u32>) -> Result<Volume<'a, S>, String> {
        let layout = detect(src).ok_or_else(|| "no ISO 9660 volume descriptor at sector 16".to_string())?;
        let sector = u64::from(layout.sector_size);
        let total_sectors = src.len() / sector;
        let mut primary: Option<[u8; SECTOR]> = None;
        let mut joliet: Option<[u8; SECTOR]> = None;
        for n in 0..MAX_DESCRIPTORS {
            let mut d = [0u8; SECTOR];
            let off = (FIRST_DESCRIPTOR + n)
                .checked_mul(sector)
                .and_then(|v| v.checked_add(u64::from(layout.data_offset)))
                .ok_or_else(|| "volume descriptor offset overflows".to_string())?;
            if !src.read_at(off, &mut d) || d.get(1..6) != Some(b"CD001") {
                break;
            }
            match d.first().copied() {
                Some(1) if primary.is_none() => primary = Some(d),
                Some(2) if joliet.is_none() && matches!(d.get(88..91), Some(b"%/@" | b"%/C" | b"%/E")) => joliet = Some(d),
                Some(255) => break,
                _ => {}
            }
        }
        let pvd = primary.ok_or_else(|| "the volume has no primary descriptor".to_string())?;
        let block = le16(&pvd, 128).unwrap_or(0);
        if usize::from(block) != SECTOR {
            return Err(format!("logical block size {block} is not supported (only 2048)"));
        }
        let declared_blocks = le32(&pvd, 80).unwrap_or(0);
        let tree = joliet.as_ref().unwrap_or(&pvd);
        let root_lba = le32(tree, 156 + 2).ok_or("root directory record unreadable")?;
        let root_size = le32(tree, 156 + 10).ok_or("root directory record unreadable")?;
        let mut vol = Volume {
            src,
            layout,
            lba_base: 0,
            volume_id: text(pvd.get(40..72).unwrap_or_default()),
            system_id: text(pvd.get(8..40).unwrap_or_default()),
            joliet: joliet.is_some(),
            declared_blocks,
            root_lba,
            root_size,
            total_sectors,
        };
        // The root directory's own `.` record points at itself; that tells which numbering the file uses.
        let base = first_block.into_iter().chain([0u32, GDROM_HIGH_DENSITY]).find(|b| {
            vol.lba_base = *b;
            vol.points_at_itself(root_lba)
        });
        match base {
            Some(b) => vol.lba_base = b,
            None => return Err("the root directory isn't where the volume says it is (a truncated or damaged image)".to_string()),
        }
        Ok(vol)
    }

    fn points_at_itself(&self, lba: u32) -> bool {
        let mut s = [0u8; SECTOR];
        self.read_block(lba, &mut s) && s.first().is_some_and(|l| *l >= 34) && s.get(32) == Some(&1) && s.get(33) == Some(&0) && le32(&s, 2) == Some(lba)
    }

    fn sector_offset(&self, lba: u64) -> Option<u64> {
        let phys = lba.checked_sub(u64::from(self.lba_base))?;
        phys.checked_mul(u64::from(self.layout.sector_size))?.checked_add(u64::from(self.layout.data_offset))
    }

    fn read_block(&self, lba: u32, out: &mut [u8; SECTOR]) -> bool {
        self.sector_offset(u64::from(lba)).is_some_and(|o| self.src.read_at(o, out))
    }

    /// Bytes of a file starting at block `lba` that the image holds, up to `size`.
    fn available(&self, lba: u32, size: u64) -> u64 {
        let Some(first) = u64::from(lba).checked_sub(u64::from(self.lba_base)) else { return 0 };
        self.total_sectors.saturating_sub(first).saturating_mul(SECTOR_U64).min(size)
    }

    /// Read file bytes: up to `buf.len()` bytes starting `offset` bytes into the extent at block
    /// `lba`. Returns how many were read (fewer than asked at the end of the image).
    pub fn read_at(&self, lba: u32, offset: u64, buf: &mut [u8]) -> usize {
        let mut done = 0usize;
        let mut pos = offset;
        while done < buf.len() {
            let Some(block) = (pos / SECTOR_U64).checked_add(u64::from(lba)) else { break };
            let within = pos % SECTOR_U64;
            let Some(base) = self.sector_offset(block) else { break };
            let Some(start) = base.checked_add(within) else { break };
            let left = buf.len() - done;
            let chunk = if self.layout.sector_size == 2048 {
                // Cooked images are contiguous: read as much as fits in one go.
                left.min(usize::try_from(self.src.len().saturating_sub(start)).unwrap_or(usize::MAX))
            } else {
                left.min(SECTOR - within as usize)
            };
            if chunk == 0 {
                break;
            }
            let Some(dst) = buf.get_mut(done..done + chunk) else { break };
            if !self.src.read_at(start, dst) {
                break;
            }
            done += chunk;
            pos = pos.saturating_add(chunk as u64);
        }
        done
    }

    /// Walk the directory tree within `limits` (and the walk budgets: [`MAX_DIR_TOTAL`],
    /// [`MAX_PATH_BYTES`], [`MAX_PATH_TOTAL`]). Folders are walked depth first in disc order, one
    /// at a time: a folder's bytes are dropped before the next is read, and a folder waiting its
    /// turn is only its entry's index and extent.
    pub fn list(&self, limits: &Limits) -> Listing {
        let mut out = Listing { complete: true, ..Listing::default() };
        let mut walk = Walk { visited: HashSet::new(), dir_bytes: 0, path_bytes: 0, stop: false };
        let mut pending = vec![Pending { entry: None, lba: self.root_lba, size: self.root_size, depth: 0 }];
        while let Some(dir) = pending.pop() {
            if self.src.cancelled() {
                out.stopped("stopped before the end: cancelled");
                break;
            }
            if out.entries.len() >= limits.max_entries {
                out.complete = false;
                break;
            }
            let subdirs = self.read_dir(&dir, limits, &mut out, &mut walk);
            if walk.stop {
                break;
            }
            pending.extend(subdirs.into_iter().rev());
        }
        let flagged = out.entries.iter().filter(|e| e.unreadable.is_some()).count();
        if flagged > 0 {
            out.headline(format!(
                "{flagged} file(s) are Mode 2 Form 2 streams (video, streamed audio) or CD audio; they are listed but their bytes aren't read"
            ));
        }
        out
    }

    /// List one folder's records into `out` and return its sub-folders, in disc order.
    fn read_dir(&self, dir: &Pending, limits: &Limits, out: &mut Listing, walk: &mut Walk) -> Vec<Pending> {
        let mut subdirs = Vec::new();
        // The folder's own path, copied once (it lives in `out.entries`, which grows below).
        let parent = dir.entry.and_then(|i| out.entries.get(i)).map(|e| e.path.clone()).unwrap_or_default();
        let parent = parent.as_str();
        let (lba, size, depth) = (dir.lba, dir.size, dir.depth);
        if depth > limits.max_depth.min(128) {
            out.warn("directory nesting limit reached");
            out.complete = false;
            return subdirs;
        }
        if !walk.visited.insert(lba) {
            out.warn(format!("{}: a directory leads back to one already read; not followed", shown(parent)));
            out.complete = false;
            return subdirs;
        }
        let want = u64::from(size).min(MAX_DIR_BYTES);
        if u64::from(size) > MAX_DIR_BYTES {
            out.warn(format!("{}: directory of {size} bytes cut to {MAX_DIR_BYTES}", shown(parent)));
            out.complete = false;
        }
        let mut have = want.min(self.available(lba, want));
        if have < want {
            out.warn(format!("{}: directory runs past the end of the image", shown(parent)));
            out.complete = false;
        }
        let left = MAX_DIR_TOTAL.saturating_sub(walk.dir_bytes);
        if have > left {
            out.stopped(format!(
                "{}: the directories add up to more than {} MiB, more than any real disc holds; the rest aren't read",
                shown(parent),
                MAX_DIR_TOTAL >> 20
            ));
            walk.stop = true;
            have = left;
        }
        walk.dir_bytes = walk.dir_bytes.saturating_add(have);
        let Ok(have_len) = usize::try_from(have) else { return subdirs };
        let mut dir = vec![0u8; have_len];
        let got = self.read_at(lba, 0, &mut dir);
        dir.truncate(got);
        let mut pos = 0usize;
        let mut records = 0usize;
        let mut too_long = 0usize;
        while pos < dir.len() {
            records += 1;
            if records.is_multiple_of(4096) && self.src.cancelled() {
                out.stopped("stopped before the end: cancelled");
                walk.stop = true;
                break;
            }
            let rec_len = usize::from(dir.get(pos).copied().unwrap_or(0));
            if rec_len == 0 {
                // Records never cross a sector: padding to the next one.
                pos = (pos / SECTOR + 1).saturating_mul(SECTOR);
                continue;
            }
            let in_sector = pos % SECTOR;
            let rec = if rec_len >= 34 && in_sector + rec_len <= SECTOR { dir.get(pos..pos + rec_len) } else { None };
            let Some(rec) = rec else {
                out.warn(format!("{}: damaged directory record; skipped to the next sector", shown(parent)));
                out.complete = false;
                pos = (pos / SECTOR + 1).saturating_mul(SECTOR);
                continue;
            };
            pos += rec_len;
            let name_len = usize::from(rec.get(32).copied().unwrap_or(0));
            let Some(id) = rec.get(33..33 + name_len) else {
                out.warn(format!("{}: directory record with a name past its end", shown(parent)));
                out.complete = false;
                continue;
            };
            if id == [0] || id == [1] {
                continue;
            }
            let (Some(e_lba), Some(e_size)) = (le32(rec, 2), le32(rec, 10)) else { continue };
            let flags = rec.get(25).copied().unwrap_or(0);
            let is_dir = flags & 2 != 0;
            let name = clean_name(&if self.joliet { ucs2(id) } else { text(id) }, is_dir, limits.max_name);
            if is_dot(&name) {
                continue;
            }
            if out.entries.len() >= limits.max_entries {
                out.stopped("entry limit reached");
                break;
            }
            let path_len = if parent.is_empty() { name.len() } else { parent.len().saturating_add(1).saturating_add(name.len()) };
            if path_len > MAX_PATH_BYTES {
                too_long += 1;
                continue;
            }
            if walk.path_bytes.saturating_add(path_len) > MAX_PATH_TOTAL {
                out.stopped(format!("the paths listed add up to more than {} MiB; the rest aren't listed", MAX_PATH_TOTAL >> 20));
                walk.stop = true;
                break;
            }
            walk.path_bytes = walk.path_bytes.saturating_add(path_len);
            let path = join(parent, &name);
            if is_dir {
                subdirs.push(Pending { entry: Some(out.entries.len()), lba: e_lba, size: e_size, depth: depth.saturating_add(1) });
                out.entries.push(DiscEntry { path, is_dir: true, size: 0, lba: e_lba, available: 0, unreadable: None });
            } else {
                if flags & 0x80 != 0 {
                    out.warn(format!("{path}: continues in another extent; only the first part is listed"));
                    out.complete = false;
                }
                let avail = self.available(e_lba, u64::from(e_size));
                if avail < u64::from(e_size) {
                    out.warn(format!("{path}: runs past the end of the image ({avail} of {e_size} bytes present)"));
                    out.complete = false;
                }
                let unreadable = xa_kind(rec, name_len);
                out.entries.push(DiscEntry { path, is_dir: false, size: u64::from(e_size), lba: e_lba, available: avail, unreadable });
            }
        }
        if too_long > 0 {
            out.warn(format!("{}: {too_long} name(s) would make paths longer than {MAX_PATH_BYTES} bytes; skipped", shown(parent)));
            out.complete = false;
        }
        subdirs
    }
}

/// Running totals of one walk.
struct Walk {
    visited: HashSet<u32>,
    /// Directory bytes read so far.
    dir_bytes: u64,
    /// Bytes of the paths listed so far.
    path_bytes: usize,
    /// A budget ran out or the walk was cancelled: nothing more is read.
    stop: bool,
}

/// A folder waiting to be read: its entry in the listing (None for the root), extent and depth.
struct Pending {
    entry: Option<usize>,
    lba: u32,
    size: u32,
    depth: usize,
}

fn shown(path: &str) -> &str {
    if path.is_empty() { "/" } else { path }
}

/// A name as one path component: version suffix and trailing dot dropped, separators replaced.
/// Otherwise names stay as the disc spells them (a `:` or a Windows device name included): the
/// listing reports what is there, and the tools that write files refuse names that aren't safe.
fn clean_name(raw: &str, is_dir: bool, max: usize) -> String {
    let mut s = raw.to_string();
    if !is_dir
        && let Some(i) = s.rfind(';')
        && s.get(i + 1..).is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
    {
        s.truncate(i);
    }
    if !is_dir && s.ends_with('.') && s.len() > 1 {
        s.pop();
    }
    let s: String = s.chars().map(|c| if matches!(c, '/' | '\\' | '\0') { '_' } else { c }).take(max.clamp(1, 255)).collect();
    if s.is_empty() { "_".to_string() } else { s }
}

/// Read a cooked or raw image held in memory and recover its files (the [`super::read`] seam).
pub fn read(data: &[u8], limits: &Limits) -> FsImage {
    let mut img = FsImage::new("iso9660");
    let vol = match Volume::open(data) {
        Ok(v) => v,
        Err(why) => {
            img.warn(why);
            img.complete = false;
            return img;
        }
    };
    let list = vol.list(limits);
    img.complete = list.complete;
    for w in list.warnings {
        img.warn(w);
    }
    for e in list.entries {
        if e.is_dir {
            img.entries.push(FsEntry::dir(e.path));
            continue;
        }
        let fits =
            e.size <= limits.max_file_bytes && img.bytes.saturating_add(e.size) <= limits.max_total_bytes && e.available == e.size && e.unreadable.is_none();
        let bytes = usize::try_from(e.size).ok().filter(|_| fits).and_then(|n| {
            let mut buf = vec![0u8; n];
            (vol.read_at(e.lba, 0, &mut buf) == n).then_some(buf)
        });
        match (&bytes, e.unreadable) {
            (Some(b), _) => img.bytes = img.bytes.saturating_add(b.len() as u64),
            (None, Some(why)) => {
                img.warn(format!("{}: {}", e.path, why.describe()));
                img.complete = false;
            }
            (None, None) if e.size > 0 => {
                img.warn(format!("{}: file data not recovered (size or bounds)", e.path));
                img.complete = false;
            }
            (None, None) => {}
        }
        img.entries.push(FsEntry::file(e.path, e.size, bytes));
    }
    img
}

#[cfg(test)]
mod tests {
    use super::super::FsKind;
    use super::*;
    use craft_anything_testkit::disc::{GDROM_HIGH_DENSITY, Node, Wrap, iso_image as image};

    fn tree() -> Vec<Node> {
        vec![
            Node::File("readme.txt", b"hello disc".to_vec()),
            Node::Dir(
                "PSP_GAME",
                vec![
                    Node::File("param.sfo", vec![7u8; 5000]),
                    Node::Dir("SYSDIR", vec![Node::File("eboot.bin", (0..=255u8).cycle().take(4096 + 17).collect())]),
                ],
            ),
        ]
    }

    fn check(img: &[u8], base: u32, layout_size: u32, off: u32) {
        let vol = Volume::open(img).expect("opens");
        assert_eq!(vol.layout, Layout { sector_size: layout_size, data_offset: off });
        assert_eq!(vol.lba_base, base);
        assert_eq!(vol.system_id, "PSP GAME");
        assert_eq!(vol.volume_id, "TESTDISC");
        let fs = read(img, &Limits::default());
        assert!(fs.complete, "{:?}", fs.warnings);
        let f = |p: &str| fs.entries.iter().find(|e| e.path == p).expect("entry present");
        assert_eq!(f("README.TXT").data.as_deref(), Some(b"hello disc".as_slice()));
        assert_eq!(f("PSP_GAME").kind, FsKind::Dir);
        assert_eq!(f("PSP_GAME/PARAM.SFO").data.as_deref(), Some(vec![7u8; 5000].as_slice()));
        let boot: Vec<u8> = (0..=255u8).cycle().take(4096 + 17).collect();
        assert_eq!(f("PSP_GAME/SYSDIR/EBOOT.BIN").data.as_deref(), Some(boot.as_slice()));
    }

    /// A cooked image with both trees: the primary one names a file `LONGFI~1.TXT`, the Joliet one
    /// (UCS-2, escape `%/E`) gives the names in full, one with an accent.
    fn joliet_image() -> Vec<u8> {
        use craft_anything_testkit::disc::{pack_records, record};
        let ucs2 = |s: &str| s.encode_utf16().flat_map(u16::to_be_bytes).collect::<Vec<u8>>();
        let descriptor = |kind: u8, root: u32| {
            let mut d = [0u8; SECTOR];
            d[0] = kind;
            d[1..6].copy_from_slice(b"CD001");
            d[6] = 1;
            d[8..40].fill(b' ');
            d[8..16].copy_from_slice(b"PSP GAME");
            d[40..72].fill(b' ');
            d[40..47].copy_from_slice(b"JOLIET1");
            d[80..84].copy_from_slice(&24u32.to_le_bytes());
            d[128..130].copy_from_slice(&2048u16.to_le_bytes());
            if kind == 2 {
                d[88..91].copy_from_slice(b"%/E");
            }
            d[156..190].copy_from_slice(&record(root, SECTOR as u32, true, &[0])[..34]);
            d
        };
        let mut terminator = [0u8; SECTOR];
        terminator[0] = 255;
        terminator[1..6].copy_from_slice(b"CD001");
        let dir = |root: u32, files: Vec<Vec<u8>>| {
            let mut records = vec![record(root, SECTOR as u32, true, &[0]), record(root, SECTOR as u32, true, &[1])];
            records.extend(files);
            pack_records(&records).remove(0)
        };
        let primary = dir(20, vec![record(22, 5, false, b"LONGFI~1.TXT;1")]);
        let joliet = dir(21, vec![record(22, 5, false, &ucs2("Long File Name.txt;1")), record(23, 5, false, &ucs2("Café.txt;1"))]);
        let data = |s: &[u8]| {
            let mut b = [0u8; SECTOR];
            b[..s.len()].copy_from_slice(s);
            b
        };
        let mut sectors = vec![[0u8; SECTOR]; 16];
        sectors.extend([descriptor(1, 20), descriptor(2, 21), terminator, [0u8; SECTOR], primary, joliet, data(b"hello"), data(b"world")]);
        sectors.concat()
    }

    #[test]
    fn joliet_names_are_read_in_full_from_the_joliet_tree() {
        let img = joliet_image();
        let vol = Volume::open(img.as_slice()).expect("opens");
        assert!(vol.joliet);
        let fs = read(&img, &Limits::default());
        assert!(fs.complete, "{:?}", fs.warnings);
        let f = |p: &str| fs.entries.iter().find(|e| e.path == p).unwrap_or_else(|| panic!("{p} missing: {:?}", fs.entries));
        assert_eq!(f("Long File Name.txt").data.as_deref(), Some(b"hello".as_slice()));
        assert_eq!(f("Café.txt").data.as_deref(), Some(b"world".as_slice()));
        assert!(fs.entries.iter().all(|e| e.path != "LONGFI~1.TXT"), "the primary tree's short names aren't listed too");
    }

    #[test]
    fn reads_a_cooked_image() {
        check(&image(&tree(), Wrap::Cooked, "PSP GAME", "TESTDISC", false), 0, 2048, 0);
    }

    #[test]
    fn reads_raw_mode_1_and_mode_2_form_1() {
        check(&image(&tree(), Wrap::RawMode1, "PSP GAME", "TESTDISC", false), 0, 2352, 16);
        check(&image(&tree(), Wrap::RawMode2Form1, "PSP GAME", "TESTDISC", false), 0, 2352, 24);
    }

    #[test]
    fn reads_a_gdrom_high_density_track() {
        check(&image(&tree(), Wrap::GdRom, "PSP GAME", "TESTDISC", false), GDROM_HIGH_DENSITY, 2352, 16);
    }

    #[test]
    fn a_directory_that_leads_back_is_not_followed() {
        let img = image(&tree(), Wrap::Cooked, "PSP GAME", "TESTDISC", true);
        let fs = read(&img, &Limits::default());
        assert!(!fs.complete);
        assert!(fs.warnings.iter().any(|w| w.contains("leads back")), "{:?}", fs.warnings);
        assert!(fs.entries.iter().any(|e| e.path == "PSP_GAME/SYSDIR/EBOOT.BIN"));
    }

    #[test]
    fn a_cut_short_image_warns_about_the_file_it_loses() {
        let img = image(&tree(), Wrap::Cooked, "PSP GAME", "TESTDISC", false);
        let cut = img.get(..img.len() - 3000).unwrap_or_default();
        let fs = read(cut, &Limits::default());
        assert!(fs.warnings.iter().any(|w| w.contains("past the end") || w.contains("not recovered")), "{:?}", fs.warnings);
        assert!(!fs.complete);
        let ok = fs.entries.iter().find(|e| e.path == "README.TXT").and_then(|e| e.data.clone());
        assert_eq!(ok.as_deref(), Some(b"hello disc".as_slice()));
    }

    #[test]
    fn limits_are_honoured() {
        let img = image(&tree(), Wrap::Cooked, "PSP GAME", "TESTDISC", false);
        let fs = read(&img, &Limits { max_entries: 2, ..Limits::default() });
        assert!(fs.entries.len() <= 2 && !fs.complete);
        let fs = read(&img, &Limits { max_depth: 0, ..Limits::default() });
        assert!(!fs.complete);
        let fs = read(&img, &Limits { max_file_bytes: 100, ..Limits::default() });
        assert!(fs.entries.iter().any(|e| e.path == "README.TXT" && e.data.is_some()));
        assert!(fs.entries.iter().any(|e| e.path == "PSP_GAME/PARAM.SFO" && e.data.is_none()));
    }

    #[test]
    fn names_lose_version_suffix_and_trailing_dot() {
        assert_eq!(clean_name("FILE.;1", false, 255), "FILE");
        assert_eq!(clean_name("A.BIN;12", false, 255), "A.BIN");
        assert_eq!(clean_name("DIR.X", true, 255), "DIR.X");
        assert_eq!(clean_name("a/b\\c", false, 255), "a_b_c");
        assert_eq!(clean_name("", false, 255), "_");
    }

    #[test]
    fn form_2_and_audio_files_are_listed_but_not_read() {
        let nodes = vec![
            Node::File("plain.bin", vec![1u8; 3000]),
            Node::XaFile("movie.str", vec![2u8; 4096], 0x1000 | 0x2000),
            Node::XaFile("track.da", vec![3u8; 2048], 0x4000),
            Node::XaFile("form1.dat", vec![4u8; 2100], 0x0800),
        ];
        for wrap in [Wrap::Cooked, Wrap::RawMode2Form1] {
            let img = image(&nodes, wrap, "PLAYSTATION", "XA", false);
            let list = Volume::open(img.as_slice()).expect("opens").list(&Limits::default());
            let kind = |p: &str| list.entries.iter().find(|e| e.path == p).map(|e| e.unreadable);
            assert_eq!(kind("PLAIN.BIN"), Some(None));
            assert_eq!(kind("FORM1.DAT"), Some(None));
            assert_eq!(kind("MOVIE.STR"), Some(Some(Unreadable::Form2)));
            assert_eq!(kind("TRACK.DA"), Some(Some(Unreadable::CdAudio)));
            assert!(list.warnings.iter().any(|w| w.contains("2 file(s) are Mode 2 Form 2")), "{:?}", list.warnings);
            let fs = read(&img, &Limits::default());
            let data = |p: &str| fs.entries.iter().find(|e| e.path == p).and_then(|e| e.data.clone());
            assert_eq!(data("FORM1.DAT"), Some(vec![4u8; 2100]));
            assert_eq!(data("MOVIE.STR"), None, "wrong bytes must not pass for the file");
            assert_eq!(data("TRACK.DA"), None);
            assert!(!fs.complete);
        }
    }

    /// Folders that overlap list the same records again and again, each time under a longer path
    /// (once ~8.5 GB of paths from a 1 MiB image). Paths are capped one by one and in total.
    #[test]
    fn overlapping_folders_with_long_names_stay_within_the_path_budgets() {
        // 40 sectors: the chain of folders gets 20 deep before the paths add up (the cap on one
        // path is what stops it); 512 sectors (1 MiB): the paths add up first.
        for (blocks, expect) in [(40, "longer than 4096"), (512, "paths listed add up to more than 32 MiB")] {
            let img = craft_anything_testkit::disc::overlapping_dirs(blocks);
            assert!(img.len() < 1_200_000);
            let started = std::time::Instant::now();
            let list = Volume::open(img.as_slice()).expect("opens").list(&Limits::default());
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "took {:?}", started.elapsed());
            assert!(!list.complete);
            let total: usize = list.entries.iter().map(|e| e.path.len()).sum();
            assert!(total <= MAX_PATH_TOTAL, "{total} bytes of paths");
            assert!(list.entries.iter().all(|e| e.path.len() <= MAX_PATH_BYTES));
            assert!(list.warnings.iter().any(|w| w.contains(expect)), "{blocks}: {:?}", list.warnings.last());
        }
    }

    /// Counts what a walk reads, and gives up after `cancel_after` reads.
    struct Counting<'a> {
        data: &'a [u8],
        read: std::cell::Cell<u64>,
        reads: std::cell::Cell<u64>,
        cancel_after: u64,
    }

    impl Source for Counting<'_> {
        fn len(&self) -> u64 {
            self.data.len() as u64
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> bool {
            self.read.set(self.read.get() + buf.len() as u64);
            self.reads.set(self.reads.get() + 1);
            self.data.read_at(offset, buf)
        }
        fn cancelled(&self) -> bool {
            self.reads.get() >= self.cancel_after
        }
    }

    /// A thousand folders, each claiming 4 GiB and spanning the rest of the image.
    fn many_big_folders(count: u32) -> Vec<u8> {
        use craft_anything_testkit::disc::{FIRST_BLOCK, SECTOR, cooked_volume, pack_records, record};
        let names: Vec<String> = (0..count).map(|i| format!("D{i:04}")).collect();
        let root_blocks = u32::try_from(pack_records(&names.iter().map(|n| record(0, 0, true, n.as_bytes())).collect::<Vec<_>>()).len() + 1).unwrap_or(0);
        let mut recs = vec![record(FIRST_BLOCK, root_blocks * SECTOR as u32, true, &[0])];
        recs.extend(names.iter().enumerate().map(|(i, n)| record(FIRST_BLOCK + root_blocks + i as u32, u32::MAX, true, n.as_bytes())));
        let mut blocks = pack_records(&recs);
        blocks.resize(root_blocks as usize + count as usize, [0u8; SECTOR]);
        cooked_volume(&blocks, root_blocks * SECTOR as u32)
    }

    #[test]
    fn directory_bytes_read_have_a_total_budget() {
        let img = many_big_folders(1000);
        let src = Counting { data: &img, read: 0.into(), reads: 0.into(), cancel_after: u64::MAX };
        let started = std::time::Instant::now();
        let list = Volume::open(&src).expect("opens").list(&Limits::default());
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "took {:?}", started.elapsed());
        assert!(!list.complete);
        assert!(list.warnings.iter().any(|w| w.contains("directories add up to more than 64 MiB")), "{:?}", list.warnings.last());
        // Without the budget this reads about 1 GB (each folder rereads the rest of the image).
        assert!(src.read.get() <= MAX_DIR_TOTAL + (1 << 20), "{} bytes read", src.read.get());
    }

    #[test]
    fn a_walk_stops_when_its_source_is_cancelled() {
        let img = many_big_folders(1000);
        let probe = Counting { data: &img, read: 0.into(), reads: 0.into(), cancel_after: u64::MAX };
        let opening = {
            let _ = Volume::open(&probe).expect("opens");
            probe.reads.get()
        };
        // Given up before the walk, then part-way: it stops at the next folder, not at the end.
        for (after, listed) in [(opening, false), (opening + 5, true)] {
            let src = Counting { data: &img, read: 0.into(), reads: 0.into(), cancel_after: after };
            let list = Volume::open(&src).expect("opens").list(&Limits::default());
            assert!(!list.complete);
            assert_eq!(!list.entries.is_empty(), listed);
            assert!(list.warnings.iter().any(|w| w.contains("cancelled")), "{:?}", list.warnings);
            assert!(src.read.get() < 16 << 20, "{} bytes read after cancelling", src.read.get());
        }
    }

    #[test]
    fn a_track_numbered_from_where_it_starts_opens() {
        let img = craft_anything_testkit::disc::iso_image_numbered(&tree(), Wrap::RawMode1, "PSP GAME", "TESTDISC", 4000);
        assert!(Volume::open(img.as_slice()).is_err(), "its blocks point 4000 blocks ahead");
        let vol = Volume::open_numbered(img.as_slice(), Some(4000)).expect("opens");
        assert_eq!(vol.lba_base, 4000);
        assert_eq!(vol.list(&Limits::default()).entries.len(), 5);
    }

    #[test]
    fn hostile_input_never_panics() {
        let base = image(&tree(), Wrap::Cooked, "PSP GAME", "TESTDISC", false);
        for m in craft_anything_testkit::mutations(&base) {
            let _ = read(&m, &Limits::default());
        }
        for len in (0..base.len().min(40_000)).step_by(997) {
            let _ = read(base.get(..len).unwrap_or_default(), &Limits::default());
        }
        for (i, b) in base.iter().enumerate().skip(16 * 2048).take(8192) {
            let mut m = base.clone();
            if let Some(x) = m.get_mut(i) {
                *x = b.wrapping_add(0x55) ^ 0xff;
            }
            let _ = read(&m, &Limits::default());
        }
        let raw = image(&tree(), Wrap::GdRom, "PSP GAME", "TESTDISC", false);
        for len in (0..raw.len()).step_by(1013) {
            let _ = read(raw.get(..len).unwrap_or_default(), &Limits::default());
        }
        for seed in 0..16 {
            let _ = read(&craft_anything_testkit::noise(seed, 60_000), &Limits::default());
        }
    }
}

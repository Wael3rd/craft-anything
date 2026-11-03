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
//! Everything is bounded (entries, depth, directory bytes) and a corrupt or looping directory
//! becomes a warning, never a panic or a hang.
//!
//! Not read: Rock Ridge extensions, multi-extent files (joined), interleaved files and the 2324-byte
//! Form 2 sectors some PlayStation video files use.

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
        let base = [0u32, GDROM_HIGH_DENSITY].into_iter().find(|b| {
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

    /// Walk the directory tree within `limits`.
    pub fn list(&self, limits: &Limits) -> Listing {
        let mut out = Listing { complete: true, ..Listing::default() };
        let mut visited = HashSet::new();
        self.walk(self.root_lba, self.root_size, "", 0, limits, &mut out, &mut visited);
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(&self, lba: u32, size: u32, parent: &str, depth: usize, limits: &Limits, out: &mut Listing, visited: &mut HashSet<u32>) {
        if depth > limits.max_depth.min(128) {
            out.warn("directory nesting limit reached");
            out.complete = false;
            return;
        }
        if !visited.insert(lba) {
            out.warn(format!("{}: a directory leads back to one already read; not followed", shown(parent)));
            out.complete = false;
            return;
        }
        let want = u64::from(size).min(MAX_DIR_BYTES);
        if u64::from(size) > MAX_DIR_BYTES {
            out.warn(format!("{}: directory of {size} bytes cut to {MAX_DIR_BYTES}", shown(parent)));
            out.complete = false;
        }
        let have = want.min(self.available(lba, want));
        if have < want {
            out.warn(format!("{}: directory runs past the end of the image", shown(parent)));
            out.complete = false;
        }
        let Ok(have_len) = usize::try_from(have) else { return };
        let mut dir = vec![0u8; have_len];
        let got = self.read_at(lba, 0, &mut dir);
        dir.truncate(got);
        let mut subdirs: Vec<(String, u32, u32)> = Vec::new();
        let mut pos = 0usize;
        while pos < dir.len() {
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
                out.warn("entry limit reached");
                out.complete = false;
                break;
            }
            let path = join(parent, &name);
            if is_dir {
                out.entries.push(DiscEntry { path: path.clone(), is_dir: true, size: 0, lba: e_lba, available: 0 });
                subdirs.push((path, e_lba, e_size));
            } else {
                if flags & 0x80 != 0 {
                    out.warn(format!("{path}: continues in another extent; only the first part is listed"));
                    out.complete = false;
                }
                let avail = self.available(e_lba, u64::from(e_size));
                if avail < u64::from(e_size) {
                    out.warn(format!("{path}: runs past the end of the image ({avail} of {e_size} bytes present)"));
                }
                out.entries.push(DiscEntry { path, is_dir: false, size: u64::from(e_size), lba: e_lba, available: avail });
            }
        }
        for (path, l, s) in subdirs {
            if out.entries.len() >= limits.max_entries {
                out.complete = false;
                break;
            }
            self.walk(l, s, &path, depth + 1, limits, out, visited);
        }
    }
}

fn shown(path: &str) -> &str {
    if path.is_empty() { "/" } else { path }
}

/// A name as one path component: version suffix and trailing dot dropped, separators replaced.
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
        let fits = e.size <= limits.max_file_bytes && img.bytes.saturating_add(e.size) <= limits.max_total_bytes && e.available == e.size;
        let bytes = usize::try_from(e.size).ok().filter(|_| fits).and_then(|n| {
            let mut buf = vec![0u8; n];
            (vol.read_at(e.lba, 0, &mut buf) == n).then_some(buf)
        });
        match &bytes {
            Some(b) => img.bytes = img.bytes.saturating_add(b.len() as u64),
            None if e.size > 0 => {
                img.warn(format!("{}: file data not recovered (size or bounds)", e.path));
                img.complete = false;
            }
            None => {}
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

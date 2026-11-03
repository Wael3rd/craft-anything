//! PSF files: the key/value tables the PlayStation Portable keeps in `PARAM.SFO` (a game's title,
//! disc id, category, the firmware it needs).
//!
//! A file is a 20-byte header (a NUL byte then `PSF`, a version, the offsets of the key table and
//! of the data table, and the number of entries), then one 16-byte index entry per value, then the
//! keys (NUL-terminated ASCII) and the data. Every offset can be wrong, so every read is checked,
//! the entry count is capped, and a bad entry gives a warning instead of an error.

use crate::FormatError;
use serde::Serialize;
use serde_json::{Map, Value};

/// The first four bytes of a PSF file.
pub const MAGIC: [u8; 4] = [0, b'P', b'S', b'F'];
const HEADER_LEN: usize = 20;
const ENTRY_LEN: usize = 16;
/// Most entries read; a real file has a dozen or so.
const MAX_ENTRIES: usize = 512;
/// Most characters kept of a key or a text value.
const MAX_TEXT: usize = 512;
/// Most warnings kept.
const MAX_WARNINGS: usize = 32;

/// A value in the data table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum PsfValue {
    Text(String),
    Number(u32),
    /// A format we don't read: only its length.
    Unread {
        format: u16,
        length: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PsfEntry {
    pub key: String,
    pub value: PsfValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Psf {
    pub version: String,
    /// Entry count the header claims (may exceed what could be read).
    pub declared_entries: u32,
    pub entries: Vec<PsfEntry>,
    pub warnings: Vec<String>,
}

/// True when the bytes start like a PSF file.
pub fn is_psf(data: &[u8]) -> bool {
    data.starts_with(&MAGIC)
}

fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(o..o.checked_add(2)?)?.try_into().ok()?))
}

fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o.checked_add(4)?)?.try_into().ok()?))
}

/// Printable ASCII only, capped; anything else becomes `?`.
fn clean(bytes: &[u8]) -> String {
    bytes.iter().take(MAX_TEXT).map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { '?' }).collect()
}

/// Text in a data slot: UTF-8, up to the first NUL, capped and made printable.
fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    let s = String::from_utf8_lossy(bytes.get(..end).unwrap_or_default());
    s.chars().filter(|c| !c.is_control()).take(MAX_TEXT).collect()
}

impl Psf {
    /// Parse a PSF file. Fails only when there is no usable header; bad entries are skipped with a warning.
    pub fn parse(data: &[u8]) -> Result<Psf, FormatError> {
        if !is_psf(data) {
            return Err(FormatError::Malformed("not a PSF file (no NUL, 'PSF' at the start)".into()));
        }
        if data.len() < HEADER_LEN {
            return Err(FormatError::Truncated("the PSF header is cut short".into()));
        }
        let (Some(v), Some(key_start), Some(data_start), Some(count)) = (u32_at(data, 4), u32_at(data, 8), u32_at(data, 12), u32_at(data, 16)) else {
            return Err(FormatError::Truncated("the PSF header is cut short".into()));
        };
        let mut warnings: Vec<String> = Vec::new();
        let warn = |w: &mut Vec<String>, m: String| {
            if w.len() < MAX_WARNINGS {
                w.push(m);
            }
        };
        let version = format!("{}.{}", v & 0xff, (v >> 8) & 0xff);
        let key_start = key_start as usize;
        let data_start = data_start as usize;
        let take = (count as usize).min(MAX_ENTRIES);
        if count as usize > MAX_ENTRIES {
            warn(&mut warnings, format!("the header claims {count} entries; only the first {MAX_ENTRIES} are read"));
        }
        let index_end = take.checked_mul(ENTRY_LEN).and_then(|n| n.checked_add(HEADER_LEN));
        if index_end.is_none_or(|e| e > data.len()) {
            warn(&mut warnings, format!("the index of {count} entries runs past the end of the file"));
        }
        if key_start > data.len() || data_start > data.len() {
            warn(&mut warnings, "the key or data table starts outside the file".into());
        }
        let mut entries = Vec::new();
        for i in 0..take {
            let Some(base) = i.checked_mul(ENTRY_LEN).and_then(|n| n.checked_add(HEADER_LEN)) else { break };
            let (Some(koff), Some(fmt), Some(len), Some(_max), Some(doff)) =
                (u16_at(data, base), u16_at(data, base + 2), u32_at(data, base + 4), u32_at(data, base + 8), u32_at(data, base + 12))
            else {
                break;
            };
            let key = key_start
                .checked_add(koff as usize)
                .and_then(|k| data.get(k..))
                .map(|rest| {
                    let end = rest.iter().take(MAX_TEXT).position(|&c| c == 0).unwrap_or(rest.len().min(MAX_TEXT));
                    clean(rest.get(..end).unwrap_or_default())
                })
                .filter(|k| !k.is_empty());
            let Some(key) = key else {
                warn(&mut warnings, format!("entry {i}: its key lies outside the file or is empty"));
                continue;
            };
            let slot = data_start.checked_add(doff as usize).and_then(|d| d.checked_add(len as usize).and_then(|e| data.get(d..e)));
            let Some(slot) = slot else {
                warn(&mut warnings, format!("entry {key}: its data lies outside the file"));
                continue;
            };
            let value = match fmt {
                0x0004 | 0x0204 => PsfValue::Text(text(slot)),
                0x0404 => match slot.get(..4).and_then(|b| b.try_into().ok()) {
                    Some(b) => PsfValue::Number(u32::from_le_bytes(b)),
                    None => {
                        warn(&mut warnings, format!("entry {key}: a number shorter than 4 bytes"));
                        continue;
                    }
                },
                other => {
                    warn(&mut warnings, format!("entry {key}: unknown data format 0x{other:04x}"));
                    PsfValue::Unread { format: other, length: len }
                }
            };
            entries.push(PsfEntry { key, value });
        }
        Ok(Psf { version, declared_entries: count, entries, warnings })
    }

    pub fn get(&self, key: &str) -> Option<&PsfValue> {
        self.entries.iter().find(|e| e.key == key).map(|e| &e.value)
    }

    pub fn text(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            PsfValue::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn number(&self, key: &str) -> Option<u32> {
        match self.get(key)? {
            PsfValue::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// The entries as a JSON object (a repeated key keeps its first value).
    pub fn fields(&self) -> Value {
        let mut m = Map::new();
        for e in &self.entries {
            if !m.contains_key(&e.key) {
                m.insert(e.key.clone(), serde_json::to_value(&e.value).unwrap_or(Value::Null));
            }
        }
        Value::Object(m)
    }

    /// What the file's CATEGORY code stands for, if we know it.
    pub fn category_plain(&self) -> Option<&'static str> {
        Some(match self.text("CATEGORY")? {
            "UG" => "a UMD game disc",
            "MG" => "a Memory Stick game",
            "EG" => "a PlayStation (PS1) game",
            "MS" => "save data",
            "UV" => "a UMD video disc",
            "UA" => "a UMD audio disc",
            _ => return None,
        })
    }

    /// A plain-English sentence about what the file describes.
    pub fn plain(&self) -> String {
        let what = self.category_plain().map_or_else(|| "a PSP title".to_string(), |c| format!("a PSP title ({c})"));
        let mut s = match self.text("TITLE") {
            Some(t) if !t.is_empty() => format!("PARAM.SFO describes {what}: \"{t}\""),
            _ => format!("PARAM.SFO describes {what} (no title)"),
        };
        if let Some(id) = self.text("DISC_ID") {
            s.push_str(&format!(", disc {id}"));
        }
        if let Some(c) = self.text("CATEGORY") {
            s.push_str(&format!(", category {c}"));
        }
        if let Some(v) = self.text("PSP_SYSTEM_VER") {
            s.push_str(&format!(", needs firmware {v}"));
        }
        s.push('.');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a PSF file: `(key, value)` pairs where a value is text or a number.
    pub(crate) fn build(entries: &[(&str, Result<&str, u32>)]) -> Vec<u8> {
        let mut keys = Vec::new();
        let mut datas = Vec::new();
        let mut index = Vec::new();
        for (k, v) in entries {
            index.extend_from_slice(&(keys.len() as u16).to_le_bytes());
            keys.extend_from_slice(k.as_bytes());
            keys.push(0);
            let (fmt, bytes, max) = match v {
                Ok(s) => (0x0204u16, [s.as_bytes(), &[0]].concat(), 64u32),
                Err(n) => (0x0404u16, n.to_le_bytes().to_vec(), 4),
            };
            index.extend_from_slice(&fmt.to_le_bytes());
            index.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            index.extend_from_slice(&max.to_le_bytes());
            index.extend_from_slice(&(datas.len() as u32).to_le_bytes());
            datas.extend_from_slice(&bytes);
            datas.resize(datas.len() + (max as usize).saturating_sub(bytes.len()), 0);
        }
        let key_start = HEADER_LEN + index.len();
        let data_start = key_start + keys.len();
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&0x0101u32.to_le_bytes());
        out.extend_from_slice(&(key_start as u32).to_le_bytes());
        out.extend_from_slice(&(data_start as u32).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        out.extend(index);
        out.extend(keys);
        out.extend(datas);
        out
    }

    pub(crate) fn sample() -> Vec<u8> {
        build(&[("CATEGORY", Ok("UG")), ("DISC_ID", Ok("ULES00000")), ("PARENTAL_LEVEL", Err(1)), ("PSP_SYSTEM_VER", Ok("3.40")), ("TITLE", Ok("TEST GAME"))])
    }

    #[test]
    fn reads_a_good_file() {
        let psf = Psf::parse(&sample()).unwrap();
        assert_eq!(psf.version, "1.1");
        assert_eq!(psf.entries.len(), 5);
        assert_eq!(psf.text("TITLE"), Some("TEST GAME"));
        assert_eq!(psf.number("PARENTAL_LEVEL"), Some(1));
        assert!(psf.warnings.is_empty(), "{:?}", psf.warnings);
        let plain = psf.plain();
        assert!(plain.contains("\"TEST GAME\"") && plain.contains("ULES00000") && plain.contains("UMD game") && plain.contains("3.40"), "{plain}");
        assert_eq!(psf.fields()["DISC_ID"], "ULES00000");
        assert_eq!(psf.fields()["PARENTAL_LEVEL"], 1);
    }

    #[test]
    fn every_truncation_is_safe() {
        let full = sample();
        for n in 0..full.len() {
            let r = Psf::parse(&full[..n]);
            if n < HEADER_LEN {
                assert!(r.is_err(), "{n}");
            } else {
                let psf = r.unwrap();
                assert!(psf.entries.len() <= 5);
                let _ = psf.plain();
            }
        }
    }

    #[test]
    fn corrupt_offsets_and_counts_give_warnings() {
        let mut huge = sample();
        huge[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        let psf = Psf::parse(&huge).unwrap();
        assert!(psf.entries.len() <= MAX_ENTRIES && !psf.warnings.is_empty());
        let mut bad_tables = sample();
        bad_tables[8..16].copy_from_slice(&[0xff; 8]);
        let psf = Psf::parse(&bad_tables).unwrap();
        assert!(psf.entries.is_empty() && !psf.warnings.is_empty());
        let mut bad_data = sample();
        bad_data[HEADER_LEN + 12..HEADER_LEN + 16].copy_from_slice(&u32::MAX.to_le_bytes());
        let psf = Psf::parse(&bad_data).unwrap();
        assert_eq!(psf.entries.len(), 4);
        assert!(psf.warnings.iter().any(|w| w.contains("outside")));
        let mut odd = sample();
        odd[HEADER_LEN + 2..HEADER_LEN + 4].copy_from_slice(&0x7777u16.to_le_bytes());
        assert!(Psf::parse(&odd).unwrap().warnings.iter().any(|w| w.contains("unknown data format")));
        assert!(Psf::parse(b"\0PSX0123456789abcdefg").is_err());
    }

    #[test]
    fn text_is_printable_and_capped() {
        let long = "A".repeat(2000);
        let psf = Psf::parse(&build(&[("TITLE", Ok(&long))])).unwrap();
        assert_eq!(psf.text("TITLE").map(str::len), Some(MAX_TEXT));
        let psf = Psf::parse(&build(&[("TITLE", Ok("a\u{1b}[31mb"))])).unwrap();
        assert!(!psf.text("TITLE").unwrap().contains('\u{1b}'));
    }
}

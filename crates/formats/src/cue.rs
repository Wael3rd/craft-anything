//! CUE sheets: the text file that says which `.bin` files make up a disc and how each track is
//! stored. A small, forgiving line parser: `FILE`, `TRACK`, `INDEX`, `PREGAP`/`POSTGAP` and the
//! Dreamcast `REM SINGLE-DENSITY AREA` / `REM HIGH-DENSITY AREA` markers; anything else is ignored.

use serde::Serialize;

/// Longest sheet read (real ones are a few hundred bytes).
pub const MAX_BYTES: usize = 1 << 20;
/// Most tracks kept.
pub const MAX_TRACKS: usize = 1000;

/// One `INDEX` line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CueIndex {
    pub number: u32,
    /// `mm:ss:ff` as written.
    pub time: String,
    /// The time as frames (75 per second), when it parsed.
    pub frames: Option<u64>,
}

/// One track and the file it lives in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CueTrack {
    pub number: u32,
    /// Name of the `.bin` (or other) file as written in the sheet.
    pub file: String,
    /// `MODE1/2352`, `MODE2/2352`, `AUDIO`…, as written.
    pub mode: String,
    /// Bytes per sector for the mode (2352, 2048…), when it says.
    pub sector_size: Option<u32>,
    /// True for a data track (not `AUDIO`).
    pub data: bool,
    pub indexes: Vec<CueIndex>,
    /// `single-density` or `high-density` when a Dreamcast `REM` marker precedes the track.
    pub area: Option<String>,
}

/// A parsed sheet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CueSheet {
    pub tracks: Vec<CueTrack>,
    /// Lines that didn't make sense or were dropped (limits).
    pub warnings: Vec<String>,
}

/// True when `text` looks like a CUE sheet (has a `FILE` line and a `TRACK` line).
pub fn looks_like_cue(text: &str) -> bool {
    let mut file = false;
    let mut track = false;
    for line in text.lines().take(2000) {
        let up = line.trim_start().to_ascii_uppercase();
        file |= up.starts_with("FILE ");
        track |= up.starts_with("TRACK ");
    }
    file && track
}

/// Split a line into words, keeping a `"quoted name"` together.
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                if !quoted {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn msf(t: &str) -> Option<u64> {
    let mut it = t.split(':');
    let (m, s, f) = (it.next()?.parse::<u64>().ok()?, it.next()?.parse::<u64>().ok()?, it.next()?.parse::<u64>().ok()?);
    if it.next().is_some() || s >= 60 || f >= 75 {
        return None;
    }
    m.checked_mul(60)?.checked_add(s)?.checked_mul(75)?.checked_add(f)
}

fn sector_size(mode: &str) -> Option<u32> {
    match mode.to_ascii_uppercase().as_str() {
        "AUDIO" => Some(2352),
        m => m.rsplit('/').next().and_then(|n| n.parse().ok()),
    }
}

/// Parse a sheet. Never fails: lines it can't read become warnings.
pub fn parse(text: &str) -> CueSheet {
    let mut sheet = CueSheet::default();
    let text = text.get(..text.len().min(MAX_BYTES)).unwrap_or(text);
    let mut file = String::new();
    let mut area: Option<String> = None;
    for raw in text.trim_start_matches('\u{feff}').lines().take(20_000) {
        let line = raw.trim();
        let w = words(line);
        let Some(cmd) = w.first().map(|c| c.to_ascii_uppercase()) else { continue };
        match cmd.as_str() {
            "REM" => {
                let rest = line.get(3..).unwrap_or("").trim().to_ascii_uppercase();
                if rest.starts_with("SINGLE-DENSITY AREA") {
                    area = Some("single-density".into());
                } else if rest.starts_with("HIGH-DENSITY AREA") {
                    area = Some("high-density".into());
                }
            }
            "FILE" => file = w.get(1).cloned().unwrap_or_default(),
            "TRACK" => {
                if sheet.tracks.len() >= MAX_TRACKS {
                    sheet.warnings.push(format!("more than {MAX_TRACKS} tracks; the rest are ignored"));
                    break;
                }
                let number = w.get(1).and_then(|n| n.parse().ok());
                let mode = w.get(2).cloned();
                match (number, mode) {
                    (Some(number), Some(mode)) => sheet.tracks.push(CueTrack {
                        number,
                        file: file.clone(),
                        sector_size: sector_size(&mode),
                        data: !mode.eq_ignore_ascii_case("AUDIO"),
                        mode,
                        indexes: Vec::new(),
                        area: area.clone(),
                    }),
                    _ => sheet.warnings.push(format!("unreadable TRACK line: {}", line.chars().take(80).collect::<String>())),
                }
            }
            "INDEX" => {
                let n = w.get(1).and_then(|n| n.parse().ok());
                let t = w.get(2).cloned();
                match (n, t, sheet.tracks.last_mut()) {
                    (Some(number), Some(time), Some(track)) => {
                        let frames = msf(&time);
                        track.indexes.push(CueIndex { number, time, frames });
                    }
                    _ => sheet.warnings.push(format!("unreadable INDEX line: {}", line.chars().take(80).collect::<String>())),
                }
            }
            _ => {}
        }
    }
    sheet
}

#[cfg(test)]
mod tests {
    use super::*;

    const DREAMCAST: &str = "REM SINGLE-DENSITY AREA\r\nFILE \"Game (Track 1).bin\" BINARY\r\n  TRACK 01 MODE1/2352\r\n    INDEX 01 00:00:00\r\nFILE \"Game (Track 2).bin\" BINARY\r\n  TRACK 02 AUDIO\r\n    INDEX 00 00:00:00\r\n    INDEX 01 00:02:00\r\nREM HIGH-DENSITY AREA\r\nFILE \"Game (Track 3).bin\" BINARY\r\n  TRACK 03 MODE1/2352\r\n    INDEX 01 00:00:00\r\n";

    #[test]
    fn reads_a_dreamcast_sheet() {
        let s = parse(DREAMCAST);
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        assert_eq!(s.tracks.len(), 3);
        assert_eq!(s.tracks[0].file, "Game (Track 1).bin");
        assert_eq!(s.tracks[0].mode, "MODE1/2352");
        assert_eq!(s.tracks[0].sector_size, Some(2352));
        assert!(s.tracks[0].data && !s.tracks[1].data);
        assert_eq!(s.tracks[1].indexes[1].frames, Some(150));
        assert_eq!(s.tracks[0].area.as_deref(), Some("single-density"));
        assert_eq!(s.tracks[2].area.as_deref(), Some("high-density"));
        assert!(looks_like_cue(DREAMCAST));
    }

    #[test]
    fn unquoted_names_and_junk_are_tolerated() {
        let s = parse("FILE game.bin BINARY\nTRACK 1 MODE2/2352\nINDEX 1 00:00:00\nTRACK x\nINDEX\nFLAGS DCP\nTITLE \"a\"\n");
        assert_eq!(s.tracks.len(), 1);
        assert_eq!(s.tracks[0].file, "game.bin");
        assert_eq!(s.warnings.len(), 2);
        assert!(!looks_like_cue("hello world"));
    }

    #[test]
    fn hostile_text_never_panics() {
        for t in ["", "\"", "FILE \"", "TRACK", "INDEX 99 99:99:99", "REM", "\u{feff}FILE \"a\" TRACK 1 AUDIO", "INDEX 1 99999999999999999999:00:00"] {
            let _ = parse(t);
        }
        let many = "TRACK 1 AUDIO\n".repeat(5000);
        assert_eq!(parse(&many).tracks.len(), MAX_TRACKS);
    }
}

//! File safety: atomic writes (temp file, fsync, rename), optionally private (0600 on Unix).

use craft_anything_evidence::ToolError;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` atomically, keeping the existing file's permissions when it exists.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    write_impl(path, bytes, false)
}

/// Write `bytes` to `path` atomically with owner-only permissions (0600 on Unix). Used for
/// configuration files that may hold paths or tokens, and for the key fallback file.
pub fn write_atomic_private(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    write_impl(path, bytes, true)
}

fn write_impl(path: &Path, bytes: &[u8], private: bool) -> Result<(), ToolError> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        Some(_) => std::path::PathBuf::from("."),
        None => return Err(ToolError::invalid(format!("{} has no parent folder", path.display()))),
    };
    std::fs::create_dir_all(&dir).map_err(|e| ToolError::io(dir.display(), e))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.tmp-{}-{n}", std::process::id()));
    let existing_perms = std::fs::metadata(path).ok().map(|m| m.permissions());
    let result = (|| {
        let mut f = std::fs::File::create(&tmp).map_err(|e| ToolError::io(tmp.display(), e))?;
        if private {
            set_private(&tmp)?;
        } else if let Some(p) = existing_perms {
            std::fs::set_permissions(&tmp, p).map_err(|e| ToolError::io(tmp.display(), e))?;
        }
        f.write_all(bytes).map_err(|e| ToolError::io(tmp.display(), e))?;
        f.sync_all().map_err(|e| ToolError::io(tmp.display(), e))?;
        drop(f);
        rename_retrying(&tmp, path).map_err(|e| ToolError::io(path.display(), e))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// On Windows a file another program has open for a moment (an antivirus scan, the search
/// indexer) can't be replaced: try again briefly before giving up.
fn rename_retrying(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut tries = 0;
    loop {
        match std::fs::rename(from, to) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && tries < 20 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            r => return r,
        }
    }
}

#[cfg(unix)]
fn set_private(path: &Path) -> Result<(), ToolError> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| ToolError::io(path.display(), e))
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> Result<(), ToolError> {
    // Windows: files under the user's profile are already private to the user by default ACLs.
    Ok(())
}

/// Copy `from` to `to` only if `to` does not exist yet (the first backup is never overwritten).
/// Returns true when a copy was made, false when a backup already existed.
pub fn backup_once(from: &Path, to: &Path) -> Result<bool, ToolError> {
    if to.exists() {
        return Ok(false);
    }
    let bytes = std::fs::read(from).map_err(|e| ToolError::io(from.display(), e))?;
    let mut f = match std::fs::OpenOptions::new().write(true).create_new(true).open(to) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(ToolError::io(to.display(), e)),
    };
    set_private(to)?;
    f.write_all(&bytes).map_err(|e| ToolError::io(to.display(), e))?;
    f.sync_all().map_err(|e| ToolError::io(to.display(), e))?;
    Ok(true)
}

/// Turn a name read out of a file (an archive member, a disc's file) into safe relative components,
/// or explain why not. Every component passes [`unsafe_component`] on every system, so a name that
/// is harmless on Linux but escapes or misbehaves on Windows (`C:x`, `NUL`) is refused everywhere.
pub fn safe_components(name: &str) -> Result<Vec<String>, &'static str> {
    if name.contains('\0') {
        return Err("name contains a NUL byte");
    }
    let norm = name.replace('\\', "/");
    if norm.starts_with('/') {
        return Err("absolute path");
    }
    let mut out = Vec::new();
    for c in norm.split('/') {
        match c {
            "" | "." => {}
            c => match unsafe_component(c) {
                Some(why) => return Err(why),
                None => out.push(c.to_string()),
            },
        }
    }
    if out.is_empty() {
        return Err("empty name");
    }
    Ok(out)
}

/// Why `c` can't be one plain file or folder name on every system we run on, or None when it can.
/// On Windows `root.join("C:x")` is not under `root` (it means "x in drive C's current folder") and
/// `NUL`, `com1.txt` or a name ending in a dot or space name devices or other files.
pub fn unsafe_component(c: &str) -> Option<&'static str> {
    if c.is_empty() {
        return Some("empty name");
    }
    if c == "." || c == ".." {
        return Some("path climbs out of the folder (..)");
    }
    if c.contains(['/', '\\']) {
        return Some("name contains a path separator");
    }
    if c.contains(':') {
        return Some("name contains ':' (on Windows a drive or a stream, which would leave the folder)");
    }
    if c.chars().any(char::is_control) {
        return Some("name contains a control character");
    }
    if c.ends_with(['.', ' ']) {
        return Some("name ends in a dot or a space (Windows drops them, so it would name another file)");
    }
    if windows_device(c) {
        return Some("a Windows device name (CON, PRN, AUX, NUL, COM1…, LPT1…)");
    }
    None
}

/// True for names Windows takes as a device whatever the folder: CON, PRN, AUX, NUL, COM0-9, LPT0-9
/// (and their superscript forms), CONIN$ and CONOUT$, in any case, with any extension.
fn windows_device(c: &str) -> bool {
    let stem = c.split('.').next().unwrap_or(c).trim_end_matches(' ').to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return true;
    }
    stem.strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|n| (n.len() == 1 && n.bytes().all(|b| b.is_ascii_digit())) || matches!(n, "¹" | "²" | "³"))
}

/// `root` with `comps` appended, when each is one plain name ([`unsafe_component`]) and the result
/// is still under `root`; None otherwise. Every path built from names read out of a file goes
/// through this before it is written or read.
pub fn join_under(root: &Path, comps: &[String]) -> Option<std::path::PathBuf> {
    let mut path = root.to_path_buf();
    for c in comps {
        if unsafe_component(c).is_some() {
            return None;
        }
        let mut parts = Path::new(c).components();
        match (parts.next(), parts.next()) {
            (Some(std::path::Component::Normal(n)), None) if n == std::ffi::OsStr::new(c) => path.push(n),
            _ => return None,
        }
    }
    path.starts_with(root).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_would_leave_the_folder_or_name_a_device_are_refused() {
        for bad in ["../x", "/x", "a\\..\\..\\x", "C:\\x", "C:x", "D:evil.bat", "x:y/z", "NUL", "nul.txt", "COM1", "lpt9.log", "a. ", "x\u{1}"] {
            assert!(safe_components(bad).is_err(), "{bad:?}");
        }
        assert_eq!(safe_components("./a//b").unwrap(), vec!["a", "b"]);
        assert!(safe_components("").is_err());
        let root = Path::new("out");
        assert_eq!(join_under(root, &["a".into(), "b.txt".into()]), Some(root.join("a").join("b.txt")));
        for bad in ["C:x", "..", "a/b", "CON"] {
            assert!(join_under(root, &[bad.to_string()]).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn atomic_write_replaces_and_private_is_0600() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("a.txt");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        let q = dir.path().join("secret");
        write_atomic_private(&q, b"k").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&q).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // no temp files left behind
        assert_eq!(std::fs::read_dir(dir.path().join("sub")).unwrap().count(), 1);
    }

    #[test]
    fn backup_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("a.bak");
        std::fs::write(&a, "first").unwrap();
        assert!(backup_once(&a, &b).unwrap());
        std::fs::write(&a, "second").unwrap();
        assert!(!backup_once(&a, &b).unwrap());
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "first");
    }
}

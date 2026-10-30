//! Saving attachments to disk (SPEC 7.5 `SaveAttachment`): the filename a message carries is
//! untrusted, so it is sanitised before it becomes a path; a save never overwrites an existing
//! file; and the download directory comes from the user's `user-dirs.dirs`, falling back to
//! `~/Downloads`. The naming and directory rules are pure and unit-tested; [`save_new`] and
//! [`download_dir`] are the only parts that touch the filesystem or the environment.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The longest sanitised filename, in bytes. Linux allows 255; this leaves room for the
/// " (n)" a duplicate gets.
const MAX_NAME: usize = 200;
/// An "extension" longer than this is part of the name, not an extension worth keeping.
const MAX_EXT: usize = 32;
/// How many numbered names a save tries before giving up.
const MAX_TRIES: u32 = 10_000;

/// Whether `c` is a bidi or invisible formatting character, which can disguise text
/// (`invoice\u{202E}fdp.exe` shows as `invoiceexe.pdf`). Shared with the AI prompt builder's
/// `strip_controls` (SPEC 16.6.4).
pub(crate) fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{061C}'                   // Arabic letter mark
        | '\u{200B}'..='\u{200F}'     // zero-width space, joiners, LRM, RLM
        | '\u{202A}'..='\u{202E}'     // embeddings and overrides
        | '\u{2060}'..='\u{2064}'     // word joiner and invisible operators
        | '\u{2066}'..='\u{2069}'     // isolates
        | '\u{FEFF}'                   // zero-width no-break space
    )
}

/// Whether `c` is stripped from a filename: path separators, NUL, C0 and C1 controls (and
/// DEL), and the bidi and invisible formatting characters of [`is_invisible`].
fn stripped(c: char) -> bool {
    matches!(c, '/' | '\\' | '\u{0}'..='\u{1F}' | '\u{7F}'..='\u{9F}') || is_invisible(c)
}

/// `name` split at its last dot into a stem and an extension (with the dot), when the stem is
/// not empty and the extension is short enough to be one.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= MAX_EXT => name.split_at(i),
        _ => (name, ""),
    }
}

/// The longest prefix of `s` that fits in `max` bytes, cut on a char boundary.
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// A safe single path component for an attachment called `name` (SPEC 7.5 step 4): stripped
/// of separators, controls and bidi or invisible formatting characters, without leading dots
/// or spaces (no hidden files, no `..`), `attachment` when nothing is left, and at most 200
/// bytes with the extension kept.
pub fn sanitise(name: &str) -> String {
    let cleaned: String = name.chars().filter(|&c| !stripped(c)).collect();
    let cleaned = cleaned.trim_start_matches(['.', ' ']).trim_end();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return "attachment".to_string();
    }
    if cleaned.len() <= MAX_NAME {
        return cleaned.to_string();
    }
    let (stem, ext) = split_ext(cleaned);
    let stem = truncate(stem, MAX_NAME - ext.len()).trim_end();
    if stem.is_empty() {
        return truncate(cleaned, MAX_NAME).to_string();
    }
    format!("{stem}{ext}")
}

/// The `n`th alternative to `name`: `name (n).ext`.
pub fn numbered(name: &str, n: u32) -> String {
    let (stem, ext) = split_ext(name);
    format!("{stem} ({n}){ext}")
}

/// Writes `bytes` to a new file called `name` in `dir`, or `name (1)`, `name (2)` and so on
/// when that exists: an existing file is never overwritten (`create_new`). A failed write
/// removes the partial file. Returns the path written.
pub fn save_new(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    for n in 0..MAX_TRIES {
        let path = dir.join(if n == 0 {
            name.to_string()
        } else {
            numbered(name, n)
        });
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        return match file.write_all(bytes).and_then(|()| file.sync_all()) {
            Ok(()) => Ok(path),
            Err(e) => {
                drop(file);
                let _ = std::fs::remove_file(&path);
                Err(e)
            }
        };
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many files with that name",
    ))
}

/// The `XDG_DOWNLOAD_DIR` that `user_dirs` (the text of `user-dirs.dirs`) sets, with a leading
/// `$HOME` expanded to `home`. Only an absolute result counts; anything else (a relative path,
/// another variable, a missing entry) is `None`.
pub fn xdg_download_dir(user_dirs: &str, home: &Path) -> Option<PathBuf> {
    let value = user_dirs.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with('#') {
            return None;
        }
        let (key, value) = line.split_once('=')?;
        (key.trim() == "XDG_DOWNLOAD_DIR").then(|| value.trim())
    })?;
    let value = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value);
    let path = match value.strip_prefix("$HOME") {
        Some("") => home.to_path_buf(),
        Some(rest) => home.join(rest.strip_prefix('/')?),
        None => PathBuf::from(value),
    };
    (path.is_absolute() && !path.as_os_str().to_string_lossy().contains('$')).then_some(path)
}

/// The download directory for a user whose home is `home` and whose `user-dirs.dirs` holds
/// `user_dirs` (when it could be read): its `XDG_DOWNLOAD_DIR`, else `~/Downloads`.
pub fn resolve_download_dir(home: &Path, user_dirs: Option<&str>) -> PathBuf {
    user_dirs
        .and_then(|text| xdg_download_dir(text, home))
        .unwrap_or_else(|| home.join("Downloads"))
}

/// The user's download directory from the environment (`$HOME`, `$XDG_CONFIG_HOME`, falling
/// back to `~/.config`), created if missing.
pub fn download_dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home folder"))?;
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|c| c.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let user_dirs = std::fs::read_to_string(config.join("user-dirs.dirs")).ok();
    let dir = resolve_download_dir(&home, user_dirs.as_deref());
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separators_nul_and_controls_are_stripped() {
        assert_eq!(sanitise("a/b\\c.txt"), "abc.txt");
        assert_eq!(sanitise("../../etc/passwd"), "etcpasswd");
        assert_eq!(sanitise("nul\0byte.pdf"), "nulbyte.pdf");
        assert_eq!(sanitise("tab\there\r\n.txt"), "tabhere.txt");
        assert_eq!(sanitise("c1\u{85}\u{9B}x.bin"), "c1x.bin");
        assert_eq!(sanitise("del\u{7F}.txt"), "del.txt");
    }

    #[test]
    fn bidi_and_invisible_characters_are_stripped() {
        assert_eq!(sanitise("invoice\u{202E}fdp.exe"), "invoicefdp.exe");
        assert_eq!(
            sanitise("a\u{200E}b\u{200F}c\u{2066}d\u{2069}.txt"),
            "abcd.txt"
        );
        assert_eq!(sanitise("\u{202A}x\u{202B}\u{202C}\u{202D}.pdf"), "x.pdf");
        assert_eq!(sanitise("zero\u{200B}width\u{FEFF}.txt"), "zerowidth.txt");
    }

    #[test]
    fn leading_dots_and_spaces_are_trimmed() {
        assert_eq!(sanitise(".bashrc"), "bashrc");
        assert_eq!(sanitise(" . .hidden.txt"), "hidden.txt");
        assert_eq!(sanitise("...x"), "x");
    }

    #[test]
    fn empty_and_dot_names_become_attachment() {
        for name in ["", ".", "..", "   ", "/", "\u{202E}", "./.."] {
            assert_eq!(sanitise(name), "attachment", "{name:?}");
        }
    }

    #[test]
    fn long_names_keep_their_extension() {
        let long = format!("{}.pdf", "a".repeat(300));
        let s = sanitise(&long);
        assert_eq!(s.len(), MAX_NAME);
        assert!(s.ends_with(".pdf"));
        // Multi-byte characters are cut on a boundary.
        let long = format!("{}.txt", "é".repeat(150));
        let s = sanitise(&long);
        assert!(s.len() <= MAX_NAME);
        assert!(s.ends_with(".txt"));
        assert!(s.starts_with('é'));
        // A dot far from the end is not an extension.
        let long = format!("x.{}", "b".repeat(300));
        assert_eq!(sanitise(&long).len(), MAX_NAME);
        // Unicode names otherwise pass through.
        assert_eq!(sanitise("Zoë Ångström ☕.pdf"), "Zoë Ångström ☕.pdf");
    }

    #[test]
    fn numbered_names_go_before_the_extension() {
        assert_eq!(numbered("report.pdf", 1), "report (1).pdf");
        assert_eq!(numbered("archive.tar.gz", 2), "archive.tar (2).gz");
        assert_eq!(numbered("README", 3), "README (3)");
        assert_eq!(numbered("attachment", 1), "attachment (1)");
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "eloquawk-files-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn saves_never_overwrite() {
        let dir = TempDir::new("unique");
        let first = save_new(&dir.0, "a.txt", b"one").unwrap();
        let second = save_new(&dir.0, "a.txt", b"two").unwrap();
        let third = save_new(&dir.0, "a.txt", b"three").unwrap();
        assert_eq!(first, dir.0.join("a.txt"));
        assert_eq!(second, dir.0.join("a (1).txt"));
        assert_eq!(third, dir.0.join("a (2).txt"));
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert_eq!(std::fs::read(&second).unwrap(), b"two");
        assert_eq!(std::fs::read(&third).unwrap(), b"three");
    }

    #[test]
    fn a_failed_save_reports_the_error() {
        let dir = TempDir::new("missing");
        let gone = dir.0.join("not-there");
        assert!(save_new(&gone, "a.txt", b"x").is_err());
        assert!(!gone.exists());
    }

    #[test]
    fn user_dirs_download_dir() {
        let home = Path::new("/home/zoe");
        let text = "# written by xdg-user-dirs-update\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\n\
                    XDG_DOWNLOAD_DIR=\"$HOME/Downloads/Mail\"\n";
        assert_eq!(
            resolve_download_dir(home, Some(text)),
            PathBuf::from("/home/zoe/Downloads/Mail")
        );
        // An absolute path.
        assert_eq!(
            resolve_download_dir(home, Some("XDG_DOWNLOAD_DIR=\"/srv/dl\"")),
            PathBuf::from("/srv/dl")
        );
        // A relative path, another variable or `$HOMEX` are rejected.
        for bad in [
            "XDG_DOWNLOAD_DIR=\"Downloads\"",
            "XDG_DOWNLOAD_DIR=\"$XDG_DATA_HOME/dl\"",
            "XDG_DOWNLOAD_DIR=\"$HOMEdir\"",
            "XDG_DOWNLOAD_DIR=\"/x/$HOME\"",
            "# XDG_DOWNLOAD_DIR=\"/commented\"",
            "",
        ] {
            assert_eq!(
                resolve_download_dir(home, Some(bad)),
                PathBuf::from("/home/zoe/Downloads"),
                "{bad}"
            );
        }
        // A missing file.
        assert_eq!(
            resolve_download_dir(home, None),
            PathBuf::from("/home/zoe/Downloads")
        );
        // `$HOME` alone means the home folder.
        assert_eq!(
            resolve_download_dir(home, Some("XDG_DOWNLOAD_DIR=\"$HOME\"")),
            PathBuf::from("/home/zoe")
        );
    }
}

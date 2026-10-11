//! Windows path helpers: normalization, case-insensitive comparison,
//! drive mirroring and long-path (`\\?\`) handling.

use std::path::{Component, Path, PathBuf, Prefix};

/// Lexically normalizes a path: unifies separators to `\` and resolves `.` and `..`.
pub fn normalize(p: &Path) -> PathBuf {
    let s = p.to_string_lossy().replace('/', "\\");
    let mut out = PathBuf::new();
    for c in Path::new(&s).components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn comp_key(c: &Component) -> String {
    c.as_os_str().to_string_lossy().to_lowercase()
}

/// Lowercased string form used for case-insensitive lookups.
pub fn key(p: &Path) -> String {
    let s = p.to_string_lossy().to_lowercase();
    s.trim_end_matches('\\').to_string()
}

pub fn eq_ci(a: &Path, b: &Path) -> bool {
    key(a) == key(b)
}

/// Case-insensitive `strip_prefix`. Returns the remainder when `path` is `base` or lies below it.
pub fn strip_prefix_ci(path: &Path, base: &Path) -> Option<PathBuf> {
    let mut it = path.components();
    for b in base.components() {
        match it.next() {
            Some(p) if comp_key(&p) == comp_key(&b) => {}
            _ => return None,
        }
    }
    Some(it.as_path().to_path_buf())
}

/// True when `path` equals `base` or is inside it.
pub fn is_under(path: &Path, base: &Path) -> bool {
    strip_prefix_ci(path, base).is_some()
}

/// Maps an absolute source path onto `root`, dropping the drive letter:
/// `D:\Emulation\ROMs` -> `<root>\Emulation\ROMs`. UNC paths keep `UNC\server\share`.
pub fn mirror(root: &Path, abs: &Path) -> PathBuf {
    let mut out = root.to_path_buf();
    for c in abs.components() {
        match c {
            Component::Prefix(p) => match p.kind() {
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    out.push("UNC");
                    out.push(server);
                    out.push(share);
                }
                _ => {}
            },
            Component::RootDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Rewrites `path` using the first `(from, to)` pair whose `from` contains it.
/// Pairs should be ordered most specific first. Unmatched paths are returned unchanged.
pub fn remap(path: &Path, remaps: &[(PathBuf, PathBuf)]) -> PathBuf {
    for (from, to) in remaps {
        if let Some(rest) = strip_prefix_ci(path, from) {
            return if rest.as_os_str().is_empty() { to.clone() } else { to.join(rest) };
        }
    }
    path.to_path_buf()
}

/// Adds the `\\?\` prefix so file APIs accept paths longer than 260 characters.
pub fn long(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if s.starts_with(r"\\?\") || !p.is_absolute() {
        return p.to_path_buf();
    }
    if let Some(rest) = s.strip_prefix(r"\\") {
        return PathBuf::from(format!(r"\\?\UNC\{rest}"));
    }
    PathBuf::from(format!(r"\\?\{s}"))
}

/// Inverse of [`long`].
pub fn short(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

/// The volume root of an absolute path, e.g. `E:\` or `\\server\share\`.
pub fn volume_root(p: &Path) -> Option<String> {
    match p.components().next()? {
        Component::Prefix(pre) => Some(format!("{}\\", pre.as_os_str().to_string_lossy())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(normalize(Path::new("D:/ES-DE/../ROMs/./snes")), PathBuf::from(r"D:\ROMs\snes"));
        assert_eq!(normalize(Path::new(r"C:\")), PathBuf::from(r"C:\"));
    }

    #[test]
    fn prefix_is_case_insensitive() {
        let p = Path::new(r"D:\Emulation\ROMs\snes\a.zip");
        assert_eq!(strip_prefix_ci(p, Path::new(r"d:\emulation\roms")), Some(PathBuf::from(r"snes\a.zip")));
        assert!(!is_under(Path::new(r"D:\Emulation\ROMs2"), Path::new(r"D:\Emulation\ROMs")));
        assert!(is_under(p, p));
    }

    #[test]
    fn mirrors_drive_letter() {
        assert_eq!(mirror(Path::new(r"E:\"), Path::new(r"D:\Emulation\ROMs")), PathBuf::from(r"E:\Emulation\ROMs"));
        assert_eq!(mirror(Path::new(r"E:\pkg"), Path::new(r"\\nas\games\snes")), PathBuf::from(r"E:\pkg\UNC\nas\games\snes"));
    }

    #[test]
    fn remaps_most_specific_first() {
        let r = vec![
            (PathBuf::from(r"D:\ES-DE\ROMs"), PathBuf::from(r"F:\ROMs")),
            (PathBuf::from(r"D:\ES-DE"), PathBuf::from(r"C:\ES-DE")),
        ];
        assert_eq!(remap(Path::new(r"D:\ES-DE\ROMs\snes"), &r), PathBuf::from(r"F:\ROMs\snes"));
        assert_eq!(remap(Path::new(r"D:\ES-DE\ES-DE.exe"), &r), PathBuf::from(r"C:\ES-DE\ES-DE.exe"));
        assert_eq!(remap(Path::new(r"X:\Other"), &r), PathBuf::from(r"X:\Other"));
    }

    #[test]
    fn long_roundtrip() {
        let p = Path::new(r"D:\a\b");
        assert_eq!(long(p), PathBuf::from(r"\\?\D:\a\b"));
        assert_eq!(short(&long(p)), p);
        assert_eq!(short(&long(Path::new(r"\\nas\s\x"))), PathBuf::from(r"\\nas\s\x"));
    }
}

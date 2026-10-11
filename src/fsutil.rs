//! Directory walking and size helpers shared by the scanner and the planner.

use std::fs::Metadata;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::paths::{key, long, short};

/// Suffix of in-progress copies. A file is only renamed to its real name once fully written.
pub const PART_EXT: &str = ".emuasist-part";

/// Visits every entry below `root` (not `root` itself), skipping `excludes` and
/// leftover `.emuasist-part` files. Errors are collected instead of aborting.
pub fn walk(
    root: &Path,
    excludes: &[PathBuf],
    recursive: bool,
    errors: &mut Vec<String>,
    mut visit: impl FnMut(&Path, &Metadata),
) {
    let ex: Vec<String> = excludes.iter().map(|e| key(e)).collect();
    let mut w = WalkDir::new(long(root)).min_depth(1).follow_links(true);
    if !recursive {
        w = w.max_depth(1);
    }
    let it = w
        .into_iter()
        .filter_entry(|e| ex.is_empty() || !ex.contains(&key(&short(e.path()))));
    for entry in it {
        match entry {
            Ok(e) => {
                let p = short(e.path());
                if p.to_string_lossy().ends_with(PART_EXT) {
                    continue;
                }
                match e.metadata() {
                    Ok(m) => visit(&p, &m),
                    Err(err) => errors.push(format!("{}: {err}", p.display())),
                }
            }
            Err(err) => errors.push(err.to_string()),
        }
    }
}

/// Returns `(file_count, total_bytes)` for a directory tree.
pub fn dir_size(root: &Path, excludes: &[PathBuf], recursive: bool) -> (u64, u64) {
    let mut files = 0;
    let mut bytes = 0;
    let mut errors = Vec::new();
    walk(root, excludes, recursive, &mut errors, |_, m| {
        if m.is_file() {
            files += 1;
            bytes += m.len();
        }
    });
    (files, bytes)
}

pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

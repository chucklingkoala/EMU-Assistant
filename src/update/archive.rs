//! Unpacking downloads (zip / 7z) and the backup zips used for rollback.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use filetime::FileTime;
use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::version::{unix_from_parts, Marker};
use crate::fsutil::{walk, PART_EXT};
use crate::paths::long;

/// Extracts a zip or 7z archive into `dest`, recognized by its signature, not its name.
pub fn extract(archive: &Path, dest: &Path, cancel: &AtomicBool) -> Result<()> {
    fs::create_dir_all(dest)?;
    let mut magic = [0u8; 6];
    let n = File::open(archive)?.read(&mut magic)?;
    match &magic[..n] {
        [0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c] => extract_7z(archive, dest, cancel),
        [b'P', b'K', 3, 4, ..] => extract_zip(archive, dest, cancel),
        _ => bail!(
            "{} is not a zip or 7z archive; set a file pattern in Settings > Update sources that picks one",
            archive.file_name().unwrap_or_default().to_string_lossy()
        ),
    }
}

fn zip_time(f: &zip::read::ZipFile<'_, impl Read>) -> Option<FileTime> {
    let t = f.last_modified()?;
    let unix = unix_from_parts(
        t.year().into(),
        t.month().into(),
        t.day().into(),
        t.hour().into(),
        t.minute().into(),
        t.second().into(),
    );
    Some(FileTime::from_unix_time(unix, 0))
}

fn extract_zip(archive: &Path, dest: &Path, cancel: &AtomicBool) -> Result<()> {
    let mut zip = ZipArchive::new(BufReader::new(File::open(archive)?))
        .with_context(|| format!("{} is not a valid zip", archive.display()))?;
    for i in 0..zip.len() {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let mut f = zip.by_index(i)?;
        // enclosed_name rejects absolute paths and `..`
        let Some(rel) = f.enclosed_name() else { continue };
        let out = long(&dest.join(rel));
        if f.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(p) = out.parent() {
            fs::create_dir_all(p)?;
        }
        let mtime = zip_time(&f);
        let mut w = File::create(&out).with_context(|| format!("creating {}", out.display()))?;
        io::copy(&mut f, &mut w)?;
        drop(w);
        if let Some(t) = mtime {
            let _ = filetime::set_file_mtime(&out, t);
        }
    }
    Ok(())
}

fn extract_7z(archive: &Path, dest: &Path, cancel: &AtomicBool) -> Result<()> {
    let res = sevenz_rust2::decompress_file_with_extract_fn(archive, dest, |entry, reader, path| {
        if cancel.load(Ordering::Relaxed) {
            return Err(sevenz_rust2::Error::Other("cancelled".into()));
        }
        sevenz_rust2::default_entry_extract_fn(entry, reader, path)
    });
    match res {
        Ok(()) => Ok(()),
        Err(_) if cancel.load(Ordering::Relaxed) => bail!("cancelled"),
        Err(e) => match seven_zip_exe() {
            // some archives use filters the built-in decoder lacks; 7-Zip handles everything
            Some(exe) => {
                let _ = fs::remove_dir_all(dest);
                fs::create_dir_all(dest)?;
                let mut cmd = std::process::Command::new(&exe);
                cmd.arg("x").arg("-y").arg(format!("-o{}", dest.display())).arg(archive);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
                }
                let out = cmd.output().with_context(|| format!("running {}", exe.display()))?;
                if !out.status.success() {
                    bail!("7-Zip could not extract {}: {}", archive.display(), String::from_utf8_lossy(&out.stderr).trim());
                }
                Ok(())
            }
            None => bail!("could not extract {}: {e}. Installing 7-Zip lets EMU Assistant handle it.", archive.display()),
        },
    }
}

fn seven_zip_exe() -> Option<PathBuf> {
    let installed = ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
        .iter()
        .filter_map(|v| std::env::var_os(v))
        .map(|p| PathBuf::from(p).join(r"7-Zip\7z.exe"));
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join("7z.exe")).collect::<Vec<_>>())
        .unwrap_or_default();
    installed.chain(on_path).find(|p| p.is_file())
}

/// The folder inside an unpacked archive that holds `exe_name` (the shallowest match),
/// so archives with or without a top-level folder both work.
pub fn find_root(staging: &Path, exe_name: &str) -> Option<PathBuf> {
    let mut errors = Vec::new();
    let mut best: Option<(usize, PathBuf)> = None;
    walk(staging, &[], true, &mut errors, |p, m| {
        if m.is_file() && p.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(exe_name)) {
            let depth = p.components().count();
            if best.as_ref().is_none_or(|(d, _)| depth < *d) {
                best = p.parent().map(|d| (depth, d.to_path_buf()));
            }
        }
    });
    best.map(|(_, d)| d)
}

/// One file saved in a backup, relative to the install folder.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SavedFile {
    pub path: String,
    pub mtime: i64,
}

/// `backup.json` inside each backup zip.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct BackupMeta {
    pub id: String,
    pub label: String,
    pub dir: PathBuf,
    pub exe: PathBuf,
    pub created_unix: i64,
    pub old_version: String,
    pub new_version: String,
    /// Files the update overwrote; their old content is in the zip under `files/`.
    pub replaced: Vec<SavedFile>,
    /// Files the update created; a restore deletes them.
    pub added: Vec<String>,
    pub old_marker: Option<Marker>,
}

const META: &str = "backup.json";
const FILES: &str = "files/";

fn zip_name(rel: &str) -> String {
    format!("{FILES}{}", rel.replace('\\', "/"))
}

/// Writes the backup zip: the current content of every `meta.replaced` file under
/// `meta.dir`, plus `backup.json`. Written to a part file and renamed when complete.
pub fn write_backup(out: &Path, meta: &BackupMeta, cancel: &AtomicBool, on_bytes: &mut dyn FnMut(u64)) -> Result<()> {
    if let Some(p) = out.parent() {
        fs::create_dir_all(p)?;
    }
    let mut part = out.as_os_str().to_owned();
    part.push(PART_EXT);
    let part = PathBuf::from(part);
    let res = (|| -> Result<()> {
        let mut zip = ZipWriter::new(File::create(&part)?);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated).large_file(true);
        zip.start_file(META, SimpleFileOptions::default())?;
        zip.write_all(serde_json::to_string_pretty(meta)?.as_bytes())?;
        let mut buf = vec![0u8; 1 << 20];
        for f in &meta.replaced {
            if cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }
            zip.start_file(zip_name(&f.path), opts)?;
            let src = long(&meta.dir.join(&f.path));
            let mut r = File::open(&src).with_context(|| format!("reading {}", src.display()))?;
            loop {
                let n = r.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                zip.write_all(&buf[..n])?;
                on_bytes(n as u64);
            }
        }
        zip.finish()?.sync_all()?;
        Ok(())
    })();
    if let Err(e) = res {
        let _ = fs::remove_file(&part);
        return Err(e);
    }
    fs::rename(&part, out)?;
    Ok(())
}

pub fn read_meta(zip_path: &Path) -> Result<BackupMeta> {
    let mut zip = ZipArchive::new(BufReader::new(File::open(zip_path)?))?;
    let mut f = zip.by_name(META).with_context(|| format!("{} is not an EMU Assistant backup", zip_path.display()))?;
    let mut s = String::new();
    f.read_to_string(&mut s)?;
    Ok(serde_json::from_str(&s)?)
}

#[derive(Default, Debug)]
pub struct RestoreResult {
    pub restored: u64,
    pub removed: u64,
    pub failed: Vec<String>,
}

/// Puts the saved files back and deletes the files the update added. `only` limits the
/// restore to those relative paths (from `replaced` or `added`).
pub fn restore(zip_path: &Path, only: Option<&BTreeSet<String>>, cancel: &AtomicBool) -> Result<RestoreResult> {
    let meta = read_meta(zip_path)?;
    let mut zip = ZipArchive::new(BufReader::new(File::open(zip_path)?))?;
    let wanted = |p: &str| only.is_none_or(|o| o.contains(p));
    let mut res = RestoreResult::default();
    for f in meta.replaced.iter().filter(|f| wanted(&f.path)) {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let dst = long(&meta.dir.join(&f.path));
        let r = (|| -> Result<()> {
            let mut entry = zip.by_name(&zip_name(&f.path))?;
            if let Some(p) = dst.parent() {
                fs::create_dir_all(p)?;
            }
            let mut part = dst.as_os_str().to_owned();
            part.push(PART_EXT);
            let part = PathBuf::from(part);
            let mut w = File::create(&part)?;
            io::copy(&mut entry, &mut w)?;
            w.sync_all()?;
            drop(w);
            filetime::set_file_mtime(&part, FileTime::from_unix_time(f.mtime, 0))?;
            fs::rename(&part, &dst)?;
            Ok(())
        })();
        match r {
            Ok(()) => res.restored += 1,
            Err(e) => res.failed.push(format!("{}: {e:#}", f.path)),
        }
    }
    for rel in meta.added.iter().filter(|p| wanted(p)) {
        let p = meta.dir.join(rel);
        match fs::remove_file(long(&p)) {
            Ok(()) => {
                res.removed += 1;
                remove_empty_parents(&p, &meta.dir);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => res.failed.push(format!("delete {rel}: {e}")),
        }
    }
    // a full restore also brings back the old version record
    if only.is_none() {
        let marker = meta.dir.join(super::version::MARKER);
        match &meta.old_marker {
            Some(m) => super::version::write_marker(&meta.dir, m)?,
            None => {
                let _ = fs::remove_file(marker);
            }
        }
    }
    Ok(res)
}

fn remove_empty_parents(file: &Path, stop: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if crate::paths::eq_ci(d, stop) || !crate::paths::is_under(d, stop) || fs::remove_dir(long(d)).is_err() {
            break;
        }
        dir = d.parent();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a zip from `(name, content)` pairs.
    pub fn make_zip(path: &Path, files: &[(&str, &[u8])]) {
        let mut z = ZipWriter::new(File::create(path).unwrap());
        for (name, data) in files {
            z.start_file(*name, SimpleFileOptions::default()).unwrap();
            z.write_all(data).unwrap();
        }
        z.finish().unwrap();
    }

    #[test]
    fn extracts_zip_and_finds_root() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.zip");
        make_zip(&a, &[("RetroArch-Win64/retroarch.exe", b"ra"), ("RetroArch-Win64/assets/x.png", b"p"), ("../evil.txt", b"x")]);
        let out = tmp.path().join("out");
        extract(&a, &out, &AtomicBool::new(false)).unwrap();
        assert!(!tmp.path().join("evil.txt").exists());
        let root = find_root(&out, "RETROARCH.EXE").unwrap();
        assert!(crate::paths::eq_ci(&root, &out.join("RetroArch-Win64")));
        assert!(find_root(&out, "missing.exe").is_none());

        // archive without a top-level folder
        let b = tmp.path().join("b.zip");
        make_zip(&b, &[("mgba.exe", b"m"), ("sub/mgba.exe", b"other")]);
        let out = tmp.path().join("out2");
        extract(&b, &out, &AtomicBool::new(false)).unwrap();
        assert!(crate::paths::eq_ci(&find_root(&out, "mgba.exe").unwrap(), &out));
    }

    #[test]
    fn backup_roundtrip_full_and_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("emu");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.dll"), b"old a").unwrap();
        fs::write(dir.join(r"sub\b.dat"), b"old b").unwrap();
        let meta = BackupMeta {
            id: "X".into(),
            dir: dir.clone(),
            replaced: vec![SavedFile { path: "a.dll".into(), mtime: 1_000_000 }, SavedFile { path: r"sub\b.dat".into(), mtime: 1_000_000 }],
            added: vec![r"new\c.txt".into()],
            ..Default::default()
        };
        let zip = tmp.path().join("bk.zip");
        write_backup(&zip, &meta, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(read_meta(&zip).unwrap().replaced.len(), 2);

        // simulate the update
        fs::write(dir.join("a.dll"), b"new a").unwrap();
        fs::write(dir.join(r"sub\b.dat"), b"new b").unwrap();
        fs::create_dir_all(dir.join("new")).unwrap();
        fs::write(dir.join(r"new\c.txt"), b"added").unwrap();

        let only: BTreeSet<String> = ["a.dll".to_string()].into();
        let r = restore(&zip, Some(&only), &AtomicBool::new(false)).unwrap();
        assert_eq!((r.restored, r.removed), (1, 0));
        assert_eq!(fs::read(dir.join("a.dll")).unwrap(), b"old a");
        assert_eq!(fs::read(dir.join(r"sub\b.dat")).unwrap(), b"new b");

        let r = restore(&zip, None, &AtomicBool::new(false)).unwrap();
        assert_eq!((r.restored, r.removed, r.failed.len()), (2, 1, 0));
        assert_eq!(fs::read(dir.join(r"sub\b.dat")).unwrap(), b"old b");
        assert!(!dir.join("new").exists());
    }
}

//! Executes a [`Plan`]: copies via `<file>.emuasist-part` + rename so an interrupted
//! run never leaves a half-written file under its real name, and stamps the source
//! modification time so the next run can skip unchanged files.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use filetime::FileTime;

use crate::fsutil::PART_EXT;
use crate::paths::long;
use crate::plan::Plan;

#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub done_files: u64,
    pub total_files: u64,
    pub current: String,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub copied: u64,
    pub copied_bytes: u64,
    pub pruned: u64,
    pub failed: Vec<String>,
    pub cancelled: bool,
    pub elapsed: Duration,
}

pub enum Event {
    Progress(Progress),
    Log(String),
    Done(Report),
}

const BUF: usize = 4 << 20;

fn part_path(dst: &Path) -> PathBuf {
    let mut s = dst.as_os_str().to_owned();
    s.push(PART_EXT);
    PathBuf::from(s)
}

/// Copies one file. Returns `Ok(false)` if cancelled (the partial file is removed).
pub fn copy_file(
    src: &Path,
    dst: &Path,
    mtime: FileTime,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
) -> io::Result<bool> {
    let (src, dst) = (long(src), long(dst));
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let part = part_path(&dst);
    let mut r = File::open(&src)?;
    let mut w = File::create(&part)?;
    let mut buf = vec![0u8; BUF];
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(w);
            let _ = fs::remove_file(&part);
            return Ok(false);
        }
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
        on_bytes(n as u64);
    }
    w.sync_all()?;
    drop(w);
    filetime::set_file_mtime(&part, mtime)?;
    if let Ok(m) = fs::metadata(&dst) {
        let mut p = m.permissions();
        if p.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            p.set_readonly(false);
            let _ = fs::set_permissions(&dst, p);
        }
    }
    fs::rename(&part, &dst)?;
    Ok(true)
}

/// Byte-for-byte comparison of two files.
pub fn same_content(a: &Path, b: &Path) -> io::Result<bool> {
    let (mut fa, mut fb) = (File::open(long(a))?, File::open(long(b))?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let (mut ba, mut bb) = (vec![0u8; BUF], vec![0u8; BUF]);
    loop {
        let n = fa.read(&mut ba)?;
        if n == 0 {
            return Ok(true);
        }
        fb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

pub fn execute(plan: &Plan, verify: bool, cancel: &AtomicBool, tx: &Sender<Event>) -> Report {
    let start = Instant::now();
    let mut report = Report::default();
    let log = |s: String| {
        let _ = tx.send(Event::Log(s));
    };
    for d in &plan.dirs {
        if let Err(e) = fs::create_dir_all(long(d)) {
            log(format!("Could not create {}: {e}", d.display()));
        }
    }

    let total_bytes = plan.copy_bytes;
    let total_files = plan.copy.len() as u64;
    let mut done = 0u64;
    let mut last = Instant::now() - Duration::from_secs(1);
    for (i, item) in plan.copy.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        let mut file_done = 0u64;
        let res = copy_file(&item.src, &item.dst, item.mtime, cancel, &mut |n| {
            file_done += n;
            if last.elapsed() >= Duration::from_millis(100) {
                last = Instant::now();
                let _ = tx.send(Event::Progress(Progress {
                    done_bytes: done + file_done,
                    total_bytes,
                    done_files: i as u64,
                    total_files,
                    current: item.dst.display().to_string(),
                }));
            }
        });
        match res {
            Ok(true) => {
                if verify && !same_content(&item.src, &item.dst).unwrap_or(false) {
                    let _ = fs::remove_file(long(&item.dst));
                    report.failed.push(format!("{}: verification failed", item.dst.display()));
                    log(format!("Verification failed, removed {}", item.dst.display()));
                } else {
                    report.copied += 1;
                    report.copied_bytes += item.size;
                }
            }
            Ok(false) => {
                report.cancelled = true;
                break;
            }
            Err(e) => {
                report.failed.push(format!("{}: {e}", item.src.display()));
                log(format!("Failed {}: {e}", item.src.display()));
            }
        }
        done += item.size;
    }

    if !report.cancelled {
        for p in &plan.prune {
            match fs::remove_file(long(p)) {
                Ok(()) => report.pruned += 1,
                Err(e) => report.failed.push(format!("delete {}: {e}", p.display())),
            }
        }
    }
    let _ = tx.send(Event::Progress(Progress {
        done_bytes: done,
        total_bytes,
        done_files: report.copied,
        total_files,
        current: String::new(),
    }));
    report.elapsed = start.elapsed();
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{build, Group, Mapper, PlanOptions};

    fn plan_for(src: &Path, dest: &Path, opts: PlanOptions) -> Plan {
        let g = Group { label: "t".into(), src: src.to_path_buf(), excludes: vec![], recursive: true, prunable: true, system: None };
        build(&[g], &Mapper::Export { dest_root: dest.to_path_buf() }, &opts, &AtomicBool::new(false), &mut |_| {}).unwrap()
    }

    const OPTS: PlanOptions = PlanOptions { protect_newer: true, prune: false };

    #[test]
    fn copies_then_skips_then_resumes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("drive");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.bin"), vec![1u8; 10_000]).unwrap();
        fs::write(src.join(r"sub\b.bin"), b"bbb").unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded();

        let p = plan_for(&src, &dest, OPTS.clone());
        assert_eq!(p.copy.len(), 2);
        let r = execute(&p, true, &AtomicBool::new(false), &tx);
        assert_eq!((r.copied, r.failed.len()), (2, 0));
        let a_dst = crate::paths::mirror(&dest, &src.join("a.bin"));
        assert_eq!(fs::read(&a_dst).unwrap().len(), 10_000);

        // second run: nothing to do
        let p = plan_for(&src, &dest, OPTS.clone());
        assert_eq!((p.copy.len(), p.skipped), (0, 2));

        // simulate an interrupted copy: partial temp file + missing target
        fs::remove_file(&a_dst).unwrap();
        fs::write(part_path(&a_dst), b"partial").unwrap();
        let p = plan_for(&src, &dest, OPTS.clone());
        assert_eq!(p.copy.len(), 1);
        execute(&p, false, &AtomicBool::new(false), &tx);
        assert_eq!(fs::read(&a_dst).unwrap().len(), 10_000);
        assert!(!part_path(&a_dst).exists());
    }

    #[test]
    fn keeps_newer_destination_files_and_prunes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("drive");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("save.srm"), b"old").unwrap();
        fs::write(src.join("gone.bin"), b"x").unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded();
        execute(&plan_for(&src, &dest, OPTS.clone()), false, &AtomicBool::new(false), &tx);

        let save_dst = crate::paths::mirror(&dest, &src.join("save.srm"));
        fs::write(&save_dst, b"newer progress").unwrap();
        let future = FileTime::from_unix_time(FileTime::now().unix_seconds() + 3600, 0);
        filetime::set_file_mtime(&save_dst, future).unwrap();
        fs::remove_file(src.join("gone.bin")).unwrap();

        let p = plan_for(&src, &dest, PlanOptions { protect_newer: true, prune: true });
        assert_eq!(p.copy.len(), 0);
        assert_eq!(p.protected.len(), 1);
        assert_eq!(p.prune.len(), 1);
        let r = execute(&p, false, &AtomicBool::new(false), &tx);
        assert_eq!(r.pruned, 1);
        assert_eq!(fs::read(&save_dst).unwrap(), b"newer progress");

        let p = plan_for(&src, &dest, PlanOptions { protect_newer: false, prune: false });
        assert_eq!(p.copy.len(), 1);
    }

    #[test]
    fn cancel_leaves_no_partial_file() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("big.bin");
        fs::write(&src, vec![0u8; BUF * 2]).unwrap();
        let dst = tmp.path().join(r"out\big.bin");
        let cancel = AtomicBool::new(false);
        let ok = copy_file(&src, &dst, FileTime::now(), &cancel, &mut |_| cancel.store(true, Ordering::Relaxed)).unwrap();
        assert!(!ok);
        assert!(!dst.exists() && !part_path(&dst).exists());
    }
}

//! Executes a [`Plan`]: copies via `<file>.emuasist-part` + rename so an interrupted
//! run never leaves a half-written file under its real name, and stamps the source
//! modification time so the next run can skip unchanged files.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use filetime::FileTime;

use crate::fsutil::PART_EXT;
use crate::paths::{eq_ci, is_under, key, long};
use crate::plan::{Plan, PlanItem};

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

/// Files smaller than this are copied on [`SMALL_WORKERS`] threads at once. For tens of
/// thousands of small media files the time goes into opening, flushing and renaming
/// each file, not into the bytes, and that waiting overlaps well.
pub const SMALL_MAX: u64 = 1 << 20;
const SMALL_WORKERS: usize = 4;

/// Copy buffers, allocated once per thread instead of once per file.
#[derive(Default)]
pub struct Bufs {
    a: Vec<u8>,
    b: Vec<u8>,
}

impl Bufs {
    fn get(&mut self) -> (&mut [u8], &mut [u8]) {
        if self.a.len() < BUF {
            self.a = vec![0u8; BUF];
            self.b = vec![0u8; BUF];
        }
        (&mut self.a, &mut self.b)
    }
}

fn part_path(dst: &Path) -> PathBuf {
    let mut s = dst.as_os_str().to_owned();
    s.push(PART_EXT);
    PathBuf::from(s)
}

/// Copies one file. Returns `Ok(false)` if cancelled. With `verify`, the copy is re-read
/// and compared with the source before it takes the destination's name. A cancelled or
/// failed copy leaves no part file behind and does not touch an existing destination file.
pub fn copy_file(
    src: &Path,
    dst: &Path,
    mtime: FileTime,
    verify: bool,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
) -> io::Result<bool> {
    copy_file_buf(src, dst, mtime, verify, cancel, &mut Bufs::default(), on_bytes)
}

/// [`copy_file`] with buffers the caller keeps between files.
pub fn copy_file_buf(
    src: &Path,
    dst: &Path,
    mtime: FileTime,
    verify: bool,
    cancel: &AtomicBool,
    bufs: &mut Bufs,
    on_bytes: &mut dyn FnMut(u64),
) -> io::Result<bool> {
    let (src, dst) = (long(src), long(dst));
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let part = part_path(&dst);
    let res = write_part(&src, &part, mtime, verify, cancel, bufs, on_bytes).and_then(|done| {
        if done {
            if let Ok(m) = fs::metadata(&dst) {
                let mut p = m.permissions();
                if p.readonly() {
                    #[allow(clippy::permissions_set_readonly_false)]
                    p.set_readonly(false);
                    let _ = fs::set_permissions(&dst, p);
                }
            }
            fs::rename(&part, &dst)?;
        }
        Ok(done)
    });
    if !matches!(res, Ok(true)) {
        let _ = fs::remove_file(&part);
    }
    res
}

/// Writes `src` to `part` (both already in long-path form). `Ok(false)` when cancelled.
fn write_part(
    src: &Path,
    part: &Path,
    mtime: FileTime,
    verify: bool,
    cancel: &AtomicBool,
    bufs: &mut Bufs,
    on_bytes: &mut dyn FnMut(u64),
) -> io::Result<bool> {
    let mut r = File::open(src)?;
    let mut w = File::create(part)?;
    let (buf, _) = bufs.get();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let n = r.read(buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
        on_bytes(n as u64);
    }
    w.sync_all()?;
    drop(w);
    if verify && !same_content_buf(src, part, bufs)? {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "verification failed: the copy differs from the source"));
    }
    filetime::set_file_mtime(part, mtime)?;
    Ok(true)
}

/// Byte-for-byte comparison of two files.
pub fn same_content(a: &Path, b: &Path) -> io::Result<bool> {
    same_content_buf(a, b, &mut Bufs::default())
}

fn same_content_buf(a: &Path, b: &Path, bufs: &mut Bufs) -> io::Result<bool> {
    let (mut fa, mut fb) = (File::open(long(a))?, File::open(long(b))?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let (ba, bb) = bufs.get();
    loop {
        let n = fa.read(ba)?;
        if n == 0 {
            return Ok(true);
        }
        fb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

/// After this many failed files in a row the run stops: the drive is most likely gone.
const MAX_CONSECUTIVE_FAILURES: u32 = 20;

/// Errors after which trying more files is pointless: disk full, device not ready or removed.
fn is_fatal(e: &io::Error) -> bool {
    // ERROR_NOT_READY 21, ERROR_HANDLE_DISK_FULL 39, ERROR_DISK_FULL 112, ERROR_DEVICE_NOT_CONNECTED 1167
    e.kind() == io::ErrorKind::StorageFull || matches!(e.raw_os_error(), Some(21 | 39 | 112 | 1167))
}

/// Removes the folders that deleting `file` left empty, up to but not including `root`.
/// Folders that exist in the source (`keep`, lowercase keys) stay even when empty.
fn remove_emptied_dirs(file: &Path, root: &Path, keep: &HashSet<String>) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if eq_ci(d, root) || !is_under(d, root) || keep.contains(&key(d)) || fs::remove_dir(long(d)).is_err() {
            break;
        }
        dir = d.parent();
    }
}

/// State of one [`execute`] run, shared by the copy threads.
struct Run<'a> {
    verify: bool,
    cancel: &'a AtomicBool,
    tx: &'a Sender<Event>,
    total_bytes: u64,
    total_files: u64,
    done_bytes: AtomicU64,
    done_files: AtomicU64,
    last_progress: Mutex<Instant>,
    consecutive: AtomicU32,
    /// A fatal error or too many failures in a row: no more files are started.
    stopped: AtomicBool,
    cancelled: AtomicBool,
    report: Mutex<Report>,
}

impl Run<'_> {
    fn log(&self, s: String) {
        let _ = self.tx.send(Event::Log(s));
    }

    /// Sends a progress event at most every 100 ms, from whichever thread gets there.
    fn progress(&self, current: &Path) {
        let Ok(mut last) = self.last_progress.try_lock() else { return };
        if last.elapsed() < Duration::from_millis(100) {
            return;
        }
        *last = Instant::now();
        let _ = self.tx.send(Event::Progress(Progress {
            done_bytes: self.done_bytes.load(Ordering::Relaxed),
            total_bytes: self.total_bytes,
            done_files: self.done_files.load(Ordering::Relaxed),
            total_files: self.total_files,
            current: current.display().to_string(),
        }));
    }

    /// Copies one item. Returns false when the run must not start another file.
    fn copy(&self, item: &PlanItem, bufs: &mut Bufs) -> bool {
        if self.stopped.load(Ordering::Relaxed) {
            return false;
        }
        if self.cancel.load(Ordering::Relaxed) {
            self.cancelled.store(true, Ordering::Relaxed);
            return false;
        }
        let mut file_done = 0u64;
        let res = copy_file_buf(&item.src, &item.dst, item.mtime, self.verify, self.cancel, bufs, &mut |n| {
            file_done += n;
            self.done_bytes.fetch_add(n, Ordering::Relaxed);
            self.progress(&item.dst);
        });
        match res {
            Ok(true) => {
                self.consecutive.store(0, Ordering::Relaxed);
                let mut r = self.report.lock().unwrap();
                r.copied += 1;
                r.copied_bytes += item.size;
            }
            Ok(false) => {
                self.cancelled.store(true, Ordering::Relaxed);
                return false;
            }
            Err(e) => {
                let msg = format!("{}: {e}", item.src.display());
                self.log(format!("Failed {msg}"));
                self.report.lock().unwrap().failed.push(msg);
                let n = self.consecutive.fetch_add(1, Ordering::Relaxed) + 1;
                if is_fatal(&e) || n >= MAX_CONSECUTIVE_FAILURES {
                    // the first thread to stop explains why
                    if !self.stopped.swap(true, Ordering::Relaxed) {
                        let why = if is_fatal(&e) {
                            format!("Stopped: {e}. The remaining files were not copied.")
                        } else {
                            format!("Stopped after {n} failed files in a row. Is the drive still connected?")
                        };
                        self.report.lock().unwrap().failed.push(why.clone());
                        self.log(why);
                    }
                    return false;
                }
            }
        }
        // count the whole file, also when it failed half-way, so the bar keeps moving
        self.done_bytes.fetch_add(item.size.saturating_sub(file_done), Ordering::Relaxed);
        self.done_files.fetch_add(1, Ordering::Relaxed);
        true
    }
}

pub fn execute(plan: &Plan, verify: bool, cancel: &AtomicBool, tx: &Sender<Event>) -> Report {
    let start = Instant::now();
    for d in &plan.dirs {
        if let Err(e) = fs::create_dir_all(long(d)) {
            let _ = tx.send(Event::Log(format!("Could not create {}: {e}", d.display())));
        }
    }
    let run = Run {
        verify,
        cancel,
        tx,
        total_bytes: plan.copy_bytes,
        total_files: plan.copy.len() as u64,
        done_bytes: AtomicU64::new(0),
        done_files: AtomicU64::new(0),
        last_progress: Mutex::new(Instant::now() - Duration::from_secs(1)),
        consecutive: AtomicU32::new(0),
        stopped: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        report: Mutex::new(Report::default()),
    };

    // small files on several threads first, then the large ones one at a time
    let (small, large): (Vec<&PlanItem>, Vec<&PlanItem>) = plan.copy.iter().partition(|i| i.size < SMALL_MAX);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..SMALL_WORKERS.min(small.len()) {
            s.spawn(|| {
                let mut bufs = Bufs::default();
                while let Some(item) = small.get(next.fetch_add(1, Ordering::Relaxed)) {
                    if !run.copy(item, &mut bufs) {
                        break;
                    }
                }
            });
        }
    });
    let mut bufs = Bufs::default();
    for item in large {
        if !run.copy(item, &mut bufs) {
            break;
        }
    }
    // a cancel that came between two files
    if cancel.load(Ordering::Relaxed) && run.done_files.load(Ordering::Relaxed) < run.total_files {
        run.cancelled.store(true, Ordering::Relaxed);
    }

    let (cancelled, stopped) = (run.cancelled.load(Ordering::Relaxed), run.stopped.load(Ordering::Relaxed));
    let mut report = std::mem::take(&mut *run.report.lock().unwrap());
    report.cancelled = cancelled;
    if !cancelled && !stopped {
        let keep: HashSet<String> = plan.dirs.iter().map(|d| key(d)).collect();
        for p in &plan.prune {
            match fs::remove_file(long(p)) {
                Ok(()) => {
                    report.pruned += 1;
                    if let Some(root) = plan.prune_roots.iter().find(|r| is_under(p, r)) {
                        remove_emptied_dirs(p, root, &keep);
                    }
                }
                Err(e) => report.failed.push(format!("delete {}: {e}", p.display())),
            }
        }
    }
    let _ = tx.send(Event::Progress(Progress {
        done_bytes: run.done_bytes.load(Ordering::Relaxed),
        total_bytes: run.total_bytes,
        done_files: report.copied,
        total_files: run.total_files,
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
        let g = Group { label: "t".into(), src: src.to_path_buf(), recursive: true, prunable: true, ..Default::default() };
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
        let ok = copy_file(&src, &dst, FileTime::now(), false, &cancel, &mut |_| cancel.store(true, Ordering::Relaxed)).unwrap();
        assert!(!ok);
        assert!(!dst.exists() && !part_path(&dst).exists());
    }

    #[test]
    fn failed_copy_leaves_no_partial_file() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.bin");
        fs::write(&src, b"data").unwrap();
        // the destination name is taken by a non-empty folder, so the final rename fails
        let dst = tmp.path().join("out");
        fs::create_dir_all(dst.join("sub")).unwrap();
        let r = copy_file(&src, &dst, FileTime::now(), true, &AtomicBool::new(false), &mut |_| {});
        assert!(r.is_err());
        assert!(!part_path(&dst).exists());
        assert!(dst.join("sub").is_dir());
    }

    #[test]
    fn prune_removes_emptied_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("drive");
        fs::create_dir_all(src.join("keep")).unwrap();
        fs::create_dir_all(src.join(r"gone\sub")).unwrap();
        fs::write(src.join(r"keep\a.bin"), b"a").unwrap();
        fs::write(src.join(r"gone\sub\b.bin"), b"b").unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let no = AtomicBool::new(false);
        execute(&plan_for(&src, &dest, OPTS.clone()), false, &no, &tx);
        let gone_dst = crate::paths::mirror(&dest, &src.join("gone"));
        assert!(gone_dst.join(r"sub\b.bin").is_file());

        fs::remove_dir_all(src.join("gone")).unwrap();
        let p = plan_for(&src, &dest, PlanOptions { protect_newer: true, prune: true });
        assert_eq!(p.prune.len(), 1);
        let r = execute(&p, false, &no, &tx);
        assert_eq!((r.pruned, r.failed.len()), (1, 0));
        assert!(!gone_dst.exists(), "the emptied game folder is removed");
        assert!(crate::paths::mirror(&dest, &src.join(r"keep\a.bin")).is_file());
    }

    #[test]
    fn copies_many_small_and_some_large_files() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("drive");
        for i in 0..500 {
            let p = src.join(format!(r"d{}\f{i}.bin", i % 7));
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, format!("small {i}")).unwrap();
        }
        let big = vec![7u8; SMALL_MAX as usize + 10];
        fs::write(src.join("big1.bin"), &big).unwrap();
        fs::write(src.join("big2.bin"), &big).unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let p = plan_for(&src, &dest, OPTS.clone());
        assert_eq!(p.copy.len(), 502);
        let r = execute(&p, true, &AtomicBool::new(false), &tx);
        assert_eq!((r.copied, r.copied_bytes, r.failed.len(), r.cancelled), (502, p.copy_bytes, 0, false));
        for item in &p.copy {
            assert_eq!(fs::read(&item.dst).unwrap(), fs::read(&item.src).unwrap());
            let t = FileTime::from_last_modification_time(&fs::metadata(&item.dst).unwrap());
            assert_eq!(t.unix_seconds(), item.mtime.unix_seconds());
        }
        // the last progress event reports everything done
        let last = rx.try_iter().filter_map(|e| if let Event::Progress(p) = e { Some(p) } else { None }).last().unwrap();
        assert_eq!((last.done_bytes, last.done_files), (p.copy_bytes, 502));
        assert_eq!(plan_for(&src, &dest, OPTS.clone()).copy.len(), 0);
    }

    #[test]
    fn small_file_failures_are_reported_and_the_rest_copied() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("drive");
        fs::create_dir_all(&src).unwrap();
        for i in 0..50 {
            fs::write(src.join(format!("f{i}.bin")), b"x").unwrap();
        }
        let p = plan_for(&src, &dest, OPTS.clone());
        // a non-empty folder where three of the files should go: their rename fails
        for i in [3, 17, 41] {
            fs::create_dir_all(crate::paths::mirror(&dest, &src.join(format!(r"f{i}.bin\sub")))).unwrap();
        }
        let (tx, _rx) = crossbeam_channel::unbounded();
        let r = execute(&p, false, &AtomicBool::new(false), &tx);
        assert_eq!((r.copied, r.failed.len()), (47, 3), "{:?}", r.failed);
    }

    #[test]
    fn cancel_during_small_files_leaves_no_part_files() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("drive");
        fs::create_dir_all(&src).unwrap();
        for i in 0..400 {
            fs::write(src.join(format!("f{i}.bin")), vec![1u8; 2000]).unwrap();
        }
        let p = plan_for(&src, &dest, OPTS.clone());
        let (tx, rx) = crossbeam_channel::unbounded();
        let cancel = AtomicBool::new(false);
        let r = std::thread::scope(|s| {
            let h = s.spawn(|| execute(&p, false, &cancel, &tx));
            // cancel as soon as the first progress event arrives
            let _ = rx.recv();
            cancel.store(true, Ordering::Relaxed);
            h.join().unwrap()
        });
        assert!(r.cancelled || r.copied == 400);
        let parts = walkdir::WalkDir::new(&dest).into_iter().flatten().filter(|e| e.path().to_string_lossy().ends_with(PART_EXT)).count();
        assert_eq!(parts, 0);
    }

    /// Copies 20,000 files of 2 KB into `EMU_BENCH_DIR` and prints files per second:
    /// `set EMU_BENCH_DIR=E:\bench` then `cargo test --release bench_small_files -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_small_files() {
        let Ok(dir) = std::env::var("EMU_BENCH_DIR") else {
            eprintln!("set EMU_BENCH_DIR to a folder on the drive to measure");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("bench_src");
        for i in 0..20_000 {
            let p = src.join(format!(r"d{}\f{i}.png", i % 100));
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, vec![(i % 251) as u8; 2048]).unwrap();
        }
        let dest = crate::paths::normalize(Path::new(&dir)).join("emuasist-bench");
        let _ = fs::remove_dir_all(&dest);
        let p = plan_for(&src, &dest, OPTS.clone());
        let (tx, _rx) = crossbeam_channel::unbounded();
        assert_eq!(p.copy.len(), 20_000, "{:?}", p.warnings);
        let r = execute(&p, false, &AtomicBool::new(false), &tx);
        assert!(r.failed.is_empty(), "{:?}", r.failed.first());
        let secs = r.elapsed.as_secs_f64();
        println!("{} files in {secs:.1} s: {:.0} files/s", r.copied, r.copied as f64 / secs);
        let _ = fs::remove_dir_all(&dest);
    }
}

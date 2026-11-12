//! Running an export: manifest, copy, and the exe on the drive. Used by the window and
//! by `--export`.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use crossbeam_channel::Sender;
use filetime::FileTime;

use crate::manifest;
use crate::paths::eq_ci;
use crate::plan::{Plan, Selection};
use crate::scan::Catalog;
use crate::sync::{self, Event, Report};

pub struct Job<'a> {
    pub cat: &'a Catalog,
    pub sel: &'a Selection,
    pub plan: &'a Plan,
    pub dest_root: &'a Path,
    /// The profile this export belongs to, recorded in the manifest.
    pub profile: Option<&'a str>,
    pub verify: bool,
    pub copy_exe: bool,
}

/// Writes the manifest (marked incomplete), copies, marks it complete when nothing
/// failed, and puts EMUAssistant.exe on the drive. Log lines go to `tx`.
pub fn run(job: &Job, cancel: &AtomicBool, tx: &Sender<Event>) -> Report {
    let log = |s: String| {
        let _ = tx.send(Event::Log(s));
    };
    let existing = manifest::load(job.dest_root).unwrap_or_else(|e| {
        log(format!("Ignoring unreadable manifest: {e:#}"));
        None
    });
    let mut m = manifest::for_export(existing, job.cat, job.sel, job.profile);
    if let Err(e) = manifest::save(job.dest_root, &m) {
        log(format!("Could not write manifest: {e:#}"));
    }
    let report = sync::execute(job.plan, job.verify, cancel, tx);
    m.complete = !report.cancelled && report.failed.is_empty();
    if let Err(e) = manifest::save(job.dest_root, &m) {
        log(format!("Could not write manifest: {e:#}"));
    }
    if job.copy_exe {
        match copy_self(job.dest_root) {
            Ok(Some(p)) => log(format!("Copied EMU Assistant to {} (run it on the target machine to deploy)", p.display())),
            Ok(None) => {}
            Err(e) => log(format!("Could not copy EMU Assistant itself to the drive: {e:#}")),
        }
    }
    report
}

/// Puts this exe on the drive so it can be run on the target machine for deployment.
/// The copy made under the old name by earlier versions is removed.
fn copy_self(dest_root: &Path) -> anyhow::Result<Option<PathBuf>> {
    let exe = std::env::current_exe()?;
    let legacy = dest_root.join(manifest::LEGACY_EXE_NAME);
    if legacy.is_file() && !eq_ci(&exe, &legacy) {
        let _ = std::fs::remove_file(&legacy);
    }
    let target = dest_root.join(manifest::EXE_NAME);
    if eq_ci(&exe, &target) {
        return Ok(None);
    }
    let meta = std::fs::metadata(&exe)?;
    let mtime = FileTime::from_last_modification_time(&meta);
    if let Ok(t) = std::fs::metadata(&target) {
        let diff = FileTime::from_last_modification_time(&t).unix_seconds() - mtime.unix_seconds();
        if t.len() == meta.len() && diff.abs() <= 2 {
            return Ok(None);
        }
    }
    sync::copy_file(&exe, &target, mtime, false, &AtomicBool::new(false), &mut |_| {})?;
    Ok(Some(target))
}

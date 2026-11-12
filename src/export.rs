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

/// `--export` arguments; unset ones come from the saved config.
#[derive(Default, Debug)]
pub struct Headless {
    pub profile: Option<String>,
    pub dest: Option<PathBuf>,
    pub es_root: Option<PathBuf>,
}

/// Why a headless export didn't start (exit code 2).
#[derive(Debug)]
pub struct NotStarted(pub String);

/// Exports the saved selection (or a profile's) without the window. Never deletes
/// anything at the destination. `out` gets every line to show and log. The config is
/// only read: a profile is applied in memory.
pub fn headless(args: &Headless, mut cfg: crate::config::Config, out: &(dyn Fn(String) + Sync)) -> Result<Report, NotStarted> {
    use crate::{drives, plan, scan, select};
    let fail = |s: String| Err(NotStarted(s));
    if let Some(p) = &args.profile {
        if !cfg.profiles.contains_key(p) {
            let known: Vec<&str> = cfg.profiles.keys().map(String::as_str).collect();
            return fail(format!(
                "no profile named \"{p}\" (saved profiles: {})",
                if known.is_empty() { "none".to_string() } else { known.join(", ") }
            ));
        }
        cfg.switch_profile(Some(p));
    }
    let Some(es_root) = args.es_root.clone().or_else(|| cfg.es_root.clone()).or_else(scan::detect_es_root_near_exe) else {
        return fail("no ES-DE folder: pass it as an argument or run an export in the window once".into());
    };
    let Some(dest) = args.dest.clone().or_else(|| cfg.dest.clone()) else {
        return fail("no destination: pass --dest or run an export in the window once".into());
    };
    let dest = crate::paths::normalize(&dest);
    if !dest.is_dir() {
        return fail(format!("the destination {} does not exist; is the drive plugged in?", dest.display()));
    }
    let no = AtomicBool::new(false);
    out(format!("Scanning {}", es_root.display()));
    let cat = scan::scan(&es_root, &no, &mut |_| {}).map_err(|e| NotStarted(format!("scan failed: {e:#}")))?;
    let mut sel = select::export_selection(&cat, &cfg);
    sel.emulators = select::effective_emus(&cat, &sel, &cfg.emu_override);
    let groups = plan::groups_for(&cat, &sel);
    if sel.systems.is_empty() && groups.is_empty() {
        return fail("nothing is selected; pick the consoles in the window first".into());
    }
    out(format!(
        "Comparing {} systems, {} emulators with {}{}",
        sel.systems.len(),
        sel.emulators.len(),
        dest.display(),
        args.profile.as_ref().map(|p| format!(" (profile {p})")).unwrap_or_default()
    ));
    let opts = plan::PlanOptions { protect_newer: cfg.protect_newer(), prune: false };
    let plan = plan::build(&groups, &plan::Mapper::Export { dest_root: dest.clone() }, &opts, &no, &mut |_| {})
        .map_err(|e| NotStarted(format!("analyze failed: {e:#}")))?;
    for w in &plan.warnings {
        out(format!("Warning: {w}"));
    }
    let mut problems = select::space_problems(&plan, |v| drives::free_space(Path::new(v)));
    problems.extend(select::fat32_problems(&plan, &drives::list()));
    if !problems.is_empty() {
        return fail(problems.join("\n"));
    }
    out(format!(
        "To copy: {} files ({}), up to date: {} files, newer at the destination (kept): {}",
        plan.copy.len(),
        crate::fsutil::human_bytes(plan.copy_bytes),
        plan.skipped,
        plan.protected.len()
    ));

    let (tx, rx) = crossbeam_channel::unbounded();
    let job = Job {
        cat: &cat,
        sel: &sel,
        plan: &plan,
        dest_root: &dest,
        profile: cfg.active_profile.as_deref(),
        verify: cfg.verify(),
        copy_exe: cfg.copy_exe(),
    };
    let report = std::thread::scope(|s| {
        s.spawn(|| {
            let mut last = std::time::Instant::now();
            for ev in rx {
                match ev {
                    Event::Log(l) => out(l),
                    Event::Progress(p) if last.elapsed().as_secs() >= 5 && p.total_bytes > 0 => {
                        last = std::time::Instant::now();
                        out(format!(
                            "{:.0}%  {} of {} files",
                            p.done_bytes as f64 * 100.0 / p.total_bytes as f64,
                            p.done_files,
                            p.total_files
                        ));
                    }
                    _ => {}
                }
            }
        });
        let r = run(&job, &no, &tx);
        drop(tx);
        r
    });
    out(crate::select::report_summary(&report));
    Ok(report)
}

/// A log file for a headless export: `logs\export-<time>.log` in the config folder.
/// The last ten are kept.
pub fn log_file() -> Option<(PathBuf, std::fs::File)> {
    const KEEP: usize = 10;
    let dir = crate::config::app_dir()?.join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let mut old: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("export-")))
        .collect();
    old.sort();
    for p in old.iter().rev().skip(KEEP - 1) {
        let _ = std::fs::remove_file(p);
    }
    let path = dir.join(format!("export-{}.log", crate::update::stamp(crate::update::version::now_unix())));
    let f = std::fs::File::create(&path).ok()?;
    Some((path, f))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Profile};
    use crate::scan::tests::fake_install;
    use std::sync::Mutex;

    #[test]
    fn headless_export_copies_the_saved_selection_and_never_deletes() {
        let tmp = tempfile::tempdir().unwrap();
        let (es, roms) = fake_install(&tmp.path().join("main"));
        let drive = tmp.path().join("drive");
        std::fs::create_dir_all(&drive).unwrap();
        // left over at the destination from an older export of a ROM deleted since
        let gone = crate::paths::mirror(&drive, &roms.join(r"snes\gone.sfc"));
        std::fs::create_dir_all(gone.parent().unwrap()).unwrap();
        std::fs::write(&gone, "old").unwrap();

        let mut cfg = Config { es_root: Some(es.clone()), copy_exe: Some(false), ..Default::default() };
        cfg.profiles.insert(
            "Ally".into(),
            Profile { dest: Some(drive.clone()), systems: ["snes".to_string()].into(), ..Default::default() },
        );
        let lines = Mutex::new(Vec::new());
        let out = |s: String| lines.lock().unwrap().push(s);

        // unknown profile, missing destination: nothing starts
        let e = headless(&Headless { profile: Some("Deck".into()), ..Default::default() }, cfg.clone(), &out).unwrap_err();
        assert!(e.0.contains("Deck") && e.0.contains("Ally"), "{}", e.0);
        let e = headless(&Headless { dest: Some(tmp.path().join("nope")), ..Default::default() }, cfg.clone(), &out).unwrap_err();
        assert!(e.0.contains("does not exist"), "{}", e.0);

        let r = headless(&Headless { profile: Some("Ally".into()), ..Default::default() }, cfg.clone(), &out).unwrap();
        assert!(r.failed.is_empty() && r.copied > 0, "{r:?}");
        assert!(crate::paths::mirror(&drive, &roms.join(r"snes\a.sfc")).is_file());
        assert!(!crate::paths::mirror(&drive, &roms.join(r"psx\b.chd")).exists(), "only the profile's systems");
        assert!(gone.is_file(), "a headless export never deletes");
        let m = crate::manifest::load(&drive).unwrap().unwrap();
        assert!(m.complete);
        assert_eq!(m.profiles["Ally"].systems, ["snes".to_string()].into());
        assert!(m.profiles["Ally"].emulators.contains("RETROARCH"));

        // run again: nothing left to copy
        let r = headless(&Headless { profile: Some("Ally".into()), ..Default::default() }, cfg, &out).unwrap();
        assert_eq!(r.copied, 0);
        assert!(lines.lock().unwrap().iter().any(|l| l.starts_with("To copy: 0 files")));
    }
}

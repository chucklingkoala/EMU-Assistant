//! Update mode: finds the programs installed for ES-DE, checks their official sources
//! for newer builds and installs them in place, backing up every file it replaces into
//! a zip that [`restore_backup`] can put back.

pub mod archive;
pub mod http;
pub mod sources;
pub mod version;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use crossbeam_channel::Sender;
use filetime::FileTime;

use crate::esde::find_rules;
use crate::fsutil::walk;
use crate::paths::{key, long, normalize};
use crate::scan;
use crate::sync::{self, Event, Progress, Report};
use archive::{BackupMeta, SavedFile};
use http::Http;
use sources::{Release, SourceSpec, BUILDBOT};
use version::{cmp_numeric, now_unix, parse_ts, pe_version, read_marker, short_version, Marker};

/// Program id used for ES-DE itself.
pub const ESDE_ID: &str = "ES-DE";

#[derive(Clone, Debug)]
pub struct Program {
    pub id: String,
    pub dir: PathBuf,
    pub exe: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    NotChecked,
    UpToDate,
    Available,
    /// The installed version can't be told; the text says what is known.
    Unknown(String),
    CantUpdate(String),
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct CoreUpdate {
    /// e.g. `snes9x_libretro.dll`
    pub file: String,
    pub url: String,
    pub remote_unix: i64,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct UpdateItem {
    pub program: Program,
    pub source: Option<SourceSpec>,
    pub installed: String,
    pub latest: Option<Release>,
    pub status: Status,
    /// RetroArch cores with newer nightly builds.
    pub cores: Vec<CoreUpdate>,
    pub cores_note: Option<String>,
}

impl UpdateItem {
    pub fn id(&self) -> &str {
        &self.program.id
    }

    /// Whether the program itself (not just its cores) would be downloaded.
    pub fn needs_main(&self) -> bool {
        self.latest.is_some() && matches!(self.status, Status::Available | Status::Unknown(_))
    }

    pub fn has_work(&self) -> bool {
        self.needs_main() || !self.cores.is_empty()
    }

    pub fn download_size(&self) -> u64 {
        let main = if self.needs_main() { self.latest.as_ref().map_or(0, |r| r.size) } else { 0 };
        main + self.cores.iter().map(|c| c.size).sum::<u64>()
    }
}

pub struct Opts {
    pub backup: bool,
    pub backup_dir: PathBuf,
    /// Backups kept per program; 0 keeps all.
    pub keep: u32,
    pub cores: bool,
    pub github_token: Option<String>,
    pub sources: BTreeMap<String, SourceSpec>,
}

pub fn source_for(id: &str, overrides: &BTreeMap<String, SourceSpec>) -> Option<SourceSpec> {
    overrides.get(id).cloned().or_else(|| sources::builtin(id))
}

/// Find-rule ids for launchers rather than emulators.
const NOT_EMULATORS: [&str; 2] = ["STEAM", "OS-SHELL"];

/// ES-DE plus every emulator from `es_find_rules.xml` whose exe exists on this machine.
/// Windows components (cmd.exe and the like) are left out.
pub fn installed_programs(es_root: &Path) -> Result<Vec<Program>> {
    let es_root = normalize(es_root);
    ensure!(scan::is_es_root(&es_root), "{} does not contain ES-DE.exe", es_root.display());
    let mut out = vec![Program { id: ESDE_ID.into(), dir: es_root.clone(), exe: es_root.join("ES-DE.exe") }];
    let rules = find_rules::load(&es_root)?;
    let mut ids: Vec<&String> = rules.keys().collect();
    ids.sort();
    let mut by_dir: HashMap<String, usize> = HashMap::new();
    for id in ids {
        let Some(found) = find_rules::resolve(id, &rules, &es_root) else { continue };
        // launchers that aren't emulators, Windows components, and installed programs
        // (Program Files) that EMU Assistant can't update are left out
        if NOT_EMULATORS.contains(&id.as_str())
            || !found.exe.is_file()
            || scan::not_copyable_reason(&found.dir, &es_root).is_some()
            || scan::is_program_files(&found.dir)
        {
            continue;
        }
        let p = Program { id: id.clone(), dir: found.dir, exe: found.exe };
        // several ids can find the same install (e.g. a "-legacy" variant); keep one,
        // preferring the id that has a built-in source
        match by_dir.get(&key(&p.dir)) {
            Some(&i) => {
                if sources::builtin(&out[i].id).is_none() && sources::builtin(&p.id).is_some() {
                    out[i] = p;
                }
            }
            None => {
                by_dir.insert(key(&p.dir), out.len());
                out.push(p);
            }
        }
    }
    Ok(out)
}

fn exe_unix(exe: &Path) -> Option<i64> {
    fs::metadata(exe).ok().map(|m| FileTime::from_last_modification_time(&m).unix_seconds())
}

fn installed_text(p: &Program) -> String {
    if let Some(m) = read_marker(&p.dir) {
        return m.version;
    }
    if let Some(v) = pe_version(&p.exe) {
        return short_version(&v);
    }
    exe_unix(&p.exe).map(|t| format!("build of {}", version::date_text(t))).unwrap_or_else(|| "unknown".into())
}

/// The list before any network check: installed versions and the programs that can't be updated.
pub fn local_items(programs: Vec<Program>, overrides: &BTreeMap<String, SourceSpec>) -> Vec<UpdateItem> {
    programs
        .into_iter()
        .map(|p| {
            let source = source_for(&p.id, overrides);
            let status = if scan::is_program_files(&p.dir) {
                Status::CantUpdate("installed with an installer; use its own updater".into())
            } else if source.is_none() {
                Status::CantUpdate("no update source; set one in Settings > Update sources".into())
            } else {
                Status::NotChecked
            };
            UpdateItem {
                installed: installed_text(&p),
                program: p,
                source,
                latest: None,
                status,
                cores: Vec::new(),
                cores_note: None,
            }
        })
        .collect()
}

/// Compares the installed program with a release.
pub fn compute_status(p: &Program, rel: &Release) -> Status {
    if let Some(m) = read_marker(&p.dir) {
        let same = m.version == rel.version && m.asset == rel.asset_name;
        // rolling builds keep their tag; a newer upload means a new build
        let rebuilt = matches!((parse_ts(&rel.published), parse_ts(&m.published)), (Some(r), Some(l)) if r > l + 60);
        return if same && !rebuilt { Status::UpToDate } else { Status::Available };
    }
    if let Some(o) = pe_version(&p.exe).and_then(|pe| cmp_numeric(&pe, &rel.version)) {
        return if o.is_lt() { Status::Available } else { Status::UpToDate };
    }
    let local = exe_unix(&p.exe);
    let hint = match (local, parse_ts(&rel.published)) {
        (Some(l), Some(r)) if r > l + 86400 => ", probably older than the latest",
        (Some(_), Some(_)) => ", probably current",
        _ => "",
    };
    Status::Unknown(format!(
        "version unknown{}{hint}",
        local.map(|t| format!(" (build of {})", version::date_text(t))).unwrap_or_default()
    ))
}

/// Looks up the latest release of every item that has a source.
pub fn check(items: &mut [UpdateItem], opts: &Opts, cancel: &AtomicBool, status: &mut dyn FnMut(String)) -> Result<()> {
    let http = Http::new(opts.github_token.clone());
    for item in items.iter_mut() {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        item.source = source_for(item.id(), &opts.sources);
        item.installed = installed_text(&item.program);
        item.cores.clear();
        item.cores_note = None;
        if matches!(item.status, Status::CantUpdate(_)) && !scan::is_program_files(&item.program.dir) {
            item.status = Status::NotChecked; // a source may have been added since
        }
        if matches!(item.status, Status::CantUpdate(_)) {
            continue;
        }
        let Some(spec) = item.source.clone() else {
            item.status = Status::CantUpdate("no update source; set one in Settings > Update sources".into());
            continue;
        };
        status(format!("Checking {}…", item.id()));
        match sources::latest(&http, &spec) {
            Ok(rel) => {
                item.status = compute_status(&item.program, &rel);
                item.latest = Some(rel);
            }
            Err(e) => {
                item.latest = None;
                item.status = Status::Failed(format!("{e:#}"));
            }
        }
        if opts.cores && is_retroarch(&item.program) {
            status("Checking RetroArch cores…".into());
            match check_cores(&http, &item.program.dir) {
                Ok(c) => item.cores = c,
                Err(e) => item.cores_note = Some(format!("could not check cores: {e:#}")),
            }
        }
    }
    Ok(())
}

pub fn is_retroarch(p: &Program) -> bool {
    p.id == "RETROARCH" || p.exe.file_name().is_some_and(|n| n.eq_ignore_ascii_case("retroarch.exe"))
}

const CORES_INDEX: &str = "/nightly/windows/x86_64/latest/";

/// Installed cores with a newer build on the buildbot (what RetroArch's online updater fetches).
fn check_cores(http: &Http, ra_dir: &Path) -> Result<Vec<CoreUpdate>> {
    let local = installed_cores(ra_dir);
    if local.is_empty() {
        return Ok(Vec::new());
    }
    let remote = parse_core_index(&http.get_text(&format!("{BUILDBOT}{CORES_INDEX}"))?);
    Ok(cores_needing_update(&local, &remote))
}

fn installed_cores(ra_dir: &Path) -> Vec<(String, i64)> {
    let Ok(rd) = fs::read_dir(ra_dir.join("cores")) else { return Vec::new() };
    let mut v: Vec<(String, i64)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let m = e.metadata().ok()?;
            (m.is_file() && name.to_lowercase().ends_with(".dll"))
                .then(|| (name, FileTime::from_last_modification_time(&m).unix_seconds()))
        })
        .collect();
    v.sort();
    v
}

/// `name.dll.zip` -> (date, size) from the buildbot's directory listing.
fn parse_core_index(html: &str) -> HashMap<String, (i64, u64)> {
    let mut out = HashMap::new();
    for row in html.split("<tr") {
        let Some(name) = row
            .split(CORES_INDEX)
            .nth(1)
            .and_then(|r| r.split('"').next())
            .filter(|n| n.to_lowercase().ends_with(".dll.zip"))
        else {
            continue;
        };
        let cell = |class: &str| row.split(&format!("class=\"{class}\">")).nth(1).and_then(|r| r.split('<').next()).unwrap_or("");
        let Some(ts) = parse_ts(cell("fb-d")) else { continue };
        let size = parse_size(cell("fb-s"));
        out.insert(name.to_lowercase(), (ts, size));
    }
    out
}

fn parse_size(s: &str) -> u64 {
    let mut it = s.split_whitespace();
    let n: f64 = it.next().and_then(|n| n.parse().ok()).unwrap_or(0.0);
    let mul = match it.next().unwrap_or("").to_uppercase().as_str() {
        "KB" => 1024.0,
        "MB" => 1024.0 * 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    (n * mul) as u64
}

fn cores_needing_update(local: &[(String, i64)], remote: &HashMap<String, (i64, u64)>) -> Vec<CoreUpdate> {
    local
        .iter()
        .filter_map(|(file, mtime)| {
            let zip = format!("{file}.zip");
            let &(ts, size) = remote.get(&zip.to_lowercase())?;
            // an hour of slack for clock and timezone differences
            (ts > mtime + 3600).then(|| CoreUpdate {
                file: file.clone(),
                url: format!("{BUILDBOT}{CORES_INDEX}{zip}"),
                remote_unix: ts,
                size,
            })
        })
        .collect()
}

/// Re-reads installed versions after an update or restore, without going online.
pub fn refresh(items: &mut [UpdateItem]) {
    for item in items.iter_mut() {
        item.installed = installed_text(&item.program);
        if let Some(rel) = &item.latest {
            item.status = compute_status(&item.program, rel);
        }
        let local: HashMap<String, i64> = installed_cores(&item.program.dir).into_iter().collect();
        item.cores.retain(|c| local.get(&c.file).is_none_or(|&m| c.remote_unix > m + 3600));
    }
}

/// True while the exe is running (Windows refuses to open a running exe for writing).
pub fn in_use(exe: &Path) -> bool {
    match fs::OpenOptions::new().write(true).open(long(exe)) {
        Err(e) => e.raw_os_error() == Some(32),
        Ok(_) => false,
    }
}

/// Files of an install that an update must never overwrite: user settings and data.
/// For ES-DE its data, ROM and emulator folders are skipped even when new.
pub fn protected(id: &str, rel: &Path, exists: bool) -> bool {
    let first = rel.components().next().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).unwrap_or_default();
    let multi = rel.components().count() > 1;
    if id == ESDE_ID && multi && ["es-de", "roms", "emulators", "emuassistant"].contains(&first.as_str()) {
        return true;
    }
    if !exists {
        return false;
    }
    let name = rel.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let ext = rel.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if name == version::MARKER.to_lowercase() || name == "portable.txt" || ["ini", "cfg", "yml", "yaml"].contains(&ext.as_str()) {
        return true;
    }
    is_retroarch_id(id) && multi && ["config", "saves", "states", "system", "playlists", "screenshots", "logs"].contains(&first.as_str())
}

fn is_retroarch_id(id: &str) -> bool {
    id == "RETROARCH"
}

fn safe(s: &str) -> String {
    let s: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect();
    if s.is_empty() { "_".into() } else { s }
}

fn stamp(unix: i64) -> String {
    let secs = unix.rem_euclid(86400);
    format!("{}-{:02}{:02}{:02}", version::date_text(unix).replace('-', ""), secs / 3600, secs % 3600 / 60, secs % 60)
}

fn work_dir(id: &str) -> PathBuf {
    std::env::temp_dir().join(crate::manifest::DIR).join("updates").join(safe(id))
}

struct Ctx<'a> {
    http: &'a Http,
    opts: &'a Opts,
    cancel: &'a AtomicBool,
    tx: &'a Sender<Event>,
    progress: Progress,
    last: Instant,
}

impl Ctx<'_> {
    fn log(&self, s: impl Into<String>) {
        let _ = self.tx.send(Event::Log(s.into()));
    }

    fn step(&mut self, s: impl Into<String>) {
        self.progress.current = s.into();
        let _ = self.tx.send(Event::Progress(self.progress.clone()));
    }

    fn bytes(&mut self, n: u64) {
        self.progress.done_bytes += n;
        if self.last.elapsed() >= Duration::from_millis(100) {
            self.last = Instant::now();
            let _ = self.tx.send(Event::Progress(self.progress.clone()));
        }
    }

    fn cancelled(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        Ok(())
    }

    fn download(&mut self, url: &str, dest: &Path) -> Result<()> {
        #[cfg(test)]
        if PRESTAGED.with(|p| p.get()) && dest.is_file() {
            return Ok(());
        }
        let (http, cancel) = (self.http, self.cancel);
        if !http.download(url, dest, cancel, &mut |n| self.bytes(n))? {
            bail!("cancelled");
        }
        Ok(())
    }
}

/// Installs the given items one after another. A failure is logged and the next one
/// still runs. Report: `copied` = files installed, `copied_bytes` = bytes downloaded.
pub fn install(items: &[UpdateItem], opts: &Opts, cancel: &AtomicBool, tx: &Sender<Event>) -> Report {
    let start = Instant::now();
    let http = Http::new(opts.github_token.clone());
    let mut ctx = Ctx {
        http: &http,
        opts,
        cancel,
        tx,
        progress: Progress {
            total_bytes: items.iter().map(UpdateItem::download_size).sum(),
            total_files: items.len() as u64,
            ..Default::default()
        },
        last: Instant::now(),
    };
    let mut report = Report::default();
    for (i, item) in items.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        ctx.progress.done_files = i as u64;
        match install_one(item, &mut ctx) {
            Ok(n) => {
                report.copied += n;
                ctx.log(format!("{}: updated ({n} files)", item.id()));
            }
            Err(e) if cancel.load(Ordering::Relaxed) => {
                ctx.log(format!("{}: cancelled ({e:#})", item.id()));
                report.cancelled = true;
            }
            Err(e) => {
                ctx.log(format!("{}: {e:#}", item.id()));
                report.failed.push(format!("{}: {e:#}", item.id()));
            }
        }
        let _ = fs::remove_dir_all(work_dir(item.id()));
    }
    report.copied_bytes = ctx.progress.done_bytes;
    ctx.progress.done_files = items.len() as u64;
    ctx.step("");
    report.elapsed = start.elapsed();
    report
}

fn install_one(item: &UpdateItem, ctx: &mut Ctx) -> Result<u64> {
    let p = &item.program;
    let id = item.id();
    if in_use(&p.exe) {
        bail!("{} is running; close it and try again", p.exe.file_name().unwrap_or_default().to_string_lossy());
    }
    let work = work_dir(id);
    let staging = work.join("x");
    for d in [&staging, &work.join("cores_x")] {
        let _ = fs::remove_dir_all(d);
    }
    fs::create_dir_all(&staging)?;

    // 1. download and unpack
    let mut root = staging.clone();
    let main = item.latest.as_ref().filter(|_| item.needs_main());
    if let Some(rel) = main {
        let file = work.join(safe(&rel.asset_name));
        ctx.step(format!("{id}: downloading {}", rel.asset_name));
        ctx.download(&rel.asset_url, &file)?;
        ctx.step(format!("{id}: unpacking"));
        archive::extract(&file, &staging, ctx.cancel)?;
        let exe_name = p.exe.file_name().unwrap_or_default().to_string_lossy().to_string();
        root = archive::find_root(&staging, &exe_name).with_context(|| {
            format!("{} does not contain {exe_name}; check the asset pattern in Settings > Update sources", rel.asset_name)
        })?;
    }
    if ctx.opts.cores && !item.cores.is_empty() {
        let cores_dir = root.join("cores");
        for (n, c) in item.cores.iter().enumerate() {
            ctx.cancelled()?;
            ctx.step(format!("{id}: core {} of {}: {}", n + 1, item.cores.len(), c.file));
            let zip = work.join("cores").join(format!("{}.zip", c.file));
            ctx.download(&c.url, &zip)?;
            let out = work.join("cores_x").join(&c.file);
            archive::extract(&zip, &out, ctx.cancel)?;
            let dll = archive::find_root(&out, &c.file).map(|d| d.join(&c.file)).context("core archive without the core")?;
            fs::create_dir_all(&cores_dir)?;
            let dst = cores_dir.join(&c.file);
            fs::rename(&dll, &dst)?;
            filetime::set_file_mtime(&dst, FileTime::from_unix_time(c.remote_unix, 0))?;
        }
    }

    // 2. what changes
    ctx.step(format!("{id}: comparing"));
    let mut replaced = Vec::new();
    let mut added = Vec::new();
    let mut errors = Vec::new();
    let mut files = Vec::new();
    walk(&root, &[], true, &mut errors, |path, m| {
        if m.is_file() {
            files.push(path.to_path_buf());
        }
    });
    ensure!(errors.is_empty(), "reading the unpacked update: {}", errors.join("; "));
    let mut todo = Vec::new();
    for src in files {
        let Some(rel) = crate::paths::strip_prefix_ci(&src, &root) else { continue };
        let dst = p.dir.join(&rel);
        let exists = fs::metadata(long(&dst)).is_ok_and(|m| m.is_file());
        if protected(id, &rel, exists) {
            continue;
        }
        let rel_s = rel.to_string_lossy().to_string();
        if exists {
            if sync::same_content(&src, &dst).unwrap_or(false) {
                continue;
            }
            let mtime = fs::metadata(long(&dst)).map(|m| FileTime::from_last_modification_time(&m).unix_seconds()).unwrap_or(0);
            replaced.push(SavedFile { path: rel_s, mtime });
        } else {
            added.push(rel_s);
        }
        todo.push((src, dst));
    }
    if todo.is_empty() {
        write_marker_for(item, main)?;
        ctx.log(format!("{id}: no files changed"));
        return Ok(0);
    }

    // 3. backup
    let backup = if ctx.opts.backup {
        let meta = BackupMeta {
            id: id.into(),
            label: id.into(),
            dir: p.dir.clone(),
            exe: p.exe.clone(),
            created_unix: now_unix(),
            old_version: item.installed.clone(),
            new_version: main.map(|r| r.version.clone()).unwrap_or_else(|| "cores".into()),
            replaced,
            added,
            old_marker: read_marker(&p.dir),
        };
        let out = ctx.opts.backup_dir.join(format!("{}-{}-{}.zip", safe(id), safe(&meta.old_version), stamp(meta.created_unix)));
        ctx.step(format!("{id}: backing up {} files", meta.replaced.len()));
        let (cancel, tx) = (ctx.cancel, ctx.tx);
        let mut last = Instant::now();
        let mut done = 0u64;
        archive::write_backup(&out, &meta, cancel, &mut |n| {
            done += n;
            if last.elapsed() >= Duration::from_millis(250) {
                last = Instant::now();
                let _ = tx.send(Event::Progress(Progress { current: format!("{id}: backing up ({})", crate::fsutil::human_bytes(done)), ..ctx.progress.clone() }));
            }
        })
        .context("writing the backup (nothing was changed)")?;
        ctx.log(format!("{id}: backup saved to {}", out.display()));
        prune_backups(&ctx.opts.backup_dir, id, ctx.opts.keep);
        Some(out)
    } else {
        None
    };

    // 4. install; on any failure put the backup back
    ctx.step(format!("{id}: installing {} files", todo.len()));
    let never = AtomicBool::new(false);
    for (src, dst) in &todo {
        let mtime = fs::metadata(src).map(|m| FileTime::from_last_modification_time(&m)).unwrap_or_else(|_| FileTime::now());
        // not cancellable mid-way: a half-installed program is worse than a finished one
        if let Err(e) = sync::copy_file(src, dst, mtime, &never, &mut |_| {}) {
            let msg = format!("installing {}: {e}", dst.display());
            return Err(match &backup {
                Some(zip) => match archive::restore(zip, None, &never) {
                    Ok(r) if r.failed.is_empty() => anyhow::anyhow!("{msg}. The previous version was restored from the backup."),
                    Ok(r) => anyhow::anyhow!("{msg}. Restoring the backup also failed for: {}", r.failed.join("; ")),
                    Err(e2) => anyhow::anyhow!("{msg}. Restoring the backup failed: {e2:#}"),
                },
                None => anyhow::anyhow!("{msg}. No backup was made; the program may be partly updated."),
            });
        }
    }
    write_marker_for(item, main)?;
    Ok(todo.len() as u64)
}

fn write_marker_for(item: &UpdateItem, main: Option<&Release>) -> Result<()> {
    let (Some(rel), Some(spec)) = (main, &item.source) else { return Ok(()) };
    version::write_marker(
        &item.program.dir,
        &Marker {
            source: spec.describe(),
            version: rel.version.clone(),
            asset: rel.asset_name.clone(),
            published: rel.published.clone(),
            installed_unix: now_unix(),
        },
    )
}

/// Deletes the oldest backups of a program beyond `keep` (0 keeps all).
fn prune_backups(dir: &Path, id: &str, keep: u32) {
    if keep == 0 {
        return;
    }
    let mut mine: Vec<(i64, PathBuf)> = list_backups(dir)
        .into_iter()
        .filter(|(_, m)| m.id == id)
        .map(|(p, m)| (m.created_unix, p))
        .collect();
    mine.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    for (_, p) in mine.into_iter().skip(keep as usize) {
        let _ = fs::remove_file(p);
    }
}

/// Backups in `dir`, newest first.
pub fn list_backups(dir: &Path) -> Vec<(PathBuf, BackupMeta)> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<(PathBuf, BackupMeta)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")))
        .filter_map(|p| archive::read_meta(&p).ok().map(|m| (p, m)))
        .collect();
    v.sort_by_key(|(_, m)| std::cmp::Reverse(m.created_unix));
    v
}

/// Restores backups (all files, or only `only[zip]` for a zip listed there).
pub fn restore_backups(jobs: &[(PathBuf, Option<BTreeSet<String>>)], cancel: &AtomicBool, tx: &Sender<Event>) -> Report {
    let start = Instant::now();
    let mut report = Report::default();
    let log = |s: String| {
        let _ = tx.send(Event::Log(s));
    };
    for (i, (zip, only)) in jobs.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        let name = zip.file_name().unwrap_or_default().to_string_lossy().to_string();
        let _ = tx.send(Event::Progress(Progress {
            done_files: i as u64,
            total_files: jobs.len() as u64,
            current: format!("restoring {name}"),
            ..Default::default()
        }));
        let r = archive::read_meta(zip).and_then(|meta| {
            if in_use(&meta.exe) {
                bail!("{} is running; close it and try again", meta.exe.display());
            }
            archive::restore(zip, only.as_ref(), cancel)
        });
        match r {
            Ok(r) => {
                report.copied += r.restored;
                report.pruned += r.removed;
                log(format!("{name}: restored {} files, removed {} added files", r.restored, r.removed));
                for f in r.failed {
                    log(format!("{name}: {f}"));
                    report.failed.push(f);
                }
            }
            Err(e) => {
                log(format!("{name}: {e:#}"));
                report.failed.push(format!("{name}: {e:#}"));
            }
        }
    }
    let _ = tx.send(Event::Progress(Progress { done_files: jobs.len() as u64, total_files: jobs.len() as u64, ..Default::default() }));
    report.elapsed = start.elapsed();
    report
}

/// Prints the update table for `--check-updates`.
pub fn print_check(es_root: &Path, opts: &Opts) -> Result<()> {
    let programs = installed_programs(es_root)?;
    let mut items = local_items(programs, &opts.sources);
    check(&mut items, opts, &AtomicBool::new(false), &mut |s| eprintln!("{s}"))?;
    println!("{:<18} {:<20} {:<20} {}", "Program", "Installed", "Latest", "Status");
    for it in &items {
        let status = match &it.status {
            Status::NotChecked => "not checked".to_string(),
            Status::UpToDate => "up to date".into(),
            Status::Available => "UPDATE AVAILABLE".into(),
            Status::Unknown(s) | Status::CantUpdate(s) => s.clone(),
            Status::Failed(e) => format!("error: {e}"),
        };
        println!(
            "{:<18} {:<20} {:<20} {status}",
            it.id(),
            it.installed,
            it.latest.as_ref().map(|r| r.version.as_str()).unwrap_or("-")
        );
        println!("{:<18} {}", "", it.program.dir.display());
        if !it.cores.is_empty() {
            println!("{:<18} {} cores have newer builds", "", it.cores.len());
        }
        if let Some(n) = &it.cores_note {
            println!("{:<18} {n}", "");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::tests::fake_install;

    fn opts(backup_dir: &Path) -> Opts {
        Opts { backup: true, backup_dir: backup_dir.into(), keep: 2, cores: false, github_token: None, sources: BTreeMap::new() }
    }

    #[test]
    fn lists_only_installed_programs() {
        let tmp = tempfile::tempdir().unwrap();
        let (es, _) = fake_install(tmp.path());
        let progs = installed_programs(&es).unwrap();
        let ids: Vec<_> = progs.iter().map(|p| p.id.as_str()).collect();
        // DUCKSTATION has a rule but isn't installed
        assert_eq!(ids, vec![ESDE_ID, "BSNES", "RETROARCH"]);
        let items = local_items(progs, &BTreeMap::new());
        assert!(matches!(items[1].status, Status::CantUpdate(_)), "bsnes has no built-in source");
        assert_eq!(items[2].status, Status::NotChecked);
    }

    #[test]
    fn protects_user_files() {
        assert!(protected("RETROARCH", Path::new("retroarch.cfg"), true));
        assert!(!protected("RETROARCH", Path::new("retroarch.cfg"), false));
        assert!(protected("RETROARCH", Path::new(r"saves\a.srm"), true));
        assert!(!protected("RETROARCH", Path::new(r"assets\x.png"), true));
        assert!(protected(ESDE_ID, Path::new(r"ROMs\snes\systeminfo.txt"), false));
        assert!(!protected(ESDE_ID, Path::new(r"resources\systems\windows\es_systems.xml"), true));
        assert!(protected("PCSX2", Path::new(r"inis\PCSX2.ini"), true));
    }

    #[test]
    fn core_index_and_staleness() {
        let html = r#"<tr><td class="fb-n"><a href="/nightly/windows/x86_64/latest/snes9x_libretro.dll.zip">snes9x_libretro.dll.zip</a></td><td class="fb-d">2026-10-12 06:30</td><td class="fb-s">1316 KB</td></tr>
<tr><td class="fb-n"><a href="/nightly/windows/x86_64/latest/fbneo_libretro.dll.zip">x</a></td><td class="fb-d">2026-09-01 00:00</td><td class="fb-s">20 MB</td></tr>"#;
        let idx = parse_core_index(html);
        assert_eq!(idx["snes9x_libretro.dll.zip"].1, 1316 * 1024);
        let t = version::unix_from_parts(2026, 9, 25, 0, 0, 0);
        let local = vec![("snes9x_libretro.dll".to_string(), t), ("fbneo_libretro.dll".to_string(), t), ("own_libretro.dll".to_string(), 0)];
        let ups = cores_needing_update(&local, &idx);
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].file, "snes9x_libretro.dll");
    }

    #[test]
    fn status_from_marker_and_pe() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        fs::write(dir.join("emu.exe"), b"x").unwrap();
        let p = Program { id: "X".into(), dir: dir.clone(), exe: dir.join("emu.exe") };
        let rel = Release { version: "1.2.0".into(), published: "2026-10-01T00:00:00Z".into(), asset_name: "emu-1.2.0.zip".into(), ..Default::default() };
        assert!(matches!(compute_status(&p, &rel), Status::Unknown(_)));
        let mut m = Marker { version: "1.2.0".into(), asset: "emu-1.2.0.zip".into(), published: rel.published.clone(), ..Default::default() };
        version::write_marker(&dir, &m).unwrap();
        assert_eq!(compute_status(&p, &rel), Status::UpToDate);
        // same tag rebuilt later (rolling release)
        let rebuilt = Release { published: "2026-10-05T00:00:00Z".into(), ..rel.clone() };
        assert_eq!(compute_status(&p, &rebuilt), Status::Available);
        m.version = "1.1.0".into();
        version::write_marker(&dir, &m).unwrap();
        assert_eq!(compute_status(&p, &rel), Status::Available);
    }

    /// Installs a locally built "release" over the fake RetroArch, then rolls it back.
    #[test]
    fn install_backup_and_restore_offline() {
        let tmp = tempfile::tempdir().unwrap();
        let (es, _) = fake_install(tmp.path());
        let ra = es.join(r"Emulators\RetroArch-Win64");
        let before = fs::read(ra.join("retroarch.exe")).unwrap();
        let archive_path = tmp.path().join("RetroArch.zip");
        archive::tests::make_zip(
            &archive_path,
            &[
                ("RetroArch-Win64/retroarch.exe", b"new ra"),
                ("RetroArch-Win64/retroarch.cfg", b"shipped default cfg"),
                ("RetroArch-Win64/system/bios.bin", b"shipped bios"),
                ("RetroArch-Win64/assets/new.png", b"png"),
            ],
        );
        let url = format!("file:{}", archive_path.display());
        let item = UpdateItem {
            program: Program { id: "RETROARCH".into(), dir: ra.clone(), exe: ra.join("retroarch.exe") },
            source: sources::builtin("RETROARCH"),
            installed: "unknown".into(),
            latest: Some(Release { version: "9.9.9".into(), asset_name: "RetroArch.zip".into(), asset_url: url, ..Default::default() }),
            status: Status::Available,
            cores: vec![],
            cores_note: None,
        };
        let bdir = tmp.path().join("backups");
        let o = opts(&bdir);
        // the http client can't fetch file: links, so stage the download by hand
        let (tx, rx) = crossbeam_channel::unbounded();
        let cancel = AtomicBool::new(false);
        let r = install_from_local(&item, &archive_path, &o, &cancel, &tx);
        drop(tx);
        let logs: Vec<String> = rx.try_iter().filter_map(|e| if let Event::Log(s) = e { Some(s) } else { None }).collect();
        assert!(r.failed.is_empty(), "{:?} {logs:?}", r.failed);
        assert_eq!(fs::read(ra.join("retroarch.exe")).unwrap(), b"new ra");
        assert_eq!(fs::read(ra.join(r"system\bios.bin")).unwrap(), b"bios", "user files untouched");
        assert_ne!(fs::read(ra.join("retroarch.cfg")).unwrap(), b"shipped default cfg");
        assert!(ra.join(r"assets\new.png").is_file());
        assert_eq!(read_marker(&ra).unwrap().version, "9.9.9");

        let backups = list_backups(&bdir);
        assert_eq!(backups.len(), 1);
        let (zip, meta) = &backups[0];
        assert_eq!(meta.replaced.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["retroarch.exe"]);
        assert_eq!(meta.added, vec![r"assets\new.png".to_string()]);

        let (tx, _rx) = crossbeam_channel::unbounded();
        let r = restore_backups(&[(zip.clone(), None)], &cancel, &tx);
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(fs::read(ra.join("retroarch.exe")).unwrap(), before);
        assert!(!ra.join(r"assets\new.png").exists());
        assert!(read_marker(&ra).is_none(), "marker removed with a full restore");
    }

    /// Downloads and unpacks a real release: `EMUASIST_TEST_URL` + `EMUASIST_TEST_EXE`.
    /// `cargo test live_download -- --ignored`
    #[test]
    #[ignore]
    fn live_download_and_extract() {
        let url = std::env::var("EMUASIST_TEST_URL").unwrap();
        let exe = std::env::var("EMUASIST_TEST_EXE").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join(url.rsplit('/').next().unwrap());
        let mut total = 0;
        assert!(Http::new(None).download(&url, &file, &AtomicBool::new(false), &mut |n| total += n).unwrap());
        let out = tmp.path().join("x");
        let t = Instant::now();
        archive::extract(&file, &out, &AtomicBool::new(false)).unwrap();
        let root = archive::find_root(&out, &exe).expect("exe in archive");
        eprintln!("{total} bytes, unpacked in {:?}, root {}", t.elapsed(), root.display());
        for e in fs::read_dir(&root).unwrap().flatten() {
            eprintln!("  {}{}", e.file_name().to_string_lossy(), if e.path().is_dir() { "\\" } else { "" });
        }
    }

    /// Full online run against a throwaway ES-DE folder: installs mGBA 0.10.4, updates
    /// it to the latest release, then restores the backup.
    /// `EMUASIST_FIND_RULES` = path of a real es_find_rules.xml.
    /// `cargo test live_update_mgba -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_update_mgba() {
        let rules = std::env::var("EMUASIST_FIND_RULES").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let es = tmp.path().join("ES-DE");
        fs::create_dir_all(es.join(r"resources\systems\windows")).unwrap();
        fs::write(es.join("ES-DE.exe"), b"x").unwrap();
        fs::copy(&rules, es.join(r"resources\systems\windows\es_find_rules.xml")).unwrap();
        let http = Http::new(None);
        let old = tmp.path().join("old.7z");
        let url = "https://github.com/mgba-emu/mgba/releases/download/0.10.4/mGBA-0.10.4-win64.7z";
        assert!(http.download(url, &old, &AtomicBool::new(false), &mut |_| {}).unwrap());
        let x = tmp.path().join("old");
        archive::extract(&old, &x, &AtomicBool::new(false)).unwrap();
        let mgba = es.join(r"Emulators\mGBA");
        fs::create_dir_all(es.join("Emulators")).unwrap();
        fs::rename(archive::find_root(&x, "mGBA.exe").unwrap(), &mgba).unwrap();
        fs::write(mgba.join("config.ini"), b"[ports.qt]\nmy=settings\n").unwrap();
        let old_exe = fs::read(mgba.join("mGBA.exe")).unwrap();

        let mut items = local_items(installed_programs(&es).unwrap(), &BTreeMap::new());
        let o = Opts { backup: true, backup_dir: tmp.path().join("bk"), keep: 3, cores: false, github_token: None, sources: BTreeMap::new() };
        check(&mut items, &o, &AtomicBool::new(false), &mut |s| eprintln!("{s}")).unwrap();
        let it = items.iter().find(|i| i.id() == "MGBA").expect("mGBA found via the real find rules").clone();
        eprintln!("installed {} latest {:?} status {:?}", it.installed, it.latest.as_ref().map(|r| &r.version), it.status);
        assert!(it.has_work());

        let (tx, rx) = crossbeam_channel::unbounded();
        let r = install(std::slice::from_ref(&it), &o, &AtomicBool::new(false), &tx);
        drop(tx);
        for e in rx.try_iter() {
            if let Event::Log(s) = e {
                eprintln!("log: {s}");
            }
        }
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_ne!(fs::read(mgba.join("mGBA.exe")).unwrap(), old_exe);
        assert_eq!(fs::read(mgba.join("config.ini")).unwrap(), b"[ports.qt]\nmy=settings\n");
        let m = read_marker(&mgba).unwrap();
        eprintln!("marker {m:?}");
        refresh(&mut items);
        assert_eq!(items.iter().find(|i| i.id() == "MGBA").unwrap().status, Status::UpToDate);

        let backups = list_backups(&o.backup_dir);
        assert_eq!(backups.len(), 1);
        let (tx, _rx) = crossbeam_channel::unbounded();
        let r = restore_backups(&[(backups[0].0.clone(), None)], &AtomicBool::new(false), &tx);
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(fs::read(mgba.join("mGBA.exe")).unwrap(), old_exe);
        assert!(read_marker(&mgba).is_none());
        eprintln!("restored {} files, removed {}", r.copied, r.pruned);
    }

    /// Test hook: runs [`install_one`] with the archive already downloaded.
    fn install_from_local(item: &UpdateItem, archive_path: &Path, opts: &Opts, cancel: &AtomicBool, tx: &Sender<Event>) -> Report {
        let work = work_dir(item.id());
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        let rel = item.latest.as_ref().unwrap();
        fs::copy(archive_path, work.join(safe(&rel.asset_name))).unwrap();
        PRESTAGED.with(|p| p.set(true));
        let r = install(std::slice::from_ref(item), opts, cancel, tx);
        PRESTAGED.with(|p| p.set(false));
        r
    }
}

#[cfg(test)]
thread_local! {
    /// Tests put the download in place themselves; skip fetching it.
    static PRESTAGED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

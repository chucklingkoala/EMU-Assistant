//! egui front-end: Export (ES-DE -> drive), Deploy (drive -> this machine) and
//! Update (newer ES-DE / emulator builds), plus the Settings and Restore windows.

mod games_view;
mod self_update;
mod settings;
mod shell;
mod sync_view;
mod theme;
mod update_view;
mod widgets;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, TryRecvError};
use eframe::egui::{self, Color32, RichText};

use crate::config::{self, Config};
use crate::deploy;
use crate::drives::{self, DriveInfo};
use crate::fsutil::human_bytes;
use crate::games::{self, GameList};
use crate::gauge::{self, TankReading, TankTextures, ACCENT};
use crate::manifest::{self, Manifest};
use crate::paths::{eq_ci, mirror, normalize, volume_root};
use crate::plan::{self, Mapper, Plan, PlanOptions, Selection};
use crate::scan::{self, Catalog, Roots};
use crate::sync::{self, Event, Progress, Report};
use crate::update::app::{self as app_update, AppRelease, OldCopy};
use crate::update::archive::BackupMeta;
use crate::update::sources::{self, Kind};
use crate::update::{self, Status, UpdateItem};
use crate::APP_NAME;

use theme::*;
use widgets::*;

use crate::select::{self, emu_bytes, fat32_problems, fmt_secs, report_summary};

const MAX_LOG: usize = 5000;

fn space_problems(plan: &Plan) -> Vec<String> {
    select::space_problems(plan, |vol| drives::free_space(Path::new(vol)))
}

#[derive(PartialEq, Clone, Copy, serde::Serialize)]
enum Mode {
    Export,
    Deploy,
    Update,
}

#[derive(PartialEq, Clone, Copy)]
enum SettingsTab {
    General,
    Export,
    Deploy,
    Updates,
    Sources,
}

/// Messages from scan / analyze worker threads.
enum Msg<T> {
    Status(String),
    Done(anyhow::Result<T>),
}

/// What a finished copy-style job was, to decide what happens next.
#[derive(Clone, Copy)]
enum After {
    /// Export / deploy: analyze again.
    Replan,
    Updated,
    Restored,
    /// EMU Assistant itself was replaced: restart it.
    AppUpdated,
}

enum Job {
    Scan { rx: Receiver<Msg<Catalog>>, cancel: Arc<AtomicBool> },
    Plan { rx: Receiver<Msg<Plan>>, cancel: Arc<AtomicBool>, fp: String },
    Check { rx: Receiver<Msg<Vec<UpdateItem>>>, cancel: Arc<AtomicBool> },
    Copy { rx: Receiver<Event>, cancel: Arc<AtomicBool>, after: After },
    /// Deploy: listing the games of some systems on the drive; then opens the picker for `open`.
    Games { rx: Receiver<Msg<Vec<(String, GameList)>>>, cancel: Arc<AtomicBool>, open: Option<String> },
}

impl Job {
    fn cancel(&self) {
        match self {
            Job::Scan { cancel, .. }
            | Job::Plan { cancel, .. }
            | Job::Check { cancel, .. }
            | Job::Copy { cancel, .. }
            | Job::Games { cancel, .. } => cancel.store(true, Ordering::Relaxed),
        }
    }
}

/// Files ticked in one backup of the Restore window.
struct RestorePick {
    files: BTreeSet<String>,
    expanded: bool,
}

pub struct App {
    cfg: Config,
    mode: Mode,
    // export
    es_root: String,
    dest: String,
    export_cat: Option<Arc<Catalog>>,
    // deploy
    pkg_root: String,
    manifest: Option<Manifest>,
    deploy_cat: Option<Arc<Catalog>>,
    t_es: String,
    t_rom: String,
    t_media: String,
    // selection & options
    sel: Selection,
    emu_override: BTreeMap<String, bool>,
    filter: String,
    prune: bool,
    /// System whose game picker is open, and the picker's filter text.
    games_open: Option<String>,
    games_filter: String,
    // update
    upd_items: Vec<UpdateItem>,
    upd_root: Option<PathBuf>,
    upd_checked: bool,
    /// Outcome of the last update / restore: (text, succeeded).
    upd_result: Option<(String, bool)>,
    // windows
    settings_open: bool,
    settings_tab: SettingsTab,
    /// Program outlined in Update sources, and whether to scroll to it.
    settings_focus: Option<(String, bool)>,
    restore_open: bool,
    backups: Vec<(PathBuf, BackupMeta)>,
    restore_picks: BTreeMap<PathBuf, RestorePick>,
    // EMU Assistant's own updates
    app_rx: Option<Receiver<anyhow::Result<Option<AppRelease>>>>,
    /// A newer release, once a check found one.
    app_release: Option<AppRelease>,
    /// Result of the last check for Settings: (text, is an error).
    app_check_note: Option<(String, bool)>,
    app_window: bool,
    app_error: Option<String>,
    restart: bool,
    /// Older copies installed on this machine, offered for replacement by the drive's exe.
    old_copies: Vec<(OldCopy, bool)>,
    old_copies_dismissed: bool,
    // state
    drives: Vec<DriveInfo>,
    job: Option<Job>,
    status: String,
    cur_fp: String,
    plan: Option<(String, Arc<Plan>)>,
    progress: Option<Progress>,
    copy_started: Option<Instant>,
    report: Option<Report>,
    log: Vec<String>,
    confirm_prune: bool,
    confirm_close: bool,
    /// Quit as soon as the cancelled job has stopped.
    quit_when_idle: bool,
    /// A job just finished: flash the taskbar button if the window isn't in front.
    attention: bool,
    tank: TankTextures,
    icon: egui::TextureHandle,
}

impl App {
    pub fn new(cc: &eframe::CreationContext) -> Self {
        apply_theme(&cc.egui_ctx);
        let cfg = config::load();
        let mut app = App {
            mode: Mode::Export,
            es_root: String::new(),
            dest: cfg.dest.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            export_cat: None,
            pkg_root: String::new(),
            manifest: None,
            deploy_cat: None,
            t_es: String::new(),
            t_rom: String::new(),
            t_media: String::new(),
            sel: Selection::default(),
            emu_override: BTreeMap::new(),
            filter: String::new(),
            prune: false,
            games_open: None,
            games_filter: String::new(),
            upd_items: Vec::new(),
            upd_root: None,
            upd_checked: false,
            upd_result: None,
            settings_open: false,
            settings_tab: SettingsTab::General,
            settings_focus: None,
            restore_open: false,
            backups: Vec::new(),
            restore_picks: BTreeMap::new(),
            app_rx: None,
            app_release: None,
            app_check_note: None,
            app_window: false,
            app_error: None,
            restart: false,
            old_copies: Vec::new(),
            old_copies_dismissed: false,
            drives: drives::list(),
            job: None,
            status: String::new(),
            cur_fp: String::new(),
            plan: None,
            progress: None,
            copy_started: None,
            report: None,
            log: Vec::new(),
            confirm_prune: false,
            confirm_close: false,
            quit_when_idle: false,
            attention: false,
            tank: TankTextures::load(&cc.egui_ctx),
            icon: gauge::load_png(&cc.egui_ctx, "app_icon", crate::ICON_PNG, Some(64)),
            cfg,
        };
        thread::spawn(app_update::cleanup);
        let exe = std::env::current_exe().ok();
        let exe_dir = exe.as_ref().and_then(|p| p.parent().map(Path::to_path_buf));
        let on_drive = exe_dir.as_ref().is_some_and(|d| manifest::exists(d));
        // remember where this machine keeps EMU Assistant, so a newer one on a drive can update it
        if let (false, false, Some(exe)) = (cfg!(debug_assertions), on_drive, &exe) {
            if app.cfg.home_exe.as_ref() != Some(exe) {
                app.cfg.home_exe = Some(exe.clone());
                config::save(&app.cfg);
            }
        }
        if !cfg!(debug_assertions) && app.cfg.check_app_updates() {
            app.start_app_check();
        }
        if let Some(dir) = exe_dir.filter(|_| on_drive) {
            // started from a drive that holds an export: deploy
            app.mode = Mode::Deploy;
            app.pkg_root = dir.display().to_string();
            app.load_package();
        } else if let Some(root) = scan::detect_es_root_near_exe().or_else(|| app.cfg.es_root.clone()) {
            app.es_root = root.display().to_string();
            app.start_scan();
        } else {
            app.status = "Pick your ES-DE folder to begin.".into();
        }
        app
    }

    fn log(&mut self, s: impl Into<String>) {
        self.log.push(s.into());
        if self.log.len() > MAX_LOG {
            self.log.drain(..self.log.len() - MAX_LOG);
        }
    }

    fn cat(&self) -> Option<Arc<Catalog>> {
        match self.mode {
            Mode::Export => self.export_cat.clone(),
            Mode::Deploy => self.deploy_cat.clone(),
            Mode::Update => None,
        }
    }

    fn target_roots(&self) -> Roots {
        Roots {
            es_root: normalize(Path::new(self.t_es.trim())),
            rom_dir: normalize(Path::new(self.t_rom.trim())),
            media_dir: normalize(Path::new(self.t_media.trim())),
        }
    }

    fn fingerprint(&self) -> String {
        serde_json::to_string(&(
            self.mode,
            self.es_root.trim(),
            self.dest.trim(),
            self.pkg_root.trim(),
            self.t_es.trim(),
            self.t_rom.trim(),
            self.t_media.trim(),
            &self.sel,
            self.cfg.protect_newer(),
            self.prune,
        ))
        .unwrap_or_default()
    }

    fn plan_is_fresh(&self) -> bool {
        self.plan.as_ref().is_some_and(|(fp, _)| *fp == self.cur_fp)
    }

    // ---------- selection ----------

    fn apply_defaults(&mut self) {
        self.plan = None;
        self.report = None;
        self.progress = None;
        self.games_open = None;
        let Some(cat) = self.cat() else { return };
        match self.mode {
            Mode::Update => {}
            Mode::Export => {
                self.sel = select::export_selection(&cat, &self.cfg);
                self.emu_override = self.cfg.emu_override.clone();
            }
            Mode::Deploy => {
                self.sel = select::deploy_selection(&cat, &self.cfg);
                self.emu_override.clear();
            }
        }
    }

    /// Emulators referenced by the selected systems: id -> (used by default/override, system names).
    fn relevant_emus(&self, cat: &Catalog) -> BTreeMap<String, (bool, Vec<String>)> {
        select::relevant_emus(cat, &self.sel)
    }

    fn effective_emus(&self) -> BTreeSet<String> {
        let Some(cat) = self.cat() else { return BTreeSet::new() };
        select::effective_emus(&cat, &self.sel, &self.emu_override)
    }

    fn picked_games<'a>(&'a self, s: &'a scan::SysEntry) -> Option<(&'a GameList, &'a BTreeSet<String>)> {
        select::picked_games(s, &self.sel)
    }

    fn rom_bytes(&self, s: &scan::SysEntry) -> (u64, f64) {
        select::rom_bytes(s, &self.sel)
    }

    fn system_bytes(&self, s: &scan::SysEntry) -> u64 {
        select::system_bytes(s, &self.sel)
    }

    /// Opens the game picker, listing the games on the drive first in Deploy mode.
    fn open_games(&mut self, name: &str) {
        if self.mode == Mode::Deploy && self.start_list_games(&[name.to_string()], Some(name.to_string())) {
            return;
        }
        self.games_filter.clear();
        self.games_open = Some(name.to_string());
    }

    /// Deploy: lists the games on the drive of those `names` that aren't listed yet, on a
    /// worker thread. Returns false when there is nothing to list.
    fn start_list_games(&mut self, names: &[String], open: Option<String>) -> bool {
        let Some(cat) = &self.deploy_cat else { return false };
        let pkg = normalize(Path::new(self.pkg_root.trim()));
        let todo: Vec<(String, PathBuf)> = cat
            .systems
            .iter()
            .filter(|s| s.games.is_none() && names.contains(&s.name))
            .map(|s| (s.name.clone(), mirror(&pkg, &s.rom_dir)))
            .collect();
        if todo.is_empty() {
            return false;
        }
        let (tx, rx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        thread::spawn(move || {
            let mut out = Vec::new();
            for (name, dir) in todo {
                if c.load(Ordering::Relaxed) {
                    let _ = tx.send(Msg::Done(Err(anyhow::anyhow!("cancelled"))));
                    return;
                }
                let _ = tx.send(Msg::Status(format!("Listing the {name} games on the drive…")));
                out.push((name, games::list(&dir)));
            }
            let _ = tx.send(Msg::Done(Ok(out)));
        });
        self.status = "Listing games on the drive…".into();
        self.job = Some(Job::Games { rx, cancel, open });
        true
    }

    /// Applies the picker's ticks: every game means no filter (new games are copied too),
    /// none unticks the system.
    fn set_games(&mut self, system: &str, picked: BTreeSet<String>, total: usize) {
        select::set_games(&mut self.sel, system, picked, total);
        if self.mode == Mode::Export {
            self.cfg.games = self.sel.games.clone();
            config::save(&self.cfg);
        }
    }

    fn selected_bytes(&self, cat: &Catalog) -> u64 {
        select::selected_bytes(cat, &self.sel)
    }

    // ---------- jobs ----------

    fn start_scan(&mut self) {
        let root = normalize(Path::new(self.es_root.trim()));
        if root.as_os_str().is_empty() {
            return;
        }
        let (tx, rx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        thread::spawn(move || {
            let res = scan::scan(&root, &c, &mut |s| {
                let _ = tx.send(Msg::Status(s));
            });
            let _ = tx.send(Msg::Done(res));
        });
        self.export_cat = None;
        self.plan = None;
        self.report = None;
        self.status = "Scanning…".into();
        self.job = Some(Job::Scan { rx, cancel });
    }

    fn load_package(&mut self) {
        let root = normalize(Path::new(self.pkg_root.trim()));
        self.manifest = None;
        self.deploy_cat = None;
        match manifest::load(&root) {
            Ok(Some(m)) => {
                let r = m.catalog.roots.clone();
                self.t_es = r.es_root.display().to_string();
                self.t_rom = r.rom_dir.display().to_string();
                self.t_media = r.media_dir.display().to_string();
                self.status = format!("Loaded package with {} systems", m.catalog.systems.len());
                if !m.complete {
                    self.log("Warning: the last export to this drive did not finish cleanly. Missing files are reported during Analyze.");
                }
                self.deploy_cat = Some(Arc::new(m.catalog.clone()));
                self.manifest = Some(m);
                self.apply_defaults();
                self.find_old_copies();
            }
            Ok(None) => self.status = format!("No EMU Assistant package found in {}", root.display()),
            Err(e) => self.status = format!("Could not read package: {e:#}"),
        }
    }

    fn mapper(&self) -> Result<Mapper, String> {
        let abs = |s: &str, what: &str| {
            let p = normalize(Path::new(s.trim()));
            if p.is_absolute() {
                Ok(p)
            } else {
                Err(format!("{what} must be a full path"))
            }
        };
        match self.mode {
            Mode::Update => Err("not used in Update mode".into()),
            Mode::Export => {
                let dest = abs(&self.dest, "Destination")?;
                if !dest.is_dir() {
                    return Err(format!("Destination {} does not exist", dest.display()));
                }
                Ok(Mapper::Export { dest_root: dest })
            }
            Mode::Deploy => {
                let cat = self.deploy_cat.as_ref().ok_or_else(|| "Load a package first".to_string())?;
                let pkg = abs(&self.pkg_root, "Package folder")?;
                abs(&self.t_es, "Target ES-DE folder")?;
                abs(&self.t_rom, "Target ROM folder")?;
                abs(&self.t_media, "Target media folder")?;
                Ok(Mapper::deploy(&pkg, &cat.roots, &self.target_roots()))
            }
        }
    }

    fn start_plan(&mut self) {
        let Some(cat) = self.cat() else { return };
        let mapper = match self.mapper() {
            Ok(m) => m,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        let groups = plan::groups_for(&cat, &self.sel);
        if groups.is_empty() {
            self.status = "Nothing selected".into();
            return;
        }
        let opts = PlanOptions { protect_newer: self.cfg.protect_newer(), prune: self.prune };
        let (tx, rx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        thread::spawn(move || {
            let res = plan::build(&groups, &mapper, &opts, &c, &mut |s| {
                let _ = tx.send(Msg::Status(s));
            });
            let _ = tx.send(Msg::Done(res));
        });
        self.plan = None;
        self.report = None;
        self.progress = None;
        self.status = "Analyzing…".into();
        self.job = Some(Job::Plan { rx, cancel, fp: self.cur_fp.clone() });
    }

    fn remember_export(&mut self, dest: &Path) {
        self.cfg.es_root = Some(normalize(Path::new(self.es_root.trim())));
        self.cfg.dest = Some(dest.to_path_buf());
        self.cfg.systems = self.sel.systems.clone();
        self.cfg.globals = Some(self.sel.globals.clone());
        self.cfg.media = Some(self.sel.media);
        self.cfg.gamelists = Some(self.sel.gamelists);
        self.cfg.emu_override = self.emu_override.clone();
        self.cfg.exclude_cheats = Some(self.sel.exclude_cheats);
        self.cfg.exclude_thumbnails = Some(self.sel.exclude_thumbnails);
        self.cfg.games = self.sel.games.clone();
        config::save(&self.cfg);
    }

    fn start_copy(&mut self) {
        let Some((_, plan)) = self.plan.clone() else { return };
        let (tx, rx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        let verify = self.cfg.verify();
        match self.mode {
            Mode::Update => return,
            Mode::Export => {
                let (Some(cat), Ok(Mapper::Export { dest_root })) = (self.export_cat.clone(), self.mapper()) else {
                    return;
                };
                let sel = self.sel.clone();
                let copy_exe = self.cfg.copy_exe();
                self.remember_export(&dest_root);
                thread::spawn(move || {
                    let _awake = crate::power::KeepAwake::start();
                    let job = crate::export::Job { cat: &cat, sel: &sel, plan: &plan, dest_root: &dest_root, verify, copy_exe };
                    let report = crate::export::run(&job, &c, &tx);
                    let _ = tx.send(Event::Done(report));
                });
            }
            Mode::Deploy => {
                let target = self.target_roots();
                let fix_settings = self.cfg.fix_settings();
                thread::spawn(move || {
                    let _awake = crate::power::KeepAwake::start();
                    let report = sync::execute(&plan, verify, &c, &tx);
                    if !report.cancelled && fix_settings {
                        match deploy::fix_settings(&target) {
                            Ok(msgs) => msgs.into_iter().for_each(|m| {
                                let _ = tx.send(Event::Log(m));
                            }),
                            Err(e) => {
                                let _ = tx.send(Event::Log(format!("Could not update es_settings.xml: {e:#}")));
                            }
                        }
                    }
                    let _ = tx.send(Event::Done(report));
                });
            }
        }
        self.begin_copy_job(rx, cancel, After::Replan, "Copying…");
    }

    fn begin_copy_job(&mut self, rx: Receiver<Event>, cancel: Arc<AtomicBool>, after: After, status: &str) {
        self.progress = None;
        self.report = None;
        self.upd_result = None;
        self.copy_started = Some(Instant::now());
        self.status = status.into();
        self.job = Some(Job::Copy { rx, cancel, after });
    }

    // ---------- update mode ----------

    fn es_root_path(&self) -> PathBuf {
        normalize(Path::new(self.es_root.trim()))
    }

    /// Lists the installed programs (local, no network).
    fn load_programs(&mut self) {
        let root = self.es_root_path();
        self.upd_checked = false;
        match update::installed_programs(&root) {
            Ok(p) => {
                self.upd_items = update::local_items(p, &self.cfg.update_sources);
                self.upd_root = Some(root);
                self.status = format!("{} installed programs found. Press Check for updates.", self.upd_items.len());
            }
            Err(e) => {
                self.upd_items.clear();
                self.upd_root = None;
                self.status = format!("{e:#}");
            }
        }
    }

    fn start_check(&mut self) {
        let Some(root) = self.upd_root.clone() else { return };
        let mut items = self.upd_items.clone();
        let opts = self.cfg.update_opts(&root);
        let (tx, rx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        thread::spawn(move || {
            let res = update::check(&mut items, &opts, &c, &mut |s| {
                let _ = tx.send(Msg::Status(s));
            })
            .map(|()| items);
            let _ = tx.send(Msg::Done(res));
        });
        self.status = "Checking for updates…".into();
        self.upd_result = None;
        self.job = Some(Job::Check { rx, cancel });
    }

    fn update_targets(&self) -> Vec<UpdateItem> {
        self.upd_items.iter().filter(|i| self.cfg.update_selected.contains(i.id()) && i.has_work()).cloned().collect()
    }

    fn start_update(&mut self) {
        let Some(root) = self.upd_root.clone() else { return };
        let items = self.update_targets();
        if items.is_empty() {
            return;
        }
        let opts = self.cfg.update_opts(&root);
        let (tx, rx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        thread::spawn(move || {
            let _awake = crate::power::KeepAwake::start();
            let report = update::install(&items, &opts, &c, &tx);
            let _ = tx.send(Event::Done(report));
        });
        self.begin_copy_job(rx, cancel, After::Updated, "Updating…");
    }

    fn backup_dir(&self) -> PathBuf {
        self.cfg.backup_dir(&self.es_root_path())
    }

    fn open_restore(&mut self) {
        self.backups = update::list_backups(&self.backup_dir());
        self.restore_picks.retain(|p, _| self.backups.iter().any(|(b, _)| b == p));
        self.restore_open = true;
    }

    fn start_restore(&mut self) {
        // newest first, so restoring several backups of one program ends at the oldest state
        let jobs: Vec<(PathBuf, Option<BTreeSet<String>>)> = self
            .backups
            .iter()
            .filter_map(|(path, meta)| {
                let pick = self.restore_picks.get(path).filter(|p| !p.files.is_empty())?;
                let whole = pick.files.len() == meta.replaced.len() + meta.added.len();
                Some((path.clone(), (!whole).then(|| pick.files.clone())))
            })
            .collect();
        if jobs.is_empty() {
            return;
        }
        let (tx, rx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        thread::spawn(move || {
            let _awake = crate::power::KeepAwake::start();
            let report = update::restore_backups(&jobs, &c, &tx);
            let _ = tx.send(Event::Done(report));
        });
        self.restore_picks.clear();
        self.begin_copy_job(rx, cancel, After::Restored, "Restoring…");
    }

    fn set_update_selected(&mut self, id: &str, on: bool) {
        toggle(&mut self.cfg.update_selected, id, on);
        config::save(&self.cfg);
    }

    // ---------- EMU Assistant's own updates ----------

    fn start_app_check(&mut self) {
        let (tx, rx) = unbounded();
        let token = self.cfg.github_token();
        thread::spawn(move || {
            let http = update::http::Http::with_cache(token, update::http::cache_path());
            let res = app_update::check(&http);
            http.save_cache();
            let _ = tx.send(res);
        });
        self.app_check_note = Some(("Checking…".into(), false));
        self.app_rx = Some(rx);
    }

    fn poll_app_check(&mut self) {
        let Some(rx) = &self.app_rx else { return };
        let res = match rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("the check stopped unexpectedly")),
        };
        self.app_rx = None;
        self.app_check_note = Some(match res {
            Ok(Some(r)) => {
                let note = format!("EMU Assistant {} is available", r.version);
                self.log(note.clone());
                self.app_release = Some(r);
                (note, false)
            }
            Ok(None) => (format!("You have the latest version ({})", app_update::VERSION), false),
            Err(e) => {
                self.log(format!("Could not check for EMU Assistant updates: {e:#}"));
                (format!("Could not check: {e:#}"), true)
            }
        });
    }

    fn start_app_update(&mut self) {
        let Some(rel) = self.app_release.clone() else { return };
        let token = self.cfg.github_token();
        let (tx, rx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        thread::spawn(move || {
            let report = app_update::install(&update::http::Http::new(token), &rel, &c, &tx);
            let _ = tx.send(Event::Done(report));
        });
        self.app_error = None;
        self.begin_copy_job(rx, cancel, After::AppUpdated, "Updating EMU Assistant…");
    }

    /// Deploy mode: older copies of EMU Assistant on this machine that the drive's exe can replace.
    fn find_old_copies(&mut self) {
        if self.mode != Mode::Deploy || self.old_copies_dismissed {
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let es = normalize(Path::new(self.t_es.trim()));
        let found = app_update::older_copies(&app_update::candidates(self.cfg.home_exe.as_deref(), &es), &exe, app_update::VERSION);
        self.old_copies = found.into_iter().map(|c| (c, true)).collect();
    }

    fn replace_old_copies(&mut self) {
        let Ok(exe) = std::env::current_exe() else { return };
        let picked: Vec<PathBuf> = self.old_copies.iter().filter(|(_, on)| *on).map(|(c, _)| c.path.clone()).collect();
        let mut failed = 0;
        for p in picked {
            match app_update::replace(&p, &exe) {
                Ok(()) => self.log(format!("Updated {} to EMU Assistant {}", p.display(), app_update::VERSION)),
                Err(e) => {
                    failed += 1;
                    self.log(format!("Could not update {}: {e:#}", p.display()));
                }
            }
        }
        self.status = if failed == 0 {
            format!("EMU Assistant on this machine updated to {}", app_update::VERSION)
        } else {
            "Could not update EMU Assistant on this machine, see the log".into()
        };
        self.old_copies.clear();
        self.old_copies_dismissed = true;
    }

    fn poll_job(&mut self) {
        let Some(job) = self.job.take() else { return };
        self.job = match job {
            Job::Scan { rx, cancel } => match drain(&rx, &mut self.status) {
                None => Some(Job::Scan { rx, cancel }),
                Some(Ok(cat)) => {
                    self.status = format!("Found {} systems with games", cat.systems.len());
                    for w in &cat.warnings {
                        self.log(format!("Warning: {w}"));
                    }
                    self.cfg.es_root = Some(cat.roots.es_root.clone());
                    config::save(&self.cfg);
                    self.export_cat = Some(Arc::new(cat));
                    self.apply_defaults();
                    None
                }
                Some(Err(e)) => {
                    self.status = format!("Scan failed: {e:#}");
                    None
                }
            },
            Job::Plan { rx, cancel, fp } => match drain(&rx, &mut self.status) {
                None => Some(Job::Plan { rx, cancel, fp }),
                Some(Ok(plan)) => {
                    self.drives = drives::list();
                    self.status = format!(
                        "Ready: {} files ({}) to copy, {} already up to date",
                        plan.copy.len(),
                        human_bytes(plan.copy_bytes),
                        plan.skipped
                    );
                    for w in &plan.warnings {
                        self.log(format!("Warning: {w}"));
                    }
                    self.plan = Some((fp, Arc::new(plan)));
                    None
                }
                Some(Err(e)) => {
                    self.status = format!("Analyze failed: {e:#}");
                    None
                }
            },
            Job::Games { rx, cancel, open } => match drain(&rx, &mut self.status) {
                None => Some(Job::Games { rx, cancel, open }),
                Some(Ok(lists)) => {
                    if let Some(cat) = self.deploy_cat.as_mut() {
                        let cat = Arc::make_mut(cat);
                        for (name, list) in lists {
                            if let Some(s) = cat.systems.iter_mut().find(|s| s.name == name) {
                                s.games = Some(list);
                            }
                        }
                    }
                    self.status = "Games listed".into();
                    if let Some(name) = open {
                        self.games_filter.clear();
                        self.games_open = Some(name);
                    }
                    None
                }
                Some(Err(e)) => {
                    self.status = format!("Could not list the games: {e:#}");
                    None
                }
            },
            Job::Check { rx, cancel } => match drain(&rx, &mut self.status) {
                None => Some(Job::Check { rx, cancel }),
                Some(Ok(items)) => {
                    let avail = items.iter().filter(|i| i.status == Status::Available).count();
                    let cores: usize = items.iter().map(|i| i.cores.len()).sum();
                    for i in &items {
                        if let Status::Failed(e) = &i.status {
                            self.log(format!("{}: {e}", i.id()));
                        }
                        if let Some(n) = &i.cores_note {
                            self.log(format!("{}: {n}", i.id()));
                        }
                    }
                    self.status = match (avail, cores) {
                        (0, 0) => "Checked: no updates found".into(),
                        (a, 0) => format!("Checked: {a} updates available"),
                        (a, c) => format!("Checked: {a} updates available, {c} RetroArch cores have newer builds"),
                    };
                    self.upd_items = items;
                    self.upd_checked = true;
                    None
                }
                Some(Err(e)) => {
                    self.status = format!("Check failed: {e:#}");
                    None
                }
            },
            Job::Copy { rx, cancel, after } => {
                let mut done = None;
                loop {
                    match rx.try_recv() {
                        Ok(Event::Progress(p)) => self.progress = Some(p),
                        Ok(Event::Log(s)) => self.log(s),
                        Ok(Event::Done(r)) => {
                            done = Some(r);
                            break;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            done = Some(Report { failed: vec!["copy thread stopped unexpectedly".into()], ..Default::default() });
                            break;
                        }
                    }
                }
                match done {
                    None => Some(Job::Copy { rx, cancel, after }),
                    Some(r) => {
                        self.finish_copy(r, after);
                        None
                    }
                }
            }
        };
    }

    fn finish_copy(&mut self, r: Report, after: After) {
        match after {
            After::AppUpdated => {
                self.progress = None;
                if r.cancelled {
                    self.status = "Cancelled".into();
                } else if let Some(e) = r.failed.first() {
                    self.log(format!("EMU Assistant update failed: {e}"));
                    self.status = "EMU Assistant update failed".into();
                    self.app_error = Some(e.clone());
                } else {
                    self.restart = true;
                }
            }
            After::Replan => {
                for f in r.failed.iter().take(200) {
                    self.log(format!("Error: {f}"));
                }
                self.log(report_summary(&r));
                self.plan = None;
                // a deploy may have put an ES-DE folder (with an older EMU Assistant) in place
                if !r.cancelled {
                    self.find_old_copies();
                }
                // re-check right away so the gauge and columns show the real state of the destination
                if !self.quit_when_idle {
                    self.start_plan();
                }
            }
            After::Updated | After::Restored => {
                let s = update_summary(&r, after);
                self.log(s.clone());
                let ok = r.failed.is_empty() && !r.cancelled;
                self.status = match (after, ok) {
                    (After::Restored, true) => "Restore finished".into(),
                    (_, true) => "Update finished".into(),
                    (_, false) if r.cancelled => "Cancelled".into(),
                    _ => "Finished with errors, see the log".into(),
                };
                self.upd_result = Some((s, ok));
                // the bar's total is an estimate (core sizes are rounded); don't leave it half full
                self.progress = None;
                // installed versions changed: re-read them locally, no new download check
                update::refresh(&mut self.upd_items);
                // emulator folder sizes changed; Export rescans when opened
                self.export_cat = None;
                if self.restore_open {
                    self.open_restore();
                }
            }
        }
        if !matches!(after, After::AppUpdated) {
            self.attention = true;
        }
        self.report = Some(r);
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_job();
        self.poll_app_check();
        if ctx.input(|i| i.viewport().close_requested()) && self.job.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_close = true;
        }
        if self.quit_when_idle && self.job.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if std::mem::take(&mut self.attention) && !ctx.input(|i| i.focused) {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
        }
        if std::mem::take(&mut self.restart) {
            match std::env::current_exe().map_err(anyhow::Error::from).and_then(|exe| app_update::restart(&exe)) {
                Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                Err(e) => self.app_error = Some(format!("Updated, but could not restart: {e:#}. Start EMU Assistant again.")),
            }
        }
        if self.app_rx.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        self.sel.emulators = self.effective_emus();
        // these two live in the settings now
        self.sel.exclude_cheats = self.cfg.exclude_cheats.unwrap_or(true);
        self.sel.exclude_thumbnails = self.cfg.exclude_thumbnails.unwrap_or(true);
        self.cur_fp = self.fingerprint();
        let busy = self.job.is_some();
        if busy {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| self.ui_top(ui, busy));
        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| self.ui_bottom(ui));
        egui::SidePanel::right("side").resizable(true).default_width(340.0).show(ctx, |ui| {
            if self.mode == Mode::Update {
                self.ui_update_side(ui)
            } else {
                self.ui_side(ui, busy)
            }
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.mode == Mode::Update {
                self.ui_update_table(ui, busy)
            } else {
                self.ui_systems(ui, busy)
            }
        });
        if self.confirm_prune {
            self.ui_confirm_prune(ctx);
        }
        if self.settings_open {
            self.ui_settings(ctx);
        }
        if self.restore_open {
            self.ui_restore(ctx);
        }
        if self.games_open.is_some() {
            self.ui_games(ctx);
        }
        if self.app_window {
            self.ui_app_update(ctx);
        }
        if !self.old_copies.is_empty() {
            self.ui_old_copies(ctx);
        }
        if self.confirm_close {
            self.ui_confirm_close(ctx);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(j) = &self.job {
            j.cancel();
        }
    }
}

/// Collects messages from a scan/analyze worker. `None` while it is still running.
fn drain<T>(rx: &Receiver<Msg<T>>, status: &mut String) -> Option<anyhow::Result<T>> {
    loop {
        match rx.try_recv() {
            Ok(Msg::Status(s)) => *status = s,
            Ok(Msg::Done(r)) => return Some(r),
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => return Some(Err(anyhow::anyhow!("worker stopped unexpectedly"))),
        }
    }
}

fn update_summary(r: &Report, after: After) -> String {
    let mut s = match after {
        After::Restored => format!("Restore: put back {} files, removed {} added files", r.copied, r.pruned),
        _ => format!(
            "Update: installed {} files, downloaded {} in {}",
            r.copied,
            human_bytes(r.copied_bytes),
            fmt_secs(r.elapsed.as_secs_f64())
        ),
    };
    if r.cancelled {
        s += " (cancelled)";
    }
    if !r.failed.is_empty() {
        s += &format!(", {} failed (see log)", r.failed.len());
    }
    s
}

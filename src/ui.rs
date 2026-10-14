//! egui front-end: Export (ES-DE -> drive), Deploy (drive -> this machine) and
//! Update (newer ES-DE / emulator builds), plus the Settings and Restore windows.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, TryRecvError};
use eframe::egui::{self, Color32, RichText};
use filetime::FileTime;

use crate::config::{self, Config};
use crate::deploy;
use crate::drives::{self, DriveInfo};
use crate::fsutil::human_bytes;
use crate::gauge::{self, TankReading, TankTextures, ACCENT};
use crate::manifest::{self, Manifest};
use crate::paths::{eq_ci, normalize, volume_root};
use crate::plan::{self, Mapper, Plan, PlanOptions, Selection};
use crate::scan::{self, Catalog, EmuEntry, Roots};
use crate::sync::{self, Event, Progress, Report};
use crate::update::archive::BackupMeta;
use crate::update::sources::{self, Kind};
use crate::update::{self, Status, UpdateItem};
use crate::APP_NAME;

// Palette: #1a1c28 base with the icon's cyan / purple / blue as highlights.
const BASE: Color32 = Color32::from_rgb(0x1a, 0x1c, 0x28);
const DEEP: Color32 = Color32::from_rgb(0x12, 0x13, 0x1c);
const FAINT: Color32 = Color32::from_rgb(0x20, 0x23, 0x33);
const WIDGET: Color32 = Color32::from_rgb(0x26, 0x2a, 0x3d);
const HOVER: Color32 = Color32::from_rgb(0x30, 0x35, 0x4d);
const ACTIVE: Color32 = Color32::from_rgb(0x3a, 0x40, 0x60);
const PURPLE: Color32 = Color32::from_rgb(0x8b, 0x5c, 0xf6);
const BLUE: Color32 = Color32::from_rgb(0x5b, 0x8d, 0xef);
const TEXT: Color32 = Color32::from_rgb(0xd8, 0xdc, 0xec);
const WARN: Color32 = Color32::from_rgb(0xf5, 0xb9, 0x42);
const BAD: Color32 = Color32::from_rgb(0xf0, 0x61, 0x6d);
const GOOD: Color32 = Color32::from_rgb(0x4a, 0xde, 0x80);
const MAX_LOG: usize = 5000;

fn stroke(width: f32, color: Color32) -> egui::Stroke {
    egui::Stroke::new(width, color)
}

fn apply_theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BASE;
    v.window_fill = BASE;
    v.extreme_bg_color = DEEP;
    v.faint_bg_color = FAINT;
    v.code_bg_color = DEEP;
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = WARN;
    v.error_fg_color = BAD;
    v.window_stroke = stroke(1.0, ACTIVE);
    v.selection.bg_fill = PURPLE.linear_multiply(0.75);
    v.selection.stroke = stroke(1.0, ACCENT);
    let rounding = egui::Rounding::same(5.0);
    v.window_rounding = egui::Rounding::same(8.0);
    for (w, fill) in [
        (&mut v.widgets.noninteractive, BASE),
        (&mut v.widgets.inactive, WIDGET),
        (&mut v.widgets.hovered, HOVER),
        (&mut v.widgets.active, ACTIVE),
        (&mut v.widgets.open, ACTIVE),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.rounding = rounding;
    }
    v.widgets.noninteractive.bg_stroke = stroke(1.0, FAINT.gamma_multiply(1.6));
    v.widgets.noninteractive.fg_stroke = stroke(1.0, TEXT);
    v.widgets.inactive.fg_stroke = stroke(1.0, TEXT);
    v.widgets.hovered.bg_stroke = stroke(1.0, BLUE);
    v.widgets.hovered.fg_stroke = stroke(1.5, Color32::WHITE);
    v.widgets.active.bg_stroke = stroke(1.0, ACCENT);
    v.widgets.active.fg_stroke = stroke(2.0, ACCENT);
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 4.0);
    });
}

/// Emulator size with skipped optional folders (cheats / thumbnails) left out.
fn emu_bytes(e: &EmuEntry, sel: &Selection) -> u64 {
    let skipped: u64 = e.optional_dirs.iter().filter(|o| sel.excludes_kind(&o.kind)).map(|o| o.bytes).sum();
    e.bytes.saturating_sub(skipped)
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
}

enum Job {
    Scan { rx: Receiver<Msg<Catalog>>, cancel: Arc<AtomicBool> },
    Plan { rx: Receiver<Msg<Plan>>, cancel: Arc<AtomicBool>, fp: String },
    Check { rx: Receiver<Msg<Vec<UpdateItem>>>, cancel: Arc<AtomicBool> },
    Copy { rx: Receiver<Event>, cancel: Arc<AtomicBool>, after: After },
}

impl Job {
    fn cancel(&self) {
        match self {
            Job::Scan { cancel, .. } | Job::Plan { cancel, .. } | Job::Check { cancel, .. } | Job::Copy { cancel, .. } => {
                cancel.store(true, Ordering::Relaxed)
            }
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
            tank: TankTextures::load(&cc.egui_ctx),
            icon: gauge::load_png(&cc.egui_ctx, "app_icon", crate::ICON_PNG, Some(64)),
            cfg,
        };
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
        if let Some(dir) = exe_dir.filter(|d| manifest::exists(d)) {
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
        let Some(cat) = self.cat() else { return };
        let all_globals: BTreeSet<String> = cat.globals.iter().map(|g| g.id.clone()).collect();
        let all_systems = cat.systems.iter().map(|s| s.name.clone());
        match self.mode {
            Mode::Update => {}
            Mode::Export => {
                self.sel = Selection {
                    systems: all_systems.filter(|s| self.cfg.systems.contains(s)).collect(),
                    emulators: BTreeSet::new(),
                    globals: match &self.cfg.globals {
                        Some(g) => g.intersection(&all_globals).cloned().collect(),
                        None => all_globals,
                    },
                    media: self.cfg.media.unwrap_or(true),
                    gamelists: self.cfg.gamelists.unwrap_or(true),
                    exclude_cheats: self.cfg.exclude_cheats.unwrap_or(true),
                    exclude_thumbnails: self.cfg.exclude_thumbnails.unwrap_or(true),
                };
                self.emu_override = self.cfg.emu_override.clone();
            }
            Mode::Deploy => {
                self.sel = Selection {
                    systems: all_systems.collect(),
                    emulators: BTreeSet::new(),
                    globals: all_globals,
                    media: true,
                    gamelists: true,
                    exclude_cheats: self.cfg.exclude_cheats.unwrap_or(true),
                    exclude_thumbnails: self.cfg.exclude_thumbnails.unwrap_or(true),
                };
                self.emu_override.clear();
            }
        }
    }

    /// Emulators referenced by the selected systems: id -> (used by default/override, system names).
    fn relevant_emus(&self, cat: &Catalog) -> BTreeMap<String, (bool, Vec<String>)> {
        let mut m: BTreeMap<String, (bool, Vec<String>)> = BTreeMap::new();
        for s in cat.systems.iter().filter(|s| self.sel.systems.contains(&s.name)) {
            for id in &s.emus_in_use {
                let e = m.entry(id.clone()).or_default();
                e.0 = true;
                e.1.push(s.fullname.clone());
            }
            for id in &s.emus_alt {
                m.entry(id.clone()).or_default().1.push(s.fullname.clone());
            }
        }
        m
    }

    fn effective_emus(&self) -> BTreeSet<String> {
        let Some(cat) = self.cat() else { return BTreeSet::new() };
        self.relevant_emus(&cat)
            .into_iter()
            .filter(|(id, (in_use, _))| {
                cat.emulators.get(id).is_some_and(|e| {
                    e.copyable && self.emu_override.get(id).copied().unwrap_or(*in_use && e.auto)
                })
            })
            .map(|(id, _)| id)
            .collect()
    }

    /// Space one system needs with the current media / gamelist options.
    fn system_bytes(&self, s: &scan::SysEntry) -> u64 {
        s.rom_bytes
            + if self.sel.media { s.media_bytes } else { 0 }
            + if self.sel.gamelists { s.gamelist_bytes } else { 0 }
    }

    fn selected_bytes(&self, cat: &Catalog) -> u64 {
        let sys: u64 = cat
            .systems
            .iter()
            .filter(|s| self.sel.systems.contains(&s.name))
            .map(|s| self.system_bytes(s))
            .sum();
        let emu: u64 =
            self.sel.emulators.iter().filter_map(|id| cat.emulators.get(id)).map(|e| emu_bytes(e, &self.sel)).sum();
        let glob: u64 = cat.globals.iter().filter(|g| self.sel.globals.contains(&g.id)).map(|g| g.bytes).sum();
        sys + emu + glob
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
                    let log = |s: String| {
                        let _ = tx.send(Event::Log(s));
                    };
                    let existing = manifest::load(&dest_root).unwrap_or_else(|e| {
                        log(format!("Ignoring unreadable manifest: {e:#}"));
                        None
                    });
                    let mut m = manifest::for_export(existing, &cat, &sel);
                    if let Err(e) = manifest::save(&dest_root, &m) {
                        log(format!("Could not write manifest: {e:#}"));
                    }
                    let report = sync::execute(&plan, verify, &c, &tx);
                    m.complete = !report.cancelled && report.failed.is_empty();
                    if let Err(e) = manifest::save(&dest_root, &m) {
                        log(format!("Could not write manifest: {e:#}"));
                    }
                    if copy_exe {
                        match copy_self(&dest_root) {
                            Ok(Some(p)) => log(format!("Copied EMU Assistant to {} (run it on the target machine to deploy)", p.display())),
                            Ok(None) => {}
                            Err(e) => log(format!("Could not copy EMU Assistant itself to the drive: {e:#}")),
                        }
                    }
                    let _ = tx.send(Event::Done(report));
                });
            }
            Mode::Deploy => {
                let target = self.target_roots();
                let fix_settings = self.cfg.fix_settings();
                thread::spawn(move || {
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
            After::Replan => {
                for f in r.failed.iter().take(200) {
                    self.log(format!("Error: {f}"));
                }
                self.log(report_summary(&r));
                self.plan = None;
                // re-check right away so the gauge and columns show the real state of the destination
                self.start_plan();
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
        self.report = Some(r);
    }

    // ---------- UI ----------

    fn ui_top(&mut self, ui: &mut egui::Ui, busy: bool) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add(egui::Image::new(egui::load::SizedTexture::new(self.icon.id(), egui::vec2(36.0, 36.0))));
            ui.vertical(|ui| {
                ui.add_space(2.0);
                ui.label(RichText::new(APP_NAME).size(22.0).strong().color(Color32::WHITE));
                ui.label(RichText::new("ES-DE export, deploy & update").small().color(ACCENT));
            });
            ui.add_space(24.0);
            ui.add_enabled_ui(!busy, |ui| {
                if ui.selectable_label(self.mode == Mode::Export, "  Export to drive  ").clicked() && self.mode != Mode::Export {
                    self.mode = Mode::Export;
                    let stale = self.export_cat.as_ref().is_none_or(|c| !eq_ci(&c.roots.es_root, &self.es_root_path()));
                    if stale && !self.es_root.trim().is_empty() {
                        self.start_scan();
                    } else {
                        self.apply_defaults();
                    }
                }
                if ui.selectable_label(self.mode == Mode::Deploy, "  Deploy from drive  ").clicked() && self.mode != Mode::Deploy {
                    self.mode = Mode::Deploy;
                    if self.pkg_root.trim().is_empty() {
                        self.pkg_root = self.dest.clone();
                    }
                    if self.deploy_cat.is_none() && !self.pkg_root.trim().is_empty() {
                        self.load_package();
                    } else {
                        self.apply_defaults();
                    }
                }
                if ui.selectable_label(self.mode == Mode::Update, "  Update programs  ").clicked() && self.mode != Mode::Update {
                    self.mode = Mode::Update;
                    self.plan = None;
                    self.progress = None;
                    if self.upd_root.as_ref().is_none_or(|r| !eq_ci(r, &self.es_root_path())) {
                        self.load_programs();
                    }
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let cog = ui.add(egui::Button::new(RichText::new("⚙").size(20.0)).frame(false)).on_hover_text("Settings");
                if cog.clicked() {
                    self.settings_open = !self.settings_open;
                    if self.settings_open {
                        self.settings_tab = match self.mode {
                            Mode::Export => SettingsTab::Export,
                            Mode::Deploy => SettingsTab::Deploy,
                            Mode::Update => SettingsTab::Updates,
                        };
                    }
                }
            });
        });
        ui.add_space(6.0);
        ui.add_enabled_ui(!busy, |ui| match self.mode {
            Mode::Export => self.ui_export_paths(ui),
            Mode::Deploy => self.ui_deploy_paths(ui),
            Mode::Update => self.ui_update_paths(ui),
        });
        ui.add_space(6.0);
    }

    fn ui_update_paths(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("update_paths").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("ES-DE folder");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.es_root).desired_width(520.0).hint_text("folder containing ES-DE.exe"));
                if ui.button("Browse…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.es_root = p.display().to_string();
                        self.load_programs();
                    }
                }
                if ui.button("Scan").on_hover_text("Look again for installed programs").clicked() {
                    self.load_programs();
                }
            });
            ui.end_row();
        });
    }

    fn ui_export_paths(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("export_paths").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("ES-DE folder");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.es_root).desired_width(520.0).hint_text("folder containing ES-DE.exe"));
                if ui.button("Browse…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.es_root = p.display().to_string();
                        self.start_scan();
                    }
                }
                if ui.button("Scan").clicked() {
                    self.start_scan();
                }
            });
            ui.end_row();

            ui.label("Destination");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.dest).desired_width(520.0).hint_text(r"external drive, e.g. E:\"));
                if ui.button("Browse…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.dest = p.display().to_string();
                    }
                }
                let mut pick = None;
                ui.menu_button("Drives…", |ui| {
                    if self.drives.is_empty() {
                        ui.label("No drives found");
                    }
                    for d in &self.drives {
                        if ui.button(d.describe()).clicked() {
                            pick = Some(d.root.clone());
                            ui.close_menu();
                        }
                    }
                });
                if let Some(p) = pick {
                    self.dest = p;
                }
                if ui.button("Refresh").on_hover_text("Refresh the drive list").clicked() {
                    self.drives = drives::list();
                }
                free_label(ui, &self.dest);
            });
            ui.end_row();
        });
    }

    fn ui_deploy_paths(&mut self, ui: &mut egui::Ui) {
        let orig = self.deploy_cat.as_ref().map(|c| c.roots.clone());
        egui::Grid::new("deploy_paths").num_columns(3).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("Package (drive)");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.pkg_root)
                        .desired_width(520.0)
                        .hint_text(r"drive or folder holding EMUAssistant\manifest.json"),
                );
                if ui.button("Browse…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.pkg_root = p.display().to_string();
                        self.load_package();
                    }
                }
                if ui.button("Load").clicked() {
                    self.load_package();
                }
            });
            match &self.manifest {
                Some(m) => {
                    let mut t = RichText::new(format!(
                        "exported from {} {}{}",
                        if m.source_machine.is_empty() { "?" } else { &m.source_machine },
                        manifest::age_text(m),
                        if m.complete { "" } else { " (incomplete)" }
                    ));
                    if !m.complete {
                        t = t.color(WARN);
                    }
                    ui.label(t);
                }
                None => {
                    ui.label("");
                }
            }
            ui.end_row();

            if let Some(orig) = &orig {
                let old = self.t_es.clone();
                path_row(ui, "Target ES-DE folder", &mut self.t_es, &orig.es_root);
                if old != self.t_es {
                    let (old_es, new_es) = (normalize(Path::new(old.trim())), normalize(Path::new(self.t_es.trim())));
                    for (o, t) in [(&orig.rom_dir, &mut self.t_rom), (&orig.media_dir, &mut self.t_media)] {
                        if let Some(p) = deploy::follow(o, &orig.es_root, &old_es, &new_es, &normalize(Path::new(t.trim()))) {
                            *t = p.display().to_string();
                        }
                    }
                }
                path_row(ui, "Target ROM folder", &mut self.t_rom, &orig.rom_dir);
                path_row(ui, "Target media folder", &mut self.t_media, &orig.media_dir);
            }
        });
    }

    fn dest_volume(&self) -> Option<String> {
        let p = match self.mode {
            Mode::Export => &self.dest,
            Mode::Deploy => &self.t_es,
            Mode::Update => &self.es_root,
        };
        volume_root(&normalize(Path::new(p.trim())))
    }

    /// One reading per destination volume: exact after a fresh Analyze, otherwise an
    /// estimate from the selection. While copying, only the remaining bytes are added,
    /// because the drive's used space already includes what was copied so far.
    fn tank_readings(&self) -> Vec<TankReading> {
        let title = |vol: &str| {
            let label = self.drives.iter().find(|d| d.root.eq_ignore_ascii_case(vol)).map(|d| d.label.as_str());
            match label {
                Some(l) if !l.is_empty() => format!("{vol}  {l}"),
                _ => vol.to_string(),
            }
        };
        let reading = |vol: &str, sync: u64, estimate: bool| {
            let (free, total) = drives::space(Path::new(vol))?;
            Some(TankReading { title: title(vol), total, used: total.saturating_sub(free), sync, estimate })
        };
        if let Some((_, plan)) = self.plan.as_ref().filter(|(fp, _)| *fp == self.cur_fp) {
            let remaining = match (&self.job, &self.progress) {
                (Some(Job::Copy { .. }), Some(p)) if p.total_bytes > 0 => {
                    1.0 - p.done_bytes as f64 / p.total_bytes as f64
                }
                _ => 1.0,
            };
            let mut vols: Vec<(String, u64)> = plan
                .bytes_by_volume
                .iter()
                .filter(|(v, _)| !v.is_empty())
                .map(|(v, b)| (v.clone(), (*b as f64 * remaining) as u64))
                .collect();
            if vols.is_empty() {
                vols.extend(self.dest_volume().map(|v| (v, 0)));
            }
            return vols.iter().filter_map(|(v, b)| reading(v, *b, false)).collect();
        }
        let Some(cat) = self.cat() else { return Vec::new() };
        self.dest_volume().and_then(|v| reading(&v, self.selected_bytes(&cat), true)).into_iter().collect()
    }

    fn ui_tanks(&self, ui: &mut egui::Ui) {
        let readings = self.tank_readings();
        if readings.is_empty() {
            ui.label(RichText::new("Pick a destination to see how full it will be.").weak());
            return;
        }
        if readings.len() == 1 {
            gauge::tank(ui, &self.tank, &readings[0], 150.0);
        } else {
            ui.columns(readings.len(), |cols| {
                for (col, r) in cols.iter_mut().zip(&readings) {
                    let w = (col.available_width() - 8.0).min(130.0);
                    gauge::tank(col, &self.tank, r, w);
                }
            });
        }
    }

    fn ui_side(&mut self, ui: &mut egui::Ui, busy: bool) {
        let cat = self.cat();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(6.0);
            ui.heading("Destination");
            self.ui_tanks(ui);
            ui.separator();
            ui.add_enabled_ui(!busy, |ui| {
                ui.heading("Options");
                ui.checkbox(&mut self.sel.gamelists, "Gamelists of selected systems");
                ui.checkbox(&mut self.sel.media, "Scraped media of selected systems");
                ui.checkbox(&mut self.prune, "Delete files missing from source").on_hover_text(
                    "Mirror mode for ROMs and scraped media of the selected systems: destination files that no longer exist in the source are deleted. Emulators and ES-DE files are never deleted. You will be asked to confirm.",
                );
                let mut active = Vec::new();
                if self.cfg.protect_newer() {
                    active.push("newer files at the destination are kept");
                }
                if self.cfg.verify() {
                    active.push("copies are verified");
                }
                if self.sel.exclude_cheats || self.sel.exclude_thumbnails {
                    active.push("RetroArch cheats/thumbnails skipped");
                }
                ui.horizontal_wrapped(|ui| {
                    if !active.is_empty() {
                        ui.label(RichText::new(format!("Settings: {}.", active.join(", "))).small().weak());
                    }
                    if ui.link(RichText::new("Change…").small()).clicked() {
                        self.settings_open = true;
                        self.settings_tab = if self.mode == Mode::Deploy { SettingsTab::Deploy } else { SettingsTab::Export };
                    }
                });

                let Some(cat) = cat else { return };
                ui.separator();
                ui.heading("ES-DE");
                for g in &cat.globals {
                    let mut on = self.sel.globals.contains(&g.id);
                    if ui
                        .checkbox(&mut on, format!("{}  ({})", g.label, human_bytes(g.bytes)))
                        .on_hover_text(g.src.display().to_string())
                        .changed()
                    {
                        toggle(&mut self.sel.globals, &g.id, on);
                    }
                }

                ui.separator();
                ui.heading("Emulators");
                let relevant = self.relevant_emus(&cat);
                if relevant.is_empty() {
                    ui.label(RichText::new("Select systems to see their emulators.").weak());
                }
                let mut rows: Vec<_> = relevant.into_iter().collect();
                rows.sort_by_key(|(id, (in_use, _))| (!*in_use, id.clone()));
                for (id, (in_use, systems)) in rows {
                    let name = emu_name(&id);
                    match cat.emulators.get(&id) {
                        Some(e) if e.copyable => {
                            let mut on = self.sel.emulators.contains(&id);
                            let mut text = RichText::new(format!("{name}  ({})", human_bytes(emu_bytes(e, &self.sel))));
                            if !in_use {
                                text = text.weak();
                            }
                            let tip = format!(
                                "Folder: {}\nUsed by: {}{}{}",
                                e.dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default(),
                                systems.join(", "),
                                if in_use { "" } else { "\nOnly configured as an alternative emulator" },
                                e.note.as_ref().map(|n| format!("\nNote: {n}")).unwrap_or_default()
                            );
                            if ui.checkbox(&mut on, text).on_hover_text(tip).changed() {
                                self.emu_override.insert(id.clone(), on);
                            }
                        }
                        Some(e) => {
                            ui.label(RichText::new(format!("x {name}: {}", e.note.as_deref().unwrap_or("not copyable"))).color(
                                if in_use { BAD } else { Color32::GRAY },
                            ))
                            .on_hover_text(format!("Used by: {}", systems.join(", ")));
                        }
                        None => {
                            ui.label(RichText::new(format!("x {name}: not on this drive")).color(Color32::GRAY));
                        }
                    }
                }
            });
        });
    }

    fn ui_systems(&mut self, ui: &mut egui::Ui, busy: bool) {
        let Some(cat) = self.cat() else {
            ui.centered_and_justified(|ui| {
                ui.label(match self.mode {
                    Mode::Export => "Pick your ES-DE folder and press Scan.",
                    Mode::Deploy | Mode::Update => "Pick the drive holding an EMU Assistant export and press Load.",
                });
            });
            return;
        };
        let filter = self.filter.to_lowercase();
        let visible: Vec<_> = cat
            .systems
            .iter()
            .filter(|s| filter.is_empty() || s.fullname.to_lowercase().contains(&filter) || s.name.contains(&filter))
            .collect();

        ui.horizontal(|ui| {
            ui.heading("Systems");
            ui.label(format!(
                "{} of {} selected, {} in total with emulators and ES-DE",
                self.sel.systems.len(),
                cat.systems.len(),
                human_bytes(self.selected_bytes(&cat))
            ));
        });
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                ui.label("Filter");
                ui.add(egui::TextEdit::singleline(&mut self.filter).desired_width(200.0));
                if ui.button("Select all").clicked() {
                    self.sel.systems.extend(visible.iter().map(|s| s.name.clone()));
                }
                if ui.button("Select none").clicked() {
                    for s in &visible {
                        self.sel.systems.remove(&s.name);
                    }
                }
            });
        });
        for w in &cat.warnings {
            ui.colored_label(WARN, w);
        }
        ui.separator();

        // per-system "to copy" from a fresh Analyze
        let to_copy = self.plan.as_ref().filter(|(fp, _)| *fp == self.cur_fp).map(|(_, p)| p.clone());
        let cols = if to_copy.is_some() { 8 } else { 7 };

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                egui::Grid::new("systems").striped(true).num_columns(cols).spacing([14.0, 4.0]).show(ui, |ui| {
                    ui.label("");
                    ui.strong("System");
                    ui.strong("Files");
                    ui.strong("ROMs");
                    ui.strong("Media");
                    ui.strong("Total").on_hover_text("Space this system needs: ROMs plus media and gamelist when those options are on");
                    if to_copy.is_some() {
                        ui.strong("To copy").on_hover_text("What the last Analyze found still missing or changed at the destination");
                    }
                    ui.strong("Emulators");
                    ui.end_row();
                    for s in visible {
                        let mut on = self.sel.systems.contains(&s.name);
                        if ui.checkbox(&mut on, "").changed() {
                            toggle(&mut self.sel.systems, &s.name, on);
                        }
                        ui.label(&s.fullname).on_hover_text(format!("{}\n{}", s.name, s.rom_dir.display()));
                        ui.label(s.rom_files.to_string());
                        ui.label(human_bytes(s.rom_bytes));
                        ui.label(if s.media_dir.is_some() { human_bytes(s.media_bytes + s.gamelist_bytes) } else { "-".into() });
                        let total = RichText::new(human_bytes(self.system_bytes(s))).strong();
                        ui.label(if on { total.color(ACCENT) } else { total });
                        if let Some(p) = &to_copy {
                            match (on, p.copy_bytes_by_system.get(&s.name)) {
                                (false, _) => ui.label(""),
                                (true, Some(&b)) if b > 0 => ui.label(RichText::new(human_bytes(b)).color(WARN)),
                                (true, _) => ui.label(RichText::new("up to date").color(GOOD)),
                            };
                        }
                        ui.horizontal(|ui| {
                            for id in &s.emus_in_use {
                                let (color, tip) = match cat.emulators.get(id) {
                                    Some(e) if e.copyable => (GOOD, e.dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default()),
                                    Some(e) => (BAD, e.note.clone().unwrap_or_default()),
                                    None => (Color32::GRAY, "not on this drive".into()),
                                };
                                ui.label(RichText::new(emu_name(id)).color(color)).on_hover_text(tip);
                            }
                            if s.noload {
                                ui.label(RichText::new("noload.txt").color(WARN))
                                    .on_hover_text("ES-DE skips this system because of noload.txt");
                            }
                        });
                        ui.end_row();
                    }
                });
            });
        });
    }

    fn ui_update_table(&mut self, ui: &mut egui::Ui, busy: bool) {
        if self.upd_items.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label("Pick your ES-DE folder and press Scan to list the installed programs.");
            });
            return;
        }
        let targets = self.update_targets();
        ui.horizontal(|ui| {
            ui.heading("Programs");
            ui.label(format!(
                "{} installed, {} ticked with updates ({} to download)",
                self.upd_items.len(),
                targets.len(),
                human_bytes(targets.iter().map(UpdateItem::download_size).sum())
            ));
        });
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                let with_updates: Vec<String> =
                    self.upd_items.iter().filter(|i| i.status == Status::Available || !i.cores.is_empty()).map(|i| i.id().to_string()).collect();
                if ui
                    .add_enabled(!with_updates.is_empty(), egui::Button::new("Tick all with updates"))
                    .on_hover_text("Ticks every program with a known newer version (not the ones whose version is unknown)")
                    .clicked()
                {
                    self.cfg.update_selected.extend(with_updates);
                    config::save(&self.cfg);
                }
                if ui.button("Untick all").clicked() {
                    self.cfg.update_selected.clear();
                    config::save(&self.cfg);
                }
            });
        });
        if !self.upd_checked {
            ui.label(RichText::new("Press Check for updates to look up the latest versions. Only the programs you tick are updated.").weak());
        }
        ui.separator();

        let mut toggles: Vec<(String, bool)> = Vec::new();
        let mut set_source: Option<String> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("updates").striped(true).num_columns(7).spacing([14.0, 5.0]).show(ui, |ui| {
                ui.label("");
                ui.strong("Program");
                ui.strong("Installed");
                ui.strong("Latest");
                ui.strong("Download");
                ui.strong("Status");
                ui.strong("Source");
                ui.end_row();
                for it in &self.upd_items {
                    let id = it.id().to_string();
                    let can = !matches!(it.status, Status::CantUpdate(_));
                    let mut on = self.cfg.update_selected.contains(&id);
                    if ui.add_enabled(can && !busy, egui::Checkbox::without_text(&mut on)).changed() {
                        toggles.push((id.clone(), on));
                    }
                    let name = RichText::new(update_label(&id));
                    ui.label(if can { name } else { name.weak() })
                        .on_hover_text(format!("{}\n{}", it.program.dir.display(), it.program.exe.display()));
                    ui.label(&it.installed);
                    ui.label(it.latest.as_ref().map(|r| r.version.as_str()).unwrap_or("-"))
                        .on_hover_text(it.latest.as_ref().map(|r| format!("{}\n{}", r.asset_name, r.published)).unwrap_or_default());
                    let size = it.download_size();
                    ui.label(if size > 0 { human_bytes(size) } else { String::new() });
                    ui.vertical(|ui| {
                        ui.set_min_width(300.0);
                        let (text, color, tip): (String, Color32, String) = match &it.status {
                            Status::NotChecked => ("not checked".into(), Color32::GRAY, String::new()),
                            Status::UpToDate => ("up to date".into(), GOOD, String::new()),
                            Status::Available => ("update available".into(), WARN, String::new()),
                            Status::Unknown(s) => (s.clone(), ACCENT, "EMU Assistant can't read this program's version. Tick it to install the latest release; after that its version is known.".into()),
                            Status::CantUpdate(s) => (shorten(s, 60), Color32::GRAY, s.clone()),
                            Status::Failed(e) => (shorten(e, 60), BAD, e.clone()),
                        };
                        let r = ui.add(egui::Label::new(RichText::new(text).color(color)).extend());
                        if !tip.is_empty() {
                            r.on_hover_text(tip);
                        }
                        if !it.cores.is_empty() {
                            ui.label(RichText::new(format!("+ {} cores have newer builds", it.cores.len())).small().color(WARN))
                                .on_hover_text(it.cores.iter().map(|c| c.file.as_str()).collect::<Vec<_>>().join("\n"));
                        }
                        if let Some(n) = &it.cores_note {
                            ui.label(RichText::new(n).small().color(BAD));
                        }
                    });
                    let custom = self.cfg.update_sources.contains_key(&id);
                    match &it.source {
                        Some(s) => {
                            let text = format!("{}{}", s.describe(), if custom { " (custom)" } else { "" });
                            if ui.link(RichText::new(shorten(&text, 40)).small()).on_hover_text(format!("{text}\nClick to change")).clicked() {
                                set_source = Some(id.clone());
                            }
                        }
                        None if !scan::is_program_files(&it.program.dir) => {
                            if ui.link(RichText::new("Set source…").small()).clicked() {
                                set_source = Some(id.clone());
                            }
                        }
                        None => {
                            ui.label("");
                        }
                    }
                    ui.end_row();
                }
            });
        });
        for (id, on) in toggles {
            self.set_update_selected(&id, on);
        }
        if let Some(id) = set_source {
            self.settings_open = true;
            self.settings_tab = SettingsTab::Sources;
            self.settings_focus = Some((id, true));
        }
    }

    fn ui_update_side(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(6.0);
            ui.heading("Update");
            let targets = self.update_targets();
            let download: u64 = targets.iter().map(UpdateItem::download_size).sum();
            ui.label(format!("{} programs ticked with updates", targets.len()));
            ui.label(format!("Download: {}", human_bytes(download)));
            free_label(ui, &self.es_root);
            ui.separator();
            ui.heading("Safety");
            if self.cfg.update_backup() {
                ui.label("Every file an update replaces is saved in a backup zip first.");
            } else {
                ui.colored_label(WARN, "Backups are off: replaced files can't be restored.");
            }
            ui.label(RichText::new("Settings, saves and configs are never overwritten.").weak());
            let dir = self.backup_dir();
            ui.label(RichText::new(format!("Backups: {}", dir.display())).small().weak());
            ui.horizontal(|ui| {
                if ui.add_enabled(self.job.is_none(), egui::Button::new("Restore…")).on_hover_text("Roll back an update").clicked() {
                    self.open_restore();
                }
                if ui.button("Settings…").clicked() {
                    self.settings_open = true;
                    self.settings_tab = SettingsTab::Updates;
                }
            });
            ui.separator();
            ui.label(
                RichText::new(
                    "Updating here and then exporting brings the new versions to your other machine too. \
                     Programs installed under Program Files aren't listed; they update with their own installer.",
                )
                .small()
                .weak(),
            );
        });
    }

    fn ui_bottom(&mut self, ui: &mut egui::Ui) {
        if self.mode == Mode::Update {
            return self.ui_update_bottom(ui);
        }
        let busy = self.job.is_some();
        let fresh = self.plan_is_fresh();
        let plan = self.plan.as_ref().map(|(_, p)| p.clone());
        let space_issues = plan.as_ref().map(|p| space_problems(p)).unwrap_or_default();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let can_plan = !busy && self.cat().is_some();
            if ui.add_enabled(can_plan, egui::Button::new("Analyze")).on_hover_text("Compare source and destination").clicked() {
                self.start_plan();
            }
            let has_work = plan.as_ref().is_some_and(|p| !p.copy.is_empty() || !p.prune.is_empty());
            let can_copy = !busy && fresh && has_work && space_issues.is_empty();
            let label = match self.mode {
                Mode::Export => "Copy to drive",
                Mode::Deploy | Mode::Update => "Deploy to this machine",
            };
            if ui.add_enabled(can_copy, egui::Button::new(RichText::new(label).strong())).clicked() {
                if self.prune && plan.as_ref().is_some_and(|p| !p.prune.is_empty()) {
                    self.confirm_prune = true;
                } else {
                    self.start_copy();
                }
            }
            if busy && ui.button("Cancel").clicked() {
                if let Some(j) = &self.job {
                    j.cancel();
                }
            }
            ui.label(&self.status);
        });

        if let Some(p) = &plan {
            if !fresh {
                ui.colored_label(WARN, "Selection or folders changed since the last Analyze; press Analyze again.");
            }
            ui.label(format!(
                "To copy: {} files, {}    Up to date: {} files, {}    Newer at destination (kept): {}    To delete: {}",
                p.copy.len(),
                human_bytes(p.copy_bytes),
                p.skipped,
                human_bytes(p.skipped_bytes),
                p.protected.len(),
                p.prune.len()
            ));
            for s in &space_issues {
                ui.colored_label(BAD, s);
            }
            if !p.protected.is_empty() {
                egui::CollapsingHeader::new(format!("Kept {} newer files at destination", p.protected.len()))
                    .id_salt("protected")
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical().max_height(120.0).id_salt("protected_scroll").show(ui, |ui| {
                            for f in p.protected.iter().take(500) {
                                ui.label(RichText::new(f.display().to_string()).small());
                            }
                        });
                    });
            }
        }

        self.ui_progress_and_log(ui, busy, "file");
    }

    fn ui_update_bottom(&mut self, ui: &mut egui::Ui) {
        let busy = self.job.is_some();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let can_check = !busy && self.upd_root.is_some() && !self.upd_items.is_empty();
            if ui
                .add_enabled(can_check, egui::Button::new("Check for updates"))
                .on_hover_text("Look up the latest version of every installed program")
                .clicked()
            {
                self.start_check();
            }
            let n = self.update_targets().len();
            let label = if n > 0 { format!("Update selected ({n})") } else { "Update selected".into() };
            let tip = if !self.upd_checked {
                "Check for updates first, then tick the programs to update"
            } else {
                "Downloads and installs the ticked programs that have updates"
            };
            if ui.add_enabled(!busy && n > 0, egui::Button::new(RichText::new(label).strong())).on_hover_text(tip).clicked() {
                self.start_update();
            }
            if ui.add_enabled(!busy, egui::Button::new("Restore…")).on_hover_text("Roll back an update from its backup").clicked() {
                self.open_restore();
            }
            if busy && ui.button("Cancel").clicked() {
                if let Some(j) = &self.job {
                    j.cancel();
                }
            }
            ui.label(&self.status);
        });
        if let (false, Some((text, ok))) = (busy, &self.upd_result) {
            let (icon, color) = if *ok { ("✔", GOOD) } else { ("⚠", BAD) };
            ui.label(RichText::new(format!("{icon} {text}")).strong().color(color));
        }
        self.ui_progress_and_log(ui, busy, "program");
    }

    fn ui_progress_and_log(&mut self, ui: &mut egui::Ui, busy: bool, unit: &str) {
        if let Some(p) = &self.progress {
            let frac = if p.total_bytes > 0 {
                p.done_bytes as f32 / p.total_bytes as f32
            } else if p.total_files > 0 {
                p.done_files as f32 / p.total_files as f32
            } else {
                1.0
            };
            let mut text = format!(
                "{} / {}   {unit} {} of {}",
                human_bytes(p.done_bytes),
                human_bytes(p.total_bytes),
                (p.done_files + u64::from(busy)).min(p.total_files),
                p.total_files
            );
            if let (true, Some(start)) = (busy, self.copy_started) {
                let secs = start.elapsed().as_secs_f64().max(0.001);
                let speed = p.done_bytes as f64 / secs;
                text += &format!("   {}/s", human_bytes(speed as u64));
                if speed > 0.0 {
                    text += &format!("   ETA {}", fmt_secs(p.total_bytes.saturating_sub(p.done_bytes) as f64 / speed));
                }
            }
            ui.add(egui::ProgressBar::new(frac).text(text));
            if busy && !p.current.is_empty() {
                ui.label(RichText::new(&p.current).small().weak());
            }
        }

        egui::CollapsingHeader::new(format!("Log ({})", self.log.len())).id_salt("log").show(ui, |ui| {
            egui::ScrollArea::vertical().max_height(160.0).stick_to_bottom(true).id_salt("log_scroll").show(ui, |ui| {
                for l in &self.log {
                    ui.label(RichText::new(l).small());
                }
            });
        });
        ui.add_space(4.0);
    }

    fn ui_confirm_prune(&mut self, ctx: &egui::Context) {
        let Some((_, plan)) = self.plan.clone() else {
            self.confirm_prune = false;
            return;
        };
        egui::Window::new("Delete files at destination?")
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!(
                    "{} files at the destination no longer exist in the source and will be deleted:",
                    plan.prune.len()
                ));
                egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                    for f in plan.prune.iter().take(1000) {
                        ui.label(RichText::new(f.display().to_string()).small());
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Delete and copy").color(BAD)).clicked() {
                        self.confirm_prune = false;
                        self.start_copy();
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_prune = false;
                    }
                });
            });
    }

    fn ui_settings(&mut self, ctx: &egui::Context) {
        let mut open = self.settings_open;
        let mut changed = false;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(680.0)
            .default_height(460.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (tab, label) in [
                        (SettingsTab::General, "General"),
                        (SettingsTab::Export, "Export"),
                        (SettingsTab::Deploy, "Deploy"),
                        (SettingsTab::Updates, "Updates"),
                        (SettingsTab::Sources, "Update sources"),
                    ] {
                        if ui.selectable_label(self.settings_tab == tab, format!(" {label} ")).clicked() {
                            self.settings_tab = tab;
                        }
                    }
                });
                ui.separator();
                let cfg = &mut self.cfg;
                let mut check = |ui: &mut egui::Ui, value: &mut Option<bool>, default: bool, label: &str, tip: &str| {
                    let mut v = value.unwrap_or(default);
                    if ui.checkbox(&mut v, label).on_hover_text(tip).changed() {
                        *value = Some(v);
                        changed = true;
                    }
                };
                match self.settings_tab {
                    SettingsTab::General => {
                        ui.label(RichText::new("Used by Export and Deploy.").weak());
                        check(ui, &mut cfg.protect_newer, true, "Keep newer files at destination",
                            "Never overwrite a destination file that is newer than the source, e.g. save games or configs changed on the target machine.");
                        check(ui, &mut cfg.verify, false, "Verify after copy (slower)",
                            "Re-read every copied file and compare it with the source.");
                    }
                    SettingsTab::Export => {
                        check(ui, &mut cfg.exclude_cheats, true, "Skip RetroArch cheats",
                            "Leave out RetroArch's cheat database (lots of small files, re-download it from the RetroArch online updater).");
                        check(ui, &mut cfg.exclude_thumbnails, true, "Skip RetroArch thumbnails",
                            "Leave out RetroArch's own menu thumbnails (ES-DE doesn't use them, re-download from the online updater).");
                        check(ui, &mut cfg.copy_exe, true, "Copy EMUAssistant.exe to the drive",
                            "Puts this program on the drive so you can run it on the other machine to deploy.");
                    }
                    SettingsTab::Deploy => {
                        check(ui, &mut cfg.fix_settings, true, "Update ES-DE's ROM and media folders after deploying",
                            "When the ROM or media folder ends up in a different place than on the main machine, point es_settings.xml at it (the original is kept as es_settings.xml.emuasist-bak).");
                        check(ui, &mut cfg.exclude_cheats, true, "Skip RetroArch cheats", "Leave out RetroArch's cheat database when deploying.");
                        check(ui, &mut cfg.exclude_thumbnails, true, "Skip RetroArch thumbnails", "Leave out RetroArch's menu thumbnails when deploying.");
                    }
                    SettingsTab::Updates => {
                        check(ui, &mut cfg.update_backup, true, "Back up replaced files (zip)",
                            "Before an update overwrites anything, the old files are saved in a zip so the update can be rolled back with Restore.");
                        check(ui, &mut cfg.update_cores, true, "Update RetroArch cores",
                            "Also download newer builds of the cores already in RetroArch's cores folder (nightly builds, like RetroArch's online updater).");
                        ui.horizontal(|ui| {
                            ui.label("Keep the last");
                            let mut n = cfg.keep_backups();
                            if ui.add(egui::DragValue::new(&mut n).range(0..=50)).changed() {
                                cfg.keep_backups = Some(n);
                                changed = true;
                            }
                            ui.label("backups per program (0 keeps all)");
                        });
                        ui.horizontal(|ui| {
                            ui.label("Backup folder");
                            let es = normalize(Path::new(self.es_root.trim()));
                            let mut text = cfg.backup_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                            let hint = config::default_backup_dir(&es).display().to_string();
                            if ui.add(egui::TextEdit::singleline(&mut text).desired_width(380.0).hint_text(hint)).changed() {
                                cfg.backup_dir = (!text.trim().is_empty()).then(|| PathBuf::from(text.trim()));
                                changed = true;
                            }
                            if ui.button("Browse…").clicked() {
                                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                                    cfg.backup_dir = Some(p);
                                    changed = true;
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("GitHub token");
                            let mut t = cfg.github_token.clone().unwrap_or_default();
                            if ui.add(egui::TextEdit::singleline(&mut t).password(true).desired_width(300.0).hint_text("optional"))
                                .on_hover_text("Without a token GitHub allows 60 checks an hour. A token without any permissions is enough. The GITHUB_TOKEN environment variable is used instead when set.")
                                .changed()
                            {
                                cfg.github_token = (!t.trim().is_empty()).then(|| t.trim().to_string());
                                changed = true;
                            }
                        });
                    }
                    SettingsTab::Sources => {
                        changed |= Self::ui_sources(ui, cfg, &mut self.upd_items, &mut self.settings_focus);
                    }
                }
                if self.settings_tab != SettingsTab::Sources {
                    self.settings_focus = None;
                }
            });
        self.settings_open = open;
        if !open {
            self.settings_focus = None;
        }
        if changed {
            config::save(&self.cfg);
        }
    }

    /// The Update sources tab. Returns true when something changed.
    /// `focus` = (program to outline, still needs scrolling to).
    fn ui_sources(ui: &mut egui::Ui, cfg: &mut Config, items: &mut [UpdateItem], focus: &mut Option<(String, bool)>) -> bool {
        let mut changed = false;
        ui.label(
            RichText::new(
                "Where each installed program's updates come from. Edit a source for an emulator EMU Assistant doesn't know, \
                 or pick a different build with the file pattern (* matches anything).",
            )
            .weak(),
        );
        if items.is_empty() {
            ui.label("Open the Update programs tab and scan your ES-DE folder to list the installed programs.");
            return false;
        }
        let focus_id = focus.as_ref().map(|(id, _)| id.clone());
        let mut scroll = focus.as_ref().is_some_and(|(_, s)| *s);
        if let Some((_, s)) = focus.as_mut() {
            *s = false;
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for item in items.iter_mut() {
                let id = item.id().to_string();
                if scan::is_program_files(&item.program.dir) {
                    continue;
                }
                let builtin = sources::builtin(&id);
                let custom = cfg.update_sources.contains_key(&id);
                let mut row_changed = false;
                let mut spec = cfg.update_sources.get(&id).cloned().or_else(|| builtin.clone()).unwrap_or_default();
                let before = spec.clone();
                let frame = egui::Frame::group(ui.style()).stroke(if focus_id.as_deref() == Some(id.as_str()) {
                    stroke(1.5, ACCENT)
                } else {
                    ui.style().visuals.widgets.noninteractive.bg_stroke
                });
                let resp = frame.show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.strong(update_label(&id));
                        ui.label(RichText::new(match (custom, &builtin, &item.source) {
                            (true, _, _) => "custom",
                            (false, Some(_), _) => "built-in",
                            _ => "no source",
                        }).small().weak());
                        if custom && ui.small_button(if builtin.is_some() { "Reset to built-in" } else { "Remove" }).clicked() {
                            cfg.update_sources.remove(&id);
                            spec = builtin.clone().unwrap_or_default();
                            row_changed = true;
                        }
                    });
                    egui::Grid::new(format!("src_{id}")).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                        ui.label("Kind");
                        egui::ComboBox::from_id_salt(format!("kind_{id}")).selected_text(spec.kind.label()).show_ui(ui, |ui| {
                            for k in Kind::ALL {
                                ui.selectable_value(&mut spec.kind, k, k.label());
                            }
                        });
                        ui.end_row();
                        let (label, hint) = match spec.kind {
                            Kind::GitHub => ("Repository", "owner/repo"),
                            Kind::GitLab => ("Project", "group/project"),
                            Kind::Gitea => ("Repository", "owner/repo"),
                            Kind::Url => ("Link", "https://…/emulator-windows.zip"),
                            Kind::Buildbot => ("", ""),
                        };
                        if spec.kind == Kind::Gitea {
                            ui.label("Server");
                            ui.add(egui::TextEdit::singleline(&mut spec.host).desired_width(360.0).hint_text("git.example.org"));
                            ui.end_row();
                        }
                        if spec.kind != Kind::Buildbot {
                            ui.label(label);
                            ui.add(egui::TextEdit::singleline(&mut spec.repo).desired_width(360.0).hint_text(hint));
                            ui.end_row();
                        }
                        if matches!(spec.kind, Kind::GitHub | Kind::GitLab | Kind::Gitea) {
                            ui.label("File pattern");
                            ui.add(egui::TextEdit::singleline(&mut spec.asset).desired_width(360.0).hint_text("empty: the Windows x64 zip/7z"))
                                .on_hover_text("Which download of a release to use, e.g. *windows-x64*.zip");
                            ui.end_row();
                        }
                    });
                });
                if scroll && focus_id.as_deref() == Some(id.as_str()) {
                    resp.response.scroll_to_me(Some(egui::Align::Center));
                    scroll = false;
                }
                if spec != before && !row_changed {
                    if Some(&spec) == builtin.as_ref() {
                        cfg.update_sources.remove(&id);
                    } else {
                        cfg.update_sources.insert(id.clone(), spec.clone());
                    }
                    row_changed = true;
                }
                if row_changed {
                    changed = true;
                    // the old check result belongs to the old source
                    item.source = update::source_for(&id, &cfg.update_sources);
                    item.latest = None;
                    item.cores.clear();
                    item.status = match item.source {
                        Some(_) => Status::NotChecked,
                        None => Status::CantUpdate("no update source; set one in Settings > Update sources".into()),
                    };
                }
                ui.add_space(4.0);
            }
        });
        changed
    }

    fn ui_restore(&mut self, ctx: &egui::Context) {
        let mut open = self.restore_open;
        let busy = self.job.is_some();
        let mut start = false;
        egui::Window::new("Restore from backup")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(640.0)
            .default_height(420.0)
            .show(ctx, |ui| {
                ui.label(
                    "Tick a backup to undo that update completely, or expand it and tick single files. \
                     Restoring puts the old files back and deletes the files the update added.",
                );
                ui.horizontal(|ui| {
                    ui.label(RichText::new(self.backup_dir().display().to_string()).small().weak());
                    if ui.small_button("Open folder").clicked() {
                        let dir = self.backup_dir();
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = std::process::Command::new("explorer").arg(dir).spawn();
                    }
                });
                ui.separator();
                if self.backups.is_empty() {
                    ui.label("No backups yet. Each update you run creates one.");
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(300.0).show(ui, |ui| {
                    for (path, meta) in &self.backups {
                        let total = meta.replaced.len() + meta.added.len();
                        let pick = self.restore_picks.entry(path.clone()).or_insert_with(|| RestorePick { files: BTreeSet::new(), expanded: false });
                        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                        ui.horizontal(|ui| {
                            let mut all = pick.files.len() == total && total > 0;
                            let partial = !pick.files.is_empty() && !all;
                            if ui.add(egui::Checkbox::without_text(&mut all).indeterminate(partial)).changed() {
                                pick.files = if all {
                                    meta.replaced.iter().map(|f| f.path.clone()).chain(meta.added.iter().cloned()).collect()
                                } else {
                                    BTreeSet::new()
                                };
                            }
                            ui.strong(update_label(&meta.id));
                            ui.label(format!(
                                "{} to {}   {}   {} replaced, {} added   {}",
                                meta.old_version,
                                meta.new_version,
                                update::version::date_text(meta.created_unix),
                                meta.replaced.len(),
                                meta.added.len(),
                                human_bytes(size)
                            ))
                            .on_hover_text(path.display().to_string());
                            if ui.small_button(if pick.expanded { "Hide files" } else { "Files…" }).clicked() {
                                pick.expanded = !pick.expanded;
                            }
                        });
                        if pick.expanded {
                            ui.indent(path.display().to_string(), |ui| {
                                for f in meta.replaced.iter().take(2000) {
                                    let mut on = pick.files.contains(&f.path);
                                    if ui.checkbox(&mut on, RichText::new(format!("restore  {}", f.path)).small()).changed() {
                                        toggle(&mut pick.files, &f.path, on);
                                    }
                                }
                                for f in meta.added.iter().take(2000) {
                                    let mut on = pick.files.contains(f);
                                    if ui.checkbox(&mut on, RichText::new(format!("delete   {f}")).small().color(WARN)).changed() {
                                        toggle(&mut pick.files, f, on);
                                    }
                                }
                            });
                        }
                    }
                });
                ui.separator();
                let n: usize = self.restore_picks.values().map(|p| p.files.len()).sum();
                ui.horizontal(|ui| {
                    let label = format!("Restore {n} files");
                    if ui.add_enabled(!busy && n > 0, egui::Button::new(RichText::new(label).strong())).clicked() {
                        start = true;
                    }
                    ui.label(RichText::new("Close the programs first; running ones are skipped.").small().weak());
                });
            });
        self.restore_open = open;
        if start {
            self.start_restore();
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_job();
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

fn toggle(set: &mut BTreeSet<String>, id: &str, on: bool) {
    if on {
        set.insert(id.to_string());
    } else {
        set.remove(id);
    }
}

fn emu_name(id: &str) -> String {
    match id.strip_prefix("EXE:") {
        Some(p) => Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.into()),
        None => id.to_string(),
    }
}

fn path_row(ui: &mut egui::Ui, label: &str, value: &mut String, was: &Path) {
    ui.label(label);
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(value).desired_width(520.0));
        if ui.button("Browse…").clicked() {
            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                *value = p.display().to_string();
            }
        }
        free_label(ui, value);
    });
    let mut t = RichText::new(format!("source: {}", was.display())).weak();
    if !eq_ci(&normalize(Path::new(value.trim())), was) {
        t = t.color(WARN);
    }
    ui.label(t);
    ui.end_row();
}

fn free_label(ui: &mut egui::Ui, path: &str) {
    if let Some(free) = drives::free_space(Path::new(path.trim())) {
        ui.label(RichText::new(format!("{} free", human_bytes(free))).weak());
    }
}

fn space_problems(plan: &Plan) -> Vec<String> {
    plan.bytes_by_volume
        .iter()
        .filter_map(|(vol, need)| {
            let free = drives::free_space(Path::new(vol))?;
            (free < *need).then(|| {
                format!("Not enough space on {vol}: needs {}, only {} free", human_bytes(*need), human_bytes(free))
            })
        })
        .collect()
}

fn report_summary(r: &Report) -> String {
    let mut s = format!(
        "{} {} files ({}) in {}",
        if r.cancelled { "Cancelled after copying" } else { "Last run: copied" },
        r.copied,
        human_bytes(r.copied_bytes),
        fmt_secs(r.elapsed.as_secs_f64())
    );
    if r.pruned > 0 {
        s += &format!(", deleted {}", r.pruned);
    }
    if !r.failed.is_empty() {
        s += &format!(", {} errors (see log)", r.failed.len());
    }
    s
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

/// Display name of an update program id.
fn update_label(id: &str) -> String {
    match id {
        update::ESDE_ID => "ES-DE".into(),
        "RETROARCH" => "RetroArch".into(),
        _ => emu_name(id),
    }
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
    }
}

fn fmt_secs(s: f64) -> String {
    let s = s.max(0.0) as u64;
    match s {
        s if s >= 3600 => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
        s if s >= 60 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
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
    sync::copy_file(&exe, &target, mtime, &AtomicBool::new(false), &mut |_| {})?;
    Ok(Some(target))
}

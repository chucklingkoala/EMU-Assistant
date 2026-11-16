//! The Direct play window: what running ES-DE straight from the drive needs, and the
//! buttons to turn it on, start ES-DE, download what's missing and turn it off again.

use super::*;
use crate::play::{EmuState, Level, Portable};

/// Download ticks that aren't emulator ids.
pub(super) const DL_CORES: &str = "\u{1}cores";
pub(super) const DL_ESDE: &str = "\u{1}es-de";

fn level_color(l: Level) -> Color32 {
    match l {
        Level::Ok => GOOD,
        Level::Fix => ACCENT,
        Level::Attention => WARN,
    }
}

impl App {
    fn pkg_path(&self) -> PathBuf {
        normalize(Path::new(self.pkg_root.trim()))
    }

    fn play_on(&self) -> bool {
        self.manifest.as_ref().is_some_and(|m| m.play.is_some())
    }

    pub(super) fn open_play(&mut self) {
        self.play_open = true;
        if self.job.is_none() {
            self.start_play_check();
        }
    }

    pub(super) fn start_play_check(&mut self) {
        let Some(m) = self.manifest.clone() else { return };
        let pkg = self.pkg_path();
        let sources = self.cfg.update_sources.clone();
        let (tx, rx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        thread::spawn(move || {
            let res = play::check(&pkg, &m, &sources, &c, &mut |s| {
                let _ = tx.send(Msg::Status(s));
            });
            let _ = tx.send(Msg::Done(res));
        });
        self.status = "Checking the drive…".into();
        self.job = Some(Job::PlayCheck { rx, cancel });
    }

    /// Runs `f` on the drive's manifest in a copy-style job, saves the manifest, then
    /// checks again.
    fn play_job(&mut self, status: &str, f: impl FnOnce(&Path, &mut Manifest) -> (Vec<String>, Vec<String>) + Send + 'static) {
        let Some(mut m) = self.manifest.clone() else { return };
        let pkg = self.pkg_path();
        let (tx, rx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        thread::spawn(move || {
            let _awake = crate::power::KeepAwake::start();
            let (lines, mut failed) = f(&pkg, &mut m);
            for l in lines {
                let _ = tx.send(Event::Log(l));
            }
            if let Err(e) = manifest::save(&pkg, &m) {
                failed.push(format!("Could not save the manifest: {e:#}"));
            }
            let _ = tx.send(Event::Done(Report { failed, ..Default::default() }));
        });
        self.begin_copy_job(rx, cancel, After::PlayChanged, status);
    }

    fn start_play_enable(&mut self) {
        let opts = play::Options { make_portable: self.play_portable.clone() };
        self.play_job("Enabling direct play…", move |pkg, m| match play::enable(pkg, m, &opts) {
            Ok(lines) => (lines, Vec::new()),
            Err(e) => (Vec::new(), vec![format!("{e:#}")]),
        });
    }

    fn start_play_disable(&mut self) {
        let remove = self.play_remove_added;
        self.play_job("Turning direct play off…", move |pkg, m| {
            let lines = play::disable(pkg, m, remove);
            let failed = if m.play.is_some() { vec!["Turning off stopped; see the log and try again".to_string()] } else { Vec::new() };
            (lines, failed)
        });
    }

    fn start_play_download(&mut self) {
        let (Some(report), Some(mut m)) = (self.play_report.clone(), self.manifest.clone()) else { return };
        let pkg = self.pkg_path();
        let ticks = self.play_download.clone();
        let portable = self.play_portable.clone();
        let opts = update::Opts {
            backup: false,
            backup_dir: std::env::temp_dir(),
            keep: 0,
            cores: true,
            github_token: self.cfg.github_token.clone(),
            sources: self.cfg.update_sources.clone(),
        };
        let (tx, rx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        thread::spawn(move || {
            let _awake = crate::power::KeepAwake::start();
            let log = |s: String| {
                let _ = tx.send(Event::Log(s));
            };
            let http = update::http::Http::with_cache(opts.github_token.clone(), update::http::cache_path());
            let mut items = Vec::new();
            let mut fresh: Vec<(String, PathBuf)> = Vec::new();
            // folders whose new files are recorded as direct play's (ES-DE, RetroArch cores)
            let mut watched: Vec<(PathBuf, BTreeSet<String>)> = Vec::new();
            for e in &report.emulators {
                let (true, EmuState::Missing { .. }, Some((dir, exe))) = (ticks.contains(&e.id), &e.state, &e.install) else { continue };
                let Some(spec) = update::source_for(&e.id, &opts.sources) else { continue };
                let _ = tx.send(Event::Progress(Progress { current: format!("Looking up {}…", e.id), ..Default::default() }));
                match update::fresh_item(&http, &e.id, dir, exe, spec) {
                    Ok(item) => {
                        fresh.push((e.id.clone(), dir.clone()));
                        items.push(item);
                    }
                    Err(err) => log(format!("{}: {err:#}", e.id)),
                }
            }
            if ticks.contains(DL_ESDE) {
                if let Some(spec) = update::source_for(update::ESDE_ID, &opts.sources) {
                    match update::fresh_item(&http, update::ESDE_ID, &report.es_root, "ES-DE.exe", spec) {
                        Ok(item) => {
                            watched.push((report.es_root.clone(), play::listing(&report.es_root)));
                            items.push(item);
                        }
                        Err(err) => log(format!("ES-DE: {err:#}")),
                    }
                }
            }
            if ticks.contains(DL_CORES) {
                let ra = report.emulators.iter().find_map(|e| match &e.state {
                    EmuState::Ready { exe } | EmuState::NeedsRule { exe } if e.id == "RETROARCH" => exe.parent().map(Path::to_path_buf),
                    _ => None,
                });
                if let Some(ra) = ra {
                    match update::missing_cores_item(&http, &ra, &report.missing_cores) {
                        Ok(item) if !item.cores.is_empty() => {
                            // a RetroArch downloaded here is removed as a whole instead
                            if !fresh.iter().any(|(_, d)| eq_ci(d, &ra)) {
                                watched.push((ra.join("cores"), play::listing(&ra.join("cores"))));
                            }
                            items.push(item);
                        }
                        Ok(_) => log("RetroArch: the buildbot has none of the missing cores".into()),
                        Err(err) => log(format!("RetroArch cores: {err:#}")),
                    }
                }
            }
            http.save_cache();
            let mut report = update::install(&items, &opts, &c, &tx);
            // write down what came, so Deploy leaves it alone and Turn off can remove it
            let st = m.play.get_or_insert_with(Default::default);
            for (id, dir) in &fresh {
                if dir.is_dir() {
                    if let Some(rel) = crate::paths::strip_prefix_ci(dir, &pkg) {
                        st.added.insert(id.clone(), rel);
                    }
                }
            }
            for (dir, before) in &watched {
                play::record_new(&pkg, st, dir, before);
            }
            if !report.cancelled {
                match play::enable(&pkg, &mut m, &play::Options { make_portable: portable }) {
                    Ok(lines) => lines.into_iter().for_each(log),
                    Err(e) => report.failed.push(format!("{e:#}")),
                }
            }
            if let Err(e) = manifest::save(&pkg, &m) {
                report.failed.push(format!("Could not save the manifest: {e:#}"));
            }
            let _ = tx.send(Event::Done(report));
        });
        self.begin_copy_job(rx, cancel, After::PlayChanged, "Downloading…");
    }

    fn start_es_de(&mut self) {
        let Some(mut m) = self.manifest.clone() else { return };
        match play::launch(&self.pkg_path(), &mut m) {
            Ok(lines) => {
                for l in lines {
                    self.log(l);
                }
                self.status = "ES-DE started from the drive".into();
                self.manifest = Some(m);
            }
            Err(e) => self.status = format!("Could not start ES-DE: {e:#}"),
        }
    }

    pub(super) fn ui_play(&mut self, ctx: &egui::Context) {
        let mut open = self.play_open;
        let busy = self.job.is_some();
        let on = self.play_on();
        let report = self.play_report.clone();
        let touched: Vec<(PathBuf, bool)> = self
            .manifest
            .as_ref()
            .and_then(|m| m.play.as_ref())
            .map(|p| p.touched.values().filter(|t| !t.is_dir).map(|t| (t.rel.clone(), t.created)).collect())
            .unwrap_or_default();
        let has_added = self.manifest.as_ref().and_then(|m| m.play.as_ref()).is_some_and(|p| !p.added.is_empty());
        let (mut enable, mut start, mut recheck, mut off, mut download) = (false, false, false, false, false);
        egui::Window::new("Direct play")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(760.0)
            .default_height(660.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if on {
                        ui.label(RichText::new("Direct play is on").strong().color(GOOD));
                    } else {
                        ui.label(RichText::new("Direct play is off").strong());
                    }
                    ui.label(RichText::new("ES-DE and the emulators run straight from this drive, on any Windows PC.").weak());
                });
                ui.label(
                    RichText::new("Export, Deploy and Bring back keep working as before. Turn off puts the drive back as it was.").small().weak(),
                );
                let Some(r) = &report else {
                    ui.label(if busy { "Checking the drive…" } else { "Press Check again to look at the drive." });
                    return;
                };
                ui.label(RichText::new(format!("ES-DE on the drive: {}", r.es_root.display())).small().weak());
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(470.0).show(ui, |ui| {
                    if let Some(b) = &r.blocker {
                        ui.colored_label(BAD, b);
                        let esde_source = update::source_for(update::ESDE_ID, &self.cfg.update_sources).is_some();
                        if esde_source && !r.es_root.join("ES-DE.exe").is_file() {
                            let mut t = self.play_download.contains(DL_ESDE);
                            if ui.checkbox(&mut t, "Download ES-DE (portable) onto the drive").changed() {
                                toggle(&mut self.play_download, DL_ESDE, t);
                            }
                        }
                        return;
                    }
                    for l in &r.general {
                        ui.colored_label(level_color(l.level), &l.text);
                    }
                    ui.add_space(6.0);
                    ui.strong("Emulators");
                    egui::Grid::new("play_emus").striped(true).num_columns(4).show(ui, |ui| {
                        for e in &r.emulators {
                            ui.label(emu_name(&e.id));
                            ui.label(RichText::new(shorten(&e.systems.join(", "), 40)).small()).on_hover_text(e.systems.join(", "));
                            let (text, color) = match &e.state {
                                EmuState::Ready { .. } => ("ready".to_string(), GOOD),
                                EmuState::NeedsRule { .. } => ("will be registered with ES-DE".to_string(), ACCENT),
                                EmuState::Missing { .. } => (play::state_text(&e.state), WARN),
                                EmuState::External { .. } => (play::state_text(&e.state), TEXT),
                            };
                            let tip = match &e.state {
                                EmuState::Ready { exe } | EmuState::NeedsRule { exe } => exe.display().to_string(),
                                EmuState::Missing { .. } => match &e.install {
                                    Some((dir, _)) => format!("It goes in {}", dir.display()),
                                    None => "ES-DE has no folder for it below Emulators; install it yourself".into(),
                                },
                                EmuState::External { .. } => String::new(),
                            };
                            let resp = ui.label(RichText::new(text).color(color));
                            if !tip.is_empty() {
                                resp.on_hover_text(tip);
                            }
                            ui.horizontal(|ui| {
                                match (&e.state, &e.portable) {
                                    (EmuState::Missing { source: true, .. }, _) if e.install.is_some() => {
                                        let mut t = self.play_download.contains(&e.id);
                                        if ui.checkbox(&mut t, "download").changed() {
                                            toggle(&mut self.play_download, &e.id, t);
                                        }
                                    }
                                    (_, Portable::No { marker, .. }) => {
                                        let mut t = self.play_portable.contains(&e.id);
                                        if ui
                                            .checkbox(&mut t, "switch to portable mode")
                                            .on_hover_text(format!(
                                                "Creates {marker} in its folder so it keeps settings and saves on the drive. \
                                                 They start out empty there: settings and BIOS files from this PC's user profile are not copied."
                                            ))
                                            .changed()
                                        {
                                            toggle(&mut self.play_portable, &e.id, t);
                                        }
                                    }
                                    (EmuState::Ready { .. } | EmuState::NeedsRule { .. }, Portable::Unknown) => {
                                        ui.label(RichText::new("portable mode unknown").small().weak());
                                    }
                                    _ => {}
                                }
                            });
                            ui.end_row();
                        }
                    });
                    if !r.missing_cores.is_empty() {
                        ui.add_space(4.0);
                        ui.horizontal_wrapped(|ui| {
                            let mut t = self.play_download.contains(DL_CORES);
                            let n = r.missing_cores.len();
                            let label = if n == 1 { "download 1 RetroArch core that isn't on the drive".to_string() } else { format!("download {n} RetroArch cores that aren't on the drive") };
                            if ui.checkbox(&mut t, label).on_hover_text(r.missing_cores.join("\n")).changed() {
                                toggle(&mut self.play_download, DL_CORES, t);
                            }
                        });
                    }
                    if !r.configs.is_empty() {
                        egui::CollapsingHeader::new(format!("Emulator settings ({})", r.configs.len())).id_salt("play_cfgs").show(ui, |ui| {
                            ui.label(RichText::new("Folders inside these files still point at the source PC. Change them in the emulator while it runs from the drive.").small().weak());
                            for l in &r.configs {
                                ui.colored_label(level_color(l.level), RichText::new(&l.text).small());
                            }
                        });
                    }
                    if !touched.is_empty() {
                        egui::CollapsingHeader::new(format!("Changed on this drive ({})", touched.len())).id_salt("play_touched").show(ui, |ui| {
                            ui.label(RichText::new("Turn off deletes the created files and puts the originals of the changed ones back.").small().weak());
                            for (rel, created) in &touched {
                                ui.label(RichText::new(format!("{}  {}", if *created { "created" } else { "changed" }, rel.display())).small());
                            }
                        });
                    }
                });
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    let can = !busy && r.blocker.is_none();
                    let label = if on { "Apply again" } else { "Enable direct play" };
                    if ui.add_enabled(can, egui::Button::new(RichText::new(label).strong())).clicked() {
                        enable = true;
                    }
                    if ui.add_enabled(!busy && on, egui::Button::new("Start ES-DE")).on_hover_text("Applies the fixes again, then starts ES-DE from the drive").clicked() {
                        start = true;
                    }
                    let n = self.play_download.len();
                    if n > 0 {
                        let text = if on { format!("Download {n} selected") } else { format!("Download {n} selected and enable") };
                        if ui.add_enabled(!busy, egui::Button::new(text)).clicked() {
                            download = true;
                        }
                    }
                    if ui.add_enabled(!busy, egui::Button::new("Check again")).clicked() {
                        recheck = true;
                    }
                    if ui.button("Copy report").clicked() {
                        ui.ctx().copy_text(play::report_text(r));
                        self.status = "Report copied".into();
                    }
                    if ui.add_enabled(!busy && on, egui::Button::new("Turn off…")).clicked() {
                        off = true;
                    }
                });
            });
        self.play_open = open;
        if enable {
            self.start_play_enable();
        } else if start {
            self.start_es_de();
        } else if download {
            self.start_play_download();
        } else if recheck {
            self.start_play_check();
        } else if off {
            self.play_remove_added = false;
            self.play_confirm_off = true;
        }
        if self.play_confirm_off {
            self.ui_play_confirm_off(ctx, &touched, has_added);
        }
    }

    fn ui_play_confirm_off(&mut self, ctx: &egui::Context, touched: &[(PathBuf, bool)], has_added: bool) {
        let mut go = false;
        let mut cancel = false;
        egui::Window::new("Turn off direct play?")
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let created = touched.iter().filter(|t| t.1).count();
                ui.label(format!(
                    "{} changed files get their originals back and {created} files that direct play created are deleted.",
                    touched.len() - created
                ));
                ui.colored_label(WARN, "Settings changed in ES-DE or RetroArch while playing from the drive are in those files and are lost. Saves stay.");
                if has_added {
                    ui.checkbox(&mut self.play_remove_added, "Also remove the emulators downloaded for direct play");
                }
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Turn off").strong()).clicked() {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if go || cancel {
            self.play_confirm_off = false;
        }
        if go {
            self.start_play_disable();
        }
    }
}

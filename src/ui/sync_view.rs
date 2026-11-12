//! Export and Deploy screens: paths, systems table, side panel, fuel cans, bottom bar.

use super::*;

impl App {
    pub(super) fn ui_export_paths(&mut self, ui: &mut egui::Ui) {
        let mut action = None;
        egui::Grid::new("export_paths").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("Profile").on_hover_text("A named selection and destination, e.g. one per handheld");
            ui.horizontal(|ui| {
                if let Some((old, text)) = &mut self.profile_edit {
                    let r = ui.add(egui::TextEdit::singleline(text).desired_width(220.0).hint_text("e.g. ROG Ally"));
                    r.request_focus();
                    let name = text.trim().to_string();
                    let taken = self.cfg.profiles.contains_key(&name) && old.as_deref() != Some(name.as_str());
                    let ok = !name.is_empty() && !taken;
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.add_enabled(ok, egui::Button::new(if old.is_some() { "Rename" } else { "Save" })).clicked() || (ok && enter) {
                        action = Some(ProfileAction::Commit);
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        action = Some(ProfileAction::Cancel);
                    }
                    if taken {
                        ui.colored_label(WARN, "a profile with this name exists");
                    }
                } else {
                    let active = self.cfg.active_profile.clone();
                    egui::ComboBox::from_id_salt("profile")
                        .width(220.0)
                        .selected_text(active.as_deref().unwrap_or("(none)"))
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(active.is_none(), "(none)").clicked() && active.is_some() {
                                action = Some(ProfileAction::Switch(None));
                            }
                            for name in self.cfg.profiles.keys() {
                                let on = active.as_deref() == Some(name.as_str());
                                if ui.selectable_label(on, name).clicked() && !on {
                                    action = Some(ProfileAction::Switch(Some(name.clone())));
                                }
                            }
                        });
                    if ui.button("New…").on_hover_text("Save the current selection and destination under a name").clicked() {
                        action = Some(ProfileAction::StartNew);
                    }
                    if active.is_some() {
                        if ui.button("Rename…").clicked() {
                            action = Some(ProfileAction::StartRename);
                        }
                        if ui.button("Delete").on_hover_text("Forget this profile; the current selection stays").clicked() {
                            action = Some(ProfileAction::Delete);
                        }
                    }
                }
            });
            ui.end_row();

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
        if let Some(a) = action {
            self.profile_action(a);
        }
    }

    pub(super) fn ui_deploy_paths(&mut self, ui: &mut egui::Ui) {
        let orig = self.deploy_cat.as_ref().map(|c| c.roots.clone());
        let mut profile_changed = false;
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

            let names: Vec<String> = self.manifest.as_ref().map(|m| m.profiles.keys().cloned().collect()).unwrap_or_default();
            if !names.is_empty() && self.mode == Mode::Deploy {
                ui.label("Profile");
                let before = self.deploy_profile.clone();
                egui::ComboBox::from_id_salt("deploy_profile")
                    .width(220.0)
                    .selected_text(self.deploy_profile.as_deref().unwrap_or("Everything on the drive"))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.deploy_profile, None, "Everything on the drive");
                        for n in names {
                            ui.selectable_value(&mut self.deploy_profile, Some(n.clone()), n);
                        }
                    });
                ui.label(RichText::new("ticks what was exported for that device").weak());
                ui.end_row();
                if self.deploy_profile != before {
                    profile_changed = true;
                }
            }

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
        if profile_changed {
            self.apply_defaults();
        }
    }

    pub(super) fn dest_volume(&self) -> Option<String> {
        let p = match self.mode {
            Mode::Export => &self.dest,
            Mode::Deploy | Mode::BringBack => &self.t_es,
            Mode::Update => &self.es_root,
        };
        volume_root(&normalize(Path::new(p.trim())))
    }

    /// One reading per destination volume: exact after a fresh Analyze, otherwise an
    /// estimate from the selection. While copying, only the remaining bytes are added,
    /// because the drive's used space already includes what was copied so far.
    pub(super) fn tank_readings(&self) -> Vec<TankReading> {
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

    pub(super) fn ui_tanks(&self, ui: &mut egui::Ui) {
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

    pub(super) fn ui_side(&mut self, ui: &mut egui::Ui, busy: bool) {
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

    pub(super) fn ui_systems(&mut self, ui: &mut egui::Ui, busy: bool) {
        let Some(cat) = self.cat() else {
            ui.centered_and_justified(|ui| {
                ui.label(match self.mode {
                    Mode::Export => "Pick your ES-DE folder and press Scan.",
                    Mode::Deploy | Mode::Update | Mode::BringBack => "Pick the drive holding an EMU Assistant export and press Load.",
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
        let mut open_games = None;

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                egui::Grid::new("systems").striped(true).num_columns(cols).spacing([14.0, 4.0]).show(ui, |ui| {
                    ui.label("");
                    ui.strong("System");
                    ui.strong("Games").on_hover_text("Click to pick single games of a system");
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
                        let picked = self.picked_games(s);
                        let text = match (&s.games, picked) {
                            (_, Some((list, p))) => RichText::new(format!("{} / {} games", list.picked_count(p), list.games.len())).color(ACCENT),
                            (Some(list), None) => RichText::new(format!("{} games", list.games.len())),
                            (None, None) => RichText::new(format!("{} files", s.rom_files)),
                        };
                        if ui.small_button(text).on_hover_text("Pick which games of this system to copy").clicked() {
                            open_games = Some(s.name.clone());
                        }
                        ui.label(human_bytes(self.rom_bytes(s).0));
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
        if let Some(name) = open_games {
            self.open_games(&name);
        }
    }

    pub(super) fn ui_bottom(&mut self, ui: &mut egui::Ui) {
        match self.mode {
            Mode::Update => return self.ui_update_bottom(ui),
            Mode::BringBack => return self.ui_bringback_bottom(ui),
            _ => {}
        }
        let busy = self.job.is_some();
        let fresh = self.plan_is_fresh();
        let plan = self.plan.as_ref().map(|(_, p)| p.clone());
        let mut space_issues = plan.as_ref().map(|p| space_problems(p)).unwrap_or_default();
        if let Some(p) = &plan {
            space_issues.extend(fat32_problems(p, &self.drives));
        }
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
                Mode::Deploy | Mode::Update | Mode::BringBack => "Deploy to this machine",
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
                if self.mode == Mode::Deploy {
                    ui.label(RichText::new("Saves made on this machine can be copied back with the Bring back tab.").small().weak());
                }
            }
        }

        if let (false, Some(r)) = (busy, &self.report) {
            if !r.failed.is_empty() {
                ui.colored_label(BAD, format!("⚠ The last run had {} errors. Open the log below for details.", r.failed.len()));
            }
        }
        self.ui_progress_and_log(ui, busy, "file");
    }

    pub(super) fn ui_confirm_prune(&mut self, ctx: &egui::Context) {
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
}

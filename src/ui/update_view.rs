//! Update programs screen and the Restore window.

use super::*;

impl App {
    pub(super) fn ui_update_paths(&mut self, ui: &mut egui::Ui) {
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

    pub(super) fn ui_update_table(&mut self, ui: &mut egui::Ui, busy: bool) {
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

    pub(super) fn ui_update_side(&mut self, ui: &mut egui::Ui) {
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

    pub(super) fn ui_update_bottom(&mut self, ui: &mut egui::Ui) {
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

    pub(super) fn ui_restore(&mut self, ctx: &egui::Context) {
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
                    "Tick a backup to undo that update or bring-back completely, or expand it and tick single files. \
                     Restoring puts the old files back and deletes the files that were added.",
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
                            let what = match meta.kind {
                                BackupKind::Saves => "saves brought back".to_string(),
                                BackupKind::Update => format!("{} to {}", meta.old_version, meta.new_version),
                            };
                            let (replaced, added) = match meta.kind {
                                BackupKind::Saves => ("older saves", "new"),
                                BackupKind::Update => ("replaced", "added"),
                            };
                            ui.label(format!(
                                "{what}   {}   {} {replaced}, {} {added}   {}",
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

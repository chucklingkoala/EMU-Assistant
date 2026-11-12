//! Top bar, progress bar with the log, and the quit confirmation.

use super::*;

impl App {
    pub(super) fn ui_top(&mut self, ui: &mut egui::Ui, busy: bool) {
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
                if ui
                    .selectable_label(self.mode == Mode::BringBack, "  Bring back  ")
                    .on_hover_text("Copy saves made on the handheld back to the main machine")
                    .clicked()
                    && self.mode != Mode::BringBack
                {
                    self.mode = Mode::BringBack;
                    self.plan = None;
                    self.progress = None;
                    self.report = None;
                    if self.pkg_root.trim().is_empty() {
                        self.pkg_root = self.dest.clone();
                    }
                    if self.deploy_cat.is_none() && !self.pkg_root.trim().is_empty() {
                        self.load_package();
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
                if let Some(r) = &self.app_release {
                    if ui.button(RichText::new(format!("Update to {}", r.version)).color(ACCENT)).on_hover_text("A newer EMU Assistant is available").clicked() {
                        self.app_window = true;
                    }
                }
                ui.label(RichText::new(format!("v{}", app_update::VERSION)).weak());
                if cog.clicked() {
                    self.settings_open = !self.settings_open;
                    if self.settings_open {
                        self.settings_tab = match self.mode {
                            Mode::Export => SettingsTab::Export,
                            Mode::Deploy => SettingsTab::Deploy,
                            Mode::Update => SettingsTab::Updates,
                            Mode::BringBack => SettingsTab::General,
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
            Mode::BringBack => self.ui_bringback_paths(ui),
        });
        ui.add_space(6.0);
    }

    pub(super) fn ui_progress_and_log(&mut self, ui: &mut egui::Ui, busy: bool, unit: &str) {
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
            if ui.small_button("Copy log").on_hover_text("Copy the whole log to the clipboard").clicked() {
                ui.ctx().copy_text(self.log.join("\n"));
            }
            let row_h = ui.text_style_height(&egui::TextStyle::Small);
            egui::ScrollArea::vertical().max_height(160.0).stick_to_bottom(true).id_salt("log_scroll").show_rows(
                ui,
                row_h,
                self.log.len(),
                |ui, range| {
                    for l in &self.log[range] {
                        // one row per line: long lines are cut with … and shown in full on hover
                        ui.add(egui::Label::new(RichText::new(l).small()).truncate());
                    }
                },
            );
        });
        ui.add_space(4.0);
    }

    pub(super) fn ui_confirm_close(&mut self, ctx: &egui::Context) {
        if self.job.is_none() {
            self.confirm_close = false;
            return;
        }
        egui::Window::new("A task is still running")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("Quitting now cancels it. A copy can be continued later: files that were already copied are kept.");
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Cancel the task and quit").color(BAD)).clicked() {
                        if let Some(j) = &self.job {
                            j.cancel();
                        }
                        self.quit_when_idle = true;
                        self.confirm_close = false;
                        self.status = "Stopping…".into();
                    }
                    if ui.button("Keep running").clicked() {
                        self.confirm_close = false;
                    }
                });
            });
    }
}

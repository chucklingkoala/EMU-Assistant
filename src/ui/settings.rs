//! The Settings window, including Update sources.

use super::*;

impl App {
    pub(super) fn ui_settings(&mut self, ctx: &egui::Context) {
        let mut open = self.settings_open;
        let mut changed = false;
        let (mut check_now, mut open_app) = (false, false);
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
                        ui.separator();
                        check(ui, &mut cfg.check_app_updates, true, "Check for EMU Assistant updates at startup",
                            "Looks up the latest EMU Assistant release on GitHub each time the program starts.");
                        ui.horizontal(|ui| {
                            ui.label(format!("EMU Assistant {}", app_update::VERSION));
                            if ui.add_enabled(self.app_rx.is_none(), egui::Button::new("Check now")).clicked() {
                                check_now = true;
                            }
                            if let Some((text, bad)) = &self.app_check_note {
                                ui.label(RichText::new(text).color(if *bad { BAD } else { Color32::GRAY }));
                            }
                            if self.app_release.is_some() && ui.link("Update…").clicked() {
                                open_app = true;
                            }
                        });
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
        if check_now {
            self.start_app_check();
        }
        if open_app {
            self.app_window = true;
        }
    }

    /// The Update sources tab. Returns true when something changed.
    /// `focus` = (program to outline, still needs scrolling to).
    pub(super) fn ui_sources(ui: &mut egui::Ui, cfg: &mut Config, items: &mut [UpdateItem], focus: &mut Option<(String, bool)>) -> bool {
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
}

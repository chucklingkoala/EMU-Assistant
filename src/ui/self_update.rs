//! EMU Assistant's own update window and the older-copies prompt.

use super::*;

impl App {
    pub(super) fn ui_app_update(&mut self, ctx: &egui::Context) {
        let Some(rel) = self.app_release.clone() else {
            self.app_window = false;
            return;
        };
        let busy = self.job.is_some();
        let updating = matches!(self.job, Some(Job::Copy { after: After::AppUpdated, .. }));
        let (mut open, mut start, mut later) = (true, false, false);
        egui::Window::new("Update EMU Assistant")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .show(ctx, |ui| {
                let date = update::version::parse_ts(&rel.published).map(|t| format!(" ({})", update::version::date_text(t))).unwrap_or_default();
                ui.label(format!("Installed {}, latest {}{date}", app_update::VERSION, rel.version));
                if !rel.notes.is_empty() {
                    ui.separator();
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                        ui.label(&rel.notes);
                    });
                }
                ui.separator();
                if let Some(e) = &self.app_error {
                    ui.colored_label(BAD, e);
                }
                if let (true, Some(p)) = (updating, &self.progress) {
                    let frac = if p.total_bytes > 0 { p.done_bytes as f32 / p.total_bytes as f32 } else { 0.0 };
                    ui.add(egui::ProgressBar::new(frac).text(format!("{} / {}", human_bytes(p.done_bytes), human_bytes(p.total_bytes))));
                }
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!busy, egui::Button::new(RichText::new("Update and restart").strong()))
                        .on_disabled_hover_text("Finish or cancel the current task first")
                        .clicked()
                    {
                        start = true;
                    }
                    if !rel.page.is_empty() && ui.button("Release page").clicked() {
                        let _ = std::process::Command::new("explorer").arg(&rel.page).spawn();
                    }
                    if ui.button("Later").clicked() {
                        later = true;
                    }
                });
                ui.label(RichText::new("Only EMUAssistant.exe is replaced. Your settings and ES-DE setup aren't touched.").small().weak());
            });
        if start {
            self.start_app_update();
        }
        if !open || later {
            self.app_window = false;
        }
    }

    pub(super) fn ui_old_copies(&mut self, ctx: &egui::Context) {
        let busy = self.job.is_some();
        let (mut replace, mut dismiss) = (false, false);
        egui::Window::new("Update EMU Assistant on this machine?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("This machine has an older EMU Assistant. Replace it with this version ({})?", app_update::VERSION));
                for (c, on) in &mut self.old_copies {
                    let from = c.version.as_deref().unwrap_or("unknown version");
                    ui.checkbox(on, format!("{}   {from} to {}", c.path.display(), app_update::VERSION));
                }
                ui.horizontal(|ui| {
                    let any = self.old_copies.iter().any(|(_, on)| *on);
                    if ui.add_enabled(any && !busy, egui::Button::new(RichText::new("Replace").strong())).clicked() {
                        replace = true;
                    }
                    if ui.button("Not now").clicked() {
                        dismiss = true;
                    }
                });
            });
        if replace {
            self.replace_old_copies();
        } else if dismiss {
            self.old_copies.clear();
            self.old_copies_dismissed = true;
        }
    }
}

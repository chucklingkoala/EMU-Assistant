//! The game picker window.

use super::*;

impl App {
    pub(super) fn ui_games(&mut self, ctx: &egui::Context) {
        let Some(name) = self.games_open.clone() else { return };
        let Some(cat) = self.cat() else {
            self.games_open = None;
            return;
        };
        let Some((s, list)) = cat.systems.iter().find(|s| s.name == name).and_then(|s| Some((s, s.games.as_ref()?))) else {
            self.games_open = None;
            return;
        };
        let busy = self.job.is_some();
        let all: BTreeSet<String> = list.games.iter().map(|g| g.key.clone()).collect();
        let mut picked = match (self.sel.systems.contains(&name), self.sel.games.get(&name)) {
            (false, _) => BTreeSet::new(),
            (true, Some(p)) => p.intersection(&all).cloned().collect(),
            (true, None) => all.clone(),
        };
        let before = picked.clone();
        let mut open = true;
        let mut done = false;
        egui::Window::new(format!("Games: {}", s.fullname))
            .id(egui::Id::new("games_window"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(760.0)
            .default_height(520.0)
            .show(ctx, |ui| {
                let (_, bytes) = list.picked(&picked);
                ui.label(format!(
                    "{} of {} games picked, {} with their updates/DLC and the shared files",
                    picked.len(),
                    list.games.len(),
                    human_bytes(bytes)
                ));
                let filter = self.games_filter.to_lowercase();
                let visible: Vec<_> = list
                    .games
                    .iter()
                    .filter(|g| filter.is_empty() || g.key.to_lowercase().contains(&filter) || g.extras.iter().any(|e| e.to_lowercase().contains(&filter)))
                    .collect();
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Filter");
                        ui.add(egui::TextEdit::singleline(&mut self.games_filter).desired_width(220.0));
                        if ui.button("Select all").on_hover_text("With every game ticked, games added later are copied too").clicked() {
                            picked.extend(visible.iter().map(|g| g.key.clone()));
                        }
                        if ui.button("Select none").clicked() {
                            for g in &visible {
                                picked.remove(&g.key);
                            }
                        }
                    });
                });
                ui.separator();
                // Done goes in a bottom panel first, so the list gets exactly the space that is
                // left and can't push the button out of the window (the theme's button padding
                // made a hand-computed footer height come out a few pixels short)
                egui::TopBottomPanel::bottom("games_footer")
                    .frame(egui::Frame::none().inner_margin(egui::Margin { top: 6.0, ..Default::default() }))
                    .show_inside(ui, |ui| {
                        if ui.button("Done").clicked() {
                            done = true;
                        }
                    });
                let row_h = ui.spacing().interact_size.y;
                let gap = ui.spacing().item_spacing.y;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, visible.len(), |ui, range| {
                        egui::Grid::new("games")
                            .striped(true)
                            .num_columns(4)
                            .spacing([14.0, gap])
                            .min_row_height(row_h)
                            .start_row(range.start)
                            .show(ui, |ui| {
                                for g in &visible[range] {
                                    let mut on = picked.contains(&g.key);
                                    if ui.add_enabled(!busy, egui::Checkbox::without_text(&mut on)).changed() {
                                        toggle(&mut picked, &g.key, on);
                                    }
                                    ui.label(&g.key).on_hover_text(g.paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n"));
                                    ui.label(human_bytes(g.bytes));
                                    if g.extras.is_empty() {
                                        ui.label("");
                                    } else {
                                        ui.label(RichText::new(format!("+ {} updates/DLC", g.extras.len())).small().color(ACCENT))
                                            .on_hover_text(g.extras.join("\n"));
                                    }
                                    ui.end_row();
                                }
                            });
                    });
            });
        if picked != before {
            self.set_games(&name, picked, all.len());
        }
        if !open || done {
            self.games_open = None;
        }
    }
}

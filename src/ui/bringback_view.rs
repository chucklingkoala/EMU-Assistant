//! Bring back screen: new and newer files in the emulator folders, grouped by emulator
//! and kind, to copy to the drive (on the handheld) or home from it (on the main machine).

use super::*;
use crate::bringback::{Change, Class, Item};

/// One line of the list.
enum Row {
    /// An emulator: its items are `start..end`.
    Emu { start: usize, end: usize },
    /// One kind of file of an emulator.
    Section { start: usize, end: usize, class: Class },
    File(usize),
}

/// What a click in the list changes, applied after drawing.
enum ListAction {
    Toggle(String),
    Tick(std::ops::Range<usize>, bool),
}

fn group_key(emu: &str, class: Option<Class>) -> String {
    match class {
        Some(c) => format!("{emu}/{c:?}"),
        None => emu.to_string(),
    }
}

fn bytes(items: &[Item], r: std::ops::Range<usize>) -> u64 {
    items[r].iter().map(|i| i.size).sum()
}

impl App {
    fn bb_open(&self, key: &str, default: bool) -> bool {
        default != self.bb_toggled.contains(key)
    }

    /// The visible rows: emulators, their sections, and the files of open sections.
    fn bb_rows(&self, items: &[Item]) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut i = 0;
        while i < items.len() {
            let emu = &items[i].emu;
            let end = items[i..].iter().position(|x| &x.emu != emu).map_or(items.len(), |n| i + n);
            rows.push(Row::Emu { start: i, end });
            if self.bb_open(&group_key(emu, None), true) {
                let mut s = i;
                while s < end {
                    let class = items[s].class;
                    let send = items[s..end].iter().position(|x| x.class != class).map_or(end, |n| s + n);
                    rows.push(Row::Section { start: s, end: send, class });
                    if self.bb_open(&group_key(emu, Some(class)), class == Class::Save) {
                        rows.extend((s..send).map(Row::File));
                    }
                    s = send;
                }
            }
            i = end;
        }
        rows
    }

    /// A tri-state box for `range`; returns the new state when clicked.
    fn bb_range_box(&self, ui: &mut egui::Ui, range: std::ops::Range<usize>, enabled: bool) -> Option<bool> {
        let ticked = range.clone().filter(|i| self.bb_ticked.contains(i)).count();
        let mut all = ticked == range.len() && ticked > 0;
        let partial = ticked > 0 && !all;
        ui.add_enabled(enabled, egui::Checkbox::without_text(&mut all).indeterminate(partial)).changed().then_some(all)
    }

    pub(super) fn ui_bringback_list(&mut self, ui: &mut egui::Ui, busy: bool) {
        let Some(found) = self.bb_found.clone() else {
            ui.centered_and_justified(|ui| {
                ui.label(if self.deploy_cat.is_none() {
                    "Pick the drive holding an EMU Assistant export and press Load."
                } else {
                    "Press Find changes to list the new and newer files in the emulator folders."
                });
            });
            return;
        };
        let items = &found.items;
        ui.horizontal(|ui| {
            ui.heading("Changed files");
            let ticked: u64 = self.bb_ticked.iter().filter_map(|&i| items.get(i)).map(|i| i.size).sum();
            ui.label(format!("{} of {} ticked, {}", self.bb_ticked.len(), items.len(), human_bytes(ticked)));
        });
        ui.add_enabled_ui(!busy && !items.is_empty(), |ui| {
            ui.horizontal(|ui| {
                if ui.button("Tick saves only").clicked() {
                    self.bb_ticked = items.iter().enumerate().filter(|(_, i)| i.class == Class::Save).map(|(n, _)| n).collect();
                }
                if ui.button("Tick all").clicked() {
                    self.bb_ticked = (0..items.len()).collect();
                }
                if ui.button("Untick all").clicked() {
                    self.bb_ticked.clear();
                }
            });
        });
        ui.separator();
        if items.is_empty() {
            ui.label(RichText::new("Everything in the emulator folders is the same on both sides, or older here.").weak());
            return;
        }

        let rows = self.bb_rows(items);
        let mut actions = Vec::new();
        let row_h = ui.spacing().interact_size.y;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_h, rows.len(), |ui, range| {
            for row in &rows[range] {
                ui.horizontal(|ui| {
                    ui.set_height(row_h);
                    match *row {
                        Row::Emu { start, end } => {
                            let emu = &items[start].emu;
                            let k = group_key(emu, None);
                            let open = self.bb_open(&k, true);
                            if ui.small_button(if open { "▼" } else { "▶" }).clicked() {
                                actions.push(ListAction::Toggle(k));
                            }
                            if let Some(on) = self.bb_range_box(ui, start..end, !busy) {
                                actions.push(ListAction::Tick(start..end, on));
                            }
                            ui.strong(emu_name(emu));
                            ui.label(RichText::new(format!("{} files, {}", end - start, human_bytes(bytes(items, start..end)))).weak());
                        }
                        Row::Section { start, end, class } => {
                            ui.add_space(24.0);
                            let k = group_key(&items[start].emu, Some(class));
                            let open = self.bb_open(&k, class == Class::Save);
                            if ui.small_button(if open { "▼" } else { "▶" }).clicked() {
                                actions.push(ListAction::Toggle(k));
                            }
                            if let Some(on) = self.bb_range_box(ui, start..end, !busy) {
                                actions.push(ListAction::Tick(start..end, on));
                            }
                            let text = RichText::new(format!("{}  ({}, {})", class.label(), end - start, human_bytes(bytes(items, start..end))));
                            ui.label(if class == Class::Save { text.color(ACCENT) } else { text });
                        }
                        Row::File(i) => {
                            ui.add_space(72.0);
                            let it = &items[i];
                            let mut on = self.bb_ticked.contains(&i);
                            if ui.add_enabled(!busy, egui::Checkbox::without_text(&mut on)).changed() {
                                actions.push(ListAction::Tick(i..i + 1, on));
                            }
                            let (word, color) = match it.change {
                                Change::New => ("new", GOOD),
                                Change::Newer => ("newer", WARN),
                            };
                            ui.add_sized([44.0, row_h], egui::Label::new(RichText::new(word).small().color(color)));
                            ui.add_sized([72.0, row_h], egui::Label::new(RichText::new(human_bytes(it.size)).small()));
                            ui.add(egui::Label::new(it.rel.display().to_string()).truncate())
                                .on_hover_text(format!("{}\n→ {}", it.src.display(), it.dst.display()));
                        }
                    }
                });
            }
        });
        for a in actions {
            match a {
                ListAction::Toggle(k) => {
                    if !self.bb_toggled.remove(&k) {
                        self.bb_toggled.insert(k);
                    }
                }
                ListAction::Tick(r, on) => {
                    for i in r {
                        if on {
                            self.bb_ticked.insert(i);
                        } else {
                            self.bb_ticked.remove(&i);
                        }
                    }
                }
            }
        }
    }

    /// The machine and drive named for the direction choice.
    fn bb_names(&self) -> (String, String) {
        let here = std::env::var("COMPUTERNAME").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "this machine".into());
        let drive = self.pkg_root.trim();
        (here, if drive.is_empty() { "the drive".into() } else { drive.to_string() })
    }

    pub(super) fn ui_bringback_paths(&mut self, ui: &mut egui::Ui) {
        self.ui_deploy_paths(ui);
        if self.deploy_cat.is_none() {
            return;
        }
        let (here, drive) = self.bb_names();
        let before = self.bb_dir;
        ui.horizontal(|ui| {
            ui.label("Direction");
            ui.radio_value(&mut self.bb_dir, Direction::ToDrive, format!("{here} → {drive}"))
                .on_hover_text("On the handheld: copy saves made here to the drive");
            ui.radio_value(&mut self.bb_dir, Direction::ToMachine, format!("{drive} → {here}"))
                .on_hover_text("On the main machine: copy the saves on the drive into the emulator folders here");
        });
        if self.bb_dir != before {
            self.bb_found = None;
            self.bb_ticked.clear();
        }
    }

    pub(super) fn ui_bringback_side(&mut self, ui: &mut egui::Ui, busy: bool) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(6.0);
            ui.heading("Bring back");
            ui.label(match self.bb_dir {
                Direction::ToDrive => {
                    "Copies files that are new or newer in the emulator folders on this machine to the drive. \
                     Then plug the drive into the main machine, open this tab there and bring them home."
                }
                Direction::ToMachine => {
                    "Copies files that are new or newer on the drive into the emulator folders on this machine. \
                     Every file this replaces is saved in a backup zip first."
                }
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "Saves and save states are ticked for you. Settings and other files are listed so you can pick them; \
                     caches, shaders and logs are left out. Only emulator folders are compared, never ES-DE or the ROMs.",
                )
                .small()
                .weak(),
            );
            if let Some(found) = &self.bb_found {
                ui.separator();
                ui.heading("Ticked");
                for class in [Class::Save, Class::Setting, Class::Other] {
                    let (n, b) = self
                        .bb_ticked
                        .iter()
                        .filter_map(|&i| found.items.get(i))
                        .filter(|i| i.class == class)
                        .fold((0, 0u64), |(n, b), i| (n + 1, b + i.size));
                    ui.label(format!("{}: {n} files, {}", class.label(), human_bytes(b)));
                }
            }
            if self.bb_dir == Direction::ToMachine {
                ui.separator();
                ui.heading("Safety");
                let dir = self.backup_dir();
                ui.label(RichText::new(format!("Backups: {}", dir.display())).small().weak());
                if ui.add_enabled(!busy, egui::Button::new("Restore…")).on_hover_text("Put back saves that were replaced").clicked() {
                    self.open_restore();
                }
            }
        });
    }

    pub(super) fn ui_bringback_bottom(&mut self, ui: &mut egui::Ui) {
        let busy = self.job.is_some();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy && self.deploy_cat.is_some(), egui::Button::new("Find changes"))
                .on_hover_text("Compare the emulator folders here with the drive")
                .clicked()
            {
                self.start_find();
            }
            let (n, size) = match &self.bb_found {
                Some(f) => (self.bb_ticked.len(), self.bb_ticked.iter().filter_map(|&i| f.items.get(i)).map(|i| i.size).sum()),
                None => (0, 0),
            };
            let label = format!("Bring back {n} files ({})", human_bytes(size));
            if ui.add_enabled(!busy && n > 0, egui::Button::new(RichText::new(label).strong())).clicked() {
                self.start_bring_back();
            }
            if busy && ui.button("Cancel").clicked() {
                if let Some(j) = &self.job {
                    j.cancel();
                }
            }
            ui.label(&self.status);
        });
        if let (false, Some(r)) = (busy, &self.report) {
            if !r.failed.is_empty() {
                ui.colored_label(BAD, format!("⚠ The last run had {} errors. Open the log below for details.", r.failed.len()));
            }
        }
        self.ui_progress_and_log(ui, busy, "file");
    }
}

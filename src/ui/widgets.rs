//! Small widgets and text helpers shared by the screens.

use super::*;

pub(super) fn toggle(set: &mut BTreeSet<String>, id: &str, on: bool) {
    if on {
        set.insert(id.to_string());
    } else {
        set.remove(id);
    }
}

pub(super) fn emu_name(id: &str) -> String {
    match id.strip_prefix("EXE:") {
        Some(p) => Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.into()),
        None => id.to_string(),
    }
}

pub(super) fn path_row(ui: &mut egui::Ui, label: &str, value: &mut String, was: &Path) {
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

pub(super) fn free_label(ui: &mut egui::Ui, path: &str) {
    if let Some(free) = drives::free_space(Path::new(path.trim())) {
        ui.label(RichText::new(format!("{} free", human_bytes(free))).weak());
    }
}

/// Display name of an update program id.
pub(super) fn update_label(id: &str) -> String {
    match id {
        update::ESDE_ID => "ES-DE".into(),
        "RETROARCH" => "RetroArch".into(),
        _ => emu_name(id),
    }
}

pub(super) fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
    }
}

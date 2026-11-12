//! Selection logic shared by the window and headless mode: default selections, which
//! emulators go along, how much space a selection needs and whether it fits.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::Config;
use crate::drives::DriveInfo;
use crate::fsutil::human_bytes;
use crate::games::GameList;
use crate::plan::{Plan, Selection};
use crate::scan::{Catalog, EmuEntry, SysEntry};
use crate::sync::Report;

/// The Export selection remembered in the config, limited to what the catalog has.
pub fn export_selection(cat: &Catalog, cfg: &Config) -> Selection {
    let all_globals: BTreeSet<String> = cat.globals.iter().map(|g| g.id.clone()).collect();
    Selection {
        systems: cat.systems.iter().map(|s| s.name.clone()).filter(|s| cfg.systems.contains(s)).collect(),
        emulators: BTreeSet::new(),
        globals: match &cfg.globals {
            Some(g) => g.intersection(&all_globals).cloned().collect(),
            None => all_globals,
        },
        media: cfg.media.unwrap_or(true),
        gamelists: cfg.gamelists.unwrap_or(true),
        exclude_cheats: cfg.exclude_cheats.unwrap_or(true),
        exclude_thumbnails: cfg.exclude_thumbnails.unwrap_or(true),
        games: cfg.games.clone(),
    }
}

/// Deploy without a profile: everything on the drive.
pub fn deploy_selection(cat: &Catalog, cfg: &Config) -> Selection {
    Selection {
        systems: cat.systems.iter().map(|s| s.name.clone()).collect(),
        emulators: BTreeSet::new(),
        globals: cat.globals.iter().map(|g| g.id.clone()).collect(),
        media: true,
        gamelists: true,
        exclude_cheats: cfg.exclude_cheats.unwrap_or(true),
        exclude_thumbnails: cfg.exclude_thumbnails.unwrap_or(true),
        games: BTreeMap::new(),
    }
}

/// Emulators referenced by the selected systems: id -> (used by default/override, system names).
pub fn relevant_emus(cat: &Catalog, sel: &Selection) -> BTreeMap<String, (bool, Vec<String>)> {
    let mut m: BTreeMap<String, (bool, Vec<String>)> = BTreeMap::new();
    for s in cat.systems.iter().filter(|s| sel.systems.contains(&s.name)) {
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

/// The emulators that go along: ticked automatically when in use, unless overridden.
pub fn effective_emus(cat: &Catalog, sel: &Selection, overrides: &BTreeMap<String, bool>) -> BTreeSet<String> {
    relevant_emus(cat, sel)
        .into_iter()
        .filter(|(id, (in_use, _))| {
            cat.emulators
                .get(id)
                .is_some_and(|e| e.copyable && overrides.get(id).copied().unwrap_or(*in_use && e.auto))
        })
        .map(|(id, _)| id)
        .collect()
}

/// Emulator size with skipped optional folders (cheats / thumbnails) left out.
pub fn emu_bytes(e: &EmuEntry, sel: &Selection) -> u64 {
    let skipped: u64 = e.optional_dirs.iter().filter(|o| sel.excludes_kind(&o.kind)).map(|o| o.bytes).sum();
    e.bytes.saturating_sub(skipped)
}

/// The game list and picked games of a system whose games are picked one by one.
pub fn picked_games<'a>(s: &'a SysEntry, sel: &'a Selection) -> Option<(&'a GameList, &'a BTreeSet<String>)> {
    Some((s.games.as_ref()?, sel.games.get(&s.name)?))
}

/// ROM bytes of the picked games, and their share of the system's games (for media).
pub fn rom_bytes(s: &SysEntry, sel: &Selection) -> (u64, f64) {
    match picked_games(s, sel) {
        Some((list, picked)) => {
            let share = list.picked_count(picked) as f64 / list.games.len().max(1) as f64;
            (list.picked(picked).1, share)
        }
        None => (s.rom_bytes, 1.0),
    }
}

/// Space one system needs with the current media / gamelist options.
pub fn system_bytes(s: &SysEntry, sel: &Selection) -> u64 {
    let (roms, share) = rom_bytes(s, sel);
    roms + if sel.media { (s.media_bytes as f64 * share) as u64 } else { 0 } + if sel.gamelists { s.gamelist_bytes } else { 0 }
}

pub fn selected_bytes(cat: &Catalog, sel: &Selection) -> u64 {
    let sys: u64 = cat.systems.iter().filter(|s| sel.systems.contains(&s.name)).map(|s| system_bytes(s, sel)).sum();
    let emu: u64 = sel.emulators.iter().filter_map(|id| cat.emulators.get(id)).map(|e| emu_bytes(e, sel)).sum();
    let glob: u64 = cat.globals.iter().filter(|g| sel.globals.contains(&g.id)).map(|g| g.bytes).sum();
    sys + emu + glob
}

/// Applies a game pick: every game means no filter (new games are copied too), none
/// unticks the system.
pub fn set_games(sel: &mut Selection, system: &str, picked: BTreeSet<String>, total: usize) {
    if picked.is_empty() {
        sel.games.remove(system);
        sel.systems.remove(system);
    } else {
        if picked.len() >= total {
            sel.games.remove(system);
        } else {
            sel.games.insert(system.to_string(), picked);
        }
        sel.systems.insert(system.to_string());
    }
}

/// Volumes without room for what the plan copies there. `free` gives a volume's free bytes.
pub fn space_problems(plan: &Plan, free: impl Fn(&str) -> Option<u64>) -> Vec<String> {
    plan.bytes_by_volume
        .iter()
        .filter_map(|(vol, need)| {
            let free = free(vol)?;
            (free < *need)
                .then(|| format!("Not enough space on {vol}: needs {}, only {} free", human_bytes(*need), human_bytes(free)))
        })
        .collect()
}

/// FAT32 can't hold files of 4 GiB or more.
pub fn fat32_problems(plan: &Plan, drives: &[DriveInfo]) -> Vec<String> {
    const FAT32_MAX: u64 = u32::MAX as u64;
    let mut out = Vec::new();
    for (vol, largest) in &plan.largest_file_by_volume {
        if *largest <= FAT32_MAX {
            continue;
        }
        let fat32 = drives.iter().any(|d| d.root.eq_ignore_ascii_case(vol) && d.fs.eq_ignore_ascii_case("FAT32"));
        if fat32 {
            out.push(format!(
                "{vol} is formatted as FAT32, which can't hold files of 4 GB or more (the largest file here is {}). Format the drive as exFAT or NTFS.",
                human_bytes(*largest)
            ));
        }
    }
    out
}

pub fn fmt_secs(s: f64) -> String {
    let s = s.max(0.0) as u64;
    match s {
        s if s >= 3600 => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
        s if s >= 60 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

pub fn report_summary(r: &Report) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{scan, tests::fake_install};
    use std::sync::atomic::AtomicBool;

    fn cat() -> (tempfile::TempDir, Catalog) {
        let tmp = tempfile::tempdir().unwrap();
        let (es, _) = fake_install(tmp.path());
        let cat = scan(&es, &AtomicBool::new(false), &mut |_| {}).unwrap();
        (tmp, cat)
    }

    #[test]
    fn export_selection_follows_the_config() {
        let (_tmp, cat) = cat();
        let cfg = Config {
            systems: ["snes".to_string(), "gone".to_string()].into(),
            globals: Some(["themes".to_string()].into()),
            media: Some(false),
            ..Default::default()
        };
        let sel = export_selection(&cat, &cfg);
        assert_eq!(sel.systems, ["snes".to_string()].into());
        assert!(sel.globals.iter().all(|g| g == "themes"));
        assert!(!sel.media && sel.gamelists && sel.exclude_cheats);

        let all = deploy_selection(&cat, &cfg);
        assert_eq!(all.systems.len(), cat.systems.len());
        assert_eq!(all.globals.len(), cat.globals.len());
    }

    #[test]
    fn emulators_follow_systems_and_overrides() {
        let (_tmp, cat) = cat();
        let mut sel = Selection::default();
        sel.systems.insert("snes".into());
        let auto = effective_emus(&cat, &sel, &BTreeMap::new());
        assert!(auto.contains("RETROARCH"), "{auto:?}");
        let off = effective_emus(&cat, &sel, &[("RETROARCH".to_string(), false)].into());
        assert!(!off.contains("RETROARCH"));
        assert!(effective_emus(&cat, &Selection::default(), &BTreeMap::new()).is_empty());
    }

    #[test]
    fn sizes_add_up() {
        let (_tmp, cat) = cat();
        let mut sel = Selection { media: true, gamelists: true, ..Default::default() };
        sel.systems.insert("snes".into());
        let snes = cat.systems.iter().find(|s| s.name == "snes").unwrap();
        assert_eq!(system_bytes(snes, &sel), snes.rom_bytes + snes.media_bytes + snes.gamelist_bytes);
        sel.media = false;
        assert_eq!(system_bytes(snes, &sel), snes.rom_bytes + snes.gamelist_bytes);
        sel.emulators = effective_emus(&cat, &sel, &BTreeMap::new());
        assert!(selected_bytes(&cat, &sel) > system_bytes(snes, &sel));
    }

    #[test]
    fn game_picks() {
        let mut sel = Selection::default();
        set_games(&mut sel, "switch", ["a".to_string()].into(), 3);
        assert!(sel.systems.contains("switch") && sel.games["switch"].len() == 1);
        set_games(&mut sel, "switch", ["a", "b", "c"].map(String::from).into(), 3);
        assert!(sel.systems.contains("switch") && !sel.games.contains_key("switch"));
        set_games(&mut sel, "switch", BTreeSet::new(), 3);
        assert!(!sel.systems.contains("switch"));
    }

    #[test]
    fn space_and_fat32() {
        let mut plan = Plan::default();
        plan.bytes_by_volume.insert(r"E:\".into(), 100);
        plan.largest_file_by_volume.insert(r"E:\".into(), 5 << 30);
        assert_eq!(space_problems(&plan, |_| Some(50)).len(), 1);
        assert!(space_problems(&plan, |_| Some(500)).is_empty());
        let drive = |fs: &str| DriveInfo { root: r"E:\".into(), label: String::new(), fs: fs.into(), ..Default::default() };
        assert_eq!(fat32_problems(&plan, &[drive("FAT32")]).len(), 1);
        assert!(fat32_problems(&plan, &[drive("exFAT")]).is_empty());
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_secs(5.0), "5s");
        assert_eq!(fmt_secs(125.0), "2m 05s");
        assert_eq!(fmt_secs(3700.0), "1h 01m");
    }
}

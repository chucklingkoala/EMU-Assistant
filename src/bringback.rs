//! Bring back: files that are new or newer on the far side of the trip, in the emulator
//! folders only. On the handheld they go from the emulator folders to the drive, on the
//! main machine from the drive to the emulator folders. Save files are ticked by default;
//! settings and other files are listed for the user to tick; caches and logs are hidden.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Result};
use filetime::FileTime;

use crate::fsutil::walk;
use crate::paths::{key, long, volume_root};
use crate::plan::{nanos, Mapper, Plan, PlanItem, MTIME_TOLERANCE_NS};
use crate::scan::Catalog;
use crate::update::archive::{self, BackupKind, BackupMeta, SavedFile};
use crate::update::{self, version};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Run on the handheld: emulator folders here -> the drive.
    ToDrive,
    /// Run on the main machine: the drive -> emulator folders here.
    ToMachine,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Only on the side it comes from.
    New,
    /// Newer on the side it comes from.
    Newer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    Save,
    Setting,
    Other,
}

impl Class {
    pub fn label(self) -> &'static str {
        match self {
            Class::Save => "Saves",
            Class::Setting => "Settings",
            Class::Other => "Other files",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    pub emu: String,
    /// Relative to the emulator folder.
    pub rel: PathBuf,
    pub src: PathBuf,
    pub dst: PathBuf,
    pub size: u64,
    pub mtime: FileTime,
    pub change: Change,
    pub class: Class,
}

#[derive(Debug, Default)]
pub struct Found {
    /// Sorted by emulator, class, path.
    pub items: Vec<Item>,
    pub warnings: Vec<String>,
}

// Folder names and file types, compared in lowercase. Folders count anywhere in the path.

/// Never listed: re-created by the emulator, or useless elsewhere.
const HIDDEN_DIRS: [&str; 7] = ["logs", "log", "shaders", "thumbnails", "cheats", "crashes", "dumps"];
const HIDDEN_EXTS: [&str; 3] = ["log", "tmp", "dmp"];
/// Save games and save states.
const SAVE_DIRS: [&str; 16] = [
    "saves", "save", "savedata", "savefiles", "states", "state", "savestates", "sstates", "statesaves", "memcards", "memcard",
    "memorycards", "ppsspp_state", "battery", "gc", "wii",
];
const SAVE_EXTS: [&str; 14] = ["srm", "sav", "state", "mcd", "mcr", "ps2", "gci", "dsv", "sra", "eep", "fla", "mpk", "rtc", "vmu"];
const SETTING_EXTS: [&str; 8] = ["ini", "cfg", "yml", "yaml", "toml", "json", "xml", "conf"];

/// Sorts a file below an emulator folder. `None`: not listed at all.
pub fn classify(rel: &Path) -> Option<Class> {
    let lower = |s: &std::ffi::OsStr| s.to_string_lossy().to_lowercase();
    let name = rel.file_name().map(lower).unwrap_or_default();
    let ext = rel.extension().map(lower).unwrap_or_default();
    let dirs: Vec<String> = rel.parent().map(|p| p.components().map(|c| lower(c.as_os_str())).collect()).unwrap_or_default();
    if dirs.iter().any(|d| d.contains("cache") || HIDDEN_DIRS.contains(&d.as_str()))
        || HIDDEN_EXTS.contains(&ext.as_str())
        || name == version::MARKER.to_lowercase()
    {
        return None;
    }
    if dirs.iter().any(|d| SAVE_DIRS.contains(&d.as_str())) || SAVE_EXTS.contains(&ext.as_str()) || name.starts_with("vmu_save") {
        return Some(Class::Save);
    }
    if SETTING_EXTS.contains(&ext.as_str()) || name == "portable.txt" {
        return Some(Class::Setting);
    }
    Some(Class::Other)
}

/// The (from, to) form of a path from the source machine, for `dir`.
fn sides(m: &Mapper, dir: Direction, orig: &Path) -> (PathBuf, PathBuf) {
    match dir {
        Direction::ToDrive => (m.dst(orig), m.src(orig)),
        Direction::ToMachine => (m.src(orig), m.dst(orig)),
    }
}

/// Lists what is new or newer in the emulator folders on the "from" side. Emulators whose
/// folder is missing on either side (not deployed here, or not on the drive) are skipped.
pub fn find(cat: &Catalog, m: &Mapper, dir: Direction, cancel: &AtomicBool, status: &mut dyn FnMut(String)) -> Result<Found> {
    let mut found = Found::default();
    for e in cat.emulators.values().filter(|e| e.copyable) {
        let Some(orig) = &e.dir else { continue };
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let (from, to) = sides(m, dir, orig);
        if !from.is_dir() || !to.is_dir() {
            continue;
        }
        status(format!("Comparing {}…", e.id));
        let ex_from: Vec<PathBuf> = e.optional_dirs.iter().map(|o| sides(m, dir, &o.path).0).collect();
        let ex_to: Vec<PathBuf> = e.optional_dirs.iter().map(|o| sides(m, dir, &o.path).1).collect();

        let mut to_files: HashMap<String, FileTime> = HashMap::new();
        let mut to_errors = Vec::new();
        walk(&to, &ex_to, true, &mut to_errors, |p, meta| {
            if meta.is_file() {
                to_files.insert(key(p), FileTime::from_last_modification_time(meta));
            }
        });
        let to_complete = to_errors.is_empty();
        let mut errors = Vec::new();
        walk(&from, &ex_from, true, &mut errors, |p, meta| {
            if !meta.is_file() {
                return;
            }
            let Ok(rel) = p.strip_prefix(&from) else { return };
            let Some(class) = classify(rel) else { return };
            let dst = to.join(rel);
            let mtime = FileTime::from_last_modification_time(meta);
            let there = to_files.get(&key(&dst)).copied().or_else(|| {
                // the listing was incomplete: ask for this one file
                (!to_complete)
                    .then(|| std::fs::metadata(long(&dst)).ok().filter(|d| d.is_file()).map(|d| FileTime::from_last_modification_time(&d)))
                    .flatten()
            });
            let change = match there {
                None => Change::New,
                Some(t) if nanos(mtime) - nanos(t) > MTIME_TOLERANCE_NS => Change::Newer,
                Some(_) => return,
            };
            found.items.push(Item { emu: e.id.clone(), rel: rel.to_path_buf(), src: p.to_path_buf(), dst, size: meta.len(), mtime, change, class });
        });
        found.warnings.extend(errors.into_iter().chain(to_errors).map(|w| format!("{}: {w}", e.id)));
    }
    found.items.sort_by(|a, b| (&a.emu, a.class, key(&a.rel)).cmp(&(&b.emu, b.class, key(&b.rel))));
    Ok(found)
}

/// The ticked items as a copy plan, with the per-volume totals the space checks need.
pub fn to_plan(items: &[&Item]) -> Plan {
    let mut p = Plan::default();
    for i in items {
        let vol = volume_root(&i.dst).unwrap_or_default();
        *p.bytes_by_volume.entry(vol.clone()).or_default() += i.size;
        let largest = p.largest_file_by_volume.entry(vol).or_default();
        *largest = (*largest).max(i.size);
        p.copy_bytes += i.size;
        p.copy.push(PlanItem { src: i.src.clone(), dst: i.dst.clone(), size: i.size, mtime: i.mtime });
    }
    p
}

/// Before saves come home: one zip per emulator in `backup_dir` with the files they
/// replace, and the list of files they add, so Restore can undo it. Keeps the last `keep`
/// saves backups per emulator. Returns the zips written.
pub fn backup(items: &[&Item], cat: &Catalog, m: &Mapper, backup_dir: &Path, keep: u32, cancel: &AtomicBool) -> Result<Vec<PathBuf>> {
    let mut by_emu: BTreeMap<&str, Vec<&Item>> = BTreeMap::new();
    for i in items {
        by_emu.entry(i.emu.as_str()).or_default().push(i);
    }
    let mut out = Vec::new();
    for (id, its) in by_emu {
        let Some(e) = cat.emulators.get(id) else { continue };
        let (Some(dir), Some(exe)) = (&e.dir, &e.exe) else { continue };
        let mut meta = BackupMeta {
            kind: BackupKind::Saves,
            id: id.to_string(),
            label: format!("{id} saves"),
            dir: m.dst(dir),
            exe: m.dst(exe),
            created_unix: version::now_unix(),
            ..Default::default()
        };
        for i in its {
            let rel = i.rel.to_string_lossy().into_owned();
            match std::fs::metadata(long(&i.dst)) {
                Ok(d) if d.is_file() => {
                    meta.replaced.push(SavedFile { path: rel, mtime: FileTime::from_last_modification_time(&d).unix_seconds() })
                }
                _ => meta.added.push(rel),
            }
        }
        let zip = backup_dir.join(format!("{}-saves-{}.zip", update::safe(id), update::stamp(meta.created_unix)));
        archive::write_backup(&zip, &meta, cancel, &mut |_| {})?;
        update::prune_backups(backup_dir, id, BackupKind::Saves, keep);
        out.push(zip);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{build, groups_for, PlanOptions, Selection};
    use crate::scan::{scan, tests::fake_install, Roots};
    use std::fs;

    #[test]
    fn sorts_files_into_saves_settings_and_the_rest() {
        let c = |p: &str| classify(Path::new(p));
        for save in [
            r"saves\Super Mario World.srm",
            r"states\Super Mario World.state1",
            r"User\GC\USA\Card A\01-GALE-Zelda.gci",
            r"User\Wii\title\00010000\52534245\data\banner.bin",
            r"memcards\Mcd001.ps2",
            r"sstates\SLUS-20946 (A1B2C3D4).00.p2s",
            r"memcards\shared_card_1.mcd",
            r"memstick\PSP\SAVEDATA\ULUS10041\DATA.BIN",
            r"memstick\PSP\PPSSPP_STATE\ULUS10041_1.00_0.ppst",
            r"mlc01\usr\save\00050000\101c9400\user\80000001\game.dat",
            r"dev_hdd0\home\00000001\savedata\BLUS30001\SYS-DATA",
            r"user\nand\user\save\0000000000000000\01\0100000000010000\save.bin",
            r"pokemon.sav",
        ] {
            assert_eq!(c(save), Some(Class::Save), "{save}");
        }
        for setting in [r"retroarch.cfg", r"inis\PCSX2.ini", r"settings.ini", r"User\Config\Dolphin.ini", "portable.txt", r"config\Mesen.xml"] {
            assert_eq!(c(setting), Some(Class::Setting), "{setting}");
        }
        for other in [r"mlc01\usr\title\00050000\101c9400\code\app.xml.bak", r"dev_hdd0\game\NPUB30001\USRDIR\EBOOT.BIN", r"cores\snes9x_libretro.dll", "xbox_hdd.qcow2"] {
            assert_eq!(c(other), Some(Class::Other), "{other}");
        }
        for hidden in [
            r"shaders\cache\vk.bin",
            r"User\Cache\Shaders\x.cache",
            r"logs\retroarch.log",
            r"shadercache\a.bin",
            r"cheats\Nintendo - SNES\Mario.cht",
            r"thumbnails\Nintendo - SNES\Named_Boxarts\Mario.png",
            "EMUAssistant.version.json",
            "crash.dmp",
        ] {
            assert_eq!(c(hidden), None, "{hidden}");
        }
    }

    fn touch(p: &Path, text: &str, unix: i64) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
        filetime::set_file_mtime(p, FileTime::from_unix_time(unix, 0)).unwrap();
    }

    fn all(cat: &Catalog) -> Selection {
        Selection {
            systems: cat.systems.iter().map(|s| s.name.clone()).collect(),
            emulators: cat.emulators.values().filter(|e| e.copyable).map(|e| e.id.clone()).collect(),
            globals: cat.globals.iter().map(|g| g.id.clone()).collect(),
            media: true,
            gamelists: true,
            ..Default::default()
        }
    }

    /// Export to a "drive", deploy to an "ally" with ES-DE elsewhere; returns the mapper.
    fn setup(tmp: &Path) -> (Catalog, PathBuf, Mapper) {
        let (es, _) = fake_install(&tmp.join("main"));
        let cat = scan(&es, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let no = AtomicBool::new(false);
        let opts = PlanOptions { protect_newer: true, prune: false };
        let drive = tmp.join("drive");
        fs::create_dir_all(&drive).unwrap();
        let p = build(&groups_for(&cat, &all(&cat)), &Mapper::Export { dest_root: drive.clone() }, &opts, &no, &mut |_| {}).unwrap();
        assert!(crate::sync::execute(&p, false, &no, &tx).failed.is_empty());
        let ally = tmp.join("ally");
        let target = Roots { es_root: ally.join("ES-DE"), rom_dir: ally.join("ROMs"), media_dir: ally.join(r"ES-DE\ES-DE\downloaded_media") };
        let m = Mapper::deploy(&drive, &cat.roots, &target);
        let p = build(&groups_for(&cat, &all(&cat)), &m, &opts, &no, &mut |_| {}).unwrap();
        assert!(crate::sync::execute(&p, false, &no, &tx).failed.is_empty());
        (cat, ally, m)
    }

    fn run(found: &Found, ticked: impl Fn(&Item) -> bool) {
        let items: Vec<&Item> = found.items.iter().filter(|i| ticked(i)).collect();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let r = crate::sync::execute(&to_plan(&items), false, &AtomicBool::new(false), &tx);
        assert!(r.failed.is_empty(), "{:?}", r.failed);
    }

    #[test]
    fn saves_travel_home_in_two_hops() {
        let tmp = tempfile::tempdir().unwrap();
        let (cat, ally, m) = setup(tmp.path());
        let no = AtomicBool::new(false);
        let ra = |root: &Path| root.join(r"ES-DE\Emulators\RetroArch-Win64");
        let main_ra = cat.emulators["RETROARCH"].dir.clone().unwrap();
        let later = FileTime::now().unix_seconds() + 3600;

        // nothing changed yet
        assert!(find(&cat, &m, Direction::ToDrive, &no, &mut |_| {}).unwrap().items.is_empty());

        // on the handheld: a new save, a newer config, a log, and a file that is older there
        touch(&ra(&ally).join(r"saves\a.srm"), "ally save", later);
        touch(&ra(&ally).join("retroarch.cfg"), "ally cfg", later);
        touch(&ra(&ally).join(r"logs\retroarch.log"), "log", later);
        touch(&ra(&ally).join(r"system\bios.bin"), "old bios", 1_000_000);
        let f = find(&cat, &m, Direction::ToDrive, &no, &mut |_| {}).unwrap();
        let got: Vec<_> = f.items.iter().map(|i| (i.rel.to_string_lossy().into_owned(), i.class, i.change)).collect();
        assert_eq!(got, vec![(r"saves\a.srm".into(), Class::Save, Change::New), ("retroarch.cfg".into(), Class::Setting, Change::Newer)]);

        // hop 1: only the save (the pre-ticked class) goes to the drive
        run(&f, |i| i.class == Class::Save);
        assert!(find(&cat, &m, Direction::ToDrive, &no, &mut |_| {}).unwrap().items.iter().all(|i| i.class != Class::Save));

        // hop 2 on the main machine: the drive's save comes home, with a backup of nothing
        // replaced (it's new) and the file listed as added
        let home = Mapper::deploy(&tmp.path().join("drive"), &cat.roots, &cat.roots);
        let f = find(&cat, &home, Direction::ToMachine, &no, &mut |_| {}).unwrap();
        assert_eq!(f.items.len(), 1);
        assert!(f.items[0].dst.starts_with(&main_ra));
        let items: Vec<&Item> = f.items.iter().collect();
        let backups = tmp.path().join("backups");
        let zips = backup(&items, &cat, &home, &backups, 3, &no).unwrap();
        assert_eq!(zips.len(), 1);
        let meta = archive::read_meta(&zips[0]).unwrap();
        assert_eq!((meta.kind, meta.replaced.len(), meta.added.clone()), (BackupKind::Saves, 0, vec![r"saves\a.srm".to_string()]));
        run(&f, |_| true);
        assert_eq!(fs::read_to_string(main_ra.join(r"saves\a.srm")).unwrap(), "ally save");
        assert!(find(&cat, &home, Direction::ToMachine, &no, &mut |_| {}).unwrap().items.is_empty());

        // a newer save replaces the main one; the backup holds the old one and Restore puts it back
        touch(&ra(&ally).join(r"saves\a.srm"), "ally save 2", later + 100);
        let f = find(&cat, &m, Direction::ToDrive, &no, &mut |_| {}).unwrap();
        run(&f, |i| i.class == Class::Save);
        let f = find(&cat, &home, Direction::ToMachine, &no, &mut |_| {}).unwrap();
        assert_eq!((f.items.len(), f.items[0].change), (1, Change::Newer));
        let items: Vec<&Item> = f.items.iter().collect();
        let zip = backup(&items, &cat, &home, &backups, 3, &no).unwrap().remove(0);
        run(&f, |_| true);
        assert_eq!(fs::read_to_string(main_ra.join(r"saves\a.srm")).unwrap(), "ally save 2");
        archive::restore(&zip, None, &no).unwrap();
        assert_eq!(fs::read_to_string(main_ra.join(r"saves\a.srm")).unwrap(), "ally save");
    }

    #[test]
    fn emulators_missing_on_one_side_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let (cat, ally, m) = setup(tmp.path());
        fs::remove_dir_all(ally.join(r"ES-DE\Emulators\bsnes")).unwrap();
        let no = AtomicBool::new(false);
        assert!(find(&cat, &m, Direction::ToDrive, &no, &mut |_| {}).unwrap().items.is_empty());
        assert!(find(&cat, &m, Direction::ToMachine, &no, &mut |_| {}).unwrap().items.is_empty());
        assert!(find(&cat, &m, Direction::ToDrive, &AtomicBool::new(true), &mut |_| {}).is_err());
    }

    #[test]
    fn saves_backups_are_pruned_apart_from_update_backups() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("emu");
        fs::create_dir_all(&dir).unwrap();
        let backups = tmp.path().join("backups");
        for (i, kind) in [BackupKind::Update, BackupKind::Saves, BackupKind::Saves, BackupKind::Saves].into_iter().enumerate() {
            let meta = BackupMeta { kind, id: "RETROARCH".into(), dir: dir.clone(), created_unix: 1_000 + i as i64, ..Default::default() };
            archive::write_backup(&backups.join(format!("b{i}.zip")), &meta, &AtomicBool::new(false), &mut |_| {}).unwrap();
        }
        update::prune_backups(&backups, "RETROARCH", BackupKind::Saves, 2);
        let left: Vec<_> = update::list_backups(&backups).into_iter().map(|(_, m)| (m.kind, m.created_unix)).collect();
        assert_eq!(left, vec![(BackupKind::Saves, 1003), (BackupKind::Saves, 1002), (BackupKind::Update, 1000)]);
    }
}

//! Turns a selection into copy groups, maps them to source/destination paths and diffs
//! them against what already exists at the destination.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Result};
use filetime::FileTime;
use serde::{Deserialize, Serialize};

use crate::fsutil::walk;
use crate::paths::{is_under, key, long, mirror, remap, volume_root};
use crate::scan::{Catalog, Roots};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Selection {
    pub systems: BTreeSet<String>,
    pub emulators: BTreeSet<String>,
    pub globals: BTreeSet<String>,
    pub media: bool,
    pub gamelists: bool,
    /// Skip RetroArch's cheat database / thumbnails (re-downloadable loose files).
    pub exclude_cheats: bool,
    pub exclude_thumbnails: bool,
    /// Games picked per system (system -> game keys); systems not listed copy all games.
    #[serde(default)]
    pub games: BTreeMap<String, BTreeSet<String>>,
}

impl Selection {
    /// Whether an emulator's optional folder of this kind is left out.
    pub fn excludes_kind(&self, kind: &str) -> bool {
        match kind {
            "cheats" => self.exclude_cheats,
            "thumbnails" => self.exclude_thumbnails,
            _ => false,
        }
    }
}

/// A directory tree to copy, expressed in original (source machine) paths.
#[derive(Clone, Debug, Default)]
pub struct Group {
    pub label: String,
    pub src: PathBuf,
    pub excludes: Vec<PathBuf>,
    pub recursive: bool,
    /// Only content groups (ROMs, media) may have files deleted by "prune".
    pub prunable: bool,
    /// The system this group belongs to, for per-system totals.
    pub system: Option<String>,
    /// Media names (lowercase file names without extension) of games that weren't picked:
    /// such files are neither copied nor deleted.
    pub skip_stems: HashSet<String>,
}

fn skipped_stem(g: &Group, p: &Path) -> bool {
    !g.skip_stems.is_empty()
        && p.file_name().is_some_and(|n| g.skip_stems.contains(&crate::games::media_stem(&n.to_string_lossy())))
}

pub fn groups_for(cat: &Catalog, sel: &Selection) -> Vec<Group> {
    let mut out = Vec::new();
    let mut push = |label: String, src: &Path, prunable: bool, system: Option<&str>, excludes: Vec<PathBuf>, skip_stems: HashSet<String>| {
        out.push(Group {
            label,
            src: src.to_path_buf(),
            excludes,
            recursive: true,
            prunable,
            system: system.map(String::from),
            skip_stems,
        })
    };
    for s in cat.systems.iter().filter(|s| sel.systems.contains(&s.name)) {
        let sys = Some(s.name.as_str());
        // games that weren't picked stay out of the copy, and are kept if already at the destination
        let (left_out, stems) = match (sel.games.get(&s.name), &s.games) {
            (Some(picked), Some(list)) => list.left_out(picked),
            _ => Default::default(),
        };
        let rom_ex = left_out.iter().map(|p| s.rom_dir.join(p)).collect();
        push(format!("ROMs: {}", s.fullname), &s.rom_dir, true, sys, rom_ex, HashSet::new());
        if let Some(d) = s.gamelist_dir.as_ref().filter(|_| sel.gamelists) {
            push(format!("Gamelist: {}", s.fullname), d, false, sys, vec![], HashSet::new());
        }
        if let Some(d) = s.media_dir.as_ref().filter(|_| sel.media) {
            push(format!("Media: {}", s.fullname), d, true, sys, vec![], stems);
        }
    }
    for e in sel.emulators.iter().filter_map(|id| cat.emulators.get(id)) {
        if let (true, Some(d)) = (e.copyable, &e.dir) {
            let excludes = e.optional_dirs.iter().filter(|o| sel.excludes_kind(&o.kind)).map(|o| o.path.clone()).collect();
            push(format!("Emulator: {}", e.id), d, false, None, excludes, HashSet::new());
        }
    }
    for g in cat.globals.iter().filter(|g| sel.globals.contains(&g.id)) {
        out.push(Group {
            label: g.label.clone(),
            src: g.src.clone(),
            excludes: g.excludes.clone(),
            recursive: g.recursive,
            prunable: false,
            system: None,
            skip_stems: HashSet::new(),
        });
    }
    out
}

/// Where a group's files are read from and written to.
#[derive(Clone, Debug)]
pub enum Mapper {
    /// Source machine -> drive: same paths, drive letter swapped for `dest_root`.
    Export { dest_root: PathBuf },
    /// Drive -> target machine: read from the mirror under `pkg_root`, write to remapped roots.
    Deploy { pkg_root: PathBuf, remaps: Vec<(PathBuf, PathBuf)> },
}

impl Mapper {
    pub fn deploy(pkg_root: &Path, orig: &Roots, target: &Roots) -> Self {
        let mut remaps = vec![
            (orig.rom_dir.clone(), target.rom_dir.clone()),
            (orig.media_dir.clone(), target.media_dir.clone()),
            (orig.es_root.clone(), target.es_root.clone()),
        ];
        remaps.sort_by_key(|(from, _)| std::cmp::Reverse(from.components().count()));
        Mapper::Deploy { pkg_root: pkg_root.to_path_buf(), remaps }
    }

    pub fn src(&self, orig: &Path) -> PathBuf {
        match self {
            Mapper::Export { .. } => orig.to_path_buf(),
            Mapper::Deploy { pkg_root, .. } => mirror(pkg_root, orig),
        }
    }

    pub fn dst(&self, orig: &Path) -> PathBuf {
        match self {
            Mapper::Export { dest_root } => mirror(dest_root, orig),
            Mapper::Deploy { remaps, .. } => remap(orig, remaps),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PlanOptions {
    pub protect_newer: bool,
    pub prune: bool,
}

#[derive(Clone, Debug)]
pub struct PlanItem {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub size: u64,
    pub mtime: FileTime,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub copy: Vec<PlanItem>,
    pub copy_bytes: u64,
    pub skipped: u64,
    pub skipped_bytes: u64,
    /// Destination files newer than the source, left alone.
    pub protected: Vec<PathBuf>,
    pub prune: Vec<PathBuf>,
    /// Destination folders of the groups that have files in `prune`; emptied folders
    /// below them are removed after the files.
    pub prune_roots: Vec<PathBuf>,
    pub dirs: Vec<PathBuf>,
    pub bytes_by_volume: BTreeMap<String, u64>,
    /// Size of the largest file to copy per destination volume (FAT32 can't hold 4 GiB).
    pub largest_file_by_volume: BTreeMap<String, u64>,
    /// Bytes still to copy per system (ROMs + gamelist + media).
    pub copy_bytes_by_system: BTreeMap<String, u64>,
    pub warnings: Vec<String>,
}

/// FAT/exFAT store modification times with 2 second resolution.
const MTIME_TOLERANCE_NS: i128 = 2_000_000_000;

fn nanos(t: FileTime) -> i128 {
    t.unix_seconds() as i128 * 1_000_000_000 + t.nanoseconds() as i128
}

pub fn build(
    groups: &[Group],
    m: &Mapper,
    opts: &PlanOptions,
    cancel: &AtomicBool,
    status: &mut dyn FnMut(String),
) -> Result<Plan> {
    let mut plan = Plan::default();
    for g in groups {
        let (s, d) = (m.src(&g.src), m.dst(&g.src));
        if is_under(&d, &s) || is_under(&s, &d) {
            bail!("Destination {} overlaps source {}", d.display(), s.display());
        }
    }

    // dst key -> src key, to drop duplicates (nested groups) and catch collisions
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut dirs_seen = HashSet::new();
    for g in groups {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        status(format!("Comparing {}…", g.label));
        let src_root = m.src(&g.src);
        let dst_root = m.dst(&g.src);
        if !src_root.is_dir() {
            plan.warnings.push(format!("{}: {} not found, skipped", g.label, src_root.display()));
            continue;
        }
        let src_ex: Vec<PathBuf> = g.excludes.iter().map(|e| m.src(e)).collect();
        let mut group_dsts = HashSet::new();
        let mut items = Vec::new();
        let mut errors = Vec::new();
        walk(&src_root, &src_ex, g.recursive, &mut errors, |p, meta| {
            let Ok(rel) = p.strip_prefix(&src_root) else { return };
            let dst = dst_root.join(rel);
            if meta.is_dir() {
                // non-recursive groups (ROM root files) must not create other systems' folders
                if g.recursive && dirs_seen.insert(key(&dst)) {
                    plan.dirs.push(dst);
                }
            } else if meta.is_file() && !skipped_stem(g, p) {
                items.push((p.to_path_buf(), dst, meta.len(), FileTime::from_last_modification_time(meta)));
            }
        });
        let src_complete = errors.is_empty();
        plan.warnings.extend(errors.into_iter().map(|e| format!("{}: {e}", g.label)));

        // One listing of the destination instead of one metadata call per source file.
        // lowercase path -> (path as it is on disk, size, modification time)
        let mut dst_files: HashMap<String, (PathBuf, u64, FileTime)> = HashMap::new();
        let mut dst_complete = true;
        if dst_root.is_dir() {
            let dst_ex: Vec<PathBuf> = g.excludes.iter().map(|e| m.dst(e)).collect();
            let mut dst_errors = Vec::new();
            walk(&dst_root, &dst_ex, g.recursive, &mut dst_errors, |p, meta| {
                if meta.is_file() {
                    dst_files.insert(key(p), (p.to_path_buf(), meta.len(), FileTime::from_last_modification_time(meta)));
                }
            });
            dst_complete = dst_errors.is_empty();
        }

        for (src, dst, size, mtime) in items {
            if cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }
            let (dk, sk) = (key(&dst), key(&src));
            // Record the file for this group first: a file that an earlier, overlapping
            // group already planned still exists in the source and must not be pruned.
            group_dsts.insert(dk.clone());
            match seen.get(&dk) {
                Some(prev) if *prev == sk => continue,
                Some(prev) => bail!("Two source files map to {}: {prev} and {sk}", dst.display()),
                None => {}
            }
            seen.insert(dk.clone(), sk);

            let existing = match dst_files.get(&dk) {
                Some((_, len, t)) => Some((*len, *t)),
                // the listing was incomplete: ask for this one file directly
                None if !dst_complete => std::fs::metadata(long(&dst))
                    .ok()
                    .filter(|dm| dm.is_file())
                    .map(|dm| (dm.len(), FileTime::from_last_modification_time(&dm))),
                None => None,
            };
            if let Some((len, dst_mtime)) = existing {
                let diff = nanos(dst_mtime) - nanos(mtime);
                if len == size && diff.abs() <= MTIME_TOLERANCE_NS {
                    plan.skipped += 1;
                    plan.skipped_bytes += size;
                    continue;
                }
                if opts.protect_newer && diff > MTIME_TOLERANCE_NS {
                    plan.protected.push(dst);
                    continue;
                }
            }
            let vol = volume_root(&dst).unwrap_or_default();
            *plan.bytes_by_volume.entry(vol.clone()).or_default() += size;
            let largest = plan.largest_file_by_volume.entry(vol).or_default();
            *largest = (*largest).max(size);
            if let Some(sys) = &g.system {
                *plan.copy_bytes_by_system.entry(sys.clone()).or_default() += size;
            }
            plan.copy_bytes += size;
            plan.copy.push(PlanItem { src, dst, size, mtime });
        }

        if opts.prune && g.prunable {
            if !src_complete {
                // unreadable source files would look deleted
                plan.warnings.push(format!("{}: the source could not be read completely, so nothing is deleted here", g.label));
            } else {
                let mut extra: Vec<PathBuf> = dst_files
                    .iter()
                    .filter(|(k, v)| !group_dsts.contains(*k) && !skipped_stem(g, &v.0))
                    .map(|(_, v)| v.0.clone())
                    .collect();
                extra.sort();
                if !extra.is_empty() {
                    plan.prune_roots.push(dst_root.clone());
                    plan.prune.extend(extra);
                }
            }
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::eq_ci;
    use crate::scan::{scan, tests::fake_install};

    fn all(cat: &Catalog) -> Selection {
        Selection {
            systems: cat.systems.iter().map(|s| s.name.clone()).collect(),
            emulators: cat.emulators.values().filter(|e| e.copyable).map(|e| e.id.clone()).collect(),
            globals: cat.globals.iter().map(|g| g.id.clone()).collect(),
            media: true,
            gamelists: true,
            exclude_cheats: false,
            exclude_thumbnails: false,
            games: BTreeMap::new(),
        }
    }

    #[test]
    fn export_mirrors_paths_and_excludes() {
        let tmp = tempfile::tempdir().unwrap();
        let (es, _) = fake_install(&tmp.path().join("src"));
        let cat = scan(&es, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let dest = tmp.path().join("drive");
        let m = Mapper::Export { dest_root: dest.clone() };
        let plan = build(&groups_for(&cat, &all(&cat)), &m, &PlanOptions { protect_newer: true, prune: false }, &AtomicBool::new(false), &mut |_| {}).unwrap();

        let rom = plan.copy.iter().find(|i| i.src.ends_with(r"snes\a.sfc")).unwrap();
        assert!(eq_ci(&rom.dst, &mirror(&dest, &rom.src)));
        // every file copied once, even though the ES-DE program group contains other groups' parents
        let mut keys: Vec<_> = plan.copy.iter().map(|i| key(&i.dst)).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), plan.copy.len());
        assert!(plan.copy.iter().any(|i| i.src.ends_with("bios.bin")));
        assert!(plan.copy.iter().any(|i| i.src.ends_with("systems.txt")));
        assert!(!plan.copy.iter().any(|i| i.src.ends_with("duckstation.exe")));
    }

    #[test]
    fn skips_retroarch_cheats_and_thumbnails_and_totals_per_system() {
        let tmp = tempfile::tempdir().unwrap();
        let (es, _) = fake_install(&tmp.path().join("src"));
        let cat = scan(&es, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let dest = tmp.path().join("drive");
        let m = Mapper::Export { dest_root: dest };
        let opts = PlanOptions { protect_newer: true, prune: false };
        let run = |sel: &Selection| build(&groups_for(&cat, sel), &m, &opts, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let has = |p: &Plan, s: &str| p.copy.iter().any(|i| i.src.to_string_lossy().contains(s));

        let mut sel = all(&cat);
        sel.exclude_cheats = true;
        sel.exclude_thumbnails = true;
        let p = run(&sel);
        assert!(!has(&p, "Mario.cht") && !has(&p, "Named_Boxarts"));
        assert!(has(&p, "bios.bin") && has(&p, "retroarch.cfg"));

        sel.exclude_cheats = false;
        let p = run(&sel);
        assert!(has(&p, "Mario.cht") && !has(&p, "Named_Boxarts"));

        // per-system bytes cover ROMs + gamelist + media of that system only
        let snes: u64 = p
            .copy
            .iter()
            .filter(|i| {
                let s = i.src.to_string_lossy();
                s.contains(r"ROMs\snes") || s.contains(r"gamelists\snes") || s.contains(r"downloaded_media\snes")
            })
            .map(|i| i.size)
            .sum();
        assert_eq!(p.copy_bytes_by_system["snes"], snes);
        let systems: u64 = p.copy_bytes_by_system.values().sum();
        assert!(systems < p.copy_bytes);
    }

    #[test]
    fn deploy_remaps_roots() {
        let orig = Roots {
            es_root: PathBuf::from(r"D:\Emu\ES-DE"),
            rom_dir: PathBuf::from(r"D:\Games\ROMs"),
            media_dir: PathBuf::from(r"D:\Emu\ES-DE\ES-DE\downloaded_media"),
        };
        let target = Roots {
            es_root: PathBuf::from(r"C:\ES-DE"),
            rom_dir: PathBuf::from(r"F:\ROMs"),
            media_dir: PathBuf::from(r"C:\ES-DE\ES-DE\downloaded_media"),
        };
        let m = Mapper::deploy(Path::new(r"E:\"), &orig, &target);
        assert_eq!(m.src(Path::new(r"D:\Games\ROMs\snes")), PathBuf::from(r"E:\Games\ROMs\snes"));
        assert_eq!(m.dst(Path::new(r"D:\Games\ROMs\snes")), PathBuf::from(r"F:\ROMs\snes"));
        assert_eq!(
            m.dst(Path::new(r"D:\Emu\ES-DE\Emulators\RetroArch-Win64")),
            PathBuf::from(r"C:\ES-DE\Emulators\RetroArch-Win64")
        );
    }

    #[test]
    fn export_then_deploy_with_relocated_roms() {
        let tmp = tempfile::tempdir().unwrap();
        let (es, _) = fake_install(&tmp.path().join("main"));
        let cat = scan(&es, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let mut sel = all(&cat);
        sel.systems.remove("psx");
        let opts = PlanOptions { protect_newer: true, prune: false };
        let (tx, _rx) = crossbeam_channel::unbounded();
        let no = AtomicBool::new(false);

        // export to the "drive"
        let drive = tmp.path().join("drive");
        std::fs::create_dir_all(&drive).unwrap();
        let p = build(&groups_for(&cat, &sel), &Mapper::Export { dest_root: drive.clone() }, &opts, &no, &mut |_| {}).unwrap();
        assert!(crate::sync::execute(&p, true, &no, &tx).failed.is_empty());
        let m = crate::manifest::for_export(None, &cat, &sel, None);
        crate::manifest::save(&drive, &m).unwrap();

        // deploy on the "ally" with ES-DE and ROMs in different places
        let pkg = crate::manifest::load(&drive).unwrap().unwrap();
        let ally = tmp.path().join("ally");
        let target = Roots {
            es_root: ally.join("ES-DE"),
            rom_dir: ally.join("SD").join("ROMs"),
            media_dir: ally.join(r"ES-DE\ES-DE\downloaded_media"),
        };
        let m = Mapper::deploy(&drive, &pkg.catalog.roots, &target);
        let dsel = Selection { systems: pkg.catalog.systems.iter().map(|s| s.name.clone()).collect(), ..sel.clone() };
        let p = build(&groups_for(&pkg.catalog, &dsel), &m, &opts, &no, &mut |_| {}).unwrap();
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        assert!(crate::sync::execute(&p, false, &no, &tx).failed.is_empty());
        crate::deploy::fix_settings(&target).unwrap();

        assert!(target.rom_dir.join(r"snes\a.sfc").is_file());
        assert!(target.rom_dir.join("systems.txt").is_file());
        assert!(!target.rom_dir.join("psx").exists());
        assert!(target.es_root.join(r"Emulators\RetroArch-Win64\system\bios.bin").is_file());
        assert!(target.es_root.join("ES-DE.exe").is_file());
        assert!(target.media_dir.join(r"snes\screenshots\a.png").is_file());
        let st = crate::esde::settings::read(&target.es_root.join(r"ES-DE\settings\es_settings.xml")).unwrap();
        assert!(eq_ci(Path::new(&st["ROMDirectory"]), &target.rom_dir));

        // a second deploy has nothing left to do
        let p = build(&groups_for(&pkg.catalog, &dsel), &m, &opts, &no, &mut |_| {}).unwrap();
        // except es_settings.xml, which was patched on the target (and is re-patched after every deploy)
        assert!(p.copy.iter().all(|i| i.dst.ends_with("es_settings.xml")), "{:?}", p.copy);
        assert!(p.skipped > 5);
    }

    #[test]
    fn copies_picked_games_and_keeps_the_others() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("main");
        let roms = src.join(r"ROMs\switch");
        let media = src.join(r"media\switch");
        let w = |p: PathBuf| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, p.file_name().unwrap().to_string_lossy().as_bytes()).unwrap();
        };
        for f in ["A [0100000000010000][v0].nsp", "B.nsp", r"_Updates\A [0100000000010800][v1].nsp", r"_Updates\B v1.1.nsp", r"_Keys\prod.keys"] {
            w(roms.join(f));
        }
        w(media.join(r"covers\A [0100000000010000][v0].png"));
        w(media.join(r"covers\B.png"));
        let sys = |games| crate::scan::SysEntry {
            name: "switch".into(),
            fullname: "Switch".into(),
            rom_dir: roms.clone(),
            rom_files: 5,
            rom_bytes: 0,
            gamelist_dir: None,
            gamelist_bytes: 0,
            media_dir: Some(media.clone()),
            media_bytes: 0,
            emus_in_use: vec![],
            emus_alt: vec![],
            noload: false,
            games: Some(games),
        };
        let roots = Roots { es_root: src.join("ES-DE"), rom_dir: src.join("ROMs"), media_dir: src.join("media") };
        let cat = Catalog { roots: roots.clone(), systems: vec![sys(crate::games::list(&roms))], ..Default::default() };
        let pick = |key: &str| Selection {
            systems: ["switch".to_string()].into(),
            media: true,
            games: [("switch".to_string(), [key.to_string()].into())].into(),
            ..Default::default()
        };
        let names = |p: &Plan| {
            let mut v: Vec<String> = p.copy.iter().map(|i| i.src.file_name().unwrap().to_string_lossy().into_owned()).collect();
            v.sort();
            v
        };
        let no = AtomicBool::new(false);

        // B is on the drive from an earlier export, Gone.nsp was deleted at the source
        let drive = tmp.path().join("drive");
        for f in [roms.join("B.nsp"), media.join(r"covers\B.png"), roms.join("Gone.nsp")] {
            w(mirror(&drive, &f));
        }
        let sel = pick("A [0100000000010000][v0].nsp");
        let m = Mapper::Export { dest_root: drive.clone() };
        let p = build(&groups_for(&cat, &sel), &m, &PlanOptions { protect_newer: true, prune: true }, &no, &mut |_| {}).unwrap();
        assert_eq!(names(&p), vec!["A [0100000000010000][v0].nsp", "A [0100000000010000][v0].png", "A [0100000000010800][v1].nsp", "prod.keys"]);
        // the unpicked game stays on the drive, even in mirror mode
        assert_eq!(p.prune.len(), 1, "{:?}", p.prune);
        assert!(p.prune[0].ends_with("Gone.nsp"));
        let man = crate::manifest::for_export(None, &cat, &sel, None);
        let a = &cat.systems[0].games.as_ref().unwrap();
        assert_eq!((man.catalog.systems[0].rom_files, man.catalog.systems[0].rom_bytes), a.picked(&sel.games["switch"]));
        let (tx, _rx) = crossbeam_channel::unbounded();
        assert!(crate::sync::execute(&p, false, &no, &tx).failed.is_empty());

        // deploy only B from the drive (A, its update and _Keys are there too)
        let pkg = Catalog { systems: vec![sys(crate::games::list(&mirror(&drive, &roms)))], ..cat.clone() };
        let ally = tmp.path().join("ally");
        let target = Roots { es_root: ally.join("ES-DE"), rom_dir: ally.join("ROMs"), media_dir: ally.join("media") };
        let m = Mapper::deploy(&drive, &roots, &target);
        let p = build(&groups_for(&pkg, &pick("B.nsp")), &m, &PlanOptions { protect_newer: true, prune: false }, &no, &mut |_| {}).unwrap();
        assert_eq!(names(&p), vec!["B.nsp", "B.png", "prod.keys"]);
        assert!(p.copy.iter().all(|i| is_under(&i.dst, &ally)));
    }

    #[test]
    fn rejects_overlap() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a");
        std::fs::create_dir_all(&src).unwrap();
        let g = Group { label: "x".into(), src: src.clone(), recursive: true, ..Default::default() };
        // exporting to the source's own drive root maps every path onto itself
        let m = Mapper::Export { dest_root: PathBuf::from(volume_root(&src).unwrap()) };
        assert!(build(&[g], &m, &PlanOptions { protect_newer: true, prune: false }, &AtomicBool::new(false), &mut |_| {}).is_err());
    }

    #[test]
    fn overlapping_groups_do_not_prune_each_others_files() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.bin"), b"a").unwrap();
        let dest = tmp.path().join("drive");
        let m = Mapper::Export { dest_root: dest };
        let group = |label: &str| Group { label: label.into(), src: src.clone(), recursive: true, prunable: true, ..Default::default() };
        let groups = [group("one"), group("two")];
        let opts = PlanOptions { protect_newer: true, prune: true };
        let no = AtomicBool::new(false);

        let p = build(&groups, &m, &opts, &no, &mut |_| {}).unwrap();
        assert_eq!(p.copy.len(), 1);
        let (tx, _rx) = crossbeam_channel::unbounded();
        assert!(crate::sync::execute(&p, false, &no, &tx).failed.is_empty());

        // the second group sees the same file again; it must not be listed for deletion
        let p = build(&groups, &m, &opts, &no, &mut |_| {}).unwrap();
        assert!(p.copy.is_empty());
        assert!(p.prune.is_empty(), "{:?}", p.prune);
        assert_eq!(p.largest_file_by_volume.len(), 0);
    }
}

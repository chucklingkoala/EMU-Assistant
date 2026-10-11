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
#[derive(Clone, Debug)]
pub struct Group {
    pub label: String,
    pub src: PathBuf,
    pub excludes: Vec<PathBuf>,
    pub recursive: bool,
    /// Only content groups (ROMs, media) may have files deleted by "prune".
    pub prunable: bool,
    /// The system this group belongs to, for per-system totals.
    pub system: Option<String>,
}

pub fn groups_for(cat: &Catalog, sel: &Selection) -> Vec<Group> {
    let mut out = Vec::new();
    let mut push = |label: String, src: &Path, prunable: bool, system: Option<&str>, excludes: Vec<PathBuf>| {
        out.push(Group {
            label,
            src: src.to_path_buf(),
            excludes,
            recursive: true,
            prunable,
            system: system.map(String::from),
        })
    };
    for s in cat.systems.iter().filter(|s| sel.systems.contains(&s.name)) {
        let sys = Some(s.name.as_str());
        push(format!("ROMs: {}", s.fullname), &s.rom_dir, true, sys, vec![]);
        if let Some(d) = s.gamelist_dir.as_ref().filter(|_| sel.gamelists) {
            push(format!("Gamelist: {}", s.fullname), d, false, sys, vec![]);
        }
        if let Some(d) = s.media_dir.as_ref().filter(|_| sel.media) {
            push(format!("Media: {}", s.fullname), d, true, sys, vec![]);
        }
    }
    for e in sel.emulators.iter().filter_map(|id| cat.emulators.get(id)) {
        if let (true, Some(d)) = (e.copyable, &e.dir) {
            let excludes = e.optional_dirs.iter().filter(|o| sel.excludes_kind(&o.kind)).map(|o| o.path.clone()).collect();
            push(format!("Emulator: {}", e.id), d, false, None, excludes);
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
    pub dirs: Vec<PathBuf>,
    pub bytes_by_volume: BTreeMap<String, u64>,
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
            } else if meta.is_file() {
                items.push((p.to_path_buf(), dst, meta.len(), FileTime::from_last_modification_time(meta)));
            }
        });
        plan.warnings.extend(errors.into_iter().map(|e| format!("{}: {e}", g.label)));

        for (src, dst, size, mtime) in items {
            if cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }
            let (dk, sk) = (key(&dst), key(&src));
            match seen.get(&dk) {
                Some(prev) if *prev == sk => continue,
                Some(prev) => bail!("Two source files map to {}: {prev} and {sk}", dst.display()),
                None => {}
            }
            seen.insert(dk.clone(), sk);
            group_dsts.insert(dk);

            match std::fs::metadata(long(&dst)) {
                Ok(dm) if dm.is_file() => {
                    let diff = nanos(FileTime::from_last_modification_time(&dm)) - nanos(mtime);
                    if dm.len() == size && diff.abs() <= MTIME_TOLERANCE_NS {
                        plan.skipped += 1;
                        plan.skipped_bytes += size;
                        continue;
                    }
                    if opts.protect_newer && diff > MTIME_TOLERANCE_NS {
                        plan.protected.push(dst);
                        continue;
                    }
                }
                _ => {}
            }
            *plan.bytes_by_volume.entry(volume_root(&dst).unwrap_or_default()).or_default() += size;
            if let Some(sys) = &g.system {
                *plan.copy_bytes_by_system.entry(sys.clone()).or_default() += size;
            }
            plan.copy_bytes += size;
            plan.copy.push(PlanItem { src, dst, size, mtime });
        }

        if opts.prune && g.prunable && dst_root.is_dir() {
            let dst_ex: Vec<PathBuf> = g.excludes.iter().map(|e| m.dst(e)).collect();
            let mut errors = Vec::new();
            walk(&dst_root, &dst_ex, g.recursive, &mut errors, |p, meta| {
                if meta.is_file() && !group_dsts.contains(&key(p)) {
                    plan.prune.push(p.to_path_buf());
                }
            });
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
        let m = crate::manifest::for_export(None, &cat, &sel);
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
    fn rejects_overlap() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a");
        std::fs::create_dir_all(&src).unwrap();
        let g = Group { label: "x".into(), src: src.clone(), excludes: vec![], recursive: true, prunable: false, system: None };
        // exporting to the source's own drive root maps every path onto itself
        let m = Mapper::Export { dest_root: PathBuf::from(volume_root(&src).unwrap()) };
        assert!(build(&[g], &m, &PlanOptions { protect_newer: true, prune: false }, &AtomicBool::new(false), &mut |_| {}).is_err());
    }
}

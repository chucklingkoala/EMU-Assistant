//! Scans a portable ES-DE installation into a [`Catalog`] of systems, emulators and
//! global groups (the ES-DE program, its config, themes, ...).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};

use crate::esde::{find_rules, gamelist, settings, systems};
use crate::fsutil::{dir_size, human_bytes, walk};
use crate::paths::{eq_ci, is_under, normalize};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Roots {
    pub es_root: PathBuf,
    pub rom_dir: PathBuf,
    pub media_dir: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SysEntry {
    pub name: String,
    pub fullname: String,
    pub rom_dir: PathBuf,
    pub rom_files: u64,
    pub rom_bytes: u64,
    pub gamelist_dir: Option<PathBuf>,
    pub gamelist_bytes: u64,
    pub media_dir: Option<PathBuf>,
    pub media_bytes: u64,
    /// Emulators used by the default/selected launch command and per-game overrides.
    pub emus_in_use: Vec<String>,
    /// Emulators only referenced by alternative commands.
    pub emus_alt: Vec<String>,
    pub noload: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct EmuEntry {
    pub id: String,
    pub exe: Option<PathBuf>,
    pub dir: Option<PathBuf>,
    pub bytes: u64,
    /// The folder can and should be copied as a unit.
    pub copyable: bool,
    /// Ticked automatically when a system using it is selected.
    pub auto: bool,
    pub note: Option<String>,
    /// Re-downloadable sub-folders that can be skipped (RetroArch cheats / thumbnails).
    #[serde(default)]
    pub optional_dirs: Vec<OptionalDir>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OptionalDir {
    /// "cheats" or "thumbnails"
    pub kind: String,
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GlobalEntry {
    pub id: String,
    pub label: String,
    pub src: PathBuf,
    pub excludes: Vec<PathBuf>,
    pub recursive: bool,
    pub bytes: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Catalog {
    pub roots: Roots,
    pub systems: Vec<SysEntry>,
    pub emulators: BTreeMap<String, EmuEntry>,
    pub globals: Vec<GlobalEntry>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

pub fn is_es_root(p: &Path) -> bool {
    p.join("ES-DE.exe").is_file()
}

/// The ES-DE folder when this exe was placed next to `ES-DE.exe`.
pub fn detect_es_root_near_exe() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    is_es_root(&dir).then_some(dir)
}

pub fn scan(es_root: &Path, cancel: &AtomicBool, status: &mut dyn FnMut(String)) -> Result<Catalog> {
    let es_root = normalize(es_root);
    ensure!(is_es_root(&es_root), "{} does not contain ES-DE.exe", es_root.display());
    let data = es_root.join("ES-DE");
    let mut warnings = Vec::new();

    let settings_path = data.join(r"settings\es_settings.xml");
    let st = settings::read(&settings_path).unwrap_or_else(|_| {
        warnings.push(format!("{} not found, using default folders", settings_path.display()));
        Default::default()
    });
    let get = |k: &str| st.get(k).map(String::as_str).unwrap_or("");
    let rom_dir = settings::resolve_dir(get("ROMDirectory"), &es_root, None, "ROMs");
    let media_dir = settings::resolve_dir(get("MediaDirectory"), &es_root, Some(&rom_dir), r"ES-DE\downloaded_media");
    if !rom_dir.is_dir() {
        warnings.push(format!("ROM folder {} does not exist", rom_dir.display()));
    }

    status("Reading system definitions…".into());
    let defs = systems::load(&es_root)?;
    let rules = find_rules::load(&es_root)?;

    let mut emulators: BTreeMap<String, EmuEntry> = BTreeMap::new();
    let mut syslist = Vec::new();
    for def in defs {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let dir = normalize(Path::new(&def.path.replace("%ROMPATH%", &rom_dir.to_string_lossy())));
        if !dir.is_dir() {
            continue;
        }
        status(format!("Scanning {}…", def.name));
        let (rom_files, rom_bytes) = count_games(&dir);
        if rom_files == 0 {
            continue;
        }

        let gl_dir = data.join("gamelists").join(&def.name);
        let choices = gamelist::read(&gl_dir.join("gamelist.xml"));
        let default_label = choices
            .system
            .clone()
            .filter(|l| def.commands.iter().any(|c| &c.label == l))
            .or_else(|| def.commands.first().map(|c| c.label.clone()));
        let mut in_use = BTreeSet::new();
        let mut alt = BTreeSet::new();
        for c in &def.commands {
            let used = Some(&c.label) == default_label.as_ref() || choices.per_game.contains(&c.label);
            for id in command_emulators(&c.text, &es_root) {
                if used {
                    in_use.insert(id);
                } else {
                    alt.insert(id);
                }
            }
        }
        let alt: Vec<String> = alt.difference(&in_use).cloned().collect();
        for id in in_use.iter().chain(&alt) {
            if !emulators.contains_key(id) {
                status(format!("Locating emulator {id}…"));
                emulators.insert(id.clone(), make_emu(id, &rules, &es_root));
            }
        }

        let gamelist_dir = gl_dir.is_dir().then_some(gl_dir);
        let sys_media = media_dir.join(&def.name);
        let media = sys_media.is_dir().then_some(sys_media);
        syslist.push(SysEntry {
            name: def.name.clone(),
            fullname: if def.fullname.is_empty() { def.name.clone() } else { def.fullname.clone() },
            noload: dir.join("noload.txt").exists(),
            rom_dir: dir,
            rom_files,
            rom_bytes,
            gamelist_bytes: gamelist_dir.as_ref().map_or(0, |d| dir_size(d, &[], true).1),
            gamelist_dir,
            media_bytes: media.as_ref().map_or(0, |d| dir_size(d, &[], true).1),
            media_dir: media,
            emus_in_use: in_use.into_iter().collect(),
            emus_alt: alt,
        });
    }
    syslist.sort_by_key(|s| s.fullname.to_lowercase());

    status("Measuring ES-DE folders…".into());
    let mut program_ex = vec![data.clone(), es_root.join("Emulators"), es_root.join("ROMs"), rom_dir.clone(), media_dir.clone()];
    program_ex.extend(
        emulators
            .values()
            .filter_map(|e| e.dir.clone())
            .filter(|d| is_under(d, &es_root) && !eq_ci(d, &es_root)),
    );
    let config_ex = ["gamelists", "downloaded_media", "themes", "collections", "logs"]
        .iter()
        .map(|d| data.join(d))
        .chain([media_dir.clone()])
        .collect();
    let candidates = [
        ("esde_program", "ES-DE application", es_root.clone(), program_ex, true),
        ("esde_config", "ES-DE settings, custom systems, scripts", data.clone(), config_ex, true),
        ("themes", "Themes", data.join("themes"), vec![], true),
        ("collections", "Custom collections", data.join("collections"), vec![], true),
        ("rom_root_files", "ROM folder root files (systems.txt)", rom_dir.clone(), vec![], false),
    ];
    let globals = candidates
        .into_iter()
        .filter(|c| c.2.is_dir())
        .map(|(id, label, src, excludes, recursive)| GlobalEntry {
            bytes: dir_size(&src, &excludes, recursive).1,
            id: id.into(),
            label: label.into(),
            src,
            excludes,
            recursive,
        })
        .collect();

    Ok(Catalog {
        roots: Roots { es_root, rom_dir, media_dir },
        systems: syslist,
        emulators,
        globals,
        warnings,
    })
}

/// Counts game files, ignoring the helper files ES-DE drops into system folders.
fn count_games(dir: &Path) -> (u64, u64) {
    let mut files = 0;
    let mut bytes = 0;
    let mut errors = Vec::new();
    walk(dir, &[], true, &mut errors, |p, m| {
        if m.is_file() {
            bytes += m.len();
            let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            if name != "systeminfo.txt" && name != "noload.txt" {
                files += 1;
            }
        }
    });
    (files, bytes)
}

/// Emulator ids referenced by a launch command. Commands without `%EMULATOR_..%`
/// (custom systems) fall back to the first token that is an existing `.exe`.
fn command_emulators(cmd: &str, es_root: &Path) -> Vec<String> {
    let ids = systems::emulator_ids(cmd);
    if !ids.is_empty() {
        return ids;
    }
    systems::tokens(cmd)
        .into_iter()
        .map(|t| normalize(Path::new(&find_rules::expand(&t, es_root))))
        .find(|p| p.to_string_lossy().to_lowercase().ends_with(".exe") && p.is_file())
        .map(|p| vec![format!("EXE:{}", p.display())])
        .unwrap_or_default()
}

fn make_emu(id: &str, rules: &find_rules::Rules, es_root: &Path) -> EmuEntry {
    let mut e = EmuEntry {
        id: id.into(),
        exe: None,
        dir: None,
        bytes: 0,
        copyable: false,
        auto: false,
        note: None,
        optional_dirs: Vec::new(),
    };
    let found = match id.strip_prefix("EXE:") {
        Some(exe) => {
            let exe = PathBuf::from(exe);
            exe.parent().map(|d| find_rules::Found { dir: d.to_path_buf(), exe: exe.clone(), via: "command".into() })
        }
        None => find_rules::resolve(id, rules, es_root),
    };
    let Some(found) = found else {
        e.note = Some(if rules.contains_key(id) { "not installed on this machine" } else { "no find rule for this emulator" }.into());
        return e;
    };
    e.exe = Some(found.exe.clone());
    e.dir = Some(found.dir.clone());
    if let Some(reason) = not_copyable_reason(&found.dir, es_root) {
        e.note = Some(reason.into());
        return e;
    }
    e.copyable = true;
    e.bytes = dir_size(&found.dir, &[], true).1;
    let is_retroarch = found.exe.file_name().is_some_and(|n| n.eq_ignore_ascii_case("retroarch.exe"));
    if is_retroarch {
        e.optional_dirs = retroarch_optional_dirs(&found.dir);
    }
    if is_under(&found.dir, es_root) {
        e.auto = true;
    } else {
        e.auto = !is_program_files(&found.dir);
        e.note = Some(format!("outside the ES-DE folder (found via {})", found.via));
    }
    e
}

/// Cheat database and thumbnail folders of a RetroArch install, as configured in its
/// `retroarch.cfg` (`:` means the RetroArch folder), falling back to the defaults.
fn retroarch_optional_dirs(dir: &Path) -> Vec<OptionalDir> {
    let cfg = std::fs::read_to_string(dir.join("retroarch.cfg")).unwrap_or_default();
    let setting = |key: &str| {
        cfg.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == key).then(|| v.trim().trim_matches('"').to_string())
        })
    };
    [("cheats", "cheat_database_path", "cheats"), ("thumbnails", "thumbnails_directory", "thumbnails")]
        .into_iter()
        .filter_map(|(kind, key, default)| {
            let path = match setting(key).filter(|v| !v.is_empty() && v != "default") {
                Some(v) => match v.strip_prefix(':') {
                    Some(rest) => normalize(&dir.join(rest.trim_start_matches(['\\', '/']))),
                    None => normalize(Path::new(&v)),
                },
                None => dir.join(default),
            };
            // only folders inside the emulator folder are part of its copy
            (path.is_dir() && is_under(&path, dir) && !eq_ci(&path, dir)).then(|| OptionalDir {
                kind: kind.into(),
                bytes: dir_size(&path, &[], true).1,
                path,
            })
        })
        .collect()
}

fn not_copyable_reason(dir: &Path, es_root: &Path) -> Option<&'static str> {
    if dir.parent().is_none() {
        return Some("sits in a drive root, not copied");
    }
    if eq_ci(dir, es_root) {
        return Some("lives in the ES-DE folder itself, copied with ES-DE");
    }
    let windows = ["WINDIR", "SystemRoot"].iter().filter_map(|v| std::env::var_os(v)).map(PathBuf::from);
    for w in windows {
        if is_under(dir, &w) {
            return Some("Windows system component, not copied");
        }
    }
    None
}

fn is_program_files(dir: &Path) -> bool {
    ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
        .iter()
        .filter_map(|v| std::env::var_os(v))
        .any(|p| is_under(dir, Path::new(&p)))
}

pub fn print_catalog(cat: &Catalog) {
    let r = &cat.roots;
    println!("ES-DE folder : {}", r.es_root.display());
    println!("ROM folder   : {}", r.rom_dir.display());
    println!("Media folder : {}", r.media_dir.display());
    for w in &cat.warnings {
        println!("WARNING      : {w}");
    }
    println!("\nSystems ({}):", cat.systems.len());
    for s in &cat.systems {
        println!(
            "  {:<14} {:<45} {:>6} files {:>10}  media {:>10}  emulators: {}{}",
            s.name,
            s.fullname,
            s.rom_files,
            human_bytes(s.rom_bytes),
            human_bytes(s.media_bytes),
            s.emus_in_use.join(", "),
            if s.emus_alt.is_empty() { String::new() } else { format!("  (alt: {})", s.emus_alt.join(", ")) }
        );
    }
    println!("\nEmulators ({}):", cat.emulators.len());
    for e in cat.emulators.values() {
        println!(
            "  {:<22} {:<9} {:>10}  {}{}",
            e.id,
            if e.copyable { "copy" } else { "skip" },
            human_bytes(e.bytes),
            e.dir.as_ref().map(|d| d.display().to_string()).unwrap_or_else(|| "-".into()),
            e.note.as_ref().map(|n| format!("  [{n}]")).unwrap_or_default()
        );
    }
    println!("\nES-DE groups:");
    for g in &cat.globals {
        println!("  {:<16} {:>10}  {}", g.id, human_bytes(g.bytes), g.src.display());
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;

    /// Builds a minimal portable ES-DE with a ROM folder outside of it.
    pub fn fake_install(base: &Path) -> (PathBuf, PathBuf) {
        let es = base.join("ES-DE");
        let roms = base.join("Games").join("ROMs");
        let w = |p: PathBuf, s: &str| {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, s).unwrap();
        };
        w(es.join("ES-DE.exe"), "exe");
        w(es.join(r"resources\systems\windows\es_systems.xml"), r#"<systemList>
  <system><name>snes</name><fullname>Super Nintendo</fullname><path>%ROMPATH%\snes</path>
    <command label="Snes9x">%EMULATOR_RETROARCH% -L %CORE_RETROARCH%\snes9x_libretro.dll %ROM%</command>
    <command label="bsnes">%EMULATOR_BSNES% %ROM%</command></system>
  <system><name>psx</name><fullname>PlayStation</fullname><path>%ROMPATH%\psx</path>
    <command label="DuckStation">%EMULATOR_DUCKSTATION% %ROM%</command></system>
  <system><name>empty</name><fullname>Empty</fullname><path>%ROMPATH%\empty</path>
    <command label="x">%EMULATOR_RETROARCH% %ROM%</command></system>
</systemList>"#);
        w(es.join(r"resources\systems\windows\es_find_rules.xml"), r#"<ruleList>
  <emulator name="RETROARCH"><rule type="staticpath"><entry>%ESPATH%\Emulators\RetroArch-Win64\retroarch.exe</entry></rule></emulator>
  <emulator name="BSNES"><rule type="staticpath"><entry>%ESPATH%\Emulators\bsnes\bsnes.exe</entry></rule></emulator>
  <emulator name="DUCKSTATION"><rule type="staticpath"><entry>%ESPATH%\Emulators\duckstation\duckstation.exe</entry></rule></emulator>
</ruleList>"#);
        w(es.join(r"ES-DE\settings\es_settings.xml"), &format!(
            "<?xml version=\"1.0\"?>\n<string name=\"ROMDirectory\" value=\"{}\" />\n<string name=\"MediaDirectory\" value=\"\" />\n",
            roms.display()
        ));
        w(es.join(r"Emulators\RetroArch-Win64\retroarch.exe"), "ra");
        w(es.join(r"Emulators\RetroArch-Win64\system\bios.bin"), "bios");
        // custom cheat folder via retroarch.cfg, thumbnails at the default location
        w(es.join(r"Emulators\RetroArch-Win64\retroarch.cfg"), "cheat_database_path = \":\\cheatdb\"\nthumbnails_directory = \"default\"\n");
        w(es.join(r"Emulators\RetroArch-Win64\cheatdb\Nintendo - SNES\Mario.cht"), "cheat");
        w(es.join(r"Emulators\RetroArch-Win64\thumbnails\Nintendo - SNES\Named_Boxarts\Mario.png"), "thumb");
        w(es.join(r"Emulators\bsnes\bsnes.exe"), "bsnes");
        w(es.join(r"ES-DE\gamelists\snes\gamelist.xml"), "<gameList><game><path>./a.sfc</path><altemulator>bsnes</altemulator></game></gameList>");
        w(es.join(r"ES-DE\downloaded_media\snes\screenshots\a.png"), "png");
        w(es.join(r"ES-DE\themes\t\theme.xml"), "theme");
        w(roms.join(r"snes\a.sfc"), "rom-a");
        w(roms.join(r"snes\systeminfo.txt"), "info");
        w(roms.join(r"psx\b.chd"), "rom-b");
        w(roms.join(r"empty\systeminfo.txt"), "info");
        w(roms.join("systems.txt"), "snes: Super Nintendo");
        (es, roms)
    }

    #[test]
    fn scans_fake_install() {
        let tmp = tempfile::tempdir().unwrap();
        let (es, roms) = fake_install(tmp.path());
        let cat = scan(&es, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(eq_ci(&cat.roots.rom_dir, &roms));
        let names: Vec<_> = cat.systems.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["psx", "snes"]);
        let snes = cat.systems.iter().find(|s| s.name == "snes").unwrap();
        assert_eq!(snes.rom_files, 1);
        // per-game altemulator puts bsnes in use too
        assert_eq!(snes.emus_in_use, vec!["BSNES", "RETROARCH"]);
        assert!(snes.media_dir.is_some() && snes.gamelist_dir.is_some());
        assert!(cat.emulators["RETROARCH"].copyable);
        let opt = &cat.emulators["RETROARCH"].optional_dirs;
        let kinds: Vec<_> = opt.iter().map(|o| (o.kind.as_str(), o.path.file_name().unwrap().to_string_lossy().to_string())).collect();
        assert_eq!(kinds, vec![("cheats", "cheatdb".to_string()), ("thumbnails", "thumbnails".to_string())]);
        assert!(opt.iter().all(|o| o.bytes > 0));
        assert!(cat.emulators["BSNES"].optional_dirs.is_empty());
        assert!(!cat.emulators["DUCKSTATION"].copyable);
        let prog = cat.globals.iter().find(|g| g.id == "esde_program").unwrap();
        // ES-DE.exe + resources only: Emulators, ROMs and the ES-DE data folder are excluded
        assert_eq!(prog.bytes, 3 + dir_size(&es.join("resources"), &[], true).1);
    }
}

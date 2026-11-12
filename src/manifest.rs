//! `<drive>\EMUAssistant\manifest.json`: what was exported, from where. Deploy mode reads it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::eq_ci;
use crate::plan::Selection;
use crate::scan::Catalog;

pub const DIR: &str = "EMUAssistant";
/// Folders used by earlier versions, newest first.
const LEGACY_DIRS: [&str; 2] = ["EMU Assistant", "emuasist"];
pub const EXE_NAME: &str = "EMUAssistant.exe";
/// Name of the exe copied to the drive by earlier versions.
pub const LEGACY_EXE_NAME: &str = "EMU Assistant.exe";
const FORMAT: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Manifest {
    pub format: u32,
    pub tool_version: String,
    pub source_machine: String,
    pub updated_unix: u64,
    /// False while an export is running or after it was cancelled / had errors.
    pub complete: bool,
    pub catalog: Catalog,
    /// Selections exported under a profile name, so Deploy can pre-tick one device's part.
    #[serde(default)]
    pub profiles: BTreeMap<String, Selection>,
}

pub fn path(root: &Path) -> PathBuf {
    root.join(DIR).join("manifest.json")
}

/// The manifest to read: the current location, or a pre-rename one.
fn existing_path(root: &Path) -> Option<PathBuf> {
    std::iter::once(path(root))
        .chain(LEGACY_DIRS.iter().map(|d| root.join(d).join("manifest.json")))
        .find(|p| p.is_file())
}

/// Renames a package folder left by an earlier version to [`DIR`].
fn migrate_legacy(root: &Path) {
    let dir = root.join(DIR);
    if dir.exists() {
        return;
    }
    if let Some(old) = LEGACY_DIRS.iter().map(|d| root.join(d)).find(|d| d.join("manifest.json").is_file()) {
        let _ = std::fs::rename(old, dir);
    }
}

pub fn exists(root: &Path) -> bool {
    existing_path(root).is_some()
}

pub fn load(root: &Path) -> Result<Option<Manifest>> {
    let Some(p) = existing_path(root) else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    Ok(Some(serde_json::from_str(&text).with_context(|| format!("parsing {}", p.display()))?))
}

pub fn save(root: &Path, m: &Manifest) -> Result<()> {
    migrate_legacy(root);
    let p = path(root);
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(m)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Merges this export's selection into the manifest already on the drive (if it came
/// from the same ES-DE folder), so repeated exports of different systems accumulate.
/// With a `profile`, the selection is recorded under that name too.
pub fn for_export(existing: Option<Manifest>, cat: &Catalog, sel: &Selection, profile: Option<&str>) -> Manifest {
    let (mut base, mut profiles) = match existing {
        Some(m) if eq_ci(&m.catalog.roots.es_root, &cat.roots.es_root) => (m.catalog, m.profiles),
        _ => (Catalog::default(), BTreeMap::new()),
    };
    if let Some(name) = profile {
        profiles.insert(name.to_string(), sel.clone());
    }
    base.roots = cat.roots.clone();
    base.warnings.clear();

    for s in cat.systems.iter().filter(|s| sel.systems.contains(&s.name)) {
        let mut s = s.clone();
        let old = base.systems.iter().position(|o| o.name == s.name);
        let prev = old.map(|i| base.systems[i].clone());
        // only the picked games go to the drive
        if let (Some(picked), Some(list)) = (sel.games.get(&s.name), &s.games) {
            (s.rom_files, s.rom_bytes) = list.picked(picked);
        }
        if !sel.media {
            s.media_dir = prev.as_ref().and_then(|p| p.media_dir.clone());
            s.media_bytes = prev.as_ref().map_or(0, |p| p.media_bytes);
        }
        if !sel.gamelists {
            s.gamelist_dir = prev.as_ref().and_then(|p| p.gamelist_dir.clone());
            s.gamelist_bytes = prev.as_ref().map_or(0, |p| p.gamelist_bytes);
        }
        match old {
            Some(i) => base.systems[i] = s,
            None => base.systems.push(s),
        }
    }
    base.systems.sort_by_key(|s| s.fullname.to_lowercase());
    for id in &sel.emulators {
        if let Some(e) = cat.emulators.get(id).filter(|e| e.copyable) {
            base.emulators.insert(id.clone(), e.clone());
        }
    }
    for g in cat.globals.iter().filter(|g| sel.globals.contains(&g.id)) {
        base.globals.retain(|o| o.id != g.id);
        base.globals.push(g.clone());
    }

    Manifest {
        format: FORMAT,
        tool_version: env!("CARGO_PKG_VERSION").into(),
        source_machine: std::env::var("COMPUTERNAME").unwrap_or_default(),
        updated_unix: now(),
        complete: false,
        catalog: base,
        profiles,
    }
}

pub fn age_text(m: &Manifest) -> String {
    let secs = now().saturating_sub(m.updated_unix);
    match secs {
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86400 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{scan, tests::fake_install};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn merges_successive_exports() {
        let tmp = tempfile::tempdir().unwrap();
        let (es, _) = fake_install(tmp.path());
        let cat = scan(&es, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let mut sel = Selection { media: true, gamelists: true, ..Default::default() };
        sel.systems.insert("snes".into());
        sel.emulators.insert("RETROARCH".into());
        let m1 = for_export(None, &cat, &sel, Some("Ally"));
        assert_eq!(m1.catalog.systems.len(), 1);

        let mut sel2 = Selection::default();
        sel2.systems.insert("psx".into());
        sel2.systems.insert("snes".into());
        let m2 = for_export(Some(m1), &cat, &sel2, Some("Legion"));
        assert_eq!(m2.catalog.systems.len(), 2);
        // media exported earlier stays recorded even though this export skipped media
        let snes = m2.catalog.systems.iter().find(|s| s.name == "snes").unwrap();
        assert!(snes.media_dir.is_some());
        assert!(m2.catalog.emulators.contains_key("RETROARCH"));

        // both devices' selections are on the drive
        assert_eq!(m2.profiles.keys().collect::<Vec<_>>(), ["Ally", "Legion"]);
        assert_eq!(m2.profiles["Ally"].systems, ["snes".to_string()].into());
        assert!(m2.profiles["Ally"].emulators.contains("RETROARCH"));
        assert_eq!(m2.profiles["Legion"].systems.len(), 2);
        // an export without a profile leaves them alone
        let m3 = for_export(Some(m2.clone()), &cat, &sel2, None);
        assert_eq!(m3.profiles.len(), 2);

        save(tmp.path(), &m2).unwrap();
        let back = load(tmp.path()).unwrap().unwrap();
        assert_eq!((back.catalog.systems.len(), back.profiles.len()), (2, 2));
        // a manifest written before profiles still reads
        let mut old: serde_json::Value = serde_json::to_value(&back).unwrap();
        old.as_object_mut().unwrap().remove("profiles");
        assert!(serde_json::from_value::<Manifest>(old).unwrap().profiles.is_empty());
    }

    #[test]
    fn reads_pre_rename_location() {
        let tmp = tempfile::tempdir().unwrap();
        let m = for_export(None, &Catalog::default(), &Selection::default(), None);
        for legacy in LEGACY_DIRS {
            let root = tmp.path().join(legacy.replace(' ', "_"));
            let dir = root.join(legacy);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("manifest.json"), serde_json::to_string(&m).unwrap()).unwrap();
            assert!(exists(&root));
            assert!(load(&root).unwrap().is_some());
            // the next save moves the package to the new folder name
            save(&root, &m).unwrap();
            assert!(!dir.exists());
            assert!(path(&root).is_file());
        }
    }
}

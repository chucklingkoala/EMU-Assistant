//! Deploy-side helpers: pointing the target ES-DE at the (possibly relocated) ROM and media folders.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::esde::{read_text, settings};
use crate::paths::{eq_ci, strip_prefix_ci};
use crate::scan::Roots;

/// Rewrites ROMDirectory / MediaDirectory in the target es_settings.xml when they would
/// not resolve to the target folders. Keeps a `.emuasist-bak` copy of the original.
pub fn fix_settings(target: &Roots) -> Result<Vec<String>> {
    let path = target.es_root.join(r"ES-DE\settings\es_settings.xml");
    if !path.is_file() {
        return Ok(vec![format!("{} not found, ES-DE will ask for the ROM folder on first start", path.display())]);
    }
    let text = read_text(&path)?;
    let st = settings::parse(&text);
    let get = |k: &str| st.get(k).map(String::as_str).unwrap_or("");
    let cur_rom = settings::resolve_dir(get("ROMDirectory"), &target.es_root, None, "ROMs");
    let cur_media =
        settings::resolve_dir(get("MediaDirectory"), &target.es_root, Some(&cur_rom), r"ES-DE\downloaded_media");

    let mut updates = Vec::new();
    if !eq_ci(&cur_rom, &target.rom_dir) {
        updates.push(("ROMDirectory", target.rom_dir.display().to_string()));
    }
    if !eq_ci(&cur_media, &target.media_dir) {
        updates.push(("MediaDirectory", target.media_dir.display().to_string()));
    }
    if updates.is_empty() {
        return Ok(vec!["es_settings.xml already points at the target folders".into()]);
    }
    std::fs::copy(&path, path.with_extension("xml.emuasist-bak"))?;
    std::fs::write(&path, settings::patch_text(&text, &updates))?;
    Ok(updates.iter().map(|(k, v)| format!("es_settings.xml: {k} set to {v}")).collect())
}

/// When the target ES-DE folder is edited, folders that lived inside the original ES-DE
/// folder follow along, unless the user already pointed them somewhere else.
pub fn follow(orig_x: &Path, orig_es: &Path, old_es: &Path, new_es: &Path, cur: &Path) -> Option<PathBuf> {
    let rel = strip_prefix_ci(orig_x, orig_es)?;
    eq_ci(cur, &old_es.join(&rel)).then(|| new_es.join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patches_relocated_rom_dir_only() {
        let tmp = tempfile::tempdir().unwrap();
        let es = tmp.path().join("ES-DE");
        let st = es.join(r"ES-DE\settings\es_settings.xml");
        std::fs::create_dir_all(st.parent().unwrap()).unwrap();
        std::fs::write(&st, "<?xml version=\"1.0\"?>\n<string name=\"ROMDirectory\" value=\"D:\\Games\\ROMs\" />\n<string name=\"MediaDirectory\" value=\"\" />\n").unwrap();
        let target = Roots {
            es_root: es.clone(),
            rom_dir: tmp.path().join("ROMs"),
            media_dir: es.join(r"ES-DE\downloaded_media"),
        };
        let msgs = fix_settings(&target).unwrap();
        assert_eq!(msgs.len(), 1);
        let s = settings::read(&st).unwrap();
        assert!(eq_ci(Path::new(&s["ROMDirectory"]), &target.rom_dir));
        assert_eq!(s["MediaDirectory"], "");
        assert!(st.with_extension("xml.emuasist-bak").is_file());
        // idempotent
        assert_eq!(fix_settings(&target).unwrap().len(), 1);
        assert!(fix_settings(&target).unwrap()[0].contains("already"));
    }

    #[test]
    fn follows_es_root() {
        let got = follow(
            Path::new(r"D:\ES-DE\ROMs"),
            Path::new(r"D:\ES-DE"),
            Path::new(r"D:\ES-DE"),
            Path::new(r"C:\Emu\ES-DE"),
            Path::new(r"D:\ES-DE\ROMs"),
        );
        assert_eq!(got, Some(PathBuf::from(r"C:\Emu\ES-DE\ROMs")));
        assert_eq!(follow(Path::new(r"X:\ROMs"), Path::new(r"D:\ES-DE"), Path::new(r"D:\ES-DE"), Path::new(r"C:\E"), Path::new(r"X:\ROMs")), None);
    }
}

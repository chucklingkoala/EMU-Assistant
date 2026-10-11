//! Remembers the last used folders and selection in `%APPDATA%\EMU Assistant\config.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct Config {
    pub es_root: Option<PathBuf>,
    pub dest: Option<PathBuf>,
    pub systems: BTreeSet<String>,
    pub globals: Option<BTreeSet<String>>,
    pub media: Option<bool>,
    pub gamelists: Option<bool>,
    pub emu_override: BTreeMap<String, bool>,
    pub exclude_cheats: Option<bool>,
    pub exclude_thumbnails: Option<bool>,
}

fn path_in(dir: &str) -> Option<PathBuf> {
    Some(dirs::config_dir()?.join(dir).join("config.json"))
}

fn path() -> Option<PathBuf> {
    path_in("EMU Assistant")
}

/// Loads the config, falling back to the one written before the rename.
pub fn load() -> Config {
    [path(), path_in("EMUAsist")]
        .into_iter()
        .flatten()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(cfg: &Config) {
    if let Some(p) = path() {
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        if let Ok(s) = serde_json::to_string_pretty(cfg) {
            let _ = std::fs::write(p, s);
        }
    }
}

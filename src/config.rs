//! Remembers the last used folders, selection and settings in `%APPDATA%\EMUAssistant\config.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::update::sources::SourceSpec;

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
    // settings (unset means the default)
    pub verify: Option<bool>,
    pub protect_newer: Option<bool>,
    pub copy_exe: Option<bool>,
    pub fix_settings: Option<bool>,
    pub update_backup: Option<bool>,
    pub backup_dir: Option<PathBuf>,
    pub keep_backups: Option<u32>,
    pub update_cores: Option<bool>,
    pub github_token: Option<String>,
    /// Update sources edited by the user, replacing the built-in ones by program id.
    pub update_sources: BTreeMap<String, SourceSpec>,
    /// Programs ticked in Update mode.
    pub update_selected: BTreeSet<String>,
}

impl Config {
    pub fn verify(&self) -> bool {
        self.verify.unwrap_or(false)
    }
    pub fn protect_newer(&self) -> bool {
        self.protect_newer.unwrap_or(true)
    }
    pub fn copy_exe(&self) -> bool {
        self.copy_exe.unwrap_or(true)
    }
    pub fn fix_settings(&self) -> bool {
        self.fix_settings.unwrap_or(true)
    }
    pub fn update_backup(&self) -> bool {
        self.update_backup.unwrap_or(true)
    }
    pub fn keep_backups(&self) -> u32 {
        self.keep_backups.unwrap_or(3)
    }
    pub fn update_cores(&self) -> bool {
        self.update_cores.unwrap_or(true)
    }
    /// The configured backup folder, or `<ES-DE>\EMUAssistant\backups`.
    pub fn backup_dir(&self, es_root: &Path) -> PathBuf {
        self.backup_dir.clone().unwrap_or_else(|| default_backup_dir(es_root))
    }
    pub fn update_opts(&self, es_root: &Path) -> crate::update::Opts {
        crate::update::Opts {
            backup: self.update_backup(),
            backup_dir: self.backup_dir(es_root),
            keep: self.keep_backups(),
            cores: self.update_cores(),
            github_token: self.github_token(),
            sources: self.update_sources.clone(),
        }
    }
    /// `GITHUB_TOKEN` wins over the token saved in the settings.
    pub fn github_token(&self) -> Option<String> {
        std::env::var("GITHUB_TOKEN").ok().or_else(|| self.github_token.clone()).filter(|t| !t.trim().is_empty())
    }
}

pub fn default_backup_dir(es_root: &Path) -> PathBuf {
    es_root.join(crate::manifest::DIR).join("backups")
}

fn path_in(dir: &str) -> Option<PathBuf> {
    Some(dirs::config_dir()?.join(dir).join("config.json"))
}

fn path() -> Option<PathBuf> {
    path_in("EMUAssistant")
}

/// Loads the config, falling back to the ones written by earlier versions.
pub fn load() -> Config {
    [path(), path_in("EMU Assistant"), path_in("EMUAsist")]
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

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
    /// Games picked per system (system -> game keys); systems not listed copy all games.
    pub games: BTreeMap<String, BTreeSet<String>>,
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
    pub check_app_updates: Option<bool>,
    /// The EMU Assistant exe installed on this machine (the last one run outside a drive).
    pub home_exe: Option<PathBuf>,
    /// Named Export selections, one per target device. The selection fields above are the
    /// working copy of the active one.
    pub profiles: BTreeMap<String, Profile>,
    pub active_profile: Option<String>,
    /// On a target machine: the profile last deployed here.
    pub device_profile: Option<String>,
}

/// What a profile remembers: the Export selection and the destination.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Profile {
    pub dest: Option<PathBuf>,
    pub systems: BTreeSet<String>,
    pub globals: Option<BTreeSet<String>>,
    pub media: Option<bool>,
    pub gamelists: Option<bool>,
    pub emu_override: BTreeMap<String, bool>,
    pub games: BTreeMap<String, BTreeSet<String>>,
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
    pub fn check_app_updates(&self) -> bool {
        self.check_app_updates.unwrap_or(true)
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
    /// The working selection as a profile.
    pub fn snapshot(&self) -> Profile {
        Profile {
            dest: self.dest.clone(),
            systems: self.systems.clone(),
            globals: self.globals.clone(),
            media: self.media,
            gamelists: self.gamelists,
            emu_override: self.emu_override.clone(),
            games: self.games.clone(),
        }
    }

    /// Makes `p` the working selection.
    pub fn apply(&mut self, p: &Profile) {
        self.dest = p.dest.clone();
        self.systems = p.systems.clone();
        self.globals = p.globals.clone();
        self.media = p.media;
        self.gamelists = p.gamelists;
        self.emu_override = p.emu_override.clone();
        self.games = p.games.clone();
    }

    /// Stores the working selection under the active profile, then makes `name` active and
    /// loads its selection. `None` (or an unknown name) detaches: the selection stays as it is.
    pub fn switch_profile(&mut self, name: Option<&str>) {
        self.store_active();
        let next = name.and_then(|n| self.profiles.get(n).cloned());
        if let Some(p) = &next {
            self.apply(p);
        }
        self.active_profile = next.and(name.map(String::from));
    }

    /// Saves the working selection under the active profile, if there is one.
    pub fn store_active(&mut self) {
        if let Some(name) = self.active_profile.clone() {
            self.profiles.insert(name, self.snapshot());
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
    let Some(p) = path() else { return };
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let Ok(s) = serde_json::to_string_pretty(cfg) else { return };
    // write a temp file first so a crash can't leave a half-written config
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, s).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_switch_and_keep_their_selection() {
        let mut c = Config { systems: ["snes".to_string()].into(), dest: Some(PathBuf::from(r"E:\")), ..Default::default() };
        // the first profile is the current selection
        c.profiles.insert("Ally".into(), c.snapshot());
        c.active_profile = Some("Ally".into());
        c.profiles.insert("Legion".into(), Profile { systems: ["psx".to_string()].into(), dest: Some(PathBuf::from(r"F:\")), ..Default::default() });

        // a change while Ally is active stays with Ally
        c.media = Some(false);
        c.switch_profile(Some("Legion"));
        assert_eq!(c.systems, ["psx".to_string()].into());
        assert_eq!(c.dest, Some(PathBuf::from(r"F:\")));
        assert_eq!(c.media, None);
        assert_eq!(c.active_profile.as_deref(), Some("Legion"));

        c.switch_profile(Some("Ally"));
        assert_eq!(c.systems, ["snes".to_string()].into());
        assert_eq!(c.media, Some(false));

        // detaching keeps the selection; unknown names detach too
        c.switch_profile(None);
        assert_eq!((c.active_profile.as_deref(), c.systems.len()), (None, 1));
        c.switch_profile(Some("nope"));
        assert_eq!(c.active_profile, None);

        // a config from before profiles still loads
        let old: Config = serde_json::from_str(r#"{"systems":["snes"],"dest":"E:\\"}"#).unwrap();
        assert!(old.profiles.is_empty() && old.active_profile.is_none());
    }
}

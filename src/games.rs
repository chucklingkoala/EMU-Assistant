//! Splits one system's ROM folder into games that can be picked one by one. Updates and
//! DLC kept in an add-on folder such as `_Updates` are attached to the game they belong to,
//! matched by title ID / serial first and by name otherwise.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::fsutil::{walk, PART_EXT};
use crate::paths::long;

#[derive(Clone, Debug, Default)]
pub struct Game {
    /// Relative path of the game's main entry, e.g. `Game [0100..][v0].nsp` or, for an
    /// add-on without a game, `_Updates\Other [..].nsp`. This is what the selection stores.
    pub key: String,
    /// Top-level entries (relative to the system folder) that make up this game.
    pub paths: Vec<PathBuf>,
    /// Lowercase names ES-DE gives this game's media files.
    pub stems: Vec<String>,
    /// Update / DLC entries attached from add-on folders.
    pub extras: Vec<String>,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default)]
pub struct GameList {
    pub games: Vec<Game>,
    /// Entries copied with any selection (`_Firmware`, `_Keys`, systeminfo.txt).
    pub shared: Vec<PathBuf>,
    pub shared_files: u64,
    pub shared_bytes: u64,
    /// Game files (ES-DE's helper files left out) and bytes of the whole folder.
    pub files: u64,
    pub bytes: u64,
}

impl GameList {
    /// Files and bytes copied when only `picked` games are selected, shared entries included.
    pub fn picked(&self, picked: &BTreeSet<String>) -> (u64, u64) {
        self.games
            .iter()
            .filter(|g| picked.contains(&g.key))
            .fold((self.shared_files, self.shared_bytes), |(f, b), g| (f + g.files, b + g.bytes))
    }

    pub fn picked_count(&self, picked: &BTreeSet<String>) -> usize {
        self.games.iter().filter(|g| picked.contains(&g.key)).count()
    }

    /// Paths and media names of the games that are not picked.
    pub fn left_out(&self, picked: &BTreeSet<String>) -> (Vec<PathBuf>, HashSet<String>) {
        let mut paths = Vec::new();
        let mut stems = HashSet::new();
        let mut keep = HashSet::new();
        for g in &self.games {
            if picked.contains(&g.key) {
                keep.extend(g.stems.iter().cloned());
            } else {
                paths.extend(g.paths.iter().cloned());
                stems.extend(g.stems.iter().cloned());
            }
        }
        stems.retain(|s| !keep.contains(s));
        (paths, stems)
    }
}

/// The name ES-DE uses for a ROM's media files: the file name up to its last dot, lowercased.
pub fn media_stem(name: &str) -> String {
    name.rsplit_once('.').map_or(name, |(s, _)| s).to_lowercase()
}

fn is_helper(name: &str) -> bool {
    name.eq_ignore_ascii_case("systeminfo.txt") || name.eq_ignore_ascii_case("noload.txt")
}

/// `Some(true)` for an add-on folder (`_Updates`, `_DLC`), `Some(false)` for another
/// support folder copied as a whole (`_Firmware`, `_Keys`), `None` for a game folder.
fn support_folder(name: &str) -> Option<bool> {
    let l = name.to_lowercase();
    let addon = ["update", "dlc", "patch"].iter().any(|w| l.contains(w));
    if l.starts_with('_') {
        Some(addon)
    } else if matches!(l.as_str(), "update" | "updates" | "dlc" | "dlcs" | "patch" | "patches") {
        Some(true)
    } else {
        None
    }
}

/// A name with a short file extension (`.nsp`, `.ps3`) removed.
fn strip_ext(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && (1..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()) => stem,
        _ => name,
    }
}

/// Lowercase words of a name without its `[..]` / `(..)` tags and punctuation.
fn norm(name: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    for c in name.chars() {
        match c {
            '[' | '(' => {
                depth += 1;
                out.push(' ');
            }
            ']' | ')' => {
                depth = depth.saturating_sub(1);
                out.push(' ');
            }
            _ if depth > 0 => {}
            '\'' | '’' | '‘' => {}
            c if c.is_alphanumeric() => out.extend(c.to_lowercase()),
            _ => out.push(' '),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether `prefix`'s words start `name`'s words.
fn word_prefix(prefix: &str, name: &str) -> bool {
    !prefix.is_empty() && (name == prefix || name.strip_prefix(prefix).is_some_and(|r| r.starts_with(' ')))
}

/// Title IDs in a name, reduced to the game they belong to: Switch / Switch 2 title IDs
/// (16 hex digits; base, update and DLC IDs share all but the low 13 bits) and
/// PlayStation serials (`CUSA00001`, `BLUS-30001`).
fn ids(name: &str) -> Vec<String> {
    let c: Vec<char> = name.chars().collect();
    let alnum = |i: usize| c.get(i).is_some_and(|c| c.is_alphanumeric());
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if !c[i].is_ascii_alphanumeric() || (i > 0 && alnum(i - 1)) {
            i += 1;
            continue;
        }
        let run = c[i..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
        if run == 16 && !alnum(i + 16) {
            let hex: String = c[i..i + 16].iter().collect();
            if let Ok(v) = u64::from_str_radix(&hex, 16) {
                out.push(format!("{:016X}", v & !0x1FFF));
            }
            i += 16;
            continue;
        }
        let letters = c[i..].iter().take(4).filter(|c| c.is_ascii_alphabetic()).count();
        if letters == 4 {
            let mut j = i + 4;
            if matches!(c.get(j), Some('-' | '_')) {
                j += 1;
            }
            let digits = c[j.min(c.len())..].iter().take_while(|c| c.is_ascii_digit()).count();
            if digits == 5 && !c.get(j + 5).is_some_and(|c| c.is_ascii_digit()) {
                let prefix: String = c[i..i + 4].iter().collect::<String>().to_uppercase();
                let num: String = c[j..j + 5].iter().collect();
                out.push(format!("{prefix}{num}"));
                i = j + 5;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn marks_addon(name: &str) -> bool {
    let n = norm(name);
    n.split(' ').any(|w| matches!(w, "update" | "upd" | "patch" | "dlc"))
        || name.to_lowercase().contains("-update")
        || name.to_lowercase().contains("-patch")
}

/// One file or folder directly in the system folder or in an add-on folder.
struct Entry {
    rel: PathBuf,
    name: String,
    is_dir: bool,
    files: u64,
    bytes: u64,
    /// IDs of the files inside a folder (only gathered for add-on folders).
    inner_ids: Vec<String>,
}

fn entries(dir: &Path, rel: &Path, inner_ids: bool) -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir(long(dir)) else { return Vec::new() };
    let mut out: Vec<Entry> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(PART_EXT) {
                return None;
            }
            let path = dir.join(&name);
            // the directory listing already carries type and size; only links need a second look
            let meta = match e.metadata() {
                Ok(m) if !m.file_type().is_symlink() => m,
                _ => std::fs::metadata(long(&path)).ok()?,
            };
            let mut entry = Entry { rel: rel.join(&name), is_dir: meta.is_dir(), files: 0, bytes: 0, inner_ids: Vec::new(), name };
            if meta.is_dir() {
                let mut errors = Vec::new();
                walk(&path, &[], true, &mut errors, |p, m| {
                    if m.is_file() {
                        let n = p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
                        entry.bytes += m.len();
                        if !is_helper(&n) {
                            entry.files += 1;
                        }
                        if inner_ids {
                            entry.inner_ids.extend(ids(&n));
                        }
                    }
                });
            } else if meta.is_file() {
                entry.bytes = meta.len();
                entry.files = u64::from(!is_helper(&entry.name));
            } else {
                return None;
            }
            Some(entry)
        })
        .collect();
    out.sort_by_key(|e| e.name.to_lowercase());
    out
}

/// Files referenced by a `.cue`, `.gdi` or `.m3u` file, as written in it.
fn references(path: &Path) -> Vec<String> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if !matches!(ext.as_str(), "cue" | "gdi" | "m3u") {
        return Vec::new();
    }
    let text = match std::fs::metadata(long(path)) {
        Ok(m) if m.len() < 1 << 20 => std::fs::read_to_string(long(path)).unwrap_or_default(),
        _ => return Vec::new(),
    };
    let quoted = |l: &str| l.split_once('"').and_then(|(_, r)| r.split_once('"')).map(|(q, _)| q.to_string());
    text.lines()
        .map(str::trim)
        .filter_map(|l| match ext.as_str() {
            "cue" => {
                let rest = l.strip_prefix("FILE ").or_else(|| l.strip_prefix("file "))?;
                quoted(rest).or_else(|| rest.split_whitespace().next().map(String::from))
            }
            "gdi" => quoted(l).or_else(|| l.split_whitespace().nth(4).map(String::from)),
            _ => (!l.is_empty() && !l.starts_with('#')).then(|| l.to_string()),
        })
        .collect()
}

/// A game being assembled.
#[derive(Default)]
struct Unit {
    key: String,
    paths: Vec<PathBuf>,
    stems: BTreeSet<String>,
    extras: Vec<String>,
    files: u64,
    bytes: u64,
    ids: BTreeSet<String>,
    norm: String,
    addon: bool,
    alive: bool,
}

impl Unit {
    fn new(e: &Entry, ids_of: Vec<String>) -> Self {
        let mut stems = BTreeSet::from([media_stem(&e.name)]);
        if e.is_dir {
            stems.insert(e.name.to_lowercase());
        }
        Unit {
            key: e.rel.to_string_lossy().into_owned(),
            paths: vec![e.rel.clone()],
            stems,
            extras: Vec::new(),
            files: e.files,
            bytes: e.bytes,
            ids: ids_of.into_iter().collect(),
            norm: norm(strip_ext(&e.name)),
            addon: marks_addon(&e.name),
            alive: true,
        }
    }

    fn absorb(&mut self, o: Unit) {
        self.paths.extend(o.paths);
        self.stems.extend(o.stems);
        self.extras.extend(o.extras);
        self.files += o.files;
        self.bytes += o.bytes;
        self.ids.extend(o.ids);
    }

    fn attach(&mut self, e: &Entry, ids_of: &[String]) {
        self.paths.push(e.rel.clone());
        self.extras.push(e.name.clone());
        self.files += e.files;
        self.bytes += e.bytes;
        self.ids.extend(ids_of.iter().cloned());
    }
}

/// Moves unit `from` into unit `into`.
fn merge(units: &mut [Unit], into: usize, from: usize) {
    if into == from || !units[from].alive || !units[into].alive {
        return;
    }
    let taken = std::mem::take(&mut units[from]);
    units[into].absorb(taken);
}

pub fn list(dir: &Path) -> GameList {
    let mut out = GameList::default();
    let mut units: Vec<Unit> = Vec::new();
    // lowercase top-level name -> unit index
    let mut owner: HashMap<String, usize> = HashMap::new();
    let mut by_stem: HashMap<String, usize> = HashMap::new();
    let mut addon_dirs = Vec::new();

    for e in entries(dir, Path::new(""), false) {
        out.files += e.files;
        out.bytes += e.bytes;
        let support = if e.is_dir { support_folder(&e.name) } else { None };
        if is_helper(&e.name) || support == Some(false) {
            out.shared_files += e.files;
            out.shared_bytes += e.bytes;
            out.shared.push(e.rel);
            continue;
        }
        if support == Some(true) {
            addon_dirs.push(e);
            continue;
        }
        let idx = if e.is_dir {
            None
        } else {
            by_stem.get(&media_stem(&e.name)).copied()
        };
        let i = match idx {
            Some(i) => {
                let u = Unit::new(&e, ids(&e.name));
                units[i].absorb(u);
                // a playlist or cue sheet names the whole game
                if references_kind(&e.name) && !references_kind(&units[i].key) {
                    units[i].key = e.rel.to_string_lossy().into_owned();
                }
                i
            }
            None => {
                units.push(Unit::new(&e, ids(&e.name)));
                let i = units.len() - 1;
                if !e.is_dir {
                    by_stem.insert(media_stem(&e.name), i);
                }
                i
            }
        };
        owner.insert(e.name.to_lowercase(), i);
    }

    // cue sheets, gdi files and playlists pull in the files they name
    for i in 0..units.len() {
        let refs: Vec<String> = units[i]
            .paths
            .clone()
            .iter()
            .filter(|p| references_kind(&p.to_string_lossy()))
            .flat_map(|p| references(&dir.join(p)))
            .collect();
        for r in refs {
            let first = r.replace('/', "\\");
            let first = first.trim_start_matches(".\\").split('\\').next().unwrap_or_default().to_lowercase();
            if let Some(&j) = owner.get(&first) {
                if units[j].alive && units[i].alive && j != i {
                    merge(&mut units, i, j);
                    for v in owner.values_mut().filter(|v| **v == j) {
                        *v = i;
                    }
                }
            }
        }
    }

    // a root entry marked as update / DLC goes with the game sharing its ID (shadPS4's CUSA00001-UPDATE)
    for i in 0..units.len() {
        if !units[i].alive || !units[i].addon || units[i].ids.is_empty() {
            continue;
        }
        let base = (0..units.len()).find(|&j| j != i && units[j].alive && !units[j].addon && !units[j].ids.is_disjoint(&units[i].ids));
        if let Some(j) = base {
            let extra = units[i].key.clone();
            merge(&mut units, j, i);
            units[j].extras.push(extra);
        }
    }
    units.retain(|u| u.alive);
    let games = units.len();

    let mut pending: Vec<(Entry, Vec<String>, String)> = addon_dirs
        .iter()
        .flat_map(|d| entries(&dir.join(&d.name), &d.rel, true))
        .map(|e| {
            let mut i = ids(&e.name);
            if i.is_empty() {
                i = e.inner_ids.clone();
            }
            let n = norm(strip_ext(&e.name));
            (e, i, n)
        })
        .collect();
    // repeat while IDs learned from name matches attach more
    loop {
        let before = pending.len();
        pending.retain(|(e, eids, n)| {
            let by_id = units[..games].iter().position(|u| eids.iter().any(|id| u.ids.contains(id)));
            let by_name = || {
                units[..games]
                    .iter()
                    .enumerate()
                    // an add-on whose ID belongs to no known game is another game's
                    .filter(|(_, u)| eids.is_empty() || u.ids.is_empty())
                    .filter(|(_, u)| word_prefix(&u.norm, n))
                    .max_by_key(|(_, u)| u.norm.len())
                    .map(|(i, _)| i)
            };
            match by_id.or_else(by_name) {
                Some(i) => {
                    units[i].attach(e, eids);
                    false
                }
                None => true,
            }
        });
        if pending.len() == before {
            break;
        }
    }
    for (e, eids, _) in pending {
        units.push(Unit::new(&e, eids));
    }

    let mut list: Vec<Game> = units
        .into_iter()
        .map(|u| Game { key: u.key, paths: u.paths, stems: u.stems.into_iter().collect(), extras: u.extras, files: u.files, bytes: u.bytes })
        .collect();
    let add_ons = list.split_off(games);
    list.sort_by_key(|g| g.key.to_lowercase());
    list.extend(add_ons);
    out.games = list;
    out
}

fn references_kind(name: &str) -> bool {
    let l = name.to_lowercase();
    l.ends_with(".cue") || l.ends_with(".gdi") || l.ends_with(".m3u")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn w(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn game<'a>(l: &'a GameList, key: &str) -> &'a Game {
        l.games.iter().find(|g| g.key == key).unwrap_or_else(|| panic!("{key} not in {:#?}", l.games.iter().map(|g| &g.key).collect::<Vec<_>>()))
    }

    #[test]
    fn ids_and_names() {
        assert_eq!(ids("Animal Crossing [01006F8002326000][US][v0].nsp"), vec!["01006F8002326000"]);
        assert_eq!(ids("Happy Home Paradise [01006F80023273E8][v0].nsp"), vec!["01006F8002326000"]);
        assert_eq!(ids("Tony [01000AD01F94B00e][DLC].nsp"), vec!["01000AD01F94A000"]);
        assert_eq!(ids("CUSA00001-UPDATE"), vec!["CUSA00001"]);
        assert_eq!(ids("Demons Souls [BLUS-30443]"), vec!["BLUS30443"]);
        assert!(ids("Super Mario Bros Wonder v1.1.0.nsp").is_empty());
        assert_eq!(norm(strip_ext("[Update] Bayonetta 3 (v1.2) [01004A4010FEA800].nsp")), "bayonetta 3");
        assert_eq!(norm("Kirby™ Star Allies"), "kirby star allies");
        assert_eq!(norm("Luigi’s Mansion 3"), norm("Luigi's Mansion 3"));
        assert_eq!(norm(strip_ext("The Legend of_Zelda Tears of the Kingdom v1.4.3.nsp")), "the legend of zelda tears of the kingdom v1 4 3");
        assert_eq!(strip_ext("Super Mario Bros. Wonder"), "Super Mario Bros. Wonder");
        assert!(word_prefix("hades", "hades"));
        assert!(!word_prefix("hades", "hadesii"));
    }

    #[test]
    fn switch_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        w(d, "Hades [0100535012974000][US][v0].nsp", "h");
        w(d, "Hades II [0100A00019DE0000][v0].nsp", "h2");
        w(d, "Splatoon 3.nsp", "s");
        w(d, "Super Mario Bros. Wonder.nsp", "smw");
        w(d, "Mario Kart 8 Deluxe [0100152000022000][v0].nsp", "mk");
        w(d, "systeminfo.txt", "info");
        w(d, r"_Keys\prod.keys", "keys");
        w(d, r"_Firmware\Firmware.zip", "fw");
        w(d, r"_Updates\[Update] Hades (v1.0.38233) [0100535012974800][v589824].nsp", "u");
        w(d, r"_Updates\Hades II [0100A00019DE0800][v589824][UPD].nsp", "u");
        w(d, r"_Updates\Splatoon 3 [Battle-boosting bonuses] [0100C2500FC21064][v0].nsp", "dlc");
        w(d, r"_Updates\[Update] Splatoon 3 (v11.3.0) [0100C2500FC20800][v2752512].nsp", "u");
        w(d, r"_Updates\Splatoon™ 3 - Wave 2 - Side Order [0100C2500FC21066][v131072].nsp", "dlc");
        w(d, r"_Updates\Super Mario Bros Wonder v1.1.0.nsp", "u");
        w(d, r"_Updates\Super Mario Bros. Wonder [010015100B514800][v327680][1.2.1].nsp", "u");
        w(d, r"_Updates\Mario Kart 8 Deluxe Wave 1-6 dlc unlocker.nsp", "dlc");
        w(d, r"_Updates\Serious Carnage [010058102783A000][v0][Base].nsp", "base");
        let l = list(d);

        assert_eq!(l.shared.len(), 3);
        assert_eq!(l.shared_bytes, 4 + 4 + 2);
        assert_eq!(game(&l, "Hades [0100535012974000][US][v0].nsp").extras.len(), 1);
        assert_eq!(game(&l, "Hades II [0100A00019DE0000][v0].nsp").extras, vec!["Hades II [0100A00019DE0800][v589824][UPD].nsp"]);
        assert_eq!(game(&l, "Splatoon 3.nsp").extras.len(), 3);
        assert_eq!(game(&l, "Super Mario Bros. Wonder.nsp").extras.len(), 2);
        assert_eq!(game(&l, "Mario Kart 8 Deluxe [0100152000022000][v0].nsp").extras.len(), 1);
        // a base game parked in _Updates can be picked on its own, after the real games
        let last = l.games.last().unwrap();
        assert_eq!(last.key, r"_Updates\Serious Carnage [010058102783A000][v0][Base].nsp");
        assert_eq!(l.games.len(), 6);
        assert_eq!(l.files, 17 - 1);

        let picked = BTreeSet::from(["Splatoon 3.nsp".to_string()]);
        // the game and its 3 add-ons, plus _Keys, _Firmware and systeminfo.txt
        assert_eq!(l.picked(&picked), (4 + 2, 1 + 3 + 1 + 3 + 10));
        let (paths, stems) = l.left_out(&picked);
        assert!(paths.contains(&PathBuf::from(r"_Updates\Super Mario Bros Wonder v1.1.0.nsp")));
        assert!(!paths.iter().any(|p| p.to_string_lossy().contains("Splatoon")));
        assert!(stems.contains("hades [0100535012974000][us][v0]"));
        assert!(!stems.contains("splatoon 3"));
    }

    #[test]
    fn discs_folders_and_ps4_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        w(d, "Game A.cue", "FILE \"Game A (Track 1).bin\" BINARY\nFILE \"Game A (Track 2).bin\" BINARY\n");
        w(d, "Game A (Track 1).bin", "1");
        w(d, "Game A (Track 2).bin", "2");
        w(d, "Game B.m3u", "Discs/Game B (Disc 1).chd\nDiscs/Game B (Disc 2).chd\n");
        w(d, r"Discs\Game B (Disc 1).chd", "d1");
        w(d, r"Discs\Game B (Disc 2).chd", "d2");
        w(d, "Game C.iso", "c");
        w(d, "Game C.png", "x");
        w(d, r"Game D.ps3\PS3_GAME\USRDIR\EBOOT.BIN", "eboot");
        w(d, r"CUSA00001\eboot.bin", "ps4");
        w(d, r"CUSA00001-UPDATE\eboot.bin", "patch");
        let l = list(d);

        let keys: Vec<_> = l.games.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(keys, vec!["CUSA00001", "Game A.cue", "Game B.m3u", "Game C.iso", "Game D.ps3"]);
        assert_eq!(game(&l, "Game A.cue").paths.len(), 3);
        assert_eq!(game(&l, "Game B.m3u").paths, vec![PathBuf::from("Game B.m3u"), PathBuf::from("Discs")]);
        assert_eq!(game(&l, "Game C.iso").paths.len(), 2);
        assert_eq!(game(&l, "CUSA00001").extras, vec!["CUSA00001-UPDATE"]);
        assert!(game(&l, "Game D.ps3").stems.contains(&"game d".to_string()));
        assert!(game(&l, "Game D.ps3").stems.contains(&"game d.ps3".to_string()));
        assert!(l.shared.is_empty());
    }
}

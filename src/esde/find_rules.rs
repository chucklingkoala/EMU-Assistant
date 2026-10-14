//! `es_find_rules.xml`: how ES-DE locates each `%EMULATOR_XXX%`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

use super::{attr, read_text, tag_name, text};
use crate::paths::normalize;

#[derive(Clone, Debug)]
pub struct Rule {
    pub kind: String,
    pub entries: Vec<String>,
}

pub type Rules = HashMap<String, Vec<Rule>>;

pub fn parse(xml: &str) -> Result<Rules> {
    let mut r = Reader::from_str(xml);
    r.config_mut().trim_text(true);
    let mut out = Rules::new();
    let mut emu: Option<(String, Vec<Rule>)> = None;
    let mut rule: Option<Rule> = None;
    let mut tag = String::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => {
                tag = tag_name(&e);
                match tag.as_str() {
                    "emulator" => emu = attr(&e, b"name").map(|n| (n, Vec::new())),
                    "rule" if emu.is_some() => {
                        rule = Some(Rule { kind: attr(&e, b"type").unwrap_or_default(), entries: Vec::new() })
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) if tag == "entry" => {
                if let Some(rule) = rule.as_mut() {
                    rule.entries.push(text(&t));
                }
            }
            Ok(Event::End(e)) => {
                match e.name().as_ref() {
                    b"rule" => {
                        if let (Some(rl), Some((_, rules))) = (rule.take(), emu.as_mut()) {
                            rules.push(rl);
                        }
                    }
                    b"emulator" => {
                        if let Some((name, rules)) = emu.take() {
                            out.insert(name, rules);
                        }
                    }
                    _ => {}
                }
                tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => bail!("XML error at byte {}: {e}", r.buffer_position()),
            _ => {}
        }
    }
    Ok(out)
}

/// Bundled Windows rules, with `custom_systems\es_find_rules.xml` entries replacing them by name.
pub fn load(es_root: &Path) -> Result<Rules> {
    let bundled = es_root.join(r"resources\systems\windows\es_find_rules.xml");
    let mut rules = if bundled.is_file() { parse(&read_text(&bundled)?)? } else { Rules::new() };
    let custom = es_root.join(r"ES-DE\custom_systems\es_find_rules.xml");
    if custom.is_file() {
        rules.extend(parse(&read_text(&custom)?)?);
    }
    Ok(rules)
}

#[derive(Clone, Debug)]
pub struct Found {
    pub exe: PathBuf,
    pub dir: PathBuf,
    pub via: String,
}

/// Resolves an emulator id the way ES-DE does: rules in file order, first existing match wins.
pub fn resolve(id: &str, rules: &Rules, es_root: &Path) -> Option<Found> {
    for rule in rules.get(id)? {
        for entry in &rule.entries {
            let hit = match rule.kind.as_str() {
                "staticpath" => find_static(&expand(entry, es_root)),
                "systempath" => find_in_path(entry),
                "winregistrypath" => registry::app_path(entry),
                "winregistryvalue" => registry::value(entry),
                _ => None,
            };
            if let Some(p) = hit {
                let p = normalize(&p);
                let dir = if p.is_dir() { p.clone() } else { p.parent().map(Path::to_path_buf).unwrap_or_default() };
                return Some(Found { exe: p, dir, via: rule.kind.clone() });
            }
        }
    }
    None
}

/// Expands `%ESPATH%`, `~` (portable home = ES-DE folder) and any `%ENVVAR%`.
pub fn expand(entry: &str, es_root: &Path) -> String {
    let mut s = entry.replace("%ESPATH%", &es_root.to_string_lossy());
    if let Some(rest) = s.strip_prefix('~') {
        s = format!("{}{rest}", es_root.to_string_lossy());
    }
    let mut out = String::new();
    let mut rest = s.as_str();
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) => {
                let var = &after[..j];
                match std::env::var(var) {
                    Ok(v) if !var.is_empty() => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(var);
                        out.push('%');
                    }
                }
                rest = &after[j + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Checks a static path, supporting `*` wildcards within path components.
fn find_static(path: &str) -> Option<PathBuf> {
    let p = normalize(Path::new(path));
    if !path.contains('*') {
        return p.exists().then_some(p);
    }
    let mut candidates = vec![PathBuf::new()];
    for comp in p.components() {
        let c = comp.as_os_str().to_string_lossy().to_string();
        if c.contains('*') {
            let mut next = Vec::new();
            for base in &candidates {
                let Ok(rd) = std::fs::read_dir(base) else { continue };
                let mut names: Vec<_> = rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
                names.sort();
                next.extend(names.into_iter().filter(|n| wildcard(&c, n)).map(|n| base.join(n)));
            }
            candidates = next;
        } else {
            for base in candidates.iter_mut() {
                base.push(comp.as_os_str());
            }
        }
    }
    candidates.into_iter().find(|c| c.exists())
}

/// Case-insensitive match of `name` against a pattern with `*` wildcards.
pub fn wildcard(pattern: &str, name: &str) -> bool {
    let p = pattern.to_lowercase();
    let n = name.to_lowercase();
    let parts: Vec<&str> = p.split('*').collect();
    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            if !n.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if i == parts.len() - 1 {
            return n.len() >= pos + part.len() && n.ends_with(part);
        } else {
            match n[pos..].find(part) {
                Some(j) => pos += j + part.len(),
                None => return false,
            }
        }
    }
    pos == n.len()
}

fn find_in_path(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(exe)).find(|p| p.is_file())
}

#[cfg(windows)]
mod registry {
    use std::path::PathBuf;
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    pub fn app_path(exe: &str) -> Option<PathBuf> {
        let sub = format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\{exe}");
        [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE].into_iter().find_map(|hive| {
            let v: String = RegKey::predef(hive).open_subkey(&sub).ok()?.get_value("").ok()?;
            let p = PathBuf::from(v.trim_matches('"'));
            p.is_file().then_some(p)
        })
    }

    /// Entry format: `HKEY_xxx\Some\Key\ValueName`.
    pub fn value(entry: &str) -> Option<PathBuf> {
        let (hive, rest) = entry.split_once('\\')?;
        let (key, value) = rest.rsplit_once('\\')?;
        let hive = match hive {
            "HKEY_CURRENT_USER" => HKEY_CURRENT_USER,
            "HKEY_LOCAL_MACHINE" => HKEY_LOCAL_MACHINE,
            _ => return None,
        };
        let v: String = RegKey::predef(hive).open_subkey(key).ok()?.get_value(value).ok()?;
        let p = PathBuf::from(v.trim_matches('"'));
        p.exists().then_some(p)
    }
}

#[cfg(not(windows))]
mod registry {
    use std::path::PathBuf;
    pub fn app_path(_: &str) -> Option<PathBuf> {
        None
    }
    pub fn value(_: &str) -> Option<PathBuf> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<ruleList>
    <emulator name="RETROARCH">
        <!-- RetroArch -->
        <rule type="winregistrypath">
            <entry>retroarch_does_not_exist.exe</entry>
        </rule>
        <rule type="staticpath">
            <entry>%ESPATH%\RetroArch-Win64\retroarch.exe</entry>
            <entry>%ESPATH%\Emulators\RetroArch*\retroarch.exe</entry>
        </rule>
    </emulator>
    <core name="RETROARCH">
        <rule type="corepath">
            <entry>%EMUPATH%\cores</entry>
        </rule>
    </core>
</ruleList>"#;

    #[test]
    fn parses_and_resolves_with_wildcard() {
        let rules = parse(SAMPLE).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules["RETROARCH"].len(), 2);

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(r"Emulators\RetroArch-Win64");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("retroarch.exe"), b"x").unwrap();
        let f = resolve("RETROARCH", &rules, tmp.path()).unwrap();
        assert!(crate::paths::eq_ci(&f.dir, &dir));
        assert_eq!(f.via, "staticpath");
        assert!(resolve("NOPE", &rules, tmp.path()).is_none());
    }

    #[test]
    fn wildcards() {
        assert!(wildcard("retroarch*", "RetroArch-Win64"));
        assert!(wildcard("*xemu*", "app-xemu-0.7"));
        assert!(!wildcard("rpcs3*", "dolphin"));
    }
}

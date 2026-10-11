//! `ES-DE\settings\es_settings.xml`: a flat list of `<type name=".." value=".." />` elements.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use quick_xml::events::Event;
use quick_xml::Reader;

use super::{attr, read_text};
use crate::paths::normalize;

pub fn parse(xml: &str) -> HashMap<String, String> {
    let mut r = Reader::from_str(xml);
    let mut out = HashMap::new();
    loop {
        match r.read_event() {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) => {
                if let (Some(n), Some(v)) = (attr(&e, b"name"), attr(&e, b"value")) {
                    out.insert(n, v);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

pub fn read(path: &Path) -> Result<HashMap<String, String>> {
    Ok(parse(&read_text(path)?))
}

/// Resolves a directory setting the way portable ES-DE does: empty means the default
/// below the ES-DE folder, `~` is the ES-DE folder (portable home) and `%ESPATH%` /
/// `%ROMPATH%` are expanded.
pub fn resolve_dir(raw: &str, es_root: &Path, rom_dir: Option<&Path>, default_rel: &str) -> PathBuf {
    let raw = raw.trim();
    if raw.is_empty() {
        return normalize(&es_root.join(default_rel));
    }
    let mut s = raw.replace("%ESPATH%", &es_root.to_string_lossy());
    if let Some(r) = rom_dir {
        s = s.replace("%ROMPATH%", &r.to_string_lossy());
    }
    if let Some(rest) = s.strip_prefix('~') {
        s = format!("{}{rest}", es_root.to_string_lossy());
    }
    let p = normalize(Path::new(&s));
    if p.is_absolute() {
        p
    } else {
        normalize(&es_root.join(p))
    }
}

/// Replaces (or appends) `<string name=KEY value=..>` entries, leaving everything else untouched.
pub fn patch_text(xml: &str, updates: &[(&str, String)]) -> String {
    let mut lines: Vec<String> = xml.lines().map(String::from).collect();
    for (name, value) in updates {
        let needle = format!("name=\"{name}\"");
        let element = format!(
            "<string name=\"{name}\" value=\"{}\" />",
            quick_xml::escape::escape(value.as_str())
        );
        match lines.iter().position(|l| l.contains(&needle)) {
            Some(i) => {
                let indent: String = lines[i].chars().take_while(|c| c.is_whitespace()).collect();
                lines[i] = format!("{indent}{element}");
            }
            None => lines.push(element),
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<bool name="DebugMode" value="false" />
<string name="MediaDirectory" value="" />
<string name="ROMDirectory" value="D:/Games/ROMs" />
"#;

    #[test]
    fn parses_multiple_roots() {
        let s = parse(SAMPLE);
        assert_eq!(s["ROMDirectory"], "D:/Games/ROMs");
        assert_eq!(s["MediaDirectory"], "");
        assert_eq!(s["DebugMode"], "false");
    }

    #[test]
    fn resolves_dirs() {
        let root = Path::new(r"D:\ES-DE");
        assert_eq!(resolve_dir("", root, None, "ROMs"), PathBuf::from(r"D:\ES-DE\ROMs"));
        assert_eq!(resolve_dir("~/ROMs", root, None, "x"), PathBuf::from(r"D:\ES-DE\ROMs"));
        assert_eq!(resolve_dir("%ESPATH%/../ROMs", root, None, "x"), PathBuf::from(r"D:\ROMs"));
        assert_eq!(resolve_dir("E:/Games", root, None, "x"), PathBuf::from(r"E:\Games"));
    }

    #[test]
    fn patches_only_named_values() {
        let out = patch_text(SAMPLE, &[("ROMDirectory", r"F:\ROMs & more".into()), ("New", "x".into())]);
        let s = parse(&out);
        assert_eq!(s["ROMDirectory"], r"F:\ROMs & more");
        assert_eq!(s["New"], "x");
        assert_eq!(s["DebugMode"], "false");
        assert_eq!(out.lines().count(), SAMPLE.lines().count() + 1);
    }
}

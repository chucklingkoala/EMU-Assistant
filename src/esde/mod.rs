//! Readers for the ES-DE configuration files described in USERGUIDE.md.

pub mod find_rules;
pub mod gamelist;
pub mod settings;
pub mod systems;

use std::path::Path;

use anyhow::{Context, Result};
use quick_xml::events::{BytesStart, BytesText};

pub fn read_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let s = String::from_utf8_lossy(&bytes);
    Ok(s.trim_start_matches('\u{feff}').to_string())
}

pub(crate) fn attr(e: &BytesStart, name: &[u8]) -> Option<String> {
    e.attributes().flatten().find(|a| a.key.as_ref() == name).map(|a| {
        a.unescape_value()
            .map(|v| v.into_owned())
            .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned())
    })
}

pub(crate) fn text(t: &BytesText) -> String {
    t.unescape()
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| String::from_utf8_lossy(t).into_owned())
}

pub(crate) fn tag_name(e: &BytesStart) -> String {
    String::from_utf8_lossy(e.name().as_ref()).into_owned()
}

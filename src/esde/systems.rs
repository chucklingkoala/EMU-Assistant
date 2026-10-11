//! `es_systems.xml`: game system definitions (bundled, optionally overridden by custom_systems).

use std::path::Path;

use anyhow::{bail, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

use super::{attr, read_text, tag_name, text};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Command {
    pub label: String,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct SystemDef {
    pub name: String,
    pub fullname: String,
    pub path: String,
    pub commands: Vec<Command>,
}

pub fn parse(xml: &str) -> Result<Vec<SystemDef>> {
    let mut r = Reader::from_str(xml);
    r.config_mut().trim_text(true);
    let mut out = Vec::new();
    let mut cur: Option<SystemDef> = None;
    let mut tag = String::new();
    let mut label = String::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => {
                tag = tag_name(&e);
                match tag.as_str() {
                    "system" => cur = Some(SystemDef::default()),
                    "command" => label = attr(&e, b"label").unwrap_or_default(),
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(s) = cur.as_mut() {
                    let v = text(&t);
                    match tag.as_str() {
                        "name" => s.name = v,
                        "fullname" => s.fullname = v,
                        "path" => s.path = v,
                        "command" => s.commands.push(Command { label: label.clone(), text: v }),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"system" {
                    if let Some(s) = cur.take().filter(|s| !s.name.is_empty()) {
                        out.push(s);
                    }
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

/// Loads the bundled Windows systems and applies `ES-DE\custom_systems\es_systems.xml`
/// on top: entries with the same name replace the bundled ones, new ones are appended.
pub fn load(es_root: &Path) -> Result<Vec<SystemDef>> {
    let bundled = es_root.join(r"resources\systems\windows\es_systems.xml");
    if !bundled.is_file() {
        bail!("{} not found, is this a portable ES-DE folder?", bundled.display());
    }
    let mut systems = parse(&read_text(&bundled)?)?;
    let custom = es_root.join(r"ES-DE\custom_systems\es_systems.xml");
    if custom.is_file() {
        for c in parse(&read_text(&custom)?)? {
            match systems.iter_mut().find(|s| s.name == c.name) {
                Some(s) => *s = c,
                None => systems.push(c),
            }
        }
    }
    Ok(systems)
}

/// Extracts `XXX` from every `%EMULATOR_XXX%` in a launch command.
pub fn emulator_ids(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = cmd;
    while let Some(i) = rest.find("%EMULATOR_") {
        let after = &rest[i + "%EMULATOR_".len()..];
        match after.find('%') {
            Some(j) => {
                out.push(after[..j].to_string());
                rest = &after[j + 1..];
            }
            None => break,
        }
    }
    out
}

/// Splits a command line on whitespace, honouring double quotes.
pub fn tokens(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in cmd.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<!-- comment -->
<systemList>
    <system>
        <name>snes</name>
        <fullname>Nintendo SNES (Super Nintendo)</fullname>
        <path>%ROMPATH%\snes</path>
        <extension>.zip .ZIP</extension>
        <command label="Snes9x - Current">%EMULATOR_RETROARCH% -L %CORE_RETROARCH%\snes9x_libretro.dll %ROM%</command>
        <command label="bsnes">%EMULATOR_BSNES% %ROM%</command>
        <platform>snes</platform>
        <theme>snes</theme>
    </system>
    <system>
        <name>dos</name>
        <fullname>DOS &amp; PC</fullname>
        <path>%ROMPATH%/dos</path>
        <command label="DOSBox">%STARTDIR%=%GAMEDIR% %EMULATOR_DOSBOX-X% %ROM%</command>
    </system>
</systemList>"#;

    #[test]
    fn parses_systems() {
        let s = parse(SAMPLE).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].name, "snes");
        assert_eq!(s[0].commands.len(), 2);
        assert_eq!(s[0].commands[1].label, "bsnes");
        assert_eq!(s[1].fullname, "DOS & PC");
        assert_eq!(emulator_ids(&s[0].commands[0].text), vec!["RETROARCH"]);
        assert_eq!(emulator_ids(&s[1].commands[0].text), vec!["DOSBOX-X"]);
    }

    #[test]
    fn tokenizes_quotes() {
        assert_eq!(tokens(r#""C:\My Emus\a.exe" -f %ROM%"#), vec![r"C:\My Emus\a.exe", "-f", "%ROM%"]);
    }
}

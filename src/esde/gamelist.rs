//! `ES-DE\gamelists\<system>\gamelist.xml`: only the alternative emulator choices are needed.

use std::collections::BTreeSet;
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;

use super::{read_text, tag_name, text};

#[derive(Debug, Default)]
pub struct EmulatorChoices {
    /// System-wide `<alternativeEmulator><label>` override.
    pub system: Option<String>,
    /// Per-game `<altemulator>` labels.
    pub per_game: BTreeSet<String>,
}

pub fn parse(xml: &str) -> EmulatorChoices {
    let mut r = Reader::from_str(xml);
    r.config_mut().trim_text(true);
    let mut out = EmulatorChoices::default();
    let mut in_alt = false;
    let mut tag = String::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => {
                tag = tag_name(&e);
                if tag == "alternativeEmulator" {
                    in_alt = true;
                }
            }
            Ok(Event::Text(t)) => match tag.as_str() {
                "label" if in_alt => out.system = Some(text(&t)),
                "altemulator" => {
                    out.per_game.insert(text(&t));
                }
                _ => {}
            },
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"alternativeEmulator" {
                    in_alt = false;
                }
                tag.clear();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

pub fn read(path: &Path) -> EmulatorChoices {
    read_text(path).map(|s| parse(&s)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_choices() {
        let c = parse(
            r#"<?xml version="1.0"?>
<alternativeEmulator>
	<label>bsnes</label>
</alternativeEmulator>
<gameList>
	<game>
		<path>./a.zip</path>
		<name>A</name>
		<altemulator>Snes9x - Current</altemulator>
	</game>
	<game>
		<path>./b.zip</path>
		<name>label</name>
	</game>
</gameList>"#,
        );
        assert_eq!(c.system.as_deref(), Some("bsnes"));
        assert!(c.per_game.contains("Snes9x - Current"));
        assert_eq!(c.per_game.len(), 1);
    }
}

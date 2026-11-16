//! Direct play: ES-DE and its emulators run straight from an export drive, on any
//! Windows PC and under any drive letter.
//!
//! The drive stays an ordinary export drive. Every file direct play creates or changes is
//! listed in the manifest ([`PlayState::touched`]) and each changed file's original is
//! kept in `EMUAssistant\play-orig`, so Deploy can hand out the originals
//! ([`overlay`]) and [`disable`] can put the drive back as it was.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use filetime::FileTime;
use sha2::{Digest, Sha256};

use crate::esde::find_rules::{self, Rule};
use crate::esde::{read_text, settings};
use crate::fsutil::PART_EXT;
use crate::manifest::{self, Manifest, PlayState, Touched};
use crate::paths::{eq_ci, is_under, key, long, mirror, normalize, relative, strip_prefix_ci};
use crate::plan::Overlay;
use crate::scan::Catalog;
use crate::update::sources::SourceSpec;

pub const SECTION_BEGIN: &str = "<!-- EMU Assistant direct play: begin -->";
pub const SECTION_END: &str = "<!-- EMU Assistant direct play: end -->";
const SETTINGS_REL: &str = r"ES-DE\settings\es_settings.xml";
const MEDIA_DEFAULT: &str = r"ES-DE\downloaded_media";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    /// Enable will change this.
    Fix,
    /// Needs the user.
    Attention,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub level: Level,
    pub text: String,
}

fn line(level: Level, text: impl Into<String>) -> Line {
    Line { level, text: text.into() }
}

#[derive(Clone, Debug, PartialEq)]
pub enum EmuState {
    /// On the drive and found by ES-DE from its own folder.
    Ready { exe: PathBuf },
    /// On the drive outside the ES-DE folder: Enable registers it with ES-DE.
    NeedsRule { exe: PathBuf },
    /// Not on the drive. `source`: an update source is known, so it can be downloaded.
    Missing { why: String, source: bool },
    /// A launcher, or a program started by its full path on the source PC.
    External { why: String },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Portable {
    Yes,
    /// Keeps its settings in the user profile until `marker` exists in its folder.
    No { marker: &'static str, is_dir: bool },
    Unknown,
}

#[derive(Clone, Debug)]
pub struct EmuLine {
    pub id: String,
    /// Full names of the systems on the drive that use it.
    pub systems: Vec<String>,
    pub state: EmuState,
    pub portable: Portable,
    /// Where a download goes, and the exe pattern to look for (phase 2).
    pub install: Option<(PathBuf, String)>,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    /// The ES-DE folder on the drive.
    pub es_root: PathBuf,
    /// Set: direct play can't be enabled.
    pub blocker: Option<String>,
    pub general: Vec<Line>,
    pub emulators: Vec<EmuLine>,
    /// Emulator config files that still refer to the source PC.
    pub configs: Vec<Line>,
    /// RetroArch cores used by the systems but missing from the drive.
    pub missing_cores: Vec<String>,
    pub enabled: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub make_portable: BTreeSet<String>,
}

/// The package's folders on the drive.
struct Drive<'a> {
    pkg: PathBuf,
    cat: &'a Catalog,
    es: PathBuf,
    rom: PathBuf,
    media: PathBuf,
}

impl<'a> Drive<'a> {
    fn new(pkg: &Path, m: &'a Manifest) -> Self {
        let pkg = normalize(pkg);
        let r = &m.catalog.roots;
        Drive { es: mirror(&pkg, &r.es_root), rom: mirror(&pkg, &r.rom_dir), media: mirror(&pkg, &r.media_dir), cat: &m.catalog, pkg }
    }

    fn on(&self, p: &Path) -> PathBuf {
        mirror(&self.pkg, p)
    }
}

// ---------- settings ----------

/// The value for a directory setting that finds `target` from the ES-DE folder `es`
/// under any drive letter: empty for ES-DE's default, else relative to `%ESPATH%`.
pub fn setting_value(es: &Path, target: &Path, default_rel: &str) -> String {
    if eq_ci(target, &es.join(default_rel)) {
        return String::new();
    }
    match relative(es, target) {
        Some(rel) => format!(r"%ESPATH%\{}", rel.display()),
        None => target.display().to_string(),
    }
}

/// A setting that doesn't name a drive.
fn drive_independent(raw: &str) -> bool {
    let raw = raw.trim();
    raw.is_empty()
        || raw.starts_with("%ESPATH%")
        || raw.starts_with("%ROMPATH%")
        || raw.starts_with('~')
        || !matches!(Path::new(&raw.replace('/', "\\")).components().next(), Some(Component::Prefix(_) | Component::RootDir))
}

/// The ROMDirectory / MediaDirectory updates es_settings.xml needs, from its text.
fn settings_updates(text: &str, d: &Drive) -> Vec<(&'static str, String)> {
    let st = settings::parse(text);
    let get = |k: &str| st.get(k).map(String::as_str).unwrap_or("");
    let mut out = Vec::new();
    let rom_raw = get("ROMDirectory");
    if !(drive_independent(rom_raw) && eq_ci(&settings::resolve_dir(rom_raw, &d.es, None, "ROMs"), &d.rom)) {
        out.push(("ROMDirectory", setting_value(&d.es, &d.rom, "ROMs")));
    }
    let media_raw = get("MediaDirectory");
    let media_ok = drive_independent(media_raw)
        && (!media_raw.contains("%ROMPATH%") || drive_independent(rom_raw))
        && eq_ci(&settings::resolve_dir(media_raw, &d.es, Some(&d.rom), MEDIA_DEFAULT), &d.media);
    if !media_ok {
        out.push(("MediaDirectory", setting_value(&d.es, &d.media, MEDIA_DEFAULT)));
    }
    out
}

// ---------- emulators ----------

/// Emulators that keep their settings in the user profile unless a marker is in their
/// folder: (id, marker, the marker is a folder). Only entries confirmed from each
/// emulator's own documentation; anything else is reported as unknown.
const PORTABLE_MARKERS: [(&str, &str, bool); 10] = [
    ("DUCKSTATION", "portable.txt", false),
    ("PCSX2", "portable.ini", false),
    ("DOLPHIN", "portable.txt", false),
    ("XENIA", "portable.txt", false),
    ("MGBA", "portable.ini", false),
    ("CEMU", "portable", true),
    ("RYUJINX", "portable", true),
    ("CITRA", "user", true),
    ("AZAHAR", "user", true),
    ("EDEN", "user", true),
];
/// Emulators whose Windows builds always keep everything in their own folder.
const ALWAYS_PORTABLE: [&str; 2] = ["RETROARCH", "RPCS3"];

/// `PCSX2-LEGACY` counts as `PCSX2`.
fn id_is(id: &str, base: &str) -> bool {
    id == base || id.strip_prefix(base).is_some_and(|r| r.starts_with('-'))
}

pub fn portable_state(id: &str, dir: &Path) -> Portable {
    if ALWAYS_PORTABLE.iter().any(|b| id_is(id, b)) {
        return Portable::Yes;
    }
    match PORTABLE_MARKERS.iter().find(|(b, _, _)| id_is(id, b)) {
        Some((_, marker, is_dir)) => {
            let p = dir.join(marker);
            if (*is_dir && p.is_dir()) || (!*is_dir && p.is_file()) {
                Portable::Yes
            } else {
                Portable::No { marker, is_dir: *is_dir }
            }
        }
        None => Portable::Unknown,
    }
}

/// Where a downloaded emulator goes so that ES-DE's own rules find it: the folder of its
/// first bundled `%ESPATH%\Emulators\...` rule, and the exe name (may hold `*`).
pub fn install_target(id: &str, bundled: &find_rules::Rules, es: &Path) -> Option<(PathBuf, String)> {
    for rule in bundled.get(id)?.iter().filter(|r| r.kind == "staticpath") {
        for e in &rule.entries {
            let e = e.replace('/', "\\");
            let Some(rest) = e.strip_prefix(r"%ESPATH%\Emulators\") else { continue };
            let parts: Vec<&str> = rest.split('\\').collect();
            if parts.len() < 2 {
                continue;
            }
            let folder = parts[0].replace('*', "");
            if folder.is_empty() {
                continue;
            }
            return Some((es.join("Emulators").join(folder).join(parts[1..parts.len() - 1].join("\\")), parts[parts.len() - 1].to_string()));
        }
    }
    None
}

fn emulator_lines(d: &Drive, rules: &find_rules::Rules, bundled: &find_rules::Rules, sources: &BTreeMap<String, SourceSpec>) -> Vec<EmuLine> {
    let mut by_id: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for s in &d.cat.systems {
        for id in &s.emus_in_use {
            by_id.entry(id.clone()).or_default().push(s.fullname.clone());
        }
    }
    by_id
        .into_iter()
        .map(|(id, systems)| {
            let state = if crate::update::NOT_EMULATORS.contains(&id.as_str()) {
                EmuState::External { why: "a launcher; it uses what is installed on each PC".into() }
            } else if id.starts_with("EXE:") {
                EmuState::External { why: "a custom command that starts a program by its full path on the source PC".into() }
            } else if let Some(f) = find_rules::resolve_static(&id, rules, &d.es).filter(|f| is_under(&f.exe, &d.pkg)) {
                EmuState::Ready { exe: f.exe }
            } else {
                match d.cat.emulators.get(&id).and_then(|e| e.exe.as_ref()).map(|e| d.on(e)).filter(|e| e.is_file()) {
                    Some(exe) => EmuState::NeedsRule { exe },
                    None => EmuState::Missing {
                        why: "not on this drive (it was not exported, or is installed with an installer)".into(),
                        source: crate::update::source_for(&id, sources).is_some(),
                    },
                }
            };
            let portable = match &state {
                EmuState::Ready { exe } | EmuState::NeedsRule { exe } => exe.parent().map_or(Portable::Unknown, |dir| portable_state(&id, dir)),
                _ => Portable::Unknown,
            };
            let install = matches!(state, EmuState::Missing { .. }).then(|| install_target(&id, bundled, &d.es)).flatten();
            EmuLine { id, systems, state, portable, install }
        })
        .collect()
}

/// RetroArch cores named in the launch commands of the drive's systems that are not in
/// RetroArch's cores folder.
fn missing_cores(d: &Drive, ra_dir: &Path) -> Vec<String> {
    let Ok(defs) = crate::esde::systems::load(&d.es) else { return Vec::new() };
    let mut out = BTreeSet::new();
    for s in &d.cat.systems {
        let Some(def) = defs.iter().find(|x| x.name == s.name) else { continue };
        for c in &def.commands {
            if !c.text.contains("%EMULATOR_RETROARCH%") {
                continue;
            }
            for t in crate::esde::systems::tokens(&c.text) {
                let name = t.rsplit(['\\', '/']).next().unwrap_or("").to_string();
                if name.to_lowercase().ends_with("_libretro.dll") && !ra_dir.join("cores").join(&name).is_file() {
                    out.insert(name);
                }
            }
            // only the default command is used unless a game picks another
            break;
        }
    }
    out.into_iter().collect()
}

// ---------- RetroArch ----------

/// `retroarch.cfg` with paths into RetroArch's folder on the source PC turned into the
/// `:\` form RetroArch reads relative to its own folder. `None`: nothing to change.
pub fn retroarch_fix(text: &str, src_dir: &Path) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    for l in text.split_inclusive('\n') {
        let body = l.trim_end_matches(['\r', '\n']);
        let eol = &l[body.len()..];
        if let Some((k, v)) = body.split_once('=') {
            let v = v.trim().trim_matches('"');
            if !v.is_empty() && !v.starts_with(':') {
                let p = normalize(Path::new(v));
                if p.is_absolute() {
                    if let Some(rest) = strip_prefix_ci(&p, src_dir) {
                        out.push_str(&format!("{k}= \":\\{}\"{eol}", rest.display()));
                        changed = true;
                        continue;
                    }
                }
            }
        }
        out.push_str(l);
    }
    changed.then_some(out)
}

fn retroarch_dir(d: &Drive) -> Option<(PathBuf, PathBuf)> {
    let e = d.cat.emulators.values().find(|e| e.exe.as_ref().is_some_and(|x| x.file_name().is_some_and(|n| n.eq_ignore_ascii_case("retroarch.exe"))))?;
    let src = e.dir.clone()?;
    let on = d.on(&src);
    on.is_dir().then_some((src, on))
}

// ---------- find rules ----------

fn strip_section(text: &str) -> (String, bool) {
    let (Some(b), Some(e)) = (text.find(SECTION_BEGIN), text.find(SECTION_END)) else { return (text.to_string(), false) };
    if e < b {
        return (text.to_string(), false);
    }
    let start = text[..b].rfind('\n').map_or(0, |i| i + 1);
    let mut end = e + SECTION_END.len();
    if text[end..].starts_with("\r\n") {
        end += 2;
    } else if text[end..].starts_with('\n') {
        end += 1;
    }
    (format!("{}{}", &text[..start], &text[end..]), true)
}

/// `text` without the `<emulator name="id">` element.
fn remove_emulator(text: &str, id: &str) -> String {
    let mut from = 0;
    while let Some(i) = text[from..].find("<emulator").map(|i| i + from) {
        let Some(gt) = text[i..].find('>').map(|g| g + i) else { break };
        let tag = &text[i..=gt];
        let name = tag.split_once("name=\"").and_then(|(_, r)| r.split_once('"')).map(|(n, _)| quick_xml::escape::unescape(n).map(|c| c.into_owned()).unwrap_or_else(|_| n.to_string()));
        if name.as_deref() != Some(id) {
            from = gt + 1;
            continue;
        }
        let end = if tag.ends_with("/>") {
            gt + 1
        } else {
            match text[gt..].find("</emulator>") {
                Some(e) => gt + e + "</emulator>".len(),
                None => break,
            }
        };
        // the whole line when the element is alone on it
        let line_start = text[..i].rfind('\n').map_or(0, |n| n + 1);
        let (start, end) = if text[line_start..i].trim().is_empty() {
            let rest = &text[end..];
            let eol = rest.find('\n').filter(|&n| rest[..n].trim().is_empty()).map_or(0, |n| n + 1);
            (line_start, end + eol)
        } else {
            (i, end)
        };
        return format!("{}{}", &text[..start], &text[end..]);
    }
    text.to_string()
}

/// The custom find rules file with our section holding `blocks`. `None`: leave it as is.
fn rules_text(existing: Option<&str>, blocks: &str) -> Option<String> {
    let section = format!("    {SECTION_BEGIN}\n{blocks}    {SECTION_END}\n");
    match existing {
        None if blocks.is_empty() => None,
        None => Some(format!("<?xml version=\"1.0\"?>\n<ruleList>\n{section}</ruleList>\n")),
        Some(t) => {
            let (base, had) = strip_section(t);
            if blocks.is_empty() {
                return had.then_some(base);
            }
            let at = base.rfind("</ruleList>")?;
            Some(format!("{}{section}{}", &base[..at], &base[at..]))
        }
    }
}

// ---------- the ledger ----------

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn orig_of(pkg: &Path, rel: &Path) -> PathBuf {
    pkg.join(manifest::DIR).join(manifest::PLAY_ORIG).join(rel)
}

fn mtime(p: &Path) -> FileTime {
    std::fs::metadata(long(p)).map(|m| FileTime::from_last_modification_time(&m)).unwrap_or_else(|_| FileTime::now())
}

fn write_atomic(file: &Path, bytes: &[u8]) -> Result<()> {
    let mut part = file.as_os_str().to_owned();
    part.push(PART_EXT);
    let part = PathBuf::from(part);
    std::fs::write(long(&part), bytes).with_context(|| format!("writing {}", file.display()))?;
    if let Err(e) = std::fs::rename(long(&part), long(file)) {
        let _ = std::fs::remove_file(long(&part));
        return Err(e).with_context(|| format!("writing {}", file.display()));
    }
    Ok(())
}

/// Changes files on the drive and writes down what it did.
struct Ledger<'a> {
    pkg: &'a Path,
    st: &'a mut PlayState,
    log: Vec<String>,
}

impl Ledger<'_> {
    fn rel(&self, p: &Path) -> Result<PathBuf> {
        strip_prefix_ci(p, self.pkg).with_context(|| format!("{} is not on the drive", p.display()))
    }

    /// Creates the missing folders above `file`, recording them.
    fn parents(&mut self, file: &Path) -> Result<()> {
        let mut missing = Vec::new();
        let mut cur = file.parent();
        while let Some(d) = cur.filter(|d| !d.exists() && is_under(d, self.pkg)) {
            missing.push(d.to_path_buf());
            cur = d.parent();
        }
        for d in missing.into_iter().rev() {
            std::fs::create_dir(long(&d)).with_context(|| format!("creating {}", d.display()))?;
            let rel = self.rel(&d)?;
            self.st.touched.insert(key(&rel), Touched { rel, created: true, is_dir: true, written: String::new() });
        }
        Ok(())
    }

    /// The text a fix starts from: the original when the file is still as direct play
    /// wrote it (`None` when direct play created it), else the file as it is now.
    fn base(&self, file: &Path) -> Option<String> {
        let cur = read_text(file).ok();
        let rel = self.rel(file).ok()?;
        match (self.st.touched.get(&key(&rel)), &cur) {
            (Some(e), Some(_)) if std::fs::read(long(file)).is_ok_and(|b| sha(&b) == e.written) => {
                if e.created {
                    None
                } else {
                    read_text(&orig_of(self.pkg, &rel)).ok().or(cur)
                }
            }
            _ => cur,
        }
    }

    /// Gives `file` the content `new`, keeping the file as it is now as the original when
    /// direct play hasn't written it before (or something else has replaced it since).
    /// `Ok(false)`: it already had that content.
    fn write(&mut self, file: &Path, new: &[u8], what: &str) -> Result<bool> {
        let rel = self.rel(file)?;
        let k = key(&rel);
        let cur = std::fs::read(long(file)).ok();
        if cur.as_deref() == Some(new) {
            return Ok(false);
        }
        let prev = self.st.touched.get(&k).cloned();
        let entry = match (&cur, prev) {
            (None, Some(e)) => e,
            (None, None) => {
                self.parents(file)?;
                Touched { rel: rel.clone(), created: true, is_dir: false, written: String::new() }
            }
            (Some(c), prev) => match prev.filter(|e| e.written == sha(c)) {
                Some(e) => e,
                None => {
                    // this version is the one Deploy hands out and Turn off puts back
                    let orig = orig_of(self.pkg, &rel);
                    let never = AtomicBool::new(false);
                    crate::sync::copy_file(file, &orig, mtime(file), false, &never, &mut |_| {})
                        .with_context(|| format!("keeping the original of {}", file.display()))?;
                    Touched { rel: rel.clone(), created: false, is_dir: false, written: String::new() }
                }
            },
        };
        write_atomic(file, new)?;
        self.st.touched.insert(k, Touched { written: sha(new), ..entry });
        self.log.push(format!("{what}: {}", file.display()));
        Ok(true)
    }

    /// Creates an empty file or a folder unless something is there already.
    fn create(&mut self, p: &Path, is_dir: bool, what: &str) -> Result<bool> {
        if p.exists() {
            return Ok(false);
        }
        self.parents(p)?;
        if is_dir {
            std::fs::create_dir(long(p))?;
        } else {
            std::fs::write(long(p), b"")?;
        }
        let rel = self.rel(p)?;
        self.st.touched.insert(key(&rel), Touched { rel, created: true, is_dir, written: if is_dir { String::new() } else { sha(b"") } });
        self.log.push(format!("{what}: {}", p.display()));
        Ok(true)
    }

    fn step(&mut self, what: &str, r: Result<bool>) {
        if let Err(e) = r {
            self.log.push(format!("Could not {what}: {e:#}"));
        }
    }
}

// ---------- check ----------

fn general_lines(d: &Drive) -> Vec<Line> {
    let mut out = Vec::new();
    out.push(if d.es.join("portable.txt").is_file() {
        line(Level::Ok, "ES-DE runs in portable mode (portable.txt)")
    } else {
        line(Level::Fix, "portable.txt will be created so ES-DE keeps its data on the drive")
    });
    if !d.rom.is_dir() {
        out.push(line(Level::Attention, format!("The ROM folder {} is not on the drive", d.rom.display())));
    }
    let text = read_text(&d.es.join(SETTINGS_REL)).unwrap_or_default();
    let updates = settings_updates(&text, d);
    for (name, what, dir) in [("ROMDirectory", "ROM folder", &d.rom), ("MediaDirectory", "media folder", &d.media)] {
        out.push(match updates.iter().find(|(k, _)| *k == name) {
            Some((_, v)) => line(
                Level::Fix,
                format!("ES-DE's {what} will be set to {} (found under any drive letter)", if v.is_empty() { "its default" } else { v }),
            ),
            None => line(Level::Ok, format!("ES-DE finds the {what} on the drive: {}", dir.display())),
        });
    }
    let vol = crate::paths::volume_root(&d.pkg).unwrap_or_default();
    if let Some(info) = crate::drives::list().into_iter().find(|i| i.root.eq_ignore_ascii_case(&vol)) {
        if !info.fs.eq_ignore_ascii_case("NTFS") {
            out.push(line(Level::Attention, format!("The drive is {}; ES-DE starts and scrolls more slowly than on NTFS", info.fs)));
        }
    }
    out
}

/// Config files of the emulators on the drive that still name folders of the source PC.
fn config_lines(d: &Drive, emus: &[EmuLine], cancel: &AtomicBool) -> Vec<Line> {
    const MAX_FILES: usize = 2000;
    const MAX_SIZE: u64 = 1 << 20;
    let mut out = Vec::new();
    let ra = retroarch_dir(d);
    for e in emus {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let (EmuState::Ready { exe } | EmuState::NeedsRule { exe }) = &e.state else { continue };
        let Some(dir) = exe.parent() else { continue };
        // (lowercase text to look for, the folder as shown)
        let mut needles: Vec<(String, String)> = Vec::new();
        let mut roots = vec![d.cat.roots.es_root.clone(), d.cat.roots.rom_dir.clone()];
        roots.extend(d.cat.emulators.get(&e.id).and_then(|x| x.dir.clone()));
        for r in roots {
            let shown = r.display().to_string();
            let s = shown.to_lowercase();
            needles.push((s.replace('\\', "/"), shown.clone()));
            needles.push((s.replace('\\', "\\\\"), shown.clone()));
            needles.push((s, shown));
        }
        let mut files = 0;
        for entry in walkdir::WalkDir::new(long(dir)).max_depth(3).into_iter().flatten() {
            if files >= MAX_FILES {
                break;
            }
            let p = crate::paths::short(entry.path());
            let ext = p.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
            if !entry.file_type().is_file() || !crate::bringback::SETTING_EXTS.contains(&ext.as_str()) {
                continue;
            }
            if entry.metadata().map_or(true, |m| m.len() > MAX_SIZE) {
                continue;
            }
            files += 1;
            let Ok(mut text) = read_text(&p) else { continue };
            let rel = p.strip_prefix(dir).unwrap_or(&p).display().to_string();
            if let Some((src, on)) = &ra {
                if eq_ci(dir, on) && rel.eq_ignore_ascii_case("retroarch.cfg") {
                    if let Some(fixed) = retroarch_fix(&text, src) {
                        out.push(line(Level::Fix, format!("{}: retroarch.cfg folders will point into RetroArch's folder on the drive", e.id)));
                        text = fixed;
                    }
                }
            }
            let lower = text.to_lowercase();
            if let Some((_, shown)) = needles.iter().find(|(n, _)| lower.contains(n.as_str())) {
                out.push(line(Level::Attention, format!("{}: {rel} refers to {shown} on the source PC", e.id)));
            }
        }
    }
    out
}

/// Looks at the drive and says what direct play needs. Reads only.
pub fn check(pkg: &Path, m: &Manifest, sources: &BTreeMap<String, SourceSpec>, cancel: &AtomicBool, status: &mut dyn FnMut(String)) -> Result<Report> {
    let d = Drive::new(pkg, m);
    let mut r = Report { es_root: d.es.clone(), enabled: m.play.is_some(), ..Default::default() };
    if !crate::scan::is_es_root(&d.es) {
        r.blocker = Some(format!(
            "ES-DE itself is not on this drive ({} has no ES-DE.exe). Export again with \"ES-DE application\" and \"ES-DE settings\" ticked.",
            d.es.display()
        ));
        return Ok(r);
    }
    if !d.es.join(find_rules::BUNDLED_REL).is_file() {
        r.blocker = Some(format!("{} is missing; export the ES-DE application again.", d.es.join(find_rules::BUNDLED_REL).display()));
        return Ok(r);
    }
    status("Checking ES-DE on the drive…".into());
    r.general = general_lines(&d);
    status("Checking the emulators…".into());
    let rules = find_rules::load(&d.es).unwrap_or_else(|e| {
        r.general.push(line(Level::Attention, format!("Could not read the find rules: {e:#}")));
        Default::default()
    });
    let bundled = find_rules::bundled(&d.es).unwrap_or_default();
    r.emulators = emulator_lines(&d, &rules, &bundled, sources);
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    status("Reading the emulators' settings…".into());
    r.configs = config_lines(&d, &r.emulators, cancel);
    if let Some(exe) = r.emulators.iter().find_map(|e| match &e.state {
        EmuState::Ready { exe } | EmuState::NeedsRule { exe } if id_is(&e.id, "RETROARCH") => Some(exe.clone()),
        _ => None,
    }) {
        if let Some(dir) = exe.parent() {
            r.missing_cores = missing_cores(&d, dir);
        }
    }
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    Ok(r)
}

// ---------- enable / reapply / disable ----------

/// Makes the drive start ES-DE and find its emulators, recording every change in
/// `m.play` (turned on if it wasn't). Running it again changes nothing that is already
/// right. The caller saves the manifest. Returns the log lines.
pub fn enable(pkg: &Path, m: &mut Manifest, opts: &Options) -> Result<Vec<String>> {
    let mut st = m.play.take().unwrap_or_default();
    let res = apply(pkg, m, &mut st, opts);
    if st.enabled_unix == 0 {
        st.enabled_unix = crate::update::version::now_unix().max(0) as u64;
    }
    st.make_portable = opts.make_portable.clone();
    m.play = Some(st);
    res
}

fn apply(pkg: &Path, m: &Manifest, st: &mut PlayState, opts: &Options) -> Result<Vec<String>> {
    let d = Drive::new(pkg, m);
    if !crate::scan::is_es_root(&d.es) {
        bail!("ES-DE is not on this drive ({} has no ES-DE.exe)", d.es.display());
    }
    let pkg = d.pkg.clone();
    let mut l = Ledger { pkg: &pkg, st, log: Vec::new() };

    // 1. ES-DE keeps its data next to it
    let r = l.create(&d.es.join("portable.txt"), false, "Created");
    l.step("create portable.txt", r);

    // 2. ROM and media folders under any drive letter
    let settings_file = d.es.join(SETTINGS_REL);
    let base = l.base(&settings_file);
    let updates = settings_updates(base.as_deref().unwrap_or(""), &d);
    if !updates.is_empty() {
        let new = settings::patch_text(base.as_deref().unwrap_or("<?xml version=\"1.0\"?>"), &updates);
        let r = l.write(&settings_file, new.as_bytes(), "Pointed ES-DE at the drive's ROM and media folders");
        l.step("update es_settings.xml", r);
    }

    // 3. emulators outside the ES-DE folder
    let bundled = find_rules::bundled(&d.es).unwrap_or_default();
    let custom = d.es.join(find_rules::CUSTOM_REL);
    // the user's own custom rules, without what direct play added
    let users_text = l.base(&custom).map(|t| strip_section(&t).0);
    let users_rules = users_text.as_deref().and_then(|t| find_rules::parse(t).ok()).unwrap_or_default();
    let mut effective = bundled.clone();
    effective.extend(users_rules.clone());
    let mut blocks = String::new();
    let mut moved = Vec::new();
    for e in emulator_lines(&d, &effective, &bundled, &BTreeMap::new()) {
        let EmuState::NeedsRule { exe } = &e.state else { continue };
        let Some(rel) = relative(&d.es, exe) else { continue };
        let mut rs = vec![Rule { kind: "staticpath".into(), entries: vec![format!(r"%ESPATH%\{}", rel.display())] }];
        // the rules it had stay after ours: the user's own, else ES-DE's
        match users_rules.get(&e.id) {
            Some(own) => {
                rs.extend(own.iter().cloned());
                moved.push(e.id.clone());
            }
            None => rs.extend(bundled.get(&e.id).cloned().unwrap_or_default()),
        }
        blocks.push_str(&find_rules::to_xml(&e.id, &rs));
    }
    // an emulator is defined once: the user's element moves into our section
    let users_text = users_text.map(|t| moved.iter().fold(t, |t, id| remove_emulator(&t, id)));
    let cur_custom = read_text(&custom).ok();
    match rules_text(users_text.as_deref(), &blocks) {
        Some(new) => {
            let r = l.write(&custom, new.as_bytes(), "Registered the emulators outside the ES-DE folder");
            l.step("update the custom find rules", r);
        }
        None if users_text.is_some() && !blocks.is_empty() => {
            l.log.push(format!("{} has no </ruleList>; emulators outside the ES-DE folder were not registered", custom.display()))
        }
        // nothing to register any more: back to the user's own file
        None => {
            if let (Some(t), true) = (&users_text, cur_custom.as_ref().is_some_and(|c| c.contains(SECTION_BEGIN))) {
                let r = l.write(&custom, t.as_bytes(), "Updated the custom find rules");
                l.step("update the custom find rules", r);
            }
        }
    }

    // 4. RetroArch's folders
    if let Some((src, on)) = retroarch_dir(&d) {
        let cfg = on.join("retroarch.cfg");
        if let Some(new) = l.base(&cfg).and_then(|t| retroarch_fix(&t, &src)) {
            let r = l.write(&cfg, new.as_bytes(), "Pointed RetroArch's folders into its own folder");
            l.step("update retroarch.cfg", r);
        }
    }

    // 5. portable mode for the emulators the user ticked
    let rules = find_rules::load(&d.es).unwrap_or_default();
    for e in emulator_lines(&d, &rules, &bundled, &BTreeMap::new()) {
        let (EmuState::Ready { exe } | EmuState::NeedsRule { exe }) = &e.state else { continue };
        if let (true, Portable::No { marker, is_dir }, Some(dir)) = (opts.make_portable.contains(&e.id), &e.portable, exe.parent()) {
            let r = l.create(&dir.join(marker), *is_dir, &format!("Switched {} to portable mode", e.id));
            l.step(&format!("switch {} to portable mode", e.id), r);
        }
    }

    if l.log.is_empty() {
        l.log.push("Direct play: everything was already in place".into());
    }
    Ok(l.log)
}

/// [`enable`] with the choices stored on the drive; nothing when direct play is off.
pub fn reapply(pkg: &Path, m: &mut Manifest) -> Vec<String> {
    let Some(st) = &m.play else { return Vec::new() };
    let opts = Options { make_portable: st.make_portable.clone() };
    match enable(pkg, m, &opts) {
        Ok(lines) => lines.into_iter().filter(|l| !l.contains("already in place")).map(|l| format!("Direct play: {l}")).collect(),
        Err(e) => vec![format!("Direct play: {e:#}")],
    }
}

/// Puts the drive back as it was: originals restored, created files deleted, `m.play`
/// cleared. `remove_added`: also delete the programs downloaded for direct play. The
/// caller saves the manifest.
pub fn disable(pkg: &Path, m: &mut Manifest, remove_added: bool) -> Vec<String> {
    let pkg = normalize(pkg);
    let Some(st) = m.play.take() else { return vec!["Direct play was not on".into()] };
    let mut log = Vec::new();
    let mut entries: Vec<&Touched> = st.touched.values().collect();
    // files first, then folders from the deepest up
    entries.sort_by_key(|e| (e.is_dir, std::cmp::Reverse(e.rel.components().count())));
    let never = AtomicBool::new(false);
    for e in entries {
        let p = pkg.join(&e.rel);
        if e.created {
            let r = if e.is_dir { std::fs::remove_dir(long(&p)) } else { std::fs::remove_file(long(&p)) };
            match r {
                Ok(()) => log.push(format!("Removed {}", p.display())),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) if e.is_dir => log.push(format!("Left {}: it holds files now", p.display())),
                Err(err) => log.push(format!("Could not remove {}: {err}", p.display())),
            }
        } else {
            let orig = orig_of(&pkg, &e.rel);
            if !orig.is_file() {
                log.push(format!("The original of {} is missing; left as it is", p.display()));
                continue;
            }
            match crate::sync::copy_file(&orig, &p, mtime(&orig), false, &never, &mut |_| {}) {
                Ok(_) => log.push(format!("Put back the original {}", p.display())),
                Err(err) => {
                    log.push(format!("Could not put back {}: {err}", p.display()));
                    // keep the state so Turn off can be tried again
                    m.play = Some(st.clone());
                    return log;
                }
            }
        }
    }
    if remove_added {
        for (id, rel) in &st.added {
            let dir = pkg.join(rel);
            match std::fs::remove_dir_all(long(&dir)) {
                Ok(()) => log.push(format!("Removed {id} from {}", dir.display())),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => log.push(format!("Could not remove {}: {err}", dir.display())),
            }
        }
    }
    let _ = std::fs::remove_dir_all(long(&pkg.join(manifest::DIR).join(manifest::PLAY_ORIG)));
    log.push("Direct play is off; the drive is back as it was".into());
    log
}

/// `dir` and everything below it, as [`key`]s. Empty when it doesn't exist.
pub fn listing(dir: &Path) -> BTreeSet<String> {
    walkdir::WalkDir::new(long(dir)).into_iter().flatten().map(|e| key(&crate::paths::short(e.path()))).collect()
}

/// After a download into `dir`: files and folders that weren't there before (`before`
/// from [`listing`]) are recorded as created by direct play.
pub fn record_new(pkg: &Path, st: &mut PlayState, dir: &Path, before: &BTreeSet<String>) {
    let pkg = normalize(pkg);
    for e in walkdir::WalkDir::new(long(dir)).into_iter().flatten() {
        let p = crate::paths::short(e.path());
        if before.contains(&key(&p)) {
            continue;
        }
        let Some(rel) = strip_prefix_ci(&p, &pkg) else { continue };
        st.touched.entry(key(&rel)).or_insert(Touched { rel, created: true, is_dir: e.file_type().is_dir(), written: String::new() });
    }
}

/// What Deploy reads differently on this drive: direct play's own files are left out,
/// changed files are deployed from their originals.
pub fn overlay(pkg: &Path, m: &Manifest) -> Overlay {
    let mut o = Overlay::default();
    let Some(st) = &m.play else { return o };
    let pkg = normalize(pkg);
    for e in st.touched.values() {
        let p = normalize(&pkg.join(&e.rel));
        match (e.created, e.is_dir) {
            (true, true) => o.skip_dirs.push(p),
            (true, false) => {
                o.skip.insert(key(&p));
            }
            (false, _) => {
                o.swap.insert(key(&p), orig_of(&pkg, &e.rel));
            }
        }
    }
    o.skip_dirs.extend(st.added.values().map(|r| normalize(&pkg.join(r))));
    o
}

/// Applies the fixes again (an export may have replaced a file), saves the manifest when
/// that changed anything, and starts ES-DE from the drive.
pub fn launch(pkg: &Path, m: &mut Manifest) -> Result<Vec<String>> {
    if m.play.is_none() {
        bail!("direct play is not on for this drive");
    }
    let before = m.play.clone();
    let lines = reapply(pkg, m);
    if m.play != before {
        manifest::save(pkg, m)?;
    }
    let es = Drive::new(pkg, m).es;
    std::process::Command::new(es.join("ES-DE.exe"))
        .current_dir(&es)
        .spawn()
        .with_context(|| format!("starting {}", es.join("ES-DE.exe").display()))?;
    Ok(lines)
}

/// When an export from the ES-DE folder `this_es_root` (on this machine) to the drive
/// with manifest `m` must only add ROMs: direct play is on and the drive was set up by
/// another machine or from another ES-DE folder. Returns the drive's roots and owner.
pub fn merge_owner(m: &Manifest, this_es_root: &Path) -> Option<(crate::scan::Roots, String)> {
    m.play.as_ref()?;
    let here = std::env::var("COMPUTERNAME").unwrap_or_default();
    let other_machine = !here.is_empty() && !m.source_machine.is_empty() && !m.source_machine.eq_ignore_ascii_case(&here);
    let other_folder = !eq_ci(&m.catalog.roots.es_root, this_es_root);
    (other_machine || other_folder).then(|| (m.catalog.roots.clone(), if m.source_machine.is_empty() { "another PC".into() } else { m.source_machine.clone() }))
}

/// The report as plain text, for the clipboard.
pub fn report_text(r: &Report) -> String {
    let lvl = |l: Level| match l {
        Level::Ok => "ok  ",
        Level::Fix => "fix ",
        Level::Attention => "note",
    };
    let mut s = format!("EMU Assistant direct play report\nES-DE on the drive: {}\nDirect play: {}\n", r.es_root.display(), if r.enabled { "on" } else { "off" });
    if let Some(b) = &r.blocker {
        s.push_str(&format!("\nCan't enable: {b}\n"));
    }
    for l in &r.general {
        s.push_str(&format!("[{}] {}\n", lvl(l.level), l.text));
    }
    s.push_str("\nEmulators:\n");
    for e in &r.emulators {
        s.push_str(&format!("  {}  ({})  {}{}\n", e.id, e.systems.join(", "), state_text(&e.state), portable_text(&e.portable)));
    }
    if !r.missing_cores.is_empty() {
        s.push_str(&format!("\nRetroArch cores not on the drive: {}\n", r.missing_cores.join(", ")));
    }
    if !r.configs.is_empty() {
        s.push_str("\nEmulator settings:\n");
        for l in &r.configs {
            s.push_str(&format!("[{}] {}\n", lvl(l.level), l.text));
        }
    }
    s
}

pub fn state_text(s: &EmuState) -> String {
    match s {
        EmuState::Ready { .. } => "ready".into(),
        EmuState::NeedsRule { .. } => "on the drive; will be registered with ES-DE".into(),
        EmuState::Missing { why, source: true } => format!("{why}; can be downloaded"),
        EmuState::Missing { why, source: false } => why.clone(),
        EmuState::External { why } => why.clone(),
    }
}

pub fn portable_text(p: &Portable) -> String {
    match p {
        Portable::Yes => String::new(),
        Portable::No { marker, .. } => format!("; keeps its settings on each PC until switched to portable mode ({marker})"),
        Portable::Unknown => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{build, groups_for, Mapper, PlanOptions, Selection};
    use crate::scan::{scan, tests::fake_install, Roots};
    use std::fs;

    const OPTS: PlanOptions = PlanOptions { protect_newer: true, prune: false, add_only: false };

    fn all(cat: &Catalog) -> Selection {
        Selection {
            systems: cat.systems.iter().map(|s| s.name.clone()).collect(),
            emulators: cat.emulators.values().filter(|e| e.copyable).map(|e| e.id.clone()).collect(),
            globals: cat.globals.iter().map(|g| g.id.clone()).collect(),
            media: true,
            gamelists: true,
            ..Default::default()
        }
    }

    fn w(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    /// Every file below `dir` (relative path -> content hash), `EMUAssistant\manifest.json` left out.
    fn snapshot(dir: &Path) -> BTreeMap<String, String> {
        walkdir::WalkDir::new(dir)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .map(|e| (e.path().strip_prefix(dir).unwrap().display().to_string().to_lowercase(), sha(&fs::read(e.path()).unwrap())))
            .filter(|(k, _)| k != r"emuassistant\manifest.json")
            .collect()
    }

    fn folders(dir: &Path) -> BTreeSet<String> {
        walkdir::WalkDir::new(dir).into_iter().flatten().filter(|e| e.file_type().is_dir()).map(|e| e.path().display().to_string().to_lowercase()).collect()
    }

    struct Setup {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        cat: Catalog,
        drive: PathBuf,
    }

    /// A source install with an extra emulator outside the ES-DE folder, exported to a drive.
    fn setup() -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let (es, _) = fake_install(&root.join("main"));
        // DuckStation lives outside ES-DE, found by a custom rule with a full path
        let duck = root.join(r"main\Apps\DuckStation");
        w(&duck.join("duckstation.exe"), "duck");
        w(&duck.join("settings.ini"), &format!("[BIOS]\nSearchDirectory = {}\n", es.join("bios").display()));
        w(
            &es.join(find_rules::CUSTOM_REL),
            &format!(
                "<?xml version=\"1.0\"?>\n<ruleList>\n  <emulator name=\"DUCKSTATION\"><rule type=\"staticpath\"><entry>{}</entry></rule></emulator>\n  <emulator name=\"MINE\"><rule type=\"staticpath\"><entry>C:\\mine.exe</entry></rule></emulator>\n</ruleList>\n",
                duck.join("duckstation.exe").display()
            ),
        );
        // RetroArch keeps a full path to its own saves folder
        let ra = es.join(r"Emulators\RetroArch-Win64");
        w(&ra.join("retroarch.cfg"), &format!(
            "cheat_database_path = \":\\cheatdb\"\r\nsavefile_directory = \"{}\"\r\nscreenshot_directory = \"D:\\Pictures\"\r\nthumbnails_directory = \"default\"\r\n",
            ra.join("saves").display()
        ));
        let cat = scan(&es, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let drive = root.join("drive");
        fs::create_dir_all(&drive).unwrap();
        export(&cat, &drive);
        Setup { _tmp: tmp, root, cat, drive }
    }

    fn export(cat: &Catalog, drive: &Path) {
        let no = AtomicBool::new(false);
        let (tx, _rx) = crossbeam_channel::unbounded();
        let sel = all(cat);
        let p = build(&groups_for(cat, &sel), &Mapper::Export { dest_root: drive.to_path_buf() }, &OPTS, &no, &mut |_| {}).unwrap();
        assert!(crate::sync::execute(&p, false, &no, &tx).failed.is_empty());
        let existing = manifest::load(drive).unwrap();
        let mut m = manifest::for_export(existing, cat, &sel, None);
        m.complete = true;
        manifest::save(drive, &m).unwrap();
    }

    fn check_now(drive: &Path) -> Report {
        let m = manifest::load(drive).unwrap().unwrap();
        check(drive, &m, &BTreeMap::new(), &AtomicBool::new(false), &mut |_| {}).unwrap()
    }

    fn enable_now(drive: &Path, portable: &[&str]) -> Vec<String> {
        let mut m = manifest::load(drive).unwrap().unwrap();
        let opts = Options { make_portable: portable.iter().map(|s| s.to_string()).collect() };
        let log = enable(drive, &mut m, &opts).unwrap();
        manifest::save(drive, &m).unwrap();
        log
    }

    fn state<'a>(r: &'a Report, id: &str) -> &'a EmuLine {
        r.emulators.iter().find(|e| e.id == id).unwrap_or_else(|| panic!("{id} not in {:?}", r.emulators))
    }

    fn deploy(drive: &Path, target: &Path) -> crate::plan::Plan {
        let m = manifest::load(drive).unwrap().unwrap();
        let t = Roots { es_root: target.join("ES-DE"), rom_dir: target.join("ROMs"), media_dir: target.join(r"ES-DE\ES-DE\downloaded_media") };
        let mapper = Mapper::deploy(drive, &m.catalog.roots, &t).with_overlay(overlay(drive, &m));
        let no = AtomicBool::new(false);
        let p = build(&groups_for(&m.catalog, &all(&m.catalog)), &mapper, &OPTS, &no, &mut |_| {}).unwrap();
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        let (tx, _rx) = crossbeam_channel::unbounded();
        assert!(crate::sync::execute(&p, false, &no, &tx).failed.is_empty());
        p
    }

    #[test]
    fn checks_then_enables_a_drive() {
        let s = setup();
        let r = check_now(&s.drive);
        assert!(r.blocker.is_none() && !r.enabled);
        assert!(matches!(state(&r, "RETROARCH").state, EmuState::Ready { .. }));
        assert!(matches!(state(&r, "DUCKSTATION").state, EmuState::NeedsRule { .. }), "{:?}", state(&r, "DUCKSTATION"));
        assert_eq!(state(&r, "DUCKSTATION").portable, Portable::No { marker: "portable.txt", is_dir: false });
        assert!(r.general.iter().any(|l| l.level == Level::Fix && l.text.contains("ROM folder")));
        // DuckStation's settings name the source PC's ES-DE folder; RetroArch's will be fixed
        assert!(r.configs.iter().any(|l| l.level == Level::Attention && l.text.contains("settings.ini")), "{:?}", r.configs);
        assert!(r.configs.iter().any(|l| l.level == Level::Fix && l.text.contains("retroarch.cfg")));
        assert!(r.missing_cores.contains(&"snes9x_libretro.dll".to_string()));

        let log = enable_now(&s.drive, &["DUCKSTATION"]);
        assert!(log.len() >= 4, "{log:?}");
        let m = manifest::load(&s.drive).unwrap().unwrap();
        let es = mirror(&s.drive, &s.cat.roots.es_root);

        // ES-DE: relative ROM folder, the original kept with its time
        let st = settings::read(&es.join(SETTINGS_REL)).unwrap();
        assert!(st["ROMDirectory"].starts_with(r"%ESPATH%\.."), "{}", st["ROMDirectory"]);
        let rel = strip_prefix_ci(&es.join(SETTINGS_REL), &s.drive).unwrap();
        let orig = orig_of(&s.drive, &rel);
        assert_eq!(fs::read(&orig).unwrap(), fs::read(s.cat.roots.es_root.join(SETTINGS_REL)).unwrap());
        assert_eq!(mtime(&orig).unix_seconds(), mtime(&s.cat.roots.es_root.join(SETTINGS_REL)).unix_seconds());
        let back = crate::scan::scan(&es, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(eq_ci(&back.roots.rom_dir, &mirror(&s.drive, &s.cat.roots.rom_dir)));

        // DuckStation is found from the drive; the user's own rule stays
        let custom = read_text(&es.join(find_rules::CUSTOM_REL)).unwrap();
        assert!(custom.contains(SECTION_BEGIN) && custom.contains("MINE"));
        let rules = find_rules::load(&es).unwrap();
        let f = find_rules::resolve_static("DUCKSTATION", &rules, &es).unwrap();
        assert!(is_under(&f.exe, &s.drive), "{}", f.exe.display());
        assert!(rules["DUCKSTATION"][0].entries[0].starts_with(r"%ESPATH%\.."));
        assert!(back.emulators["DUCKSTATION"].dir.as_ref().is_some_and(|d| is_under(d, &s.drive)));

        // RetroArch: only the path into its own folder changed
        let cfg = read_text(&es.join(r"Emulators\RetroArch-Win64\retroarch.cfg")).unwrap();
        assert!(cfg.contains("savefile_directory = \":\\saves\"\r\n"), "{cfg}");
        assert!(cfg.contains("screenshot_directory = \"D:\\Pictures\"\r\n"));
        assert!(cfg.starts_with("cheat_database_path = \":\\cheatdb\"\r\n"));

        let r = check_now(&s.drive);
        assert!(r.enabled);
        assert!(r.general.iter().all(|l| l.level != Level::Fix), "{:?}", r.general);
        assert!(matches!(state(&r, "DUCKSTATION").state, EmuState::Ready { .. }));
        assert_eq!(state(&r, "DUCKSTATION").portable, Portable::Yes);

        // again: nothing changes
        let snap = snapshot(&s.drive);
        let ledger = m.play.clone();
        let log = enable_now(&s.drive, &["DUCKSTATION"]);
        assert_eq!(log, vec!["Direct play: everything was already in place".to_string()]);
        assert_eq!(snapshot(&s.drive), snap);
        assert_eq!(manifest::load(&s.drive).unwrap().unwrap().play.map(|p| p.touched), ledger.map(|p| p.touched));
    }

    #[test]
    fn turning_off_restores_the_drive() {
        let s = setup();
        let before = snapshot(&s.drive);
        let dirs = folders(&s.drive);
        enable_now(&s.drive, &["DUCKSTATION"]);
        assert_ne!(snapshot(&s.drive), before);
        let mut m = manifest::load(&s.drive).unwrap().unwrap();
        let log = disable(&s.drive, &mut m, false);
        manifest::save(&s.drive, &m).unwrap();
        assert!(m.play.is_none(), "{log:?}");
        assert_eq!(snapshot(&s.drive), before, "{log:?}");
        // apart from the manifest's own folder, no folders were left behind
        let mut now = folders(&s.drive);
        now.remove(&s.drive.join(manifest::DIR).display().to_string().to_lowercase());
        let mut was = dirs.clone();
        was.remove(&s.drive.join(manifest::DIR).display().to_string().to_lowercase());
        assert_eq!(now, was);
    }

    #[test]
    fn deploy_ignores_direct_play() {
        let s = setup();
        let plain = deploy(&s.drive, &s.root.join("ally1"));
        enable_now(&s.drive, &["DUCKSTATION"]);
        // a portable folder made while playing from the drive must not travel either
        let m = manifest::load(&s.drive).unwrap().unwrap();
        let p = deploy(&s.drive, &s.root.join("ally2"));
        assert_eq!(p.copy.len(), plain.copy.len());
        assert_eq!(snapshot(&s.root.join("ally1")), snapshot(&s.root.join("ally2")));
        assert!(!s.root.join(r"ally2\ES-DE\portable.txt").exists() || s.cat.roots.es_root.join("portable.txt").exists());
        assert!(m.play.is_some());
    }

    #[test]
    fn exports_keep_it_enabled() {
        let s = setup();
        enable_now(&s.drive, &[]);
        let es = mirror(&s.drive, &s.cat.roots.es_root);
        let src_settings = s.cat.roots.es_root.join(SETTINGS_REL);
        // the source's settings change; newer than the drive's copy
        let changed = format!("{}<bool name=\"ShowHelpPrompts\" value=\"false\" />\n", read_text(&src_settings).unwrap());
        fs::write(&src_settings, &changed).unwrap();
        filetime::set_file_mtime(&src_settings, FileTime::from_unix_time(FileTime::now().unix_seconds() + 3600, 0)).unwrap();

        let cfg = crate::config::Config {
            es_root: Some(s.cat.roots.es_root.clone()),
            dest: Some(s.drive.clone()),
            copy_exe: Some(false),
            systems: s.cat.systems.iter().map(|x| x.name.clone()).collect(),
            globals: Some(s.cat.globals.iter().map(|g| g.id.clone()).collect()),
            ..Default::default()
        };
        let r = crate::export::headless(&Default::default(), cfg, &|_| {}).unwrap();
        assert!(r.failed.is_empty(), "{r:?}");

        let m = manifest::load(&s.drive).unwrap().unwrap();
        assert!(m.play.is_some());
        let st = settings::read(&es.join(SETTINGS_REL)).unwrap();
        assert!(st["ROMDirectory"].starts_with("%ESPATH%"), "fixes are applied again after the export");
        assert_eq!(st["ShowHelpPrompts"], "false");
        let rel = strip_prefix_ci(&es.join(SETTINGS_REL), &s.drive).unwrap();
        assert_eq!(fs::read_to_string(orig_of(&s.drive, &rel)).unwrap(), changed, "the exported version is the new original");
    }

    #[test]
    fn works_under_another_drive_letter() {
        let s = setup();
        enable_now(&s.drive, &[]);
        let moved = s.root.join("other");
        fs::rename(&s.drive, &moved).unwrap();
        let r = check_now(&moved);
        assert!(r.general.iter().all(|l| l.level != Level::Fix), "{:?}", r.general);
        assert!(matches!(state(&r, "DUCKSTATION").state, EmuState::Ready { .. }));
        let es = mirror(&moved, &s.cat.roots.es_root);
        let cat = crate::scan::scan(&es, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(cat.systems.len(), s.cat.systems.len());
        assert!(eq_ci(&cat.roots.rom_dir, &mirror(&moved, &s.cat.roots.rom_dir)));
    }

    #[test]
    fn reports_missing_and_blocked() {
        let s = setup();
        let mut m = manifest::load(&s.drive).unwrap().unwrap();
        m.catalog.systems[0].emus_in_use.push("PCSX2".into());
        let sources = BTreeMap::new();
        let r = check(&s.drive, &m, &sources, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(matches!(state(&r, "PCSX2").state, EmuState::Missing { source: true, .. }));
        fs::remove_file(mirror(&s.drive, &s.cat.roots.es_root).join("ES-DE.exe")).unwrap();
        let r = check(&s.drive, &m, &sources, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(r.blocker.is_some());
        assert!(enable(&s.drive, &mut m, &Options::default()).is_err());
    }

    #[test]
    fn downloads_stay_on_the_drive_and_can_be_removed() {
        let s = setup();
        let before = snapshot(&s.drive);
        let es = mirror(&s.drive, &s.cat.roots.es_root);
        let mut m = manifest::load(&s.drive).unwrap().unwrap();
        // what the Download button does, without the network: a new emulator and a core
        let emu = es.join(r"Emulators\PCSX2");
        w(&emu.join("pcsx2-qt.exe"), "pcsx2");
        let cores = es.join(r"Emulators\RetroArch-Win64\cores");
        let was = listing(&cores);
        w(&cores.join("snes9x_libretro.dll"), "core");
        let st = m.play.get_or_insert_with(Default::default);
        st.added.insert("PCSX2".into(), strip_prefix_ci(&emu, &s.drive).unwrap());
        record_new(&s.drive, st, &cores, &was);
        enable(&s.drive, &mut m, &Options::default()).unwrap();
        manifest::save(&s.drive, &m).unwrap();

        // RetroArch is exported, so Deploy would copy its folder: the core stays behind
        let p = deploy(&s.drive, &s.root.join("ally"));
        assert!(!p.copy.iter().any(|i| i.src.ends_with("snes9x_libretro.dll")));
        assert!(!s.root.join(r"ally\ES-DE\Emulators\RetroArch-Win64\cores").exists());
        assert!(!s.root.join(r"ally\ES-DE\Emulators\PCSX2").exists());

        let mut m = manifest::load(&s.drive).unwrap().unwrap();
        disable(&s.drive, &mut m, true);
        assert_eq!(snapshot(&s.drive), before);
        assert!(!emu.exists() && !cores.exists());
    }

    #[test]
    fn another_pc_adds_its_roms() {
        let s = setup();
        enable_now(&s.drive, &[]);
        let es_on_drive = mirror(&s.drive, &s.cat.roots.es_root);
        let es_before = snapshot(&es_on_drive);

        // a second PC with ES-DE elsewhere: one new game, one game with the same name but
        // other content, and a system the drive doesn't have yet
        let (es_b, roms_b) = fake_install(&s.root.join(r"pc2\Stuff"));
        w(&roms_b.join(r"snes\b2.sfc"), "rom-b2");
        w(&roms_b.join(r"snes\a.sfc"), "rom-a from pc2");
        w(&roms_b.join(r"empty\x.bin"), "rom-x");
        let m = manifest::load(&s.drive).unwrap().unwrap();
        assert!(merge_owner(&m, &es_b).is_some());
        assert!(merge_owner(&m, &s.cat.roots.es_root).is_none(), "the owner exports as usual");

        let cfg = crate::config::Config {
            es_root: Some(es_b.clone()),
            dest: Some(s.drive.clone()),
            copy_exe: Some(false),
            systems: ["snes", "psx", "empty"].iter().map(|x| x.to_string()).collect(),
            globals: Some(["esde_program", "esde_config", "rom_root_files"].iter().map(|x| x.to_string()).collect()),
            ..Default::default()
        };
        let lines = std::sync::Mutex::new(Vec::new());
        let r = crate::export::headless(&Default::default(), cfg, &|l| lines.lock().unwrap().push(l)).unwrap();
        assert!(r.failed.is_empty(), "{r:?}");
        let lines = lines.into_inner().unwrap();
        assert!(lines.iter().any(|l| l.contains("Already on the drive with different content (kept): 1")), "{lines:?}");

        // the drive's own ROM folder got them; the drive's copy of a.sfc stayed
        let rom = mirror(&s.drive, &s.cat.roots.rom_dir);
        assert_eq!(fs::read_to_string(rom.join(r"snes\b2.sfc")).unwrap(), "rom-b2");
        assert_eq!(fs::read_to_string(rom.join(r"snes\a.sfc")).unwrap(), "rom-a");
        assert!(rom.join(r"empty\x.bin").is_file());
        // nothing of the second PC's ES-DE or emulators came along
        assert!(!mirror(&s.drive, &es_b).exists());
        assert_eq!(snapshot(&es_on_drive), es_before);

        let m = manifest::load(&s.drive).unwrap().unwrap();
        assert!(eq_ci(&m.catalog.roots.es_root, &s.cat.roots.es_root) && m.play.is_some());
        assert_eq!(m.contributors.len(), 1);
        let snes = m.catalog.systems.iter().find(|x| x.name == "snes").unwrap();
        assert_eq!(snes.rom_files, 2);
        assert!(m.catalog.systems.iter().any(|x| x.name == "empty" && is_under(&x.rom_dir, &s.cat.roots.rom_dir)));
        // ES-DE on the drive sees the new system, and Deploy hands it out
        let cat = crate::scan::scan(&es_on_drive, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(cat.systems.iter().any(|x| x.name == "empty"));
        let p = deploy(&s.drive, &s.root.join("ally"));
        assert!(p.copy.iter().any(|i| i.src.ends_with(r"empty\x.bin")));
    }

    #[test]
    fn removes_one_emulator_element() {
        let t = "<ruleList>\n  <emulator name=\"A\"><rule type=\"staticpath\"><entry>a</entry></rule></emulator>\n  <emulator name=\"B\">\n    <rule type=\"staticpath\"><entry>b</entry></rule>\n  </emulator>\n</ruleList>\n";
        assert_eq!(remove_emulator(t, "B"), "<ruleList>\n  <emulator name=\"A\"><rule type=\"staticpath\"><entry>a</entry></rule></emulator>\n</ruleList>\n");
        assert_eq!(remove_emulator(t, "C"), t);
        assert!(!find_rules::parse(&remove_emulator(t, "A")).unwrap().contains_key("A"));
    }

    #[test]
    fn settings_values_and_sections() {
        let es = Path::new(r"E:\Emu\ES-DE");
        assert_eq!(setting_value(es, &es.join("ROMs"), "ROMs"), "");
        assert_eq!(setting_value(es, Path::new(r"E:\Games\ROMs"), "ROMs"), r"%ESPATH%\..\..\Games\ROMs");
        assert!(drive_independent("%ESPATH%/../ROMs") && drive_independent("") && drive_independent("~/ROMs"));
        assert!(!drive_independent(r"D:\ROMs") && !drive_independent("D:/ROMs"));

        let block = |id: &str| find_rules::to_xml(id, &[Rule { kind: "staticpath".into(), entries: vec!["a".into()] }]);
        let t = rules_text(None, &block("X")).unwrap();
        assert!(find_rules::parse(&t).unwrap().contains_key("X"));
        let t2 = rules_text(Some(&t), &block("Y")).unwrap();
        assert!(!t2.contains("\"X\"") && t2.contains("\"Y\""));
        assert_eq!(rules_text(Some(&t2), "").unwrap(), "<?xml version=\"1.0\"?>\n<ruleList>\n</ruleList>\n");
        assert_eq!(rules_text(Some("<ruleList>\n</ruleList>\n"), ""), None);

        let fixed = retroarch_fix("a = \"C:\\RA\\saves\"\nb = \"C:\\RAX\"\n", Path::new(r"C:\RA")).unwrap();
        assert_eq!(fixed, "a = \":\\saves\"\nb = \"C:\\RAX\"\n");
        assert_eq!(retroarch_fix("a = \":\\saves\"\n", Path::new(r"C:\RA")), None);
    }

    #[test]
    fn portable_markers_and_install_targets() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(portable_state("RETROARCH", tmp.path()), Portable::Yes);
        assert_eq!(portable_state("PCSX2-LEGACY", tmp.path()), Portable::No { marker: "portable.ini", is_dir: false });
        assert_eq!(portable_state("SOMETHING", tmp.path()), Portable::Unknown);
        fs::create_dir(tmp.path().join("portable")).unwrap();
        assert_eq!(portable_state("CEMU", tmp.path()), Portable::Yes);

        let rules = find_rules::parse(
            r#"<ruleList><emulator name="PCSX2"><rule type="winregistrypath"><entry>pcsx2.exe</entry></rule>
            <rule type="staticpath"><entry>%ESPATH%\Emulators\PCSX2*\pcsx2*.exe</entry></rule></emulator></ruleList>"#,
        )
        .unwrap();
        let es = Path::new(r"E:\ES-DE");
        assert_eq!(install_target("PCSX2", &rules, es), Some((PathBuf::from(r"E:\ES-DE\Emulators\PCSX2"), "pcsx2*.exe".into())));
        assert_eq!(install_target("NONE", &rules, es), None);
    }
}

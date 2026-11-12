//! Updates EMU Assistant itself. A running exe can't be overwritten on Windows but it can be
//! renamed, so the new exe is put next to it as `<exe>.new`, the running one is renamed to
//! `<exe>.old` and the new one takes its name. The next start deletes the `.old` file.
//! The same swap lets the exe on a drive replace an older copy installed on the machine.

use std::cmp::Ordering;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use crossbeam_channel::Sender;
use filetime::FileTime;
use serde_json::Value;

use super::http::Http;
use super::sources::from_release;
use super::version::{cmp_numeric, pe_version, short_version};
use crate::manifest::{EXE_NAME, LEGACY_EXE_NAME};
use crate::paths::{eq_ci, long};
use crate::sync::{self, Event, Progress, Report};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A published release newer than this build.
#[derive(Clone, Debug, PartialEq)]
pub struct AppRelease {
    pub version: String,
    /// Release notes as written on GitHub.
    pub notes: String,
    /// The release's web page.
    pub page: String,
    pub published: String,
    pub asset_url: String,
    pub size: u64,
    /// SHA-256 of the exe as GitHub reports it for the asset (lowercase hex).
    pub sha256: Option<String>,
    /// The `EMUAssistant.exe.sha256` file published with the release, used when GitHub
    /// reports no digest.
    pub checksum_url: Option<String>,
}

/// An older EMU Assistant installed on this machine.
#[derive(Clone, Debug)]
pub struct OldCopy {
    pub path: PathBuf,
    /// `None` when the exe has no version information.
    pub version: Option<String>,
}

fn repo() -> &'static str {
    env!("CARGO_PKG_REPOSITORY").trim_start_matches("https://github.com/").trim_end_matches('/')
}

/// The newest stable release on GitHub, if it is newer than this build.
pub fn check(http: &Http) -> Result<Option<AppRelease>> {
    let rel = http.get_json(&format!("https://api.github.com/repos/{}/releases/latest", repo()))?;
    parse_release(&rel, VERSION)
}

fn parse_release(rel: &Value, current: &str) -> Result<Option<AppRelease>> {
    let s = |key: &str| rel.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let r = from_release(rel, EXE_NAME).with_context(|| format!("the latest release {} has no {EXE_NAME}", s("tag_name")))?;
    if cmp_numeric(&r.version, current) != Some(Ordering::Greater) {
        return Ok(None);
    }
    let assets = rel.get("assets").and_then(Value::as_array).cloned().unwrap_or_default();
    let field = |name: &str, key: &str| {
        assets.iter().find(|a| a.get("name").and_then(Value::as_str) == Some(name)).and_then(|a| a.get(key)?.as_str().map(String::from))
    };
    let sha256 = field(&r.asset_name, "digest").and_then(|d| d.strip_prefix("sha256:").map(str::to_lowercase));
    let checksum_url = field(&format!("{}.sha256", r.asset_name), "browser_download_url");
    Ok(Some(AppRelease {
        version: r.version,
        notes: s("body"),
        page: s("html_url"),
        published: r.published,
        asset_url: r.asset_url,
        size: r.size,
        sha256,
        checksum_url,
    }))
}

fn sibling(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// SHA-256 of a file as lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut f = fs::File::open(long(path))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The first word of a `sha256sum`-style line, if it looks like a SHA-256.
fn parse_checksum(text: &str) -> Option<String> {
    let word = text.split_whitespace().next()?.to_lowercase();
    (word.len() == 64 && word.chars().all(|c| c.is_ascii_hexdigit())).then_some(word)
}

/// Sanity checks a downloaded or copied exe before it replaces the real one.
fn check_exe(path: &Path, size: u64, version: Option<&str>, sha256: Option<&str>) -> Result<()> {
    let meta = fs::metadata(long(path))?;
    ensure!(size == 0 || meta.len() == size, "the download is incomplete ({} of {size} bytes)", meta.len());
    let mut head = [0u8; 2];
    fs::File::open(long(path))?.read_exact(&mut head).context("reading the new exe")?;
    ensure!(&head == b"MZ", "the download is not a Windows program");
    if let (Some(want), Some(got)) = (version, pe_version(path)) {
        ensure!(cmp_numeric(&got, want) == Some(Ordering::Equal), "the download is version {got}, expected {want}");
    }
    if let Some(want) = sha256 {
        let got = sha256_file(path)?;
        ensure!(got.eq_ignore_ascii_case(want), "the download's checksum doesn't match the release (SHA-256 {got}, expected {want})");
    }
    Ok(())
}

/// Puts `new` in place of `target`; the previous file is kept as `<target>.old` while it
/// is still running and restored if the swap fails.
fn swap(target: &Path, new: &Path) -> Result<()> {
    let old = sibling(target, ".old");
    let _ = fs::remove_file(long(&old));
    let had_target = target.exists();
    if had_target {
        fs::rename(long(target), long(&old)).with_context(|| format!("renaming {}", target.display()))?;
    }
    if let Err(e) = fs::rename(long(new), long(target)) {
        if had_target {
            let _ = fs::rename(long(&old), long(target));
        }
        return Err(e).with_context(|| format!("putting the new version in place of {}", target.display()));
    }
    // fails while the old exe is running; cleanup() removes it on the next start
    let _ = fs::remove_file(long(&old));
    Ok(())
}

/// Downloads `rel` and swaps it in for the running exe. Sends progress like a copy job.
pub fn install(http: &Http, rel: &AppRelease, cancel: &AtomicBool, tx: &Sender<Event>) -> Report {
    let started = Instant::now();
    let mut report = Report::default();
    match install_running(http, rel, cancel, tx) {
        Ok(true) => {
            report.copied = 1;
            report.copied_bytes = rel.size;
        }
        Ok(false) => report.cancelled = true,
        Err(e) => report.failed.push(format!("{e:#}")),
    }
    report.elapsed = started.elapsed();
    report
}

fn install_running(http: &Http, rel: &AppRelease, cancel: &AtomicBool, tx: &Sender<Event>) -> Result<bool> {
    let exe = std::env::current_exe()?;
    let new = sibling(&exe, ".new");
    let _ = fs::remove_file(long(&new));
    let mut progress =
        Progress { total_bytes: rel.size, total_files: 1, current: format!("Downloading EMU Assistant {}", rel.version), ..Default::default() };
    let done = http
        .download(&rel.asset_url, &new, cancel, &mut |n| {
            progress.done_bytes += n;
            let _ = tx.send(Event::Progress(progress.clone()));
        })
        .with_context(|| format!("downloading {}", rel.asset_url))?;
    if !done {
        return Ok(false);
    }
    let sha256 = match (&rel.sha256, &rel.checksum_url) {
        (Some(s), _) => Some(s.clone()),
        (None, Some(url)) => {
            let text = http.get_text(url).with_context(|| format!("reading the checksum {url}"))?;
            Some(parse_checksum(&text).with_context(|| format!("{url} holds no SHA-256"))?)
        }
        (None, None) => {
            let _ = tx.send(Event::Log("This release has no checksum; checked its size and version only.".into()));
            None
        }
    };
    let placed = check_exe(&new, rel.size, Some(&rel.version), sha256.as_deref()).and_then(|()| swap(&exe, &new));
    if let Err(e) = placed {
        let _ = fs::remove_file(long(&new));
        bail!("{e:#}. You can download it yourself from {}", rel.page);
    }
    let _ = tx.send(Event::Log(format!("EMU Assistant {} installed, restarting", rel.version)));
    Ok(true)
}

/// Starts the (new) exe.
pub fn restart(exe: &Path) -> Result<()> {
    std::process::Command::new(exe).spawn().with_context(|| format!("starting {}", exe.display()))?;
    Ok(())
}

/// Deletes the `.old` exe left by an update, waiting a little for the old process to exit.
pub fn cleanup() {
    let Ok(exe) = std::env::current_exe() else { return };
    let old = sibling(&exe, ".old");
    for _ in 0..20 {
        if !old.exists() || fs::remove_file(long(&old)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Where an installed EMU Assistant may be: the one this machine last ran (outside a drive)
/// and the ES-DE folder.
pub fn candidates(home_exe: Option<&Path>, es_root: &Path) -> Vec<PathBuf> {
    home_exe.map(Path::to_path_buf).into_iter().chain([es_root.join(EXE_NAME), es_root.join(LEGACY_EXE_NAME)]).collect()
}

/// The candidates that exist, aren't `running` and are older than `current`.
pub fn older_copies(candidates: &[PathBuf], running: &Path, current: &str) -> Vec<OldCopy> {
    let mut out: Vec<OldCopy> = Vec::new();
    for p in candidates {
        if !p.is_file() || eq_ci(p, running) || out.iter().any(|o| eq_ci(&o.path, p)) {
            continue;
        }
        let version = pe_version(p).map(|v| short_version(&v));
        if version.as_deref().is_none_or(|v| cmp_numeric(v, current) == Some(Ordering::Less)) {
            out.push(OldCopy { path: p.clone(), version });
        }
    }
    out
}

/// Replaces `target` with a copy of `source` (also when `target` is running).
pub fn replace(target: &Path, source: &Path) -> Result<()> {
    let new = sibling(target, ".new");
    let meta = fs::metadata(long(source))?;
    sync::copy_file(source, &new, FileTime::from_last_modification_time(&meta), false, &AtomicBool::new(false), &mut |_| {})
        .with_context(|| format!("copying to {}", new.display()))?;
    // the copy must be the drive's exe byte for byte
    let placed = sha256_file(source).and_then(|want| check_exe(&new, meta.len(), None, Some(&want))).and_then(|()| swap(target, &new));
    if placed.is_err() {
        let _ = fs::remove_file(long(&new));
    }
    placed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn release(tag: &str, asset: &str) -> Value {
        json!({
            "tag_name": tag,
            "html_url": format!("https://github.com/x/y/releases/tag/{tag}"),
            "body": "  What's new  ",
            "published_at": "2026-11-10T12:00:00Z",
            "assets": [{ "name": asset, "browser_download_url": "https://example.org/EMUAssistant.exe", "size": 123 }]
        })
    }

    #[test]
    fn newer_releases_only() {
        let r = parse_release(&release("v0.4.0", "EMUAssistant.exe"), "0.3.0").unwrap().unwrap();
        assert_eq!(r.version, "0.4.0");
        assert_eq!(r.notes, "What's new");
        assert_eq!(r.size, 123);
        assert!(r.page.ends_with("v0.4.0"));
        assert!(parse_release(&release("v0.3.0", "EMUAssistant.exe"), "0.3.0").unwrap().is_none());
        assert!(parse_release(&release("v0.2.9", "EMUAssistant.exe"), "0.3.0").unwrap().is_none());
        assert!(parse_release(&release("v0.3.10", "EMUAssistant.exe"), "0.3.9").unwrap().is_some());
        assert!(parse_release(&release("v0.4.0", "Other.zip"), "0.3.0").is_err());
    }

    #[test]
    fn swaps_and_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("EMUAssistant.exe");
        let new = sibling(&exe, ".new");
        fs::write(&exe, "MZ old").unwrap();
        fs::write(&new, "MZ new").unwrap();
        swap(&exe, &new).unwrap();
        assert_eq!(fs::read_to_string(&exe).unwrap(), "MZ new");
        // nothing was running it, so no .old is left
        assert!(!sibling(&exe, ".old").exists() && !new.exists());

        // the new file is missing: the original stays where it was
        assert!(swap(&exe, &new).is_err());
        assert_eq!(fs::read_to_string(&exe).unwrap(), "MZ new");
    }

    #[test]
    fn checks_the_new_exe() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("x.exe");
        fs::write(&f, "<html>").unwrap();
        assert!(check_exe(&f, 0, None, None).is_err());
        fs::write(&f, "MZ....").unwrap();
        assert!(check_exe(&f, 6, None, None).is_ok());
        assert!(check_exe(&f, 7, None, None).is_err());
        let sum = sha256_file(&f).unwrap();
        assert_eq!(sum.len(), 64);
        assert!(check_exe(&f, 6, None, Some(&sum)).is_ok());
        assert!(check_exe(&f, 6, None, Some(&sum.to_uppercase())).is_ok());
        let mut wrong = sum.clone();
        let last = wrong.pop().unwrap();
        wrong.push(if last == '0' { '1' } else { '0' });
        let err = check_exe(&f, 6, None, Some(&wrong)).unwrap_err().to_string();
        assert!(err.contains("checksum"), "{err}");
        // SHA-256 of "abc"
        fs::write(&f, "abc").unwrap();
        assert_eq!(sha256_file(&f).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn reads_the_published_checksum() {
        let sum = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(parse_checksum(&format!("{sum}  EMUAssistant.exe
")).as_deref(), Some(sum));
        assert_eq!(parse_checksum(&sum.to_uppercase()).as_deref(), Some(sum));
        assert!(parse_checksum("<html>not found</html>").is_none());
        assert!(parse_checksum("").is_none());

        // GitHub's digest wins; the .sha256 asset is remembered as the fallback
        let mut rel = release("v0.9.0", "EMUAssistant.exe");
        rel["assets"][0]["digest"] = json!(format!("sha256:{}", sum.to_uppercase()));
        rel["assets"].as_array_mut().unwrap().push(json!({
            "name": "EMUAssistant.exe.sha256",
            "browser_download_url": "https://example.org/EMUAssistant.exe.sha256",
            "size": 84
        }));
        let r = parse_release(&rel, "0.5.0").unwrap().unwrap();
        assert_eq!(r.sha256.as_deref(), Some(sum));
        assert_eq!(r.checksum_url.as_deref(), Some("https://example.org/EMUAssistant.exe.sha256"));
        let r = parse_release(&release("v0.9.0", "EMUAssistant.exe"), "0.5.0").unwrap().unwrap();
        assert_eq!((r.sha256, r.checksum_url), (None, None));
    }

    /// `cargo test live_self_update -- --ignored`: fetches the latest real release and swaps it
    /// in for a stand-in exe, as an older build would.
    #[test]
    #[ignore]
    fn live_self_update() {
        let http = Http::new(std::env::var("GITHUB_TOKEN").ok());
        let json = http.get_json(&format!("https://api.github.com/repos/{}/releases/latest", repo())).unwrap();
        let rel = parse_release(&json, "0.0.1").unwrap().expect("a release newer than 0.0.1");
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join(EXE_NAME);
        fs::write(&exe, "MZ stand-in").unwrap();
        let new = sibling(&exe, ".new");
        assert!(http.download(&rel.asset_url, &new, &AtomicBool::new(false), &mut |_| {}).unwrap());
        let sum = rel.sha256.clone().or_else(|| rel.checksum_url.as_ref().and_then(|u| parse_checksum(&http.get_text(u).ok()?)));
        println!("checksum: {sum:?}");
        check_exe(&new, rel.size, Some(&rel.version), sum.as_deref()).unwrap();
        swap(&exe, &new).unwrap();
        let got = pe_version(&exe).map(|v| short_version(&v));
        assert_eq!(got.as_deref(), Some(rel.version.as_str()));
        println!("installed {} ({} bytes) from {}", rel.version, rel.size, rel.page);
    }

    #[test]
    fn replaces_older_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let es = tmp.path().join("ES-DE");
        fs::create_dir_all(&es).unwrap();
        let drive_exe = tmp.path().join("drive").join(EXE_NAME);
        fs::create_dir_all(drive_exe.parent().unwrap()).unwrap();
        fs::write(&drive_exe, "MZ new build").unwrap();
        let installed = es.join(EXE_NAME);
        fs::write(&installed, "MZ old").unwrap();
        let home = tmp.path().join("Desktop").join(EXE_NAME);

        // the Desktop copy doesn't exist, the drive's own exe is never offered
        let cands = candidates(Some(&home), &es);
        let mut with_self = cands.clone();
        with_self.push(drive_exe.clone());
        let old = older_copies(&with_self, &drive_exe, "0.4.0");
        assert_eq!(old.len(), 1);
        assert_eq!(old[0].path, installed);
        assert!(old[0].version.is_none());

        replace(&installed, &drive_exe).unwrap();
        assert_eq!(fs::read_to_string(&installed).unwrap(), "MZ new build");
        let mtime = |p: &Path| FileTime::from_last_modification_time(&fs::metadata(p).unwrap()).unix_seconds();
        assert_eq!(mtime(&installed), mtime(&drive_exe));
        assert!(!sibling(&installed, ".old").exists() && !sibling(&installed, ".new").exists());
    }
}

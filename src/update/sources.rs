//! Where each program's updates come from, and finding the latest release there.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::http::Http;
use super::version::version_in;
use crate::esde::find_rules::wildcard;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Kind {
    /// GitHub releases of `owner/repo`.
    #[default]
    GitHub,
    /// GitLab releases of `group/project` on gitlab.com.
    GitLab,
    /// Gitea / Forgejo releases of `owner/repo` on `host`.
    Gitea,
    /// RetroArch stable builds from buildbot.libretro.com.
    Buildbot,
    /// A fixed download link; changes are detected from its Last-Modified header.
    Url,
}

impl Kind {
    pub const ALL: [Kind; 5] = [Kind::GitHub, Kind::GitLab, Kind::Gitea, Kind::Buildbot, Kind::Url];

    pub fn label(self) -> &'static str {
        match self {
            Kind::GitHub => "GitHub releases",
            Kind::GitLab => "GitLab releases",
            Kind::Gitea => "Gitea / Forgejo releases",
            Kind::Buildbot => "libretro buildbot",
            Kind::Url => "Direct download link",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(default)]
pub struct SourceSpec {
    pub kind: Kind,
    /// `owner/repo` (GitHub, Gitea), `group/project` (GitLab) or the link (Url).
    pub repo: String,
    /// Server name for Gitea / Forgejo, e.g. `git.eden-emu.dev`.
    pub host: String,
    /// Asset file name pattern with `*` wildcards. Empty picks a Windows x64 zip/7z.
    pub asset: String,
}

impl SourceSpec {
    fn new(kind: Kind, repo: &str, asset: &str) -> Self {
        SourceSpec { kind, repo: repo.into(), host: String::new(), asset: asset.into() }
    }

    pub fn describe(&self) -> String {
        match self.kind {
            Kind::GitHub => format!("github.com/{}", self.repo),
            Kind::GitLab => format!("gitlab.com/{}", self.repo),
            Kind::Gitea => format!("{}/{}", self.host, self.repo),
            Kind::Buildbot => "buildbot.libretro.com".into(),
            Kind::Url => self.repo.clone(),
        }
    }
}

/// Built-in sources by ES-DE emulator id (and `ES-DE` for ES-DE itself).
pub fn builtin(id: &str) -> Option<SourceSpec> {
    use Kind::*;
    let s = match id {
        super::ESDE_ID => SourceSpec::new(GitLab, "es-de/emulationstation-de", "*x64_Portable.zip"),
        "RETROARCH" => SourceSpec::new(Buildbot, "", ""),
        "ARES" => SourceSpec::new(GitHub, "ares-emulator/ares", "ares-windows-x64.zip"),
        "AZAHAR" => SourceSpec::new(GitHub, "azahar-emu/azahar", "azahar-windows-msvc-*.zip"),
        "CEMU" => SourceSpec::new(GitHub, "cemu-project/Cemu", "cemu-*-windows-x64.zip"),
        "DOSBOX-STAGING" => SourceSpec::new(GitHub, "dosbox-staging/dosbox-staging", "dosbox-staging-windows-x64-*.zip"),
        "DOSBOX-X" => SourceSpec::new(GitHub, "joncampbell123/dosbox-x", "dosbox-x-vsbuild-win64-*-portable.zip"),
        "DUCKSTATION" => SourceSpec::new(GitHub, "stenzek/duckstation", "duckstation-windows-x64-release.zip"),
        "EDEN" => SourceSpec {
            kind: Gitea,
            repo: "eden-emu/eden".into(),
            host: "git.eden-emu.dev".into(),
            asset: "Eden-Windows-*-amd64-msvc-standard.zip".into(),
        },
        "FLYCAST" => SourceSpec::new(GitHub, "flyinghead/flycast", "flycast-win64-*.zip"),
        "GEARBOY" => SourceSpec::new(GitHub, "drhelius/Gearboy", "Gearboy-*-desktop-windows-x64.zip"),
        "JGENESIS" => SourceSpec::new(GitHub, "jsgroth/jgenesis", "jgenesis-*-windows-x86_64.zip"),
        "MELONDS" => SourceSpec::new(GitHub, "melonDS-emu/melonDS", "melonDS-*-windows-x86_64.zip"),
        "MESEN" => SourceSpec::new(GitHub, "SourMesen/Mesen2", "Mesen_*_Windows.zip"),
        "MGBA" => SourceSpec::new(GitHub, "mgba-emu/mgba", "mGBA-*-win64.7z"),
        "NOODS" => SourceSpec::new(GitHub, "Hydr8gon/NooDS", "noods-windows.zip"),
        "PANDA3DS" => SourceSpec::new(GitHub, "wheremyfoodat/Panda3DS", "Windows-SDL.zip"),
        "PCSX2" => SourceSpec::new(GitHub, "PCSX2/pcsx2", "pcsx2-*-windows-x64-Qt.7z"),
        "PPSSPP" => SourceSpec::new(GitHub, "hrydgard/ppsspp", "PPSSPP-*-Windows-x64.zip"),
        "RPCS3" => SourceSpec::new(GitHub, "RPCS3/rpcs3-binaries-win", "rpcs3-*_win64.7z"),
        "SAMEBOY" => SourceSpec::new(GitHub, "LIJI32/SameBoy", "sameboy_winsdl_*.zip"),
        "SHADPS4" => SourceSpec::new(GitHub, "shadps4-emu/shadPS4", "shadps4-win64-sdl-*.zip"),
        "SIMPLE64" => SourceSpec::new(GitHub, "simple64/simple64", "simple64-win64-*.zip"),
        "SNES9X" => SourceSpec::new(GitHub, "snes9xgit/snes9x", "snes9x-*-win32-x64.zip"),
        "STELLA" => SourceSpec::new(GitHub, "stella-emu/stella", "Stella-*-windows.zip"),
        "VBA-M" => SourceSpec::new(GitHub, "visualboyadvance-m/visualboyadvance-m", "visualboyadvance-m-Win-x86_64.zip"),
        "VITA3K" => SourceSpec::new(GitHub, "Vita3K/Vita3K", "windows-latest.zip"),
        "XEMU" => SourceSpec::new(GitHub, "xemu-project/xemu", "xemu-win-x86_64-release.zip"),
        "XENIA" => SourceSpec::new(GitHub, "xenia-canary/xenia-canary-releases", "xenia_canary_windows.zip"),
        "YMIR" => SourceSpec::new(GitHub, "StrikerX3/Ymir", "ymir-windows-x86_64-AVX2-*.zip"),
        _ => return None,
    };
    Some(s)
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Release {
    /// Version shown and recorded: the version number in the tag or file name, else the tag.
    pub version: String,
    /// Release / asset date as given by the server (ISO 8601 or an HTTP date), may be empty.
    pub published: String,
    pub asset_name: String,
    pub asset_url: String,
    /// 0 when the server doesn't say.
    pub size: u64,
}

/// Picks the asset to download: the pattern when given, otherwise the most likely
/// portable Windows x64 archive.
pub fn pick_asset<'a>(names: impl IntoIterator<Item = &'a str>, pattern: &str) -> Option<&'a str> {
    let pattern = pattern.trim();
    if !pattern.is_empty() {
        return names.into_iter().find(|n| wildcard(pattern, n));
    }
    const BAD: [&str; 12] =
        ["debug", "dbg", "symbols", "pdb", "arm64", "aarch64", "installer", "setup", "libretro", "linux", "macos", "android"];
    names
        .into_iter()
        .filter(|n| {
            let l = n.to_lowercase();
            (l.ends_with(".zip") || l.ends_with(".7z"))
                && l.contains("win")
                && ["x64", "x86_64", "amd64", "win64"].iter().any(|a| l.contains(a))
                && !BAD.iter().any(|b| l.contains(b))
        })
        .min_by_key(|n| n.len())
}

fn release_version(tag: &str, asset: &str) -> String {
    version_in(tag).or_else(|| version_in(asset)).unwrap_or_else(|| tag.trim().to_string())
}

/// Finds the newest release of a source that has a matching Windows asset.
pub fn latest(http: &Http, spec: &SourceSpec) -> Result<Release> {
    let mut rel = match spec.kind {
        Kind::GitHub => github(http, spec),
        Kind::GitLab => gitlab(http, spec),
        Kind::Gitea => gitea(http, spec),
        Kind::Buildbot => buildbot(http),
        Kind::Url => direct(http, spec),
    }?;
    // some servers list external downloads without a size
    if rel.size == 0 {
        rel.size = http.head(&rel.asset_url).map(|h| h.size).unwrap_or(0);
    }
    Ok(rel)
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// A GitHub / Gitea release object -> Release, if it has a matching asset.
fn from_release(rel: &Value, pattern: &str) -> Option<Release> {
    let assets = rel.get("assets")?.as_array()?;
    let name = pick_asset(assets.iter().map(|a| s(a, "name")), pattern)?;
    let a = assets.iter().find(|a| s(a, "name") == name)?;
    let published = [s(a, "updated_at"), s(rel, "published_at"), s(a, "created_at")]
        .into_iter()
        .find(|d| !d.is_empty())
        .unwrap_or("");
    Some(Release {
        version: release_version(s(rel, "tag_name"), name),
        published: published.into(),
        asset_name: name.into(),
        asset_url: s(a, "browser_download_url").into(),
        size: a.get("size").and_then(Value::as_u64).unwrap_or(0),
    })
}

/// Stable releases first; projects that only publish pre-releases (rolling builds) get their newest.
fn pick_release(list: &[Value], pattern: &str) -> Option<Release> {
    let usable = |r: &&Value| !r.get("draft").and_then(Value::as_bool).unwrap_or(false);
    let pre = |r: &Value| r.get("prerelease").and_then(Value::as_bool).unwrap_or(false);
    list.iter()
        .filter(usable)
        .filter(|r| !pre(r))
        .find_map(|r| from_release(r, pattern))
        .or_else(|| list.iter().filter(usable).find_map(|r| from_release(r, pattern)))
}

fn no_asset(spec: &SourceSpec) -> anyhow::Error {
    if spec.asset.trim().is_empty() {
        anyhow::anyhow!("no Windows download found in the latest releases of {}", spec.describe())
    } else {
        anyhow::anyhow!("no download matching \"{}\" in the latest releases of {}", spec.asset, spec.describe())
    }
}

fn github(http: &Http, spec: &SourceSpec) -> Result<Release> {
    let repo = spec.repo.trim().trim_matches('/');
    let api = format!("https://api.github.com/repos/{repo}/releases");
    // /latest is the newest stable release; it 404s for projects without one
    if let Ok(rel) = http.get_json(&format!("{api}/latest")) {
        if let Some(r) = from_release(&rel, &spec.asset) {
            return Ok(r);
        }
    }
    let list: Vec<Value> = serde_json::from_value(http.get_json(&format!("{api}?per_page=15"))?)?;
    pick_release(&list, &spec.asset).ok_or_else(|| no_asset(spec))
}

fn gitea(http: &Http, spec: &SourceSpec) -> Result<Release> {
    let host = spec.host.trim().trim_start_matches("https://").trim_end_matches('/');
    if host.is_empty() {
        bail!("no server set for this Gitea / Forgejo source");
    }
    let url = format!("https://{host}/api/v1/repos/{}/releases?limit=15", spec.repo.trim().trim_matches('/'));
    let list: Vec<Value> = serde_json::from_value(http.get_json(&url)?)?;
    pick_release(&list, &spec.asset).ok_or_else(|| no_asset(spec))
}

fn gitlab(http: &Http, spec: &SourceSpec) -> Result<Release> {
    let project = spec.repo.trim().trim_matches('/').replace('/', "%2F");
    let url = format!("https://gitlab.com/api/v4/projects/{project}/releases?per_page=10");
    let list: Vec<Value> = serde_json::from_value(http.get_json(&url)?)?;
    for rel in list.iter().filter(|r| !r.get("upcoming_release").and_then(Value::as_bool).unwrap_or(false)) {
        let links = rel.pointer("/assets/links").and_then(Value::as_array).cloned().unwrap_or_default();
        let Some(name) = pick_asset(links.iter().map(|l| s(l, "name")), &spec.asset) else { continue };
        let link = links.iter().find(|l| s(l, "name") == name).context("asset vanished")?;
        let url = [s(link, "direct_asset_url"), s(link, "url")].into_iter().find(|u| !u.is_empty()).unwrap_or("");
        return Ok(Release {
            version: release_version(s(rel, "tag_name"), name),
            published: s(rel, "released_at").into(),
            asset_name: name.into(),
            asset_url: url.into(),
            size: 0,
        });
    }
    Err(no_asset(spec))
}

pub const BUILDBOT: &str = "https://buildbot.libretro.com";

fn buildbot(http: &Http) -> Result<Release> {
    let index = http.get_text(&format!("{BUILDBOT}/stable/"))?;
    let version = stable_versions(&index)
        .into_iter()
        .max_by(|a, b| super::version::cmp_numeric(a, b).unwrap_or(std::cmp::Ordering::Equal))
        .context("no versions listed on buildbot.libretro.com/stable")?;
    let url = format!("{BUILDBOT}/stable/{version}/windows/x86_64/RetroArch.7z");
    let head = http.head(&url)?;
    Ok(Release { version, published: head.last_modified, asset_name: "RetroArch.7z".into(), asset_url: url, size: head.size })
}

/// Version folders in the buildbot's `stable/` listing.
fn stable_versions(index: &str) -> Vec<String> {
    index
        .split("href=\"/stable/")
        .skip(1)
        .filter_map(|rest| rest.split_once("/\"").map(|(v, _)| v))
        .filter(|v| v.chars().any(|c| c.is_ascii_digit()) && v.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .map(String::from)
        .collect()
}

fn direct(http: &Http, spec: &SourceSpec) -> Result<Release> {
    let url = spec.repo.trim();
    if !url.starts_with("http") {
        bail!("set a download link (http…) for this program");
    }
    let head = http.head(url)?;
    let name = url.rsplit('/').next().unwrap_or(url).split('?').next().unwrap_or_default().to_string();
    // a fixed link has no version; its date identifies the build
    let version = version_in(&name).unwrap_or_else(|| head.last_modified.clone());
    Ok(Release { version, published: head.last_modified, asset_name: name, asset_url: url.into(), size: head.size })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_assets() {
        let duck = [
            "duckstation-windows-arm64-release.zip",
            "duckstation-windows-x64-installer.exe",
            "duckstation-windows-x64-release-symbols.7z",
            "duckstation-windows-x64-release.zip",
            "duckstation-windows-x64-sse2-release.zip",
        ];
        assert_eq!(pick_asset(duck, ""), Some("duckstation-windows-x64-release.zip"));
        assert_eq!(pick_asset(duck, "*sse2-release.zip"), Some("duckstation-windows-x64-sse2-release.zip"));
        let xemu = ["xemu-0.8.136-dbg-windows-x86_64.zip", "xemu-0.8.136-windows-x86_64-pdb.zip", "xemu-win-x86_64-release.zip"];
        assert_eq!(pick_asset(xemu, ""), Some("xemu-win-x86_64-release.zip"));
        assert_eq!(pick_asset(["a-linux.tar.gz", "a-macos.dmg"], ""), None);
        assert_eq!(pick_asset(["x.zip"], "nomatch*"), None);
    }

    #[test]
    fn prefers_stable_then_rolling() {
        let rel = |tag: &str, pre: bool, asset: &str| {
            serde_json::json!({"tag_name": tag, "prerelease": pre, "draft": false, "published_at": "2026-01-01T00:00:00Z",
                "assets": [{"name": asset, "browser_download_url": format!("https://x/{asset}"), "size": 5}]})
        };
        let list = vec![rel("v2.9.1", true, "pcsx2-v2.9.1-windows-x64-Qt.7z"), rel("v2.6.0", false, "pcsx2-v2.6.0-windows-x64-Qt.7z")];
        let r = pick_release(&list, "pcsx2-*-windows-x64-Qt.7z").unwrap();
        assert_eq!((r.version.as_str(), r.size), ("2.6.0", 5));
        let rolling = vec![rel("continuous", true, "windows-latest.zip")];
        assert_eq!(pick_release(&rolling, "windows-latest.zip").unwrap().version, "continuous");
        // RPCS3 tags are commit hashes; the version comes from the file name
        let rpcs3 = vec![rel("build-fff0c96b", false, "rpcs3-v0.0.18-12817-fff0c96b_win64.7z")];
        assert_eq!(pick_release(&rpcs3, "rpcs3-*_win64.7z").unwrap().version, "0.0.18");
    }

    #[test]
    fn parses_buildbot_index() {
        let html = r#"<a href="/stable/1.9.0/">1.9.0</a><a href="/stable/1.22.2/">x</a><a href="/stable/../">up</a>"#;
        let v = stable_versions(html);
        assert_eq!(v, vec!["1.9.0", "1.22.2"]);
    }

    #[test]
    fn every_builtin_has_a_pattern_or_is_special() {
        for id in [super::super::ESDE_ID, "PCSX2", "EDEN", "RETROARCH"] {
            let b = builtin(id).unwrap();
            assert!(b.kind == Kind::Buildbot || !b.asset.is_empty(), "{id}");
        }
    }
}

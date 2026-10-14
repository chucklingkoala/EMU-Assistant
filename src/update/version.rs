//! Installed versions: the marker EMU Assistant writes after an update, the exe's own
//! version resource, and comparing them with a release.

use std::cmp::Ordering;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Written into each install folder after an update; travels with Export / Deploy.
pub const MARKER: &str = "EMUAssistant.version.json";

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Marker {
    pub source: String,
    pub version: String,
    pub asset: String,
    pub published: String,
    pub installed_unix: i64,
}

pub fn read_marker(dir: &Path) -> Option<Marker> {
    serde_json::from_str(&std::fs::read_to_string(dir.join(MARKER)).ok()?).ok()
}

pub fn write_marker(dir: &Path, m: &Marker) -> Result<()> {
    std::fs::write(dir.join(MARKER), serde_json::to_string_pretty(m)?)?;
    Ok(())
}

/// The first dotted version number in a tag or file name: `v2.9.127` -> `2.9.127`,
/// `rpcs3-v0.0.18-12817-ab_win64.7z` -> `0.0.18`. Needs at least one dot.
pub fn version_in(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() && (i == 0 || !b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'v' || b[i - 1] == b'V') {
            let start = i;
            let mut end = i;
            let mut dots = 0;
            while end < b.len() && (b[end].is_ascii_digit() || (b[end] == b'.' && end + 1 < b.len() && b[end + 1].is_ascii_digit())) {
                if b[end] == b'.' {
                    dots += 1;
                }
                end += 1;
            }
            if dots > 0 {
                return Some(s[start..end].to_string());
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

fn numbers(v: &str) -> Option<Vec<u64>> {
    let mut n: Vec<u64> = version_in(v)?.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    while n.len() > 1 && n.last() == Some(&0) {
        n.pop();
    }
    Some(n)
}

/// Numeric comparison of two version strings (`3.5.0.0` == `v3.5`). `None` if either has no number.
pub fn cmp_numeric(a: &str, b: &str) -> Option<Ordering> {
    Some(numbers(a)?.cmp(&numbers(b)?))
}

/// `major.minor.build.revision` from the exe's version resource.
#[cfg(windows)]
pub fn pe_version(exe: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
    };
    let wide: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
    unsafe {
        let size = GetFileVersionInfoSizeW(wide.as_ptr(), std::ptr::null_mut());
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(wide.as_ptr(), 0, size, data.as_mut_ptr().cast()) == 0 {
            return None;
        }
        let mut info: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let root: Vec<u16> = "\\".encode_utf16().chain([0]).collect();
        if VerQueryValueW(data.as_ptr().cast(), root.as_ptr(), &mut info, &mut len) == 0 || info.is_null() {
            return None;
        }
        let f = &*(info as *const VS_FIXEDFILEINFO);
        let v = [f.dwFileVersionMS >> 16, f.dwFileVersionMS & 0xffff, f.dwFileVersionLS >> 16, f.dwFileVersionLS & 0xffff];
        (v != [0, 0, 0, 0]).then(|| v.iter().map(u32::to_string).collect::<Vec<_>>().join("."))
    }
}

#[cfg(not(windows))]
pub fn pe_version(_: &Path) -> Option<String> {
    None
}

/// Shortens `3.5.0.0` to `3.5.0` for display.
pub fn short_version(v: &str) -> String {
    let mut parts: Vec<&str> = v.split('.').collect();
    while parts.len() > 3 && parts.last() == Some(&"0") {
        parts.pop();
    }
    parts.join(".")
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn unix_from_parts(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
    days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + s
}

/// Parses `2026-10-12T06:30:00Z`, `2026-10-12 06:30` (UTC) and HTTP dates
/// (`Mon, 12 Oct 2026 06:30:00 GMT`) to unix seconds.
pub fn parse_ts(s: &str) -> Option<i64> {
    let s = s.trim();
    let nums = |t: &str| -> Vec<i64> { t.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).filter_map(|p| p.parse().ok()).collect() };
    if s.len() >= 10 && s.as_bytes()[4] == b'-' {
        let n = nums(&s[..s.len().min(19)]);
        if n.len() >= 3 {
            let g = |i: usize| n.get(i).copied().unwrap_or(0);
            return Some(unix_from_parts(g(0), g(1), g(2), g(3), g(4), g(5)));
        }
        return None;
    }
    // HTTP date
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let lower = s.to_lowercase();
    let month = MONTHS.iter().position(|m| lower.contains(&format!(" {m} ")))? as i64 + 1;
    let n = nums(s);
    let (d, y) = (*n.first()?, *n.get(1)?);
    Some(unix_from_parts(y, month, d, n.get(2).copied().unwrap_or(0), n.get(3).copied().unwrap_or(0), n.get(4).copied().unwrap_or(0)))
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `yyyy-mm-dd` for a unix time.
pub fn date_text(unix: i64) -> String {
    let days = unix.div_euclid(86400);
    // civil_from_days
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_versions() {
        assert_eq!(version_in("v2.9.127").as_deref(), Some("2.9.127"));
        assert_eq!(version_in("v.0.19.0").as_deref(), Some("0.19.0"));
        assert_eq!(version_in("rpcs3-v0.0.18-12817-fff0c96b_win64.7z").as_deref(), Some("0.0.18"));
        assert_eq!(version_in("mGBA-0.10.5-win64.7z").as_deref(), Some("0.10.5"));
        assert_eq!(version_in("build-fff0c96bf38d"), None);
        assert_eq!(version_in("continuous"), None);
        assert_eq!(version_in("x64.zip"), None);
    }

    #[test]
    fn compares_numerically() {
        assert_eq!(cmp_numeric("3.5.0.0", "v3.5.0"), Some(Ordering::Equal));
        assert_eq!(cmp_numeric("1.9.10", "1.22.2"), Some(Ordering::Less));
        assert_eq!(cmp_numeric("0.8.136", "0.8.99"), Some(Ordering::Greater));
        assert_eq!(cmp_numeric("latest", "1.0"), None);
        assert_eq!(short_version("3.5.0.0"), "3.5.0");
    }

    #[test]
    fn parses_timestamps() {
        let t = unix_from_parts(2026, 10, 12, 6, 30, 0);
        assert_eq!(parse_ts("2026-10-12T06:30:00Z"), Some(t));
        assert_eq!(parse_ts("2026-10-12 06:30"), Some(t));
        assert_eq!(parse_ts("Mon, 12 Oct 2026 06:30:00 GMT"), Some(t));
        assert_eq!(parse_ts(""), None);
        assert_eq!(date_text(t), "2026-10-12");
        assert_eq!(unix_from_parts(1970, 1, 1, 0, 0, 0), 0);
    }

    #[test]
    #[cfg(windows)]
    fn reads_own_exe_version_or_none() {
        // test binaries carry no version resource; the call must simply not crash
        let _ = pe_version(&std::env::current_exe().unwrap());
        let notepad = Path::new(r"C:\Windows\System32\notepad.exe");
        if notepad.is_file() {
            assert!(pe_version(notepad).is_some());
        }
    }
}

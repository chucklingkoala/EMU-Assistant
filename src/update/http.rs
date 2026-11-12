//! Small HTTP client for release lookups and downloads.
//!
//! Lookups can use a cache of earlier answers (`http_cache.json` next to the config):
//! a repeated request sends the stored `ETag` and a `304 Not Modified` reuses the stored
//! body. GitHub doesn't count such answers against the rate limit for requests with a
//! token; without one they still count, but the reply is a few bytes.
//!
//! Downloads resume: the `.emuasist-part` file is kept after a dropped connection or a
//! cancel, and the next attempt asks only for the missing bytes.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ureq::Agent;

use crate::fsutil::PART_EXT;

pub struct Http {
    agent: Agent,
    github_token: Option<String>,
    cache: Option<(PathBuf, Mutex<Cache>)>,
}

pub struct Head {
    pub size: u64,
    pub last_modified: String,
}

/// Answers kept between runs.
#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
#[serde(default)]
pub struct Cache {
    /// url -> the last answer that came with an ETag
    pub answers: BTreeMap<String, Cached>,
    /// GitHub repository -> which endpoint had its release last time ("latest" or "list")
    pub github_endpoint: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
#[serde(default)]
pub struct Cached {
    pub etag: String,
    pub body: String,
    pub used_unix: i64,
}

/// Answers not used for this long are dropped when the cache is saved.
const CACHE_DAYS: i64 = 90;
/// Larger answers aren't kept.
const CACHE_MAX_BODY: usize = 1 << 20;
/// Attempts per download, resuming each time.
const DOWNLOAD_ATTEMPTS: u32 = 4;

fn now() -> i64 {
    super::version::now_unix()
}

impl Cache {
    pub fn load(path: &Path) -> Cache {
        fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&mut self, path: &Path) -> Result<()> {
        let cutoff = now() - CACHE_DAYS * 86400;
        self.answers.retain(|_, c| c.used_unix >= cutoff);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string(self)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// `<config dir>\EMUAssistant\http_cache.json`
pub fn cache_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("EMUAssistant").join("http_cache.json"))
}

/// Where a download's resume information is kept: `<part>.json`.
fn resume_path(part: &Path) -> PathBuf {
    let mut s = part.as_os_str().to_owned();
    s.push(".json");
    PathBuf::from(s)
}

/// What a kept part file was downloaded from.
#[derive(Serialize, Deserialize)]
struct Resume {
    url: String,
    /// The strong ETag or Last-Modified of the answer, sent back as `If-Range`.
    validator: String,
}

fn header(resp: &ureq::http::Response<ureq::Body>, name: &str) -> String {
    resp.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

impl Http {
    pub fn new(github_token: Option<String>) -> Self {
        let config = Agent::config_builder()
            .user_agent(format!("EMUAssistant/{}", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .build();
        Http { agent: config.into(), github_token, cache: None }
    }

    /// Like [`Http::new`], with the answer cache from `path`. Call [`Http::save_cache`] when done.
    pub fn with_cache(github_token: Option<String>, path: Option<PathBuf>) -> Self {
        let mut h = Self::new(github_token);
        h.cache = path.map(|p| {
            let c = Cache::load(&p);
            (p, Mutex::new(c))
        });
        h
    }

    pub fn save_cache(&self) {
        if let Some((path, cache)) = &self.cache {
            let _ = cache.lock().unwrap().save(path);
        }
    }

    /// The GitHub endpoint that had `repo`'s release last time.
    pub fn github_endpoint(&self, repo: &str) -> Option<String> {
        let (_, cache) = self.cache.as_ref()?;
        cache.lock().unwrap().github_endpoint.get(&repo.to_lowercase()).cloned()
    }

    pub fn set_github_endpoint(&self, repo: &str, endpoint: &str) {
        if let Some((_, cache)) = &self.cache {
            cache.lock().unwrap().github_endpoint.insert(repo.to_lowercase(), endpoint.to_string());
        }
    }

    fn request(&self, url: &str) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
        let mut req = self.agent.get(url);
        if url.starts_with("https://api.github.com/") {
            req = req.header("Accept", "application/vnd.github+json");
            if let Some(t) = &self.github_token {
                req = req.header("Authorization", format!("Bearer {}", t.trim()));
            }
        }
        req
    }

    pub fn get_json(&self, url: &str) -> Result<Value> {
        let text = self.get_text(url)?;
        serde_json::from_str(&text).with_context(|| format!("unexpected reply from {url}"))
    }

    pub fn get_text(&self, url: &str) -> Result<String> {
        let cached = self.cache.as_ref().and_then(|(_, c)| c.lock().unwrap().answers.get(url).map(|a| a.etag.clone()));
        let mut req = self.request(url);
        if let Some(etag) = &cached {
            req = req.header("If-None-Match", etag);
        }
        let mut resp = req.call().map_err(|e| describe(e, url))?;
        let status = resp.status().as_u16();
        if status == 304 {
            if let Some((_, cache)) = &self.cache {
                if let Some(a) = cache.lock().unwrap().answers.get_mut(url) {
                    a.used_unix = now();
                    return Ok(a.body.clone());
                }
            }
            bail!("{url} answered 304 to a request that wasn't conditional");
        }
        check_status(status, url)?;
        let etag = header(&resp, "etag");
        let body = resp.body_mut().with_config().limit(32 << 20).read_to_string().with_context(|| format!("reading {url}"))?;
        if let Some((_, cache)) = &self.cache {
            let mut c = cache.lock().unwrap();
            if !etag.is_empty() && body.len() <= CACHE_MAX_BODY {
                c.answers.insert(url.to_string(), Cached { etag, body: body.clone(), used_unix: now() });
            } else {
                c.answers.remove(url);
            }
        }
        Ok(body)
    }

    pub fn head(&self, url: &str) -> Result<Head> {
        let resp = self.agent.head(url).call().map_err(|e| describe(e, url))?;
        check_status(resp.status().as_u16(), url)?;
        Ok(Head { size: header(&resp, "content-length").parse().unwrap_or(0), last_modified: header(&resp, "last-modified") })
    }

    /// Downloads `url` to `dest` through a `.emuasist-part` file. `on_bytes` gets the size
    /// of each new chunk (bytes already in a kept part file are reported once at the start).
    /// Returns `Ok(false)` when cancelled. A dropped connection is retried, continuing where
    /// it stopped; a part file left by an earlier run for the same URL is continued too.
    pub fn download(&self, url: &str, dest: &Path, cancel: &AtomicBool, on_bytes: &mut dyn FnMut(u64)) -> Result<bool> {
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p)?;
        }
        let mut part = dest.as_os_str().to_owned();
        part.push(PART_EXT);
        let part = PathBuf::from(part);
        let meta = resume_path(&part);
        let mut resume: Option<Resume> =
            fs::read_to_string(&meta).ok().and_then(|s| serde_json::from_str::<Resume>(&s).ok()).filter(|r| r.url == url);
        if resume.is_none() {
            let _ = fs::remove_file(&part);
            let _ = fs::remove_file(&meta);
        }
        let mut reported = 0u64;
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.fetch(url, &part, &mut resume, cancel, &mut reported, on_bytes) {
                Ok(true) => {
                    fs::rename(&part, dest)?;
                    let _ = fs::remove_file(&meta);
                    return Ok(true);
                }
                Ok(false) => {
                    match &resume {
                        Some(r) => fs::write(&meta, serde_json::to_string(r)?)?,
                        None => {
                            let _ = fs::remove_file(&part);
                        }
                    }
                    return Ok(false);
                }
                Err(e) => {
                    // keep the part file for the next attempt only when it can be continued
                    match &resume {
                        Some(r) => {
                            let _ = fs::write(&meta, serde_json::to_string(r)?);
                        }
                        None => {
                            let _ = fs::remove_file(&part);
                            let _ = fs::remove_file(&meta);
                        }
                    }
                    if attempt >= DOWNLOAD_ATTEMPTS || cancel.load(Ordering::Relaxed) || !retryable(&e) {
                        return Err(e);
                    }
                    std::thread::sleep(Duration::from_secs(attempt as u64));
                }
            }
        }
    }

    /// One attempt of [`Http::download`].
    fn fetch(
        &self,
        url: &str,
        part: &Path,
        resume: &mut Option<Resume>,
        cancel: &AtomicBool,
        reported: &mut u64,
        on_bytes: &mut dyn FnMut(u64),
    ) -> Result<bool> {
        let have = match resume {
            Some(_) => fs::metadata(part).map(|m| m.len()).unwrap_or(0),
            None => 0,
        };
        let mut req = self.request(url);
        if let (Some(r), true) = (resume.as_ref(), have > 0) {
            req = req.header("Range", format!("bytes={have}-")).header("If-Range", &r.validator);
        }
        let resp = req.call().map_err(|e| describe(e, url))?;
        let status = resp.status().as_u16();
        if status == 416 {
            // the kept part doesn't fit the file on the server any more: start over
            let _ = fs::remove_file(part);
            *resume = None;
            bail!(Retry("the server refused to continue the download; starting over".into()));
        }
        check_status(status, url)?;
        let partial = status == 206;
        // a strong validator lets the next attempt continue this answer
        let etag = header(&resp, "etag");
        let validator = if !etag.is_empty() && !etag.starts_with("W/") { etag } else { header(&resp, "last-modified") };
        *resume = (!validator.is_empty()).then(|| Resume { url: url.to_string(), validator });
        let (offset, total) = if partial {
            // Content-Range: bytes <start>-<end>/<total>
            let range = header(&resp, "content-range");
            let start: u64 = range.trim_start_matches("bytes ").split('-').next().and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);
            if start != have {
                let _ = fs::remove_file(part);
                *resume = None;
                bail!(Retry(format!("the server continued at byte {start} instead of {have}; starting over")));
            }
            (have, range.rsplit('/').next().and_then(|t| t.parse::<u64>().ok()))
        } else {
            (0, header(&resp, "content-length").parse::<u64>().ok())
        };
        if offset > *reported {
            on_bytes(offset - *reported);
            *reported = offset;
        }
        let mut out = if partial {
            OpenOptions::new().append(true).open(part).with_context(|| format!("opening {}", part.display()))?
        } else {
            File::create(part).with_context(|| format!("creating {}", part.display()))?
        };
        let mut reader = resp.into_body().into_with_config().limit(u64::MAX).reader();
        let mut buf = vec![0u8; 1 << 20];
        let mut written = offset;
        loop {
            if cancel.load(Ordering::Relaxed) {
                out.sync_all()?;
                return Ok(false);
            }
            let n = reader.read(&mut buf).map_err(|e| anyhow::Error::new(Retry(format!("downloading {url}: {e}"))))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            written += n as u64;
            if written > *reported {
                on_bytes(written - *reported);
                *reported = written;
            }
        }
        out.sync_all()?;
        if let Some(t) = total.filter(|t| *t != written) {
            bail!(Retry(format!("the download stopped at {written} of {t} bytes")));
        }
        Ok(true)
    }
}

/// An error worth another attempt: the connection dropped or the server cut the answer short.
#[derive(Debug)]
struct Retry(String);

impl std::fmt::Display for Retry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Retry {}

fn retryable(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Retry>().is_some() || e.downcast_ref::<ureq::Error>().is_some_and(|u| matches!(u, ureq::Error::Io(_) | ureq::Error::Timeout(_) | ureq::Error::ConnectionFailed))
}

/// Statuses of 400 and above become errors (the agent hands them back as answers).
fn check_status(status: u16, url: &str) -> Result<()> {
    if status >= 400 {
        return Err(describe(ureq::Error::StatusCode(status), url));
    }
    Ok(())
}

fn describe(e: ureq::Error, url: &str) -> anyhow::Error {
    match e {
        ureq::Error::StatusCode(403 | 429) if url.contains("api.github.com") => anyhow::anyhow!(
            "GitHub refused the request (rate limit of 60 requests an hour without a token). Add a GitHub token in Settings > Updates, or try again later."
        ),
        ureq::Error::StatusCode(404) => anyhow::anyhow!("not found: {url}"),
        ureq::Error::StatusCode(code) => anyhow::anyhow!("server answered {code} for {url}"),
        // keep connection errors as ureq errors so a download can tell they are worth a retry
        other @ (ureq::Error::Io(_) | ureq::Error::Timeout(_) | ureq::Error::ConnectionFailed) => {
            anyhow::Error::new(other).context(url.to_string())
        }
        other => anyhow::anyhow!("{url}: {other}"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;
    use std::sync::Arc;

    /// A request as the test server saw it.
    pub struct Req {
        pub path: String,
        pub headers: BTreeMap<String, String>,
    }

    /// A one-thread HTTP server on 127.0.0.1 that answers `n` connections with `answer`.
    /// `answer` returns raw bytes to write; returning fewer bytes than announced simulates
    /// a dropped connection. Returns the base URL and the requests seen.
    pub fn serve(n: usize, answer: impl Fn(usize, &Req) -> Vec<u8> + Send + 'static) -> (String, Arc<Mutex<Vec<Req>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        std::thread::spawn(move || {
            for i in 0..n {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut headers = BTreeMap::new();
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':') {
                        headers.insert(k.trim().to_lowercase(), v.trim().to_string());
                    }
                }
                let req = Req { path, headers };
                let out = answer(i, &req);
                seen2.lock().unwrap().push(req);
                let _ = stream.write_all(&out);
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });
        (base, seen)
    }

    pub fn reply(status: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
        let mut s = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
        for (k, v) in headers {
            s += &format!("{k}: {v}\r\n");
        }
        s += "\r\n";
        let mut out = s.into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn data() -> Vec<u8> {
        (0..200_000u32).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn cache_answers_with_etag_and_expires() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache.json");
        let (base, seen) = serve(2, |i, _| match i {
            0 => reply("200 OK", &[("ETag", "\"v1\"".into()), ("Content-Length", "5".into())], b"hello"),
            _ => reply("304 Not Modified", &[("ETag", "\"v1\"".into())], b""),
        });
        let url = format!("{base}/rel");
        let http = Http::with_cache(None, Some(path.clone()));
        assert_eq!(http.get_text(&url).unwrap(), "hello");
        http.set_github_endpoint("Owner/Repo", "list");
        http.save_cache();

        let http = Http::with_cache(None, Some(path.clone()));
        assert_eq!(http.get_text(&url).unwrap(), "hello", "the 304 reuses the stored body");
        assert_eq!(http.github_endpoint("owner/repo").as_deref(), Some("list"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].path, "/rel");
        assert!(!seen[0].headers.contains_key("if-none-match"));
        assert_eq!(seen[1].headers.get("if-none-match").map(String::as_str), Some("\"v1\""));

        // an answer unused for longer than CACHE_DAYS is dropped on save
        let mut c = Cache::load(&path);
        c.answers.get_mut(&url).unwrap().used_unix = now() - (CACHE_DAYS + 1) * 86400;
        c.save(&path).unwrap();
        assert!(Cache::load(&path).answers.is_empty());
        assert_eq!(Cache::load(&path).github_endpoint.len(), 1);
    }

    #[test]
    fn download_resumes_after_a_dropped_connection() {
        let full = data();
        let f2 = full.clone();
        let (base, seen) = serve(2, move |i, req| {
            let headers = |extra: Vec<(&'static str, String)>| {
                let mut h = vec![("ETag", "\"abc\"".to_string())];
                h.extend(extra);
                h
            };
            match i {
                // announces everything, sends 60,000 bytes, then hangs up
                0 => reply("200 OK", &headers(vec![("Content-Length", f2.len().to_string())]), &f2[..60_000]),
                _ => {
                    assert_eq!(req.headers.get("if-range").map(String::as_str), Some("\"abc\""));
                    let start: usize = req.headers["range"].trim_start_matches("bytes=").trim_end_matches('-').parse().unwrap();
                    let rest = &f2[start..];
                    let range = format!("bytes {start}-{}/{}", f2.len() - 1, f2.len());
                    reply("206 Partial Content", &headers(vec![("Content-Length", rest.len().to_string()), ("Content-Range", range)]), rest)
                }
            }
        });
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("file.zip");
        let mut got = 0u64;
        let ok = Http::new(None).download(&format!("{base}/file.zip"), &dest, &AtomicBool::new(false), &mut |n| got += n).unwrap();
        assert!(ok);
        assert_eq!(fs::read(&dest).unwrap(), full);
        assert_eq!(got, full.len() as u64, "every byte is reported once");
        assert_eq!(seen.lock().unwrap()[1].headers["range"], "bytes=60000-");
        assert!(!resume_path(&PathBuf::from(format!("{}{PART_EXT}", dest.display()))).exists());
    }

    #[test]
    fn download_starts_over_when_the_server_ignores_range() {
        let full = data();
        let f2 = full.clone();
        let (base, _) = serve(2, move |i, _| {
            let len = ("Content-Length", f2.len().to_string());
            match i {
                0 => reply("200 OK", &[("ETag", "\"abc\"".into()), len], &f2[..1000]),
                // a plain 200: the whole file again
                _ => reply("200 OK", &[("ETag", "\"abc\"".into()), len], &f2),
            }
        });
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("file.zip");
        assert!(Http::new(None).download(&format!("{base}/f"), &dest, &AtomicBool::new(false), &mut |_| {}).unwrap());
        assert_eq!(fs::read(&dest).unwrap(), full);
    }

    #[test]
    fn kept_part_from_an_earlier_run_is_continued_or_dropped() {
        let full = data();
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("file.zip");
        let part = PathBuf::from(format!("{}{PART_EXT}", dest.display()));
        let keep = |url: &str| {
            fs::write(&part, &full[..5000]).unwrap();
            fs::write(resume_path(&part), serde_json::to_string(&Resume { url: url.into(), validator: "\"abc\"".into() }).unwrap()).unwrap();
        };

        // same URL: only the rest is asked for
        let f2 = full.clone();
        let (base, seen) = serve(1, move |_, req| {
            let start: usize = req.headers["range"].trim_start_matches("bytes=").trim_end_matches('-').parse().unwrap();
            let range = format!("bytes {start}-{}/{}", f2.len() - 1, f2.len());
            reply("206 Partial Content", &[("Content-Range", range), ("Content-Length", (f2.len() - start).to_string())], &f2[start..])
        });
        let url = format!("{base}/f");
        keep(&url);
        assert!(Http::new(None).download(&url, &dest, &AtomicBool::new(false), &mut |_| {}).unwrap());
        assert_eq!(fs::read(&dest).unwrap(), full);
        assert_eq!(seen.lock().unwrap()[0].headers["range"], "bytes=5000-");

        // a different URL (a newer release): the old part is thrown away
        let f3 = full.clone();
        let (base, seen) = serve(1, move |_, _| reply("200 OK", &[("Content-Length", f3.len().to_string())], &f3));
        keep("http://example.invalid/old");
        assert!(Http::new(None).download(&format!("{base}/f"), &dest, &AtomicBool::new(false), &mut |_| {}).unwrap());
        assert_eq!(fs::read(&dest).unwrap(), full);
        assert!(!seen.lock().unwrap()[0].headers.contains_key("range"));
    }

    /// Asks GitHub twice for the same release, the second time with `If-None-Match`, and
    /// prints the status and the rate limit left, to see whether a 304 costs a request.
    /// Measured 2026-11-12 without a token: it does (58, 57, 56).
    #[test]
    #[ignore]
    fn live_etag_probe() {
        let agent: ureq::Agent = ureq::Agent::config_builder().user_agent("EMUAssistant-test").build().into();
        let url = "https://api.github.com/repos/mgba-emu/mgba/releases/latest";
        let first = agent.get(url).header("Accept", "application/vnd.github+json").call().unwrap();
        let etag = header(&first, "etag");
        println!("first: {} etag={etag} remaining={}", first.status(), header(&first, "x-ratelimit-remaining"));
        let second = agent.get(url).header("Accept", "application/vnd.github+json").header("If-None-Match", &etag).call();
        match second {
            Ok(r) => println!("second: {} remaining={}", r.status(), header(&r, "x-ratelimit-remaining")),
            Err(e) => println!("second: error {e}"),
        }
    }
}

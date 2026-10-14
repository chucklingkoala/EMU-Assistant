//! Small HTTP client for release lookups and downloads.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use ureq::Agent;

use crate::fsutil::PART_EXT;

pub struct Http {
    agent: Agent,
    github_token: Option<String>,
}

pub struct Head {
    pub size: u64,
    pub last_modified: String,
}

impl Http {
    pub fn new(github_token: Option<String>) -> Self {
        let config = Agent::config_builder()
            .user_agent(format!("EMUAssistant/{}", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .build();
        Http { agent: config.into(), github_token }
    }

    fn get(&self, url: &str) -> Result<ureq::http::Response<ureq::Body>> {
        let mut req = self.agent.get(url);
        if url.starts_with("https://api.github.com/") {
            req = req.header("Accept", "application/vnd.github+json");
            if let Some(t) = &self.github_token {
                req = req.header("Authorization", format!("Bearer {}", t.trim()));
            }
        }
        req.call().map_err(|e| describe(e, url))
    }

    pub fn get_json(&self, url: &str) -> Result<Value> {
        let text = self.get_text(url)?;
        serde_json::from_str(&text).with_context(|| format!("unexpected reply from {url}"))
    }

    pub fn get_text(&self, url: &str) -> Result<String> {
        let mut resp = self.get(url)?;
        resp.body_mut().with_config().limit(32 << 20).read_to_string().with_context(|| format!("reading {url}"))
    }

    pub fn head(&self, url: &str) -> Result<Head> {
        let resp = self.agent.head(url).call().map_err(|e| describe(e, url))?;
        let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        Ok(Head { size: header("content-length").parse().unwrap_or(0), last_modified: header("last-modified") })
    }

    /// Downloads `url` to `dest` through a `.emuasist-part` file. `on_bytes` gets each
    /// chunk's size. Returns `Ok(false)` when cancelled.
    pub fn download(&self, url: &str, dest: &Path, cancel: &AtomicBool, on_bytes: &mut dyn FnMut(u64)) -> Result<bool> {
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p)?;
        }
        let mut part = dest.as_os_str().to_owned();
        part.push(PART_EXT);
        let part = std::path::PathBuf::from(part);
        let resp = self.get(url)?;
        let mut reader = resp.into_body().into_with_config().limit(u64::MAX).reader();
        let mut out = File::create(&part).with_context(|| format!("creating {}", part.display()))?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            if cancel.load(Ordering::Relaxed) {
                drop(out);
                let _ = fs::remove_file(&part);
                return Ok(false);
            }
            let n = reader.read(&mut buf).with_context(|| format!("downloading {url}"))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            on_bytes(n as u64);
        }
        out.sync_all()?;
        drop(out);
        fs::rename(&part, dest)?;
        Ok(true)
    }
}

fn describe(e: ureq::Error, url: &str) -> anyhow::Error {
    match e {
        ureq::Error::StatusCode(403 | 429) if url.contains("api.github.com") => anyhow::anyhow!(
            "GitHub refused the request (rate limit of 60 checks an hour without a token). Add a GitHub token in Settings > Updates, or try again later."
        ),
        ureq::Error::StatusCode(404) => anyhow::anyhow!("not found: {url}"),
        ureq::Error::StatusCode(code) => anyhow::anyhow!("server answered {code} for {url}"),
        other => anyhow::anyhow!("{url}: {other}"),
    }
}

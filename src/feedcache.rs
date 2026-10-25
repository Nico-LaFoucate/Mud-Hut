// SPDX-License-Identifier: Apache-2.0
//! The product feed and build manifests for `check-updates`: fetched at most once per platform
//! per run, cached under `$XDG_CACHE_HOME/mudhut/`, and never a reason for a slow or failed
//! check.
//!
//! - The feed (every build of every Adobe product, ~14 MB of XML, ~0.4 MB gzip on the wire) is
//!   reused while younger than `--max-age`, else fetched with ONE try and short timeouts (the
//!   install path's 8 tries over 3 minutes would make an offline check take minutes). It is kept
//!   gzip-compressed on disk. Offline, the cached copy answers and the result says so.
//! - A build's manifest (package list, sizes, deltas) is immutable per build GUID, so it is
//!   cached forever once fetched.
//! - `MUDHUT_FEED_URL` (with `{platform}`, `file://` allowed) points the check at a test feed.

use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ledger::Ledger;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Use the cache while younger than max-age, else fetch.
    Normal,
    /// Fetch now.
    Refresh,
    /// Never touch the network.
    Offline,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Meta {
    fetched_at: u64,
    url: String,
    bytes: u64,
    #[serde(default)]
    last_attempt_at: u64,
    #[serde(default)]
    last_attempt_ok: bool,
    #[serde(default)]
    last_error: Option<String>,
}

pub struct FeedLoad {
    pub xml: Option<String>,
    /// The technical reason behind `error`, for logs and bug reports.
    pub error_detail: Option<String>,
    /// Adobe answered on this run, or (cache / offline) the last attempt succeeded.
    pub online: bool,
    pub from_cache: bool,
    pub fetched_at: Option<u64>,
    /// Plain sentence when Adobe could not be reached.
    pub error: Option<String>,
}

fn cache_dir() -> Option<PathBuf> {
    Some(crate::download::xdg_dir("XDG_CACHE_HOME", ".cache").ok()?.join("mudhut"))
}

fn feed_url(ledger: &Ledger, platform: &str) -> String {
    std::env::var("MUDHUT_FEED_URL")
        .unwrap_or_else(|_| ledger.endpoints.products_feed.clone())
        .replace("{platform}", platform)
}

fn agent() -> crate::net::Client {
    crate::net::quick_agent(Duration::from_secs(5), Duration::from_secs(60))
}

fn get(url: &str, headers: &std::collections::BTreeMap<String, String>, guid: Option<&str>) -> Result<String, String> {
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read_to_string(path).map_err(|e| e.to_string());
    }
    let mut req = agent().get(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    if let Some(g) = guid {
        req = req.set("x-adobe-build-guid", g);
    }
    // ureq's `gzip` feature asks for gzip and decompresses; read without the 10 MiB cap of
    // `into_string()` (the decompressed feed is larger).
    let resp = req.call().map_err(|e| e.to_string())?;
    let mut body = String::new();
    resp.into_reader().take(256 << 20).read_to_string(&mut body).map_err(|e| e.to_string())?;
    Ok(body)
}

/// A blocking flock on `path` for the duration of the returned file.
fn lock_file(path: &PathBuf) -> Option<std::fs::File> {
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).ok()?;
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    Some(f)
}

fn read_gz(path: &PathBuf) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut s = String::new();
    flate2::read::GzDecoder::new(f).read_to_string(&mut s).ok()?;
    Some(s)
}

fn write_gz(path: &PathBuf, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("part");
    {
        let f = std::fs::File::create(&tmp)?;
        let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
        enc.write_all(text.as_bytes())?;
        enc.finish()?;
    }
    std::fs::rename(&tmp, path)
}

/// Load the feed for `platform` per `mode`.
pub fn feed(ledger: &Ledger, platform: &str, max_age: Duration, mode: Mode) -> FeedLoad {
    let now = crate::clock::now_secs();
    let Some(dir) = cache_dir().map(|d| d.join("feed")) else {
        return FeedLoad { xml: None, error_detail: None, online: false, from_cache: false, fetched_at: None, error: Some("No cache directory (HOME is not set).".into()) };
    };
    let _ = std::fs::create_dir_all(&dir);
    let safe = platform.replace([',', '/'], "_");
    let gz = dir.join(format!("{safe}.xml.gz"));
    let meta_p = dir.join(format!("{safe}.json"));
    // One fetch per platform even when a timer run and a click overlap: the second waits here
    // and then finds a fresh cache.
    let _guard = lock_file(&dir.join(format!("{safe}.lock")));
    let mut meta: Meta = std::fs::read_to_string(&meta_p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let cached = || read_gz(&gz);
    let fresh = meta.fetched_at > 0 && now.saturating_sub(meta.fetched_at) < max_age.as_secs();

    if mode == Mode::Offline || (mode == Mode::Normal && fresh) {
        let xml = cached();
        return FeedLoad {
            error_detail: None,
            online: meta.last_attempt_ok && xml.is_some(),
            from_cache: true,
            fetched_at: xml.as_ref().map(|_| meta.fetched_at),
            error: if xml.is_none() { Some("No copy of Adobe's product list is stored yet.".into()) } else { None },
            xml,
        };
    }
    let url = feed_url(ledger, platform);
    meta.last_attempt_at = now;
    match get(&url, &ledger.honest_headers(), None).and_then(|x| {
        roxmltree::Document::parse(&x).map_err(|e| format!("Adobe's product list was unreadable ({e})"))?;
        Ok(x)
    }) {
        Ok(xml) => {
            let _ = write_gz(&gz, &xml);
            meta = Meta { fetched_at: now, url, bytes: xml.len() as u64, last_attempt_at: now, last_attempt_ok: true, last_error: None };
            let _ = std::fs::write(&meta_p, serde_json::to_vec(&meta).unwrap_or_default());
            FeedLoad { xml: Some(xml), error_detail: None, online: true, from_cache: false, fetched_at: Some(now), error: None }
        }
        Err(e) => {
            meta.last_attempt_ok = false;
            meta.last_error = Some(e.clone());
            let _ = std::fs::write(&meta_p, serde_json::to_vec(&meta).unwrap_or_default());
            let xml = cached();
            FeedLoad {
                online: false,
                from_cache: xml.is_some(),
                fetched_at: xml.as_ref().map(|_| meta.fetched_at),
                error: Some(if xml.is_some() {
                    "Couldn't reach Adobe's servers. Showing what was found last time.".into()
                } else {
                    "Couldn't reach Adobe's servers, so no updates can be shown yet.".into()
                }),
                error_detail: Some(e),
                xml,
            }
        }
    }
}

/// The manifest (`Application.json`) of one build: from the forever cache, else (not offline)
/// one short fetch. None when unavailable; the caller then reports no download size.
pub fn manifest(ledger: &Ledger, guid: &str, offline: bool) -> Option<(crate::feed::Manifest, String)> {
    let dir = cache_dir()?.join("manifests");
    let path = dir.join(format!("{}.json", guid.replace(['/', '.'], "_")));
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(m) = serde_json::from_str(&raw) {
            return Some((m, raw));
        }
    }
    if offline {
        return None;
    }
    let raw = get(&ledger.endpoints.application_manifest, &ledger.honest_headers(), Some(guid)).ok()?;
    let m: crate::feed::Manifest = serde_json::from_str(&raw).ok()?;
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&path, &raw);
    Some((m, raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_round_trip() {
        let p = std::env::temp_dir().join(format!("mudhut-gz-{}.xml.gz", std::process::id()));
        write_gz(&p, "<products/>").unwrap();
        assert_eq!(read_gz(&p).as_deref(), Some("<products/>"));
        let _ = std::fs::remove_file(&p);
    }
}

// SPDX-License-Identifier: Apache-2.0
//! Retries for Adobe's servers. One transient answer must not end a multi-gigabyte install: on
//! 2026-10-07 a release test lost a Photoshop install to a single HTTP 503 on a 411-byte
//! validation file that answered 200 a minute later.

use std::io::ErrorKind;
use std::time::Duration;

use anyhow::Result;

/// Tries per request, waiting 2, 4, 8, then 16 seconds between them.
pub const ATTEMPTS: u32 = 5;

/// Worth another try: server hiccups (408, 429, 5xx gateway/unavailable) and network failures,
/// including a connection that drops mid-download. A 4xx is Adobe's answer, and a local failure
/// (permissions, a full disk) or a checksum mismatch is not the network's.
pub fn transient(e: &anyhow::Error) -> bool {
    for cause in e.chain() {
        if let Some(u) = cause.downcast_ref::<ureq::Error>() {
            return match u {
                ureq::Error::Status(code, _) => matches!(code, 408 | 429 | 500 | 502 | 503 | 504),
                ureq::Error::Transport(_) => true,
            };
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return !matches!(io.kind(), ErrorKind::PermissionDenied | ErrorKind::NotFound | ErrorKind::AlreadyExists)
                && io.raw_os_error() != Some(28); // ENOSPC
        }
    }
    false
}

/// Run `f` until it succeeds, fails for good, or has had ATTEMPTS tries. `note` hears each retry.
pub fn retry<T>(what: &str, mut note: impl FnMut(&str), mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let mut wait = 2;
    let mut attempt = 1;
    loop {
        match f() {
            Err(e) if attempt < ATTEMPTS && transient(&e) => {
                note(&format!("{what}: {e:#}; trying again in {wait} s ({attempt}/{ATTEMPTS})"));
                std::thread::sleep(Duration::from_secs(wait));
                wait *= 2;
                attempt += 1;
            }
            r => return r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    fn status(code: u16) -> anyhow::Error {
        let resp = ureq::Response::new(code, "status", "").unwrap();
        anyhow::Error::new(ureq::Error::Status(code, resp)).context("GET https://example")
    }

    #[test]
    fn server_hiccups_are_transient_answers_are_not() {
        assert!(transient(&status(503)));
        assert!(transient(&status(502)));
        assert!(transient(&status(429)));
        assert!(!transient(&status(404)));
        assert!(!transient(&status(403)));
        assert!(!transient(&anyhow::anyhow!("segment 3 hash mismatch")));
    }

    #[test]
    fn dropped_connection_is_transient_full_disk_is_not() {
        let reset: Result<()> = Err(std::io::Error::from(ErrorKind::ConnectionReset)).context("reading from CDN");
        assert!(transient(&reset.unwrap_err()));
        let full: Result<()> = Err(std::io::Error::from_raw_os_error(28)).context("writing to disk");
        assert!(!transient(&full.unwrap_err()));
    }

    #[test]
    fn retry_stops_on_success_and_on_a_real_answer() {
        let mut n = 0;
        let r = retry("t", |_| {}, || { n += 1; if n < 2 { Err(status(503)) } else { Ok(n) } });
        assert_eq!(r.unwrap(), 2);
        let mut m = 0;
        let r: Result<()> = retry("t", |_| {}, || { m += 1; Err(status(404)) });
        assert!(r.is_err());
        assert_eq!(m, 1);
    }
}

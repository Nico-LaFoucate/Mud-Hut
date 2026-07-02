// SPDX-License-Identifier: Apache-2.0
//! `mudhut auth` — Adobe device / QR sign-in, driven OUTSIDE the embedded browser.
//!
//! The Creative Cloud installer's built-in sign-in is an HTML SPA that doesn't
//! render under Wine (wine-gecko is too old). Adobe's *delegated device auth* is
//! the same flow the installer uses internally, and it's fully drivable over plain
//! HTTP — so we drive it ourselves and let Collider render the QR + link natively
//! (thin-GUI / engine-CLI pattern, same as Collider drives `neutron`).
//!
//! Streams NDJSON in --json mode:
//!   {"event":"auth_prompt","url":..,"qr":..,"request_id":..}   <- show this
//!   {"event":"note","msg":"status: CREATED"} ...               <- while polling
//!   {"event":"result","status":"complete","tokens":{...}}      <- terminal
//! Rust port of tools/adobe_device_auth.py.

use std::io::Read;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::output::Emitter;

const API: &str = "https://delegated.identity.adobe.com/darq/delegation/browser/v1";
const CLIENT_ID: &str = "CreativeCloudInstaller_v1_0";
// redirect_uri, pre-percent-encoded (the only value that needs it).
const REDIRECT_ENC: &str = "https%3A%2F%2Foobe.adobe.com%2F";
const SCOPE: &str = "openid,AdobeID,creative_cloud,creative_sdk";

#[derive(Serialize)]
struct AuthResult {
    status: &'static str,
    request_id: String,
    device_id: String,
    /// Raw exchange response (token shape) — captured verbatim for analysis until
    /// the token->install handoff is finalized.
    exchange: Value,
}

/// 16 random bytes -> 32 uppercase hex, like the installer's device_id.
fn new_device_id() -> Result<String> {
    let mut f = std::fs::File::open("/dev/urandom").context("open /dev/urandom")?;
    let mut b = [0u8; 16];
    f.read_exact(&mut b).context("read /dev/urandom")?;
    Ok(b.iter().map(|x| format!("{x:02X}")).collect())
}

fn http_get(url: &str) -> Result<String> {
    ureq::get(url)
        .set("User-Agent", "Creative Cloud")
        .set("Accept", "application/json")
        .timeout(Duration::from_secs(30))
        .call()
        .with_context(|| format!("GET {url}"))?
        .into_string()
        .context("read body")
}

pub fn cmd_auth(em: &Emitter, timeout_secs: u64) -> Result<()> {
    let device_id = new_device_id()?;

    // 1. Mint the QR + login link.
    let qr_url = format!(
        "{API}/qr?client_id={CLIENT_ID}&redirect_uri={REDIRECT_ENC}\
         &scope={SCOPE},allow_ac_dt_exchange&for=qr&mode=light&device_id={device_id}"
    );
    let body = http_get(&qr_url)?;
    let mint: Value = serde_json::from_str(&body).context("parse qr response")?;
    let url = mint.get("url").and_then(|v| v.as_str()).context("qr response missing url")?;
    let request_id = mint.get("request_id").and_then(|v| v.as_str())
        .context("qr response missing request_id")?.to_string();
    let qr = mint.get("qr").and_then(|v| v.as_str()).unwrap_or("");

    // Tell the frontend to display the link + QR (Collider renders it natively).
    em.event(json!({
        "event": "auth_prompt", "url": url, "qr": qr, "request_id": request_id,
    }));
    if !em.is_json() {
        println!("\nOpen this link in a browser and sign in to Adobe:\n\n    {url}\n");
    }

    // 2. Poll until the user authorizes elsewhere.
    let poll_url = format!("{API}/requests?delegated_request_id={request_id}&client_id={CLIENT_ID}");
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut interval = 5u64;
    let code = loop {
        if Instant::now() >= deadline {
            bail!("timed out after {timeout_secs}s waiting for sign-in");
        }
        let d: Value = serde_json::from_str(&http_get(&poll_url)?).unwrap_or_else(|_| json!({}));
        let status = d.get("status").and_then(|v| v.as_str()).unwrap_or("?");
        interval = d.get("retry_interval").and_then(|v| v.as_u64()).unwrap_or(interval);
        em.note(&format!("status: {status}"));
        match status {
            "COMPLETE" => {
                break d.get("authorization_code").and_then(|v| v.as_str())
                    .context("COMPLETE but no authorization_code")?.to_string();
            }
            "EXPIRED" => bail!("sign-in request expired (took too long)"),
            _ => sleep(Duration::from_secs(interval)),
        }
    };

    // 3. Exchange the authorization_code for tokens. Capture the raw response
    //    (redirect Location / body) — the exact token shape feeds the handoff work.
    let ex_url = format!(
        "{API}/ac_dt_exchange_redirect?client_id={CLIENT_ID}&device_id={device_id}\
         &code={code}&redirect_uri={REDIRECT_ENC}&error_redirect={REDIRECT_ENC}"
    );
    let agent = ureq::builder().redirects(0).build(); // capture the redirect, don't follow
    let (status, location, ex_body) = match agent
        .get(&ex_url)
        .set("User-Agent", "Creative Cloud")
        .timeout(Duration::from_secs(30))
        .call()
    {
        Ok(r) => (r.status(), r.header("location").map(String::from), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => {
            (code, r.header("location").map(String::from), r.into_string().unwrap_or_default())
        }
        Err(e) => bail!("exchange request failed: {e}"),
    };

    let result = AuthResult {
        status: "complete",
        request_id,
        device_id,
        exchange: json!({
            "http_status": status,
            "location": location,
            "body": ex_body.chars().take(4000).collect::<String>(),
        }),
    };
    if em.is_json() {
        em.result(&result);
    } else {
        println!("Signed in. Exchange HTTP {status}; location={:?}", result.exchange.get("location"));
    }
    Ok(())
}

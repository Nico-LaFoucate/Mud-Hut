// SPDX-License-Identifier: Apache-2.0
//! `mudhut auth` — Adobe device / QR sign-in, driven OUTSIDE the embedded browser.
//!
//! The Creative Cloud installer's built-in sign-in is an HTML SPA that doesn't
//! render under Wine (wine-gecko too old). Adobe's *delegated device auth* is the
//! same flow the installer uses internally and is drivable over plain HTTP, so we
//! drive it and let Collider render the QR + link natively.
//!
//! Two one-shot subcommands, matching Collider's launch/poll pattern (frontend
//! calls `begin` once, then polls `poll` every few seconds):
//!   `mudhut --json auth begin`
//!       -> result: { url, qr, request_id, device_id }
//!   `mudhut --json auth poll --request-id <id> --device-id <dev>`
//!       -> result: { status: "pending"|"complete"|"expired", retry_interval,
//!                    exchange? }   (exchange present only when complete)

use std::io::Read;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::output::Emitter;

const API: &str = "https://delegated.identity.adobe.com/darq/delegation/browser/v1";
const CLIENT_ID: &str = "CreativeCloudInstaller_v1_0";
const REDIRECT_ENC: &str = "https%3A%2F%2Foobe.adobe.com%2F"; // pre-encoded
const SCOPE: &str = "openid,AdobeID,creative_cloud,creative_sdk";

#[derive(Serialize)]
struct BeginResult {
    url: String,
    qr: String,
    request_id: String,
    device_id: String,
}

#[derive(Serialize)]
struct PollResult {
    status: &'static str, // "pending" | "complete" | "expired"
    retry_interval: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    exchange: Option<Value>,
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

/// `auth begin` — mint the QR + login link.
pub fn cmd_begin(em: &Emitter) -> Result<()> {
    let device_id = new_device_id()?;
    let url = format!(
        "{API}/qr?client_id={CLIENT_ID}&redirect_uri={REDIRECT_ENC}\
         &scope={SCOPE},allow_ac_dt_exchange&for=qr&mode=light&device_id={device_id}"
    );
    let mint: Value = serde_json::from_str(&http_get(&url)?).context("parse qr response")?;
    let res = BeginResult {
        url: mint.get("url").and_then(|v| v.as_str()).context("qr response missing url")?.to_string(),
        qr: mint.get("qr").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        request_id: mint.get("request_id").and_then(|v| v.as_str())
            .context("qr response missing request_id")?.to_string(),
        device_id,
    };
    if em.is_json() {
        em.result(&res);
    } else {
        println!("Open this link in a browser and sign in to Adobe:\n\n    {}\n", res.url);
        println!("(request_id={} device_id={})", res.request_id, res.device_id);
    }
    Ok(())
}

/// `auth poll` — one poll of the delegated request; runs the token exchange when
/// the user has finished signing in.
pub fn cmd_poll(em: &Emitter, request_id: &str, device_id: &str) -> Result<()> {
    let url = format!("{API}/requests?delegated_request_id={request_id}&client_id={CLIENT_ID}");
    let d: Value = serde_json::from_str(&http_get(&url)?).unwrap_or_else(|_| json!({}));
    let status = d.get("status").and_then(|v| v.as_str()).unwrap_or("?");
    let retry = d.get("retry_interval").and_then(|v| v.as_u64()).unwrap_or(5);

    let res = match status {
        "COMPLETE" => {
            let code = d.get("authorization_code").and_then(|v| v.as_str())
                .context("COMPLETE but no authorization_code")?;
            PollResult { status: "complete", retry_interval: retry, exchange: Some(exchange(device_id, code)?) }
        }
        "EXPIRED" => PollResult { status: "expired", retry_interval: retry, exchange: None },
        _ => PollResult { status: "pending", retry_interval: retry, exchange: None },
    };
    if em.is_json() {
        em.result(&res);
    } else {
        println!("status: {} (retry {}s)", res.status, res.retry_interval);
    }
    Ok(())
}

/// Exchange the authorization_code for tokens. Captures the raw response (the
/// token shape) verbatim — feeds the token->install handoff work.
fn exchange(device_id: &str, code: &str) -> Result<Value> {
    let url = format!(
        "{API}/ac_dt_exchange_redirect?client_id={CLIENT_ID}&device_id={device_id}\
         &code={code}&redirect_uri={REDIRECT_ENC}&error_redirect={REDIRECT_ENC}"
    );
    let agent = ureq::builder().redirects(0).build(); // capture the redirect, don't follow
    let call = agent.get(&url).set("User-Agent", "Creative Cloud").timeout(Duration::from_secs(30)).call();
    let (http_status, location, body) = match call {
        Ok(r) => (r.status(), r.header("location").map(String::from), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(c, r)) => (c, r.header("location").map(String::from), r.into_string().unwrap_or_default()),
        Err(e) => bail!("exchange request failed: {e}"),
    };
    Ok(json!({
        "http_status": http_status,
        "location": location,
        "body": body.chars().take(4000).collect::<String>(),
    }))
}

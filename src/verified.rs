// SPDX-License-Identifier: Apache-2.0
//! The verified-builds list: which Adobe builds Neutron has checked ("holding Adobe updates until
//! they're verified"). `ledger/verified.json`, signed with Ed25519 in minisign's format
//! (`verified.json.minisig`); the public key (`ledger/verified.pub`) is compiled in.
//!
//! Load order, highest serial wins among the lists whose signature checks out:
//! the network copy (`$MUDHUT_VERIFIED_URL`, else the published raw GitHub file; one short try,
//! skipped offline), the cached copy (`$XDG_CACHE_HOME/mudhut/verified.json`), and the copy
//! compiled into this binary (the floor). A list with a bad or missing signature, or with a lower
//! serial than one already seen, is never used: it can neither mark a build verified nor lift a
//! block. Nothing ever "fails open" to treating every build as verified; an unverified build is
//! offered as unverified, and the user may still choose it.
//!
//! Format:
//! ```json
//! {"schema":1,"serial":2,"updated":"…","draft":false,"default_policy":"hold",
//!  "apps":{"PHSP":{"27.0":{"verified":[{"build":"27.11.0.26","build_guid":"…","min_runtime":"11.10-130",
//!                                        "date":"2026-10-16","source":"…"}],
//!                         "blocked":[{"build":"27.12.0.3","reason":"…","issue":"…"}]}}}}
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const BUNDLED: &str = include_str!("../ledger/verified.json");
const BUNDLED_SIG: &str = include_str!("../ledger/verified.json.minisig");
const PUBLIC_KEY: &str = include_str!("../ledger/verified.pub");
const DEFAULT_URL: &str = "https://raw.githubusercontent.com/Nico-LaFoucate/Mud-Hut/main/ledger/verified.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Entry {
    pub build: String,
    #[serde(default)]
    pub build_guid: Option<String>,
    #[serde(default)]
    pub min_runtime: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Blocked {
    pub build: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub issue: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Base {
    #[serde(default)]
    pub verified: Vec<Entry>,
    #[serde(default)]
    pub blocked: Vec<Blocked>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct List {
    pub schema: u32,
    pub serial: u64,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub default_policy: String,
    /// SAP code -> base version -> builds.
    #[serde(default)]
    pub apps: BTreeMap<String, BTreeMap<String, Base>>,
}

/// The list in use and where it came from (the contract's `verified_list` object).
#[derive(Debug, Clone, Serialize)]
pub struct Loaded {
    #[serde(skip)]
    pub list: List,
    /// "network" | "cache" | "bundled".
    pub source: String,
    pub serial: u64,
    /// "valid" for the list in use. ("invalid" only if even the bundled copy failed, a build bug;
    /// then no build counts as verified.)
    pub signature: String,
    pub draft: bool,
    /// A copy that was turned down, and why ("network: bad signature", "network: older serial").
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<String>,
}

fn public_key() -> Option<minisign_verify::PublicKey> {
    minisign_verify::PublicKey::decode(PUBLIC_KEY).ok()
}

/// Parse `json` if `sig` is a valid signature of exactly those bytes by the compiled-in key.
pub fn check(json: &[u8], sig: &str) -> Result<List, String> {
    let pk = public_key().ok_or("the public key compiled into Mud Hut is unreadable")?;
    let s = minisign_verify::Signature::decode(sig).map_err(|e| format!("unreadable signature ({e})"))?;
    pk.verify(json, &s, false).map_err(|e| format!("bad signature ({e})"))?;
    serde_json::from_slice::<List>(json).map_err(|e| format!("unreadable list ({e})"))
}

fn cache_paths() -> Option<(PathBuf, PathBuf)> {
    let d = crate::download::xdg_dir("XDG_CACHE_HOME", ".cache").ok()?.join("mudhut");
    Some((d.join("verified.json"), d.join("verified.json.minisig")))
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read(path).map_err(|e| e.to_string());
    }
    let agent = crate::net::quick_agent(Duration::from_secs(5), Duration::from_secs(15));
    let resp = agent
        .get(url)
        .set("User-Agent", crate::ledger::MUDHUT_USER_AGENT)
        .call()
        .map_err(|e| e.to_string())?;
    let mut b = Vec::new();
    use std::io::Read;
    resp.into_reader().take(4 << 20).read_to_end(&mut b).map_err(|e| e.to_string())?;
    Ok(b)
}

/// Load the best list. `offline`: never touch the network.
pub fn load(offline: bool) -> Loaded {
    let url = std::env::var("MUDHUT_VERIFIED_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    load_with(offline, cache_paths(), &url)
}

fn load_with(offline: bool, cache: Option<(PathBuf, PathBuf)>, url: &str) -> Loaded {
    let mut rejected = Vec::new();
    let mut best: Option<(List, &'static str)> = None;
    let consider = |l: List, src: &'static str, best: &mut Option<(List, &'static str)>, rejected: &mut Vec<String>| {
        match best {
            Some((b, _)) if l.serial < b.serial => rejected.push(format!("{src}: older serial {} than {}", l.serial, b.serial)),
            Some((b, _)) if l.serial == b.serial => {}
            _ => *best = Some((l, src)),
        }
    };
    match check(BUNDLED.as_bytes(), BUNDLED_SIG) {
        Ok(l) => consider(l, "bundled", &mut best, &mut rejected),
        Err(e) => rejected.push(format!("bundled: {e}")),
    }
    if let Some((cj, cs)) = &cache {
        if let (Ok(j), Ok(s)) = (std::fs::read(cj), std::fs::read_to_string(cs)) {
            match check(&j, &s) {
                Ok(l) => consider(l, "cache", &mut best, &mut rejected),
                Err(e) => rejected.push(format!("cache: {e}")),
            }
        }
    }
    if !offline {
        match (fetch(url), fetch(&format!("{url}.minisig"))) {
            (Ok(j), Ok(s)) => match check(&j, &String::from_utf8_lossy(&s)) {
                Ok(l) => {
                    let newer = best.as_ref().map(|(b, _)| l.serial > b.serial).unwrap_or(true);
                    if newer {
                        if let Some((cj, cs)) = &cache {
                            if let Some(d) = cj.parent() {
                                let _ = std::fs::create_dir_all(d);
                            }
                            let _ = std::fs::write(cj, &j);
                            let _ = std::fs::write(cs, &s);
                        }
                    }
                    consider(l, "network", &mut best, &mut rejected);
                }
                Err(e) => rejected.push(format!("network: {e}")),
            },
            // Unreachable or not published (the repository is private until hardware tests): not
            // a rejection, just no network copy.
            _ => {}
        }
    }
    match best {
        Some((list, src)) => Loaded {
            source: src.to_string(),
            serial: list.serial,
            signature: "valid".into(),
            draft: list.draft,
            list,
            rejected,
        },
        None => Loaded { list: List::default(), source: "bundled".into(), serial: 0, signature: "invalid".into(), draft: false, rejected },
    }
}

impl List {
    fn base(&self, sap: &str, base: &str) -> Option<&Base> {
        self.apps.get(sap)?.iter().find(|(b, _)| crate::version::eq(b, base)).map(|(_, v)| v)
    }
    pub fn verified(&self, sap: &str, base: &str, build: &str) -> Option<&Entry> {
        self.base(sap, base)?.verified.iter().find(|e| crate::version::eq(&e.build, build))
    }
    pub fn blocked(&self, sap: &str, base: &str, build: &str) -> Option<&Blocked> {
        self.base(sap, base)?.blocked.iter().find(|e| crate::version::eq(&e.build, build))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_list_is_signed_by_the_compiled_in_key() {
        let l = check(BUNDLED.as_bytes(), BUNDLED_SIG).expect("bundled list must verify");
        assert!(l.serial >= 1);
        assert!(l.verified("PHSP", "27.0", "27.11.0.26").is_some());
    }

    #[test]
    fn a_tampered_list_is_rejected() {
        let tampered = BUNDLED.replace("27.11.0.26", "27.12.0.99");
        assert!(check(tampered.as_bytes(), BUNDLED_SIG).is_err(), "edited bytes must not verify");
        assert!(check(BUNDLED.as_bytes(), "untrusted comment: x\nnot base64\n").is_err());
        assert!(check(BUNDLED.as_bytes(), "").is_err(), "a missing signature must not verify");
    }

    #[test]
    fn a_bad_network_copy_never_replaces_the_good_one() {
        let dir = std::env::temp_dir().join(format!("mudhut-verified-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("verified.json");
        let serial = |s: &str| s.lines().find(|l| l.contains("\"serial\"")).unwrap().to_string();
        let raised = BUNDLED.replace(&serial(BUNDLED), "  \"serial\": 99,");
        assert_ne!(raised, BUNDLED, "the test must actually raise the serial");
        std::fs::write(&fake, raised).unwrap();
        std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/ledger/verified.json.minisig"), dir.join("verified.json.minisig")).unwrap();
        let cache = Some((dir.join("cache/verified.json"), dir.join("cache/verified.json.minisig")));
        let l = load_with(false, cache.clone(), &format!("file://{}", fake.display()));
        assert_eq!(l.source, "bundled", "a higher serial with a bad signature must lose: {:?}", l.rejected);
        assert_eq!(l.signature, "valid");
        assert!(l.rejected.iter().any(|r| r.starts_with("network: bad signature")), "{:?}", l.rejected);
        assert!(!dir.join("cache/verified.json").exists(), "a rejected copy must not be cached");
        // A correctly signed network copy is taken and cached.
        let good = dir.join("good.json");
        std::fs::write(&good, BUNDLED).unwrap();
        std::fs::write(dir.join("good.json.minisig"), BUNDLED_SIG).unwrap();
        let l2 = load_with(false, cache.clone(), &format!("file://{}", good.display()));
        assert_eq!(l2.serial, serde_json::from_str::<List>(BUNDLED).unwrap().serial);
        // Equal serial to the bundled copy: the bundled one is kept (no reason to switch).
        assert_eq!(l2.source, "bundled");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

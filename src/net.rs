// SPDX-License-Identifier: Apache-2.0
//! Retries for Adobe's servers. One transient answer must not end a multi-gigabyte install: on
//! 2026-10-07 a release test lost a Photoshop install to a single HTTP 503 on a 411-byte
//! validation file that answered 200 a minute later. The validation service sits behind
//! CloudFront; a file not yet in its cache goes to Adobe's origin, which can answer 503 for more
//! than 30 s (the second run outlasted 5 tries over 30 s). Once cached, it answers at once.

use std::io::ErrorKind;
use std::time::Duration;

use anyhow::Result;

/// Every Mud Hut request goes through a `Client`. It does two things a bare ureq agent doesn't:
///
/// - It uses the proxy the environment names, read the way curl reads it (`proxy_var`,
///   `no_proxy`). ureq 2 reads no proxy variable unless told to, so before 0.3.6 nothing
///   could be installed behind a school or studio proxy. Only HTTP proxies: ureq is built
///   without SOCKS, and a SOCKS or TLS proxy address is reported (`notes`) and not used.
/// - It tries IPv4 addresses before IPv6. On a network whose IPv6 route is broken (common, and the
///   case on the build machine), trying the AAAA addresses first cost ~4 s per request before the
///   working IPv4 one was reached; an install makes a few hundred requests.
#[derive(Clone)]
pub struct Client {
    direct: ureq::Agent,
    /// (scheme, agent through that scheme's proxy).
    proxied: Vec<(&'static str, ureq::Agent)>,
    no_proxy: Vec<String>,
    /// Which proxy is used, or why one named is not: said once in an install's log.
    pub notes: Vec<String>,
}

impl Client {
    /// `timeouts` sets the agent's timeouts on a builder that already has the rest.
    pub fn new(timeouts: impl Fn(ureq::AgentBuilder) -> ureq::AgentBuilder) -> Client {
        Self::with_env(timeouts, &|k| std::env::var(k).ok())
    }

    fn with_env(timeouts: impl Fn(ureq::AgentBuilder) -> ureq::AgentBuilder, env: &dyn Fn(&str) -> Option<String>) -> Client {
        let base = || timeouts(ureq::builder().resolver(ipv4_first).try_proxy_from_env(false));
        let mut proxied = Vec::new();
        let mut notes = Vec::new();
        for scheme in ["https", "http"] {
            let Some((var, value)) = proxy_var(scheme, env) else { continue };
            match http_proxy(&value) {
                Ok(p) => {
                    notes.push(format!("{scheme} requests go through the proxy {} (from {var})", shown(&value)));
                    proxied.push((scheme, base().proxy(p).build()));
                }
                Err(why) => notes.push(format!("not using {var}={}: {why}; connecting directly", shown(&value))),
            }
        }
        Client { direct: base().build(), proxied, no_proxy: no_proxy(env), notes }
    }

    /// A client that ignores the environment's proxy (tests against a local server).
    #[cfg(test)]
    pub fn direct() -> Client {
        Self::with_env(|b| b, &|_| None)
    }

    pub fn get(&self, url: &str) -> ureq::Request {
        match self.proxy_for(url) {
            Some(i) => self.proxied[i].1.get(url),
            None => self.direct.get(url),
        }
    }

    /// Which entry of `proxied` serves `url` (None: connect directly).
    fn proxy_for(&self, url: &str) -> Option<usize> {
        let (scheme, host) = scheme_host(url);
        if bypassed(&self.no_proxy, &host) {
            return None;
        }
        self.proxied.iter().position(|(s, _)| *s == scheme)
    }
}

/// A client for the quick, single-try requests (`check-updates`, the lists): short timeouts.
pub fn quick_agent(connect: Duration, total: Duration) -> Client {
    Client::new(|b| b.timeout_connect(connect).timeout(total))
}

/// A client for package downloads: no overall limit, but a stalled connection ends.
pub fn download_client() -> Client {
    Client::new(|b| b.timeout_connect(Duration::from_secs(30)).timeout_read(Duration::from_secs(120)))
}

fn ipv4_first(addr: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs;
    let mut v: Vec<std::net::SocketAddr> = addr.to_socket_addrs()?.collect();
    v.sort_by_key(|a| a.is_ipv6());
    Ok(v)
}

/// The variable naming the proxy for `scheme`, in curl's order: for https, `https_proxy`,
/// `HTTPS_PROXY`, then `all_proxy`, `ALL_PROXY`; for http, `http_proxy` (never `HTTP_PROXY`,
/// which curl ignores because a web server's CGI sets it from a request header), then the
/// `all_proxy` pair. An empty value counts as unset.
fn proxy_var(scheme: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<(&'static str, String)> {
    let names: &[&'static str] = match scheme {
        "https" => &["https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"],
        _ => &["http_proxy", "all_proxy", "ALL_PROXY"],
    };
    names.iter().find_map(|n| env(n).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).map(|v| (*n, v)))
}

/// An HTTP proxy address (`http://[user:password@]host[:port]`, or without the scheme).
fn http_proxy(value: &str) -> Result<ureq::Proxy, String> {
    match value.split_once("://") {
        Some((scheme, _)) if !scheme.eq_ignore_ascii_case("http") => {
            return Err(format!("Mud Hut can use HTTP proxies only, not {scheme}://"));
        }
        _ => {}
    }
    ureq::Proxy::new(value).map_err(|e| format!("unreadable proxy address ({e})"))
}

/// A proxy address for the log: scheme, host and port, never the user name or password.
fn shown(value: &str) -> String {
    let (scheme, rest) = match value.split_once("://") {
        Some((s, r)) => (format!("{s}://"), r),
        None => (String::new(), value),
    };
    let host = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
    format!("{scheme}{}", host.trim_end_matches('/'))
}

/// `no_proxy` (then `NO_PROXY`): hosts reached directly. Each entry matches that host and its
/// subdomains (a leading `.` or `*.` is dropped), `*` matches every host, and a `:port` is ignored.
fn no_proxy(env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let v = env("no_proxy").filter(|v| !v.trim().is_empty()).or_else(|| env("NO_PROXY")).unwrap_or_default();
    v.split(|c: char| c == ',' || c.is_whitespace())
        .map(|e| {
            let e = e.trim().to_ascii_lowercase();
            let e = e.trim_start_matches("*.").trim_start_matches('.');
            // "host:port" -> "host"; an IPv6 address (several colons) stays whole.
            let e = match e.rsplit_once(':') {
                Some((h, p)) if !h.contains(':') && !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => h,
                _ => e,
            };
            e.trim_start_matches('[').trim_end_matches(']').to_string()
        })
        .filter(|e| !e.is_empty())
        .collect()
}

fn bypassed(list: &[String], host: &str) -> bool {
    list.iter().any(|e| e == "*" || host == e || host.strip_suffix(e.as_str()).is_some_and(|h| h.ends_with('.')))
}

/// (scheme, host) of a URL, both lowercase; the host without user info, port or IPv6 brackets.
fn scheme_host(url: &str) -> (String, String) {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    let auth = auth.rsplit_once('@').map(|(_, h)| h).unwrap_or(auth);
    let host = if let Some(v6) = auth.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        auth.split(':').next().unwrap_or("")
    };
    (scheme.to_ascii_lowercase(), host.to_ascii_lowercase())
}

/// Tries per request: waits of 2, 4, 8, 16, 32, 60 and 60 seconds, about 3 minutes in all.
pub const ATTEMPTS: u32 = 8;
const MAX_WAIT: u64 = 60;

/// Worth another try: server hiccups (408, 429, 5xx gateway/unavailable) and network failures,
/// including a connection that drops mid-download. A 4xx is Adobe's answer, a proxy that refuses
/// the user's credentials will refuse them again, and a local failure (permissions, a full disk)
/// or a checksum mismatch is not the network's.
pub fn transient(e: &anyhow::Error) -> bool {
    for cause in e.chain() {
        if let Some(u) = cause.downcast_ref::<ureq::Error>() {
            return match u {
                ureq::Error::Status(code, _) => matches!(code, 408 | 429 | 500 | 502 | 503 | 504),
                ureq::Error::Transport(t) => !matches!(t.kind(), ureq::ErrorKind::ProxyUnauthorized | ureq::ErrorKind::InvalidProxyUrl),
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
                wait = (wait * 2).min(MAX_WAIT);
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

    fn env(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    fn client(vars: &'static [(&'static str, &'static str)]) -> Client {
        Client::with_env(|b| b.timeout(Duration::from_secs(5)), &env(vars))
    }

    #[test]
    fn the_proxy_is_read_the_way_curl_reads_it() {
        let e = env(&[("HTTPS_PROXY", "http://a:3128"), ("ALL_PROXY", "http://b:1"), ("HTTP_PROXY", "http://evil:80")]);
        assert_eq!(proxy_var("https", &e), Some(("HTTPS_PROXY", "http://a:3128".into())));
        // HTTP_PROXY is never read (curl: a CGI sets it from a request header); all_proxy is.
        assert_eq!(proxy_var("http", &e), Some(("ALL_PROXY", "http://b:1".into())));
        let lower = env(&[("https_proxy", "p:8080"), ("HTTPS_PROXY", "q:8080")]);
        assert_eq!(proxy_var("https", &lower).unwrap().0, "https_proxy");
        assert_eq!(proxy_var("https", &env(&[("https_proxy", " "), ("ALL_PROXY", "r")])).unwrap().0, "ALL_PROXY");
        assert_eq!(proxy_var("https", &env(&[])), None);
    }

    #[test]
    fn only_http_proxies_are_used_and_the_log_never_shows_a_password() {
        let c = client(&[("HTTPS_PROXY", "http://user:secret@proxy.school.example:3128/")]);
        assert_eq!(c.notes, ["https requests go through the proxy http://proxy.school.example:3128 (from HTTPS_PROXY)"]);
        assert_eq!(c.proxy_for("https://ccmdls.adobe.com/x"), Some(0));
        assert_eq!(c.proxy_for("http://127.0.0.1:9/x"), None, "an https proxy is not used for http");
        let s = client(&[("ALL_PROXY", "socks5://u:pw@localhost:1080")]);
        assert!(s.proxied.is_empty());
        assert_eq!(s.notes[0], "not using ALL_PROXY=socks5://localhost:1080: Mud Hut can use HTTP proxies only, not socks5://; connecting directly");
        assert!(s.notes.iter().all(|n| !n.contains("pw")));
        assert_eq!(s.proxy_for("https://ccmdls.adobe.com/x"), None);
        assert!(client(&[]).notes.is_empty());
    }

    #[test]
    fn no_proxy_matches_hosts_and_their_subdomains() {
        let c = client(&[("https_proxy", "proxy:3128"), ("NO_PROXY", "localhost, .internal.example,adobe.com:443 ,[::1]")]);
        assert_eq!(c.proxy_for("https://ccmdls.adobe.com/a"), None);
        assert_eq!(c.proxy_for("https://adobe.com/a"), None);
        assert_eq!(c.proxy_for("https://notadobe.com/a"), Some(0), "a suffix that is not a subdomain");
        assert_eq!(c.proxy_for("https://raw.githubusercontent.com/a"), Some(0));
        assert_eq!(c.proxy_for("https://x.internal.example/a"), None);
        assert_eq!(c.proxy_for("https://[::1]:8443/a"), None);
        assert_eq!(c.proxy_for("https://user@LOCALHOST:8443/a"), None);
        let all = client(&[("https_proxy", "proxy:3128"), ("no_proxy", "*")]);
        assert_eq!(all.proxy_for("https://ccmdls.adobe.com/a"), None);
    }

    /// The wiring, not just the choice: a request really goes to the proxy, with its credentials.
    /// The fake proxy answers the CONNECT with 407, so nothing leaves this machine, and a refused
    /// password is not retried (it cost 3 minutes of tries in the lab).
    #[test]
    fn a_request_goes_to_the_proxy() {
        use std::io::{BufRead, BufReader, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        l.set_nonblocking(true).unwrap();
        let seen = std::thread::spawn(move || {
            // Up to 10 s: a client that skips the proxy must fail the test, not hang it.
            let s = (0..100).find_map(|_| match l.accept() {
                Ok((s, _)) => Some(s),
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            });
            let Some(s) = s else { return vec!["(the proxy was never contacted)".to_string()] };
            s.set_nonblocking(false).unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut head = Vec::new();
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                head.push(line.trim_end().to_string());
            }
            (&s).write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n").unwrap();
            head
        });
        let proxy: &'static str = Box::leak(format!("http://u:p@127.0.0.1:{port}").into_boxed_str());
        let vars: &'static [(&str, &str)] = Box::leak(vec![("https_proxy", proxy)].into_boxed_slice());
        let r = client(vars).get("https://ccmdls.adobe.example/pkg.zip").call();
        let e = anyhow::Error::new(r.expect_err("the fake proxy refuses"));
        assert!(!transient(&e), "{e:#}");
        let head = seen.join().unwrap();
        assert_eq!(head[0], "CONNECT ccmdls.adobe.example:443 HTTP/1.1");
        assert!(head.iter().any(|h| h.eq_ignore_ascii_case("Proxy-Authorization: basic dTpw")), "{head:?}");
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

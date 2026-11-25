//! URL validation for everything the app fetches on the outside: station
//! stream URLs (GStreamer playbin and the ICY metadata connection) and
//! station icon URLs (tray and notification icon fetches).
//!
//! A malicious or compromised station entry may point at any URL. Without
//! a guard the app would happily fetch `file://` paths and local or
//! private network addresses (SSRF): stream URLs go to GStreamer and to
//! reqwest, both of which follow arbitrary destinations — and reqwest
//! follows redirects by default, so a public host that 302s to
//! `http://127.0.0.1/...` reaches the local machine as well.
//!
//! `validate` is the single place that decides which URLs the app may
//! touch, and `safe_redirect_policy` re-checks every redirect hop of the
//! reqwest clients against the same rules (plus a hop budget).
//!
//! Residual: GStreamer's own redirect handling inside `playbin` is not
//! intercepted; the initial-target check still covers the direct cases.

use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};

use url::{Host, Url};

/// The maximum number of redirect hops the app follows for station and
/// icon URLs.
pub const MAX_REDIRECTS: usize = 3;

/// Parse and validate a URL the app is allowed to fetch: it must be
/// plain `http(s)` and point at a public address.
///
/// DNS names are resolved here; if *any* of their addresses is local or
/// private the URL is refused. A name that does not resolve at all is
/// refused as well (fail closed: the stream could not be played anyway).
pub fn validate(raw: &str) -> anyhow::Result<Url> {
    let url = Url::parse(raw).map_err(|e| anyhow::anyhow!("not a valid URL: {e}"))?;
    check_url(&url)?;
    Ok(url)
}

/// The scheme + public-address check shared by `validate` and the
/// redirect policy.
fn check_url(url: &Url) -> anyhow::Result<()> {
    match url.scheme() {
        "http" | "https" => {}
        other => anyhow::bail!("URL scheme '{other}' is not allowed (only http/https)"),
    }
    match url.host() {
        Some(Host::Ipv4(ip)) => ensure_public(IpAddr::V4(ip))?,
        Some(Host::Ipv6(ip)) => ensure_public(IpAddr::V6(ip))?,
        // A hostname: check *every* address it resolves to.
        Some(Host::Domain(host)) => {
            let port = url.port_or_known_default().unwrap_or(80);
            let host: &str = host;
            let mut resolved = false;
            for addr in (host, port)
                .to_socket_addrs()
                .map_err(|e| anyhow::anyhow!("could not resolve {host}: {e}"))?
            {
                resolved = true;
                ensure_public(addr.ip())?;
            }
            if !resolved {
                anyhow::bail!("could not resolve {host}");
            }
        }
        None => anyhow::bail!("URL has no host"),
    }
    Ok(())
}

fn ensure_public(ip: IpAddr) -> anyhow::Result<()> {
    if is_public_ip(ip) {
        Ok(())
    } else {
        anyhow::bail!("URL points at a local or private address ({ip}); refusing")
    }
}

/// Whether an address is public: not loopback, not private (RFC 1918),
/// not CGNAT (RFC 6598), not link-local, not ULA, not
/// unspecified/multicast/broadcast, and not an IPv4 in disguise (IPv4-
/// mapped or the deprecated IPv4-compatible form) of any of those.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_unspecified()
                && !v4.is_private()
                && !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_broadcast()
                && !v4.is_multicast()
                && !is_cgnat(v4)
        }
        IpAddr::V6(v6) => {
            // ::ffff:a.b.c.d is an IPv4 address in disguise.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            // ::/96: the mapped form was checked above; the rest (incl.
            // the deprecated IPv4-compatible form ::a.b.c.d, RFC 4291)
            // is reserved space where no public service lives.
            if v6.segments().iter().take(6).all(|&s| s == 0) {
                return false;
            }
            !v6.is_unspecified()
                && !v6.is_loopback()
                && !v6.is_multicast()
                // fc00::/7 (unique local) and fe80::/10 (link-local).
                && v6.segments()[0] & 0xfe00 != 0xfc00
                && v6.segments()[0] & 0xffc0 != 0xfe80
        }
    }
}

/// 100.64.0.0/10 (RFC 6598, carrier-grade NAT): not RFC 1918, but
/// effectively "not the public internet" as far as SSRF is concerned.
fn is_cgnat(v4: Ipv4Addr) -> bool {
    let octets = v4.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

/// Whether a redirect hop may be followed: within the hop budget and the
/// target passes the same check as the initial URL. `hops_so_far` is the
/// number of URLs already requested in the chain (reqwest's `previous`).
pub fn redirect_allowed(next: &Url, hops_so_far: usize) -> anyhow::Result<()> {
    if hops_so_far > MAX_REDIRECTS {
        anyhow::bail!("too many redirects");
    }
    check_url(next)
}

/// The redirect policy for the reqwest clients that follow station and
/// icon URLs (see `redirect_allowed`).
pub fn safe_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let next = attempt.url().clone();
        let hops = attempt.previous().len();
        if let Err(e) = redirect_allowed(&next, hops) {
            tracing::warn!("refusing redirect to {next}: {e:#}");
            return attempt.stop();
        }
        attempt.follow()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str) {
        assert!(validate(raw).is_ok(), "{raw} should be allowed");
    }
    fn bad(raw: &str) {
        assert!(validate(raw).is_err(), "{raw} should be refused");
    }

    #[test]
    fn refuses_bad_schemes() {
        bad("file:///etc/passwd");
        bad("rtsp://streams.example.com/stream");
        bad("ftp://8.8.8.8/file.mp3");
        bad("not a url");
        bad("http:///no-host");
    }

    #[test]
    fn refuses_local_and_private_ips() {
        bad("http://127.0.0.1:8080/admin");
        bad("http://localhost:3000/"); // resolves to 127.0.0.1
        bad("http://10.0.0.5/");
        bad("http://192.168.1.10/");
        bad("http://172.16.5.5/");
        bad("http://100.64.0.1/"); // CGNAT
        bad("http://169.254.169.254/latest/meta-data/");
        bad("http://0.0.0.0/");
        bad("http://[::1]/");
        bad("http://[::2]/"); // reserved ::/96, not loopback
        bad("http://[fc00::1]/");
        bad("http://[fe80::1]/");
        bad("http://[::ffff:127.0.0.1]/"); // IPv4-mapped loopback
        bad("http://[::7f00:1]/"); // deprecated IPv4-compatible loopback
    }

    #[test]
    fn allows_public_ips() {
        ok("http://93.184.216.34/stream.mp3");
        ok("https://[2606:2800:220:1:248:1893:25c8:1946]/live");
    }

    #[test]
    fn is_public_ip_ranges() {
        assert!(!is_public_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("10.1.2.3".parse().unwrap()));
        assert!(!is_public_ip("172.31.255.255".parse().unwrap()));
        assert!(!is_public_ip("192.168.1.1".parse().unwrap()));
        assert!(!is_public_ip("100.127.255.255".parse().unwrap())); // CGNAT edge
        assert!(!is_public_ip("169.254.10.10".parse().unwrap()));
        assert!(!is_public_ip("255.255.255.255".parse().unwrap()));
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        assert!(is_public_ip("172.32.0.1".parse().unwrap())); // just outside RFC 1918
        assert!(is_public_ip("100.128.0.1".parse().unwrap())); // just outside CGNAT
    }

    #[test]
    fn redirect_guard_blocks_local_hops() {
        let local = Url::parse("http://127.0.0.1:8080/admin").unwrap();
        assert!(redirect_allowed(&local, 1).is_err());
        let imds = Url::parse("http://169.254.169.254/latest/meta-data/").unwrap();
        assert!(redirect_allowed(&imds, 1).is_err());
        let ftp = Url::parse("ftp://example.com/x").unwrap();
        assert!(redirect_allowed(&ftp, 1).is_err());
        // A public hop is fine...
        let pub_url = Url::parse("http://93.184.216.34/x").unwrap();
        assert!(redirect_allowed(&pub_url, 1).is_ok());
        // ...until the hop budget is spent.
        assert!(redirect_allowed(&pub_url, MAX_REDIRECTS + 1).is_err());
    }
}

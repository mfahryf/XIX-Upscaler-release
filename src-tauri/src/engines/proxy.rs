//! Shared proxy rotation for all network engines.
//!
//! Each engine holds one `ProxyRotator` (per batch run): the live proxy is
//! reused until it fails, dead proxies stay blacklisted for the rest of the
//! run, and `next_live` round-robins through the remaining candidates. The
//! health check is supplied by the caller: v1 health-checks the cheap
//! paywall endpoint serially via `next_live`, while v2 probes every
//! candidate concurrently via `next_live_parallel` (a cheap reachability
//! GET) so a dead proxy list is validated in one probe window instead of
//! stalling the batch for minutes.

use crate::engines::EngineError;
use futures_util::stream::{FuturesUnordered, StreamExt};
use parking_lot::Mutex as ParkingMutex;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Mutex;

#[derive(Default)]
struct Inner {
    idx: usize,
    live: Option<String>,
    blacklist: HashSet<String>,
}

pub struct ProxyRotator {
    inner: Mutex<Inner>,
}

/// Sticky Tor SOCKS identity shared across files in one engine instance.
///
/// Reusing one isolation credential consumes that exit IP's available quota
/// before rotating. This avoids forcing Tor to build a new circuit per file,
/// which exhausts viable circuits during large batches.
#[derive(Default)]
pub struct TorSession {
    current: ParkingMutex<Option<(String, String)>>,
}

impl TorSession {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the current isolated proxy, minting one when absent or when the
    /// configured Tor endpoint changed.
    pub fn proxy(&self, base: &str) -> String {
        let mut current = self.current.lock();
        if let Some((current_base, proxy)) = current.as_ref() {
            if current_base == base {
                return proxy.clone();
            }
        }
        let proxy = ProxyRotator::with_random_isolation(base);
        *current = Some((base.to_string(), proxy.clone()));
        proxy
    }

    /// Drop this identity after quota/transport failure. The next `proxy()`
    /// call mints a new credential and therefore a new Tor circuit.
    pub fn invalidate(&self, failed_proxy: &str) {
        let mut current = self.current.lock();
        if current
            .as_ref()
            .is_some_and(|(_, proxy)| proxy == failed_proxy)
        {
            *current = None;
        }
    }
}

impl Default for ProxyRotator {
    fn default() -> Self {
        ProxyRotator {
            inner: Mutex::new(Inner::default()),
        }
    }
}

impl ProxyRotator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Normalize a user proxy line: add `http://` when no scheme is present.
    pub fn normalize(raw: &str) -> Option<String> {
        let s = raw.trim();
        if s.is_empty() {
            return None;
        }
        if s.contains("://") {
            Some(s.to_string())
        } else {
            Some(format!("http://{s}"))
        }
    }

    /// Turn a base SOCKS endpoint into a stream-isolated Tor proxy URL by
    /// injecting a random `user:pass`. Tor treats distinct SOCKS credentials
    /// as separate circuits (IsolateSOCKSAuth is on by default), so each call
    /// yields a fresh exit IP — the whole point of "Tor mode". Any existing
    /// credentials on `base` are dropped. Returns `base` unchanged if it has
    /// no `://` scheme.
    pub fn with_random_isolation(base: &str) -> String {
        let base = base.trim();
        let Some((scheme, rest)) = base.split_once("://") else {
            return base.to_string();
        };
        // strip any existing user:pass@ (keep only host:port)
        let host = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
        let id = uuid::Uuid::new_v4().simple().to_string();
        format!("{scheme}://{id}:{id}@{host}")
    }

    /// Reuse the live proxy, or round-robin through candidates (skipping
    /// blacklisted) until `health` passes. `None` → direct connection.
    pub async fn next_live<F, Fut>(&self, proxies: &[String], health: F) -> Option<String>
    where
        F: Fn(&str) -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        {
            let rot = self.inner.lock().unwrap();
            if let Some(l) = &rot.live {
                return Some(l.clone());
            }
        }
        let n = proxies.len();
        for _ in 0..n {
            let candidate = {
                let mut rot = self.inner.lock().unwrap();
                let p = proxies[rot.idx % n].clone();
                rot.idx += 1;
                p
            };
            {
                let rot = self.inner.lock().unwrap();
                if rot.blacklist.contains(&candidate) {
                    continue;
                }
            }
            if health(&candidate).await {
                self.inner.lock().unwrap().live = Some(candidate.clone());
                return Some(candidate);
            }
        }
        None
    }

    /// Like [`next_live`], but probes every non-blacklisted candidate
    /// concurrently instead of serially. With a short per-probe timeout this
    /// validates the whole list in ~one probe window rather than
    /// `n × timeout`, so a batch never stalls minutes on dead proxies. Dead
    /// candidates are blacklisted; the first live one (in list order) becomes
    /// `live` and is returned. `None` → no live proxy (caller falls back to
    /// direct).
    pub async fn next_live_parallel<F, Fut>(&self, proxies: &[String], health: F) -> Option<String>
    where
        F: Fn(&str) -> Fut,
        Fut: Future<Output = bool>,
    {
        {
            let rot = self.inner.lock().unwrap();
            if let Some(l) = &rot.live {
                return Some(l.clone());
            }
        }
        let candidates: Vec<String> = {
            let rot = self.inner.lock().unwrap();
            proxies
                .iter()
                .filter(|p| !rot.blacklist.contains(*p))
                .cloned()
                .collect()
        };
        // Probe all candidates at once; keep results paired with list index so
        // the winner is deterministic (lowest index among the live ones).
        let mut probes: FuturesUnordered<_> = candidates
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let fut = health(p);
                async move { (i, p.clone(), fut.await) }
            })
            .collect();
        let mut best: Option<usize> = None;
        while let Some((i, proxy, alive)) = probes.next().await {
            if alive {
                best = Some(best.map_or(i, |b| b.min(i)));
            } else {
                self.inner.lock().unwrap().blacklist.insert(proxy);
            }
        }
        let winner = candidates.get(best?)?.clone();
        self.inner.lock().unwrap().live = Some(winner.clone());
        Some(winner)
    }

    /// Blacklist a proxy and drop it as the live one.
    pub fn mark_dead(&self, proxy: &str) {
        let mut rot = self.inner.lock().unwrap();
        rot.blacklist.insert(proxy.to_string());
        if rot.live.as_deref() == Some(proxy) {
            rot.live = None;
        }
    }

    /// True for proxy-side transport failures worth rotating on. Covers the
    /// generic reqwest wrapper ("error sending request for url") plus the
    /// real causes that land in the source chain: dead proxy / connection
    /// refused / timeout / DNS / TLS handshake / reset / EOF.
    pub fn is_transport_error(e: &EngineError) -> bool {
        let msg = e.to_string().to_lowercase();
        [
            "error sending request",
            "send request",
            "timeout",
            "timed out",
            "connect",
            "connection",
            "refused",
            "reset",
            "broken pipe",
            "eof",
            "proxy",
            "socket",
            "resolve",
            "dns",
            "name resolution",
            "tls",
            "handshake",
            "stream",
            "unreachable",
            "tunnel",
        ]
        .iter()
        .any(|k| msg.contains(k))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_reqwest_transport_error_is_detected() {
        // Pesan persis yang muncul di UI saat proxy mati / koneksi gagal —
        // dulu tidak dikenali sebagai transport error → rotasi tidak jalan.
        let e = EngineError::Network(
            "error sending request for url (https://svg.new/api/image/vectorize)".into(),
        );
        assert!(ProxyRotator::is_transport_error(&e));
    }

    #[test]
    fn non_transport_errors_are_not_detected() {
        for msg in [
            "HTTP 500 Internal Server Error",
            "HTTP 413 Payload Too Large",
            "response bukan SVG",
            "quota exhausted (402)",
        ] {
            assert!(
                !ProxyRotator::is_transport_error(&EngineError::Network(msg.into())),
                "{msg} bukan transport error"
            );
        }
    }

    #[test]
    fn isolation_injects_unique_creds_and_keeps_host() {
        let a = ProxyRotator::with_random_isolation("socks5://127.0.0.1:9050");
        let b = ProxyRotator::with_random_isolation("socks5://127.0.0.1:9050");
        assert!(a.starts_with("socks5://") && a.ends_with("@127.0.0.1:9050"));
        assert_ne!(a, b, "each call mints a distinct circuit credential");
    }

    #[test]
    fn isolation_replaces_existing_creds() {
        let out = ProxyRotator::with_random_isolation("socks5://old:pass@1.2.3.4:9050");
        assert!(out.ends_with("@1.2.3.4:9050"));
        assert!(!out.contains("old:pass"), "stale creds dropped");
    }

    #[test]
    fn isolation_passthrough_without_scheme() {
        assert_eq!(ProxyRotator::with_random_isolation("nonsense"), "nonsense");
    }
}

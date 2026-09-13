//! Free public proxy list sources (HProxy JSON + ProxyScrape plain text).

use serde_json::Value;
use std::time::Duration;

/// Hard cap on merged free-proxy candidates per batch.
pub const FREE_PROXY_CAP: usize = 1000;

/// One normalized proxy endpoint, `scheme://ip:port`.
#[derive(Debug)]
pub struct Candidate {
    pub url: String,
}

/// HProxy JSON rows: keep only `status == "alive"`, map first known protocol
/// (`socks5|http|https|socks4`) to a URL prefix (`https` → `http`).
pub fn parse_hproxy_json(body: &str) -> Vec<Candidate> {
    let Ok(rows) = serde_json::from_str::<Vec<Value>>(body) else {
        return Vec::new();
    };
    rows.iter()
        .filter(|row| row.get("status").and_then(Value::as_str) == Some("alive"))
        .filter_map(|row| {
            let ip = row.get("ip")?.as_str()?;
            let port = match row.get("port")? {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                _ => return None,
            };
            let protocols = row.get("protocols")?.as_array()?;
            // First known protocol wins; socks5 beats socks4, https maps to http.
            let scheme = ["socks5", "http", "https", "socks4"]
                .iter()
                .find(|want| protocols.iter().any(|p| p.as_str() == Some(*want)))
                .copied()?;
            let scheme = if scheme == "https" { "http" } else { scheme };
            Some(Candidate {
                url: format!("{scheme}://{ip}:{port}"),
            })
        })
        .collect()
}

/// ProxyScrape plain text: one `ip:port` per line; junk lines skipped.
pub fn parse_proxyscrape_text(body: &str, scheme: &str) -> Vec<Candidate> {
    body.lines()
        .map(str::trim)
        .filter(|line| is_ip_port(line))
        .map(|line| Candidate {
            url: format!("{scheme}://{line}"),
        })
        .collect()
}

fn is_ip_port(line: &str) -> bool {
    let Some((ip, port)) = line.split_once(':') else {
        return false;
    };
    ip.split('.').count() == 4 && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())
}

/// Dedup by `ip:port` (scheme ignored), HProxy entries first, cap after dedup.
/// Errors when the merged list would be empty.
pub fn aggregate(
    hproxy: Option<Vec<Candidate>>,
    proxyscrape: Vec<Candidate>,
    cap: usize,
) -> Result<Vec<Candidate>, String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<Candidate> = Vec::new();
    for c in hproxy.into_iter().flatten().chain(proxyscrape) {
        // Key strips any `scheme://` prefix so `socks5://1.1.1.1:80` and
        // `http://1.1.1.1:80` collide; the first source (HProxy) keeps its URL.
        let key = match c.url.split_once("://") {
            Some((_, rest)) => rest.to_string(),
            None => c.url.clone(),
        };
        if seen.insert(key) {
            if out.len() < cap {
                out.push(c);
            }
        }
    }
    if out.is_empty() {
        return Err(
            "Gagal mengambil proxy gratis: tidak ada kandidat dari HProxy maupun ProxyScrape."
                .to_string(),
        );
    }
    Ok(out)
}

const HPROXY_URLS: [&str; 2] = [
    "https://hproxy.com/api/proxy-list?format=json&protocol=socks5,http&min_uptime_pct=70&sort=uptime&limit=300",
    "https://hproxy.com/api/proxy-list?format=json&protocol=socks4,https&min_uptime_pct=70&sort=uptime&limit=300",
];

const PROXYSCRAPE_URLS: [&str; 2] = [
    "https://api.proxyscrape.com/v2/?request=getproxies&protocol=socks5&timeout=10000",
    "https://api.proxyscrape.com/v2/?request=getproxies&protocol=http&timeout=10000",
];

/// GET all four source URLs with a 15s per-request timeout. HProxy failing
/// alone is non-fatal (skipped); only when every request fails, or the merged
/// aggregate is empty, does this return an error.
pub async fn fetch_free_candidates(
    hproxy_key: Option<&str>,
    http: &reqwest::Client,
) -> Result<Vec<Candidate>, String> {
    const TIMEOUT: Duration = Duration::from_secs(15);

    async fn get_text(client: &reqwest::Client, url: &str, key: Option<&str>) -> Option<String> {
        let mut req = client.get(url).timeout(TIMEOUT);
        if let Some(k) = key {
            req = req.header("X-API-Key", k);
        }
        let resp = req.send().await.ok()?;
        resp.error_for_status().ok()?.text().await.ok()
    }

    let (hp_a, hp_b) = (HPROXY_URLS[0], HPROXY_URLS[1]);
    let (ps_socks, ps_http) = (PROXYSCRAPE_URLS[0], PROXYSCRAPE_URLS[1]);
    let (h1, h2, s5, ht) = tokio::join!(
        get_text(http, hp_a, hproxy_key),
        get_text(http, hp_b, hproxy_key),
        get_text(http, ps_socks, None),
        get_text(http, ps_http, None),
    );

    let hproxy = match (h1, h2) {
        (None, None) => {
            eprintln!("FREEROXY: kedua permintaan HProxy gagal, lanjut dengan ProxyScrape");
            None
        }
        (h1, h2) => Some(
            h1.map(|b| parse_hproxy_json(&b))
                .unwrap_or_default()
                .into_iter()
                .chain(h2.map(|b| parse_hproxy_json(&b)).unwrap_or_default())
                .collect(),
        ),
    };
    let proxyscrape_failed = s5.is_none() && ht.is_none();
    let proxyscrape = s5
        .map(|b| parse_proxyscrape_text(&b, "socks5"))
        .unwrap_or_default()
        .into_iter()
        .chain(
            ht.map(|b| parse_proxyscrape_text(&b, "http"))
                .unwrap_or_default(),
        )
        .collect();

    let hproxy_failed = hproxy.is_none();
    let result = aggregate(hproxy, proxyscrape, FREE_PROXY_CAP);
    match result {
        Ok(v) => Ok(v),
        Err(e) if !hproxy_failed && !proxyscrape_failed => Err(e),
        Err(_) => Err("Gagal mengambil daftar proxy gratis: tidak ada kandidat — semua sumber (HProxy dan ProxyScrape) gagal atau kosong.".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hproxy_parser_keeps_alive_rows_and_maps_protocols() {
        let body = r#"[
      {"ip":"1.2.3.4","port":1080,"protocols":["socks4","socks5"],"status":"alive"},
      {"ip":"5.6.7.8","port":8080,"protocols":["http"],"status":"recently_alive"},
      {"ip":"9.9.9.9","port":80,"protocols":["http"],"status":"alive"}
    ]"#;
        let got = parse_hproxy_json(body);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].url, "socks5://1.2.3.4:1080");
        assert_eq!(got[1].url, "http://9.9.9.9:80");
    }

    #[test]
    fn proxyscrape_parser_skips_junk_lines() {
        let got = parse_proxyscrape_text("1.1.1.1:8080\nnot-a-proxy\n\n2.2.2.2:3128\n", "socks5");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].url, "socks5://1.1.1.1:8080");
    }

    fn cand(url: &str) -> Candidate {
        Candidate {
            url: url.to_string(),
        }
    }

    #[test]
    fn aggregate_dedups_by_ip_port_preferring_hproxy() {
        let h = vec![cand("socks5://1.1.1.1:80"), cand("http://2.2.2.2:80")];
        let p = vec![cand("http://1.1.1.1:80"), cand("http://3.3.3.3:80")];
        let got = aggregate(Some(h), p, 1000).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].url, "socks5://1.1.1.1:80"); // hproxy wins dedup
    }

    #[test]
    fn aggregate_caps_candidates() {
        let p: Vec<Candidate> = (0..1100)
            .map(|i| cand(&format!("http://10.{i}.0.1:80")))
            .collect();
        assert_eq!(aggregate(None, p, 1000).unwrap().len(), 1000);
    }

    #[test]
    fn aggregate_errors_when_both_sources_empty_or_failed() {
        let err = aggregate(None, Vec::new(), 1000).unwrap_err();
        assert!(err.contains("tidak ada"));
    }
}

//! Sending alert notifications: a webhook (Alertmanager's JSON payload, so
//! receivers written for Alertmanager work), ntfy (a POST of plain text to a
//! topic) and Telegram (the Bot API's sendMessage). One small HTTP/1.1
//! client over TCP or TLS (system roots from webpki-roots).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);

/// POST `body` to `url`; returns the status code and the response body.
pub async fn post(url: &str, headers: &[(&str, String)], content_type: &str, body: Vec<u8>) -> Result<(u16, String)> {
    tokio::time::timeout(TIMEOUT, post_inner(url, headers, content_type, body)).await.context("timed out")?
}

async fn post_inner(url: &str, headers: &[(&str, String)], content_type: &str, body: Vec<u8>) -> Result<(u16, String)> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        bail!("unsupported URL {url:?}: http:// or https://");
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (hostname, port) = match host.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => (h.trim_start_matches('[').trim_end_matches(']'), p.parse().context("bad port")?),
        _ => (host, if secure { 443 } else { 80 }),
    };
    let tcp = tokio::net::TcpStream::connect((hostname, port)).await.with_context(|| format!("cannot connect to {host}"))?;
    let req = {
        let mut b = Request::post(path).header(hyper::header::HOST, host).header(hyper::header::CONTENT_TYPE, content_type).header(hyper::header::USER_AGENT, "litek8s-alerts");
        for (k, v) in headers {
            b = b.header(*k, v);
        }
        b.body(Full::new(Bytes::from(body)))?
    };
    let resp = if secure {
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(hostname.to_string())?;
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls)).await?;
        tokio::spawn(conn);
        sender.send_request(req).await?
    } else {
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await?;
        tokio::spawn(conn);
        sender.send_request(req).await?
    };
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await?.to_bytes();
    Ok((status, String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]).into_owned()))
}

/// One alert as notifications carry it.
#[derive(Clone, Debug)]
pub struct Note {
    pub firing: bool,
    pub labels: std::collections::BTreeMap<String, String>,
    pub annotations: std::collections::BTreeMap<String, String>,
    pub starts_at: String,
    pub ends_at: String,
    pub fingerprint: String,
}

impl Note {
    fn title(&self) -> String {
        let name = self.labels.get("alertname").map(String::as_str).unwrap_or("alert");
        let target = ["namespace", "pod", "node", "persistentvolumeclaim", "container"]
            .iter()
            .filter_map(|k| self.labels.get(*k))
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("/");
        let state = if self.firing { "FIRING" } else { "RESOLVED" };
        if target.is_empty() { format!("[{state}] {name}") } else { format!("[{state}] {name} {target}") }
    }

    fn text(&self) -> String {
        let mut t = self.title();
        for key in ["summary", "description", "message"] {
            if let Some(v) = self.annotations.get(key) {
                t.push('\n');
                t.push_str(v);
            }
        }
        t
    }
}

/// Alertmanager's webhook payload (version 4).
pub fn webhook_payload(receiver: &str, notes: &[Note]) -> Value {
    let firing = notes.iter().any(|n| n.firing);
    let alerts: Vec<Value> = notes
        .iter()
        .map(|n| {
            json!({
                "status": if n.firing { "firing" } else { "resolved" },
                "labels": n.labels,
                "annotations": n.annotations,
                "startsAt": n.starts_at,
                "endsAt": n.ends_at,
                "generatorURL": "",
                "fingerprint": n.fingerprint,
            })
        })
        .collect();
    let common = |f: &dyn Fn(&Note) -> &std::collections::BTreeMap<String, String>| -> std::collections::BTreeMap<String, String> {
        let Some(first) = notes.first() else { return Default::default() };
        f(first).iter().filter(|(k, v)| notes.iter().all(|n| f(n).get(*k) == Some(v))).map(|(k, v)| (k.clone(), v.clone())).collect()
    };
    let common_labels = common(&|n| &n.labels);
    json!({
        "version": "4",
        "groupKey": format!("{{}}:{{receiver=\"{receiver}\"}}"),
        "truncatedAlerts": 0,
        "status": if firing { "firing" } else { "resolved" },
        "receiver": receiver,
        "groupLabels": {},
        "commonLabels": common_labels,
        "commonAnnotations": common(&|n| &n.annotations),
        "externalURL": "",
        "alerts": alerts,
    })
}

/// Where a receiver sends, with its secrets resolved.
#[derive(Clone, Debug)]
pub enum Target {
    Webhook { url: String, bearer: Option<String> },
    Ntfy { url: String, topic: String, token: Option<String> },
    Telegram { token: String, chat_id: String, api: String },
}

pub async fn send(receiver: &str, target: &Target, notes: &[Note]) -> Result<()> {
    if notes.is_empty() {
        return Ok(());
    }
    let (status, body) = match target {
        Target::Webhook { url, bearer } => {
            let headers: Vec<(&str, String)> = bearer.iter().map(|t| ("authorization", format!("Bearer {t}"))).collect();
            post(url, &headers, "application/json", serde_json::to_vec(&webhook_payload(receiver, notes))?).await?
        }
        Target::Ntfy { url, topic, token } => {
            // One message per alert: ntfy shows them one by one.
            let mut last = (200, String::new());
            for n in notes {
                let critical = n.labels.get("severity").is_some_and(|s| s == "critical");
                let mut headers = vec![
                    ("title", n.title()),
                    ("priority", if !n.firing { "low" } else if critical { "urgent" } else { "high" }.to_string()),
                    ("tags", if !n.firing { "white_check_mark" } else if critical { "rotating_light" } else { "warning" }.to_string()),
                ];
                if let Some(t) = token {
                    headers.push(("authorization", format!("Bearer {t}")));
                }
                let body = n.annotations.get("description").or(n.annotations.get("summary")).cloned().unwrap_or_else(|| n.title());
                last = post(&format!("{}/{}", url.trim_end_matches('/'), topic), &headers, "text/plain; charset=utf-8", body.into_bytes()).await?;
                if !(200..300).contains(&last.0) {
                    break;
                }
            }
            last
        }
        Target::Telegram { token, chat_id, api } => {
            let text = notes.iter().map(Note::text).collect::<Vec<_>>().join("\n\n");
            let body = json!({"chat_id": chat_id, "text": text, "disable_web_page_preview": true});
            post(&format!("{}/bot{token}/sendMessage", api.trim_end_matches('/')), &[], "application/json", serde_json::to_vec(&body)?).await?
        }
    };
    if !(200..300).contains(&status) {
        bail!("{receiver}: answered {status}: {body}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(firing: bool) -> Note {
        Note {
            firing,
            labels: [("alertname", "KubePodCrashLooping"), ("namespace", "default"), ("pod", "web"), ("severity", "warning")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            annotations: [("summary", "web is crash looping")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            starts_at: "2026-10-19T10:00:00Z".into(),
            ends_at: "0001-01-01T00:00:00Z".into(),
            fingerprint: "abc".into(),
        }
    }

    #[test]
    fn payloads() {
        let p = webhook_payload("ops", &[note(true)]);
        assert_eq!(p["status"], "firing");
        assert_eq!(p["alerts"][0]["labels"]["pod"], "web");
        assert_eq!(p["commonLabels"]["alertname"], "KubePodCrashLooping");
        assert_eq!(note(false).title(), "[RESOLVED] KubePodCrashLooping default/web");
    }

    #[tokio::test]
    async fn posts_to_a_local_receiver() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            let mut got = Vec::new();
            loop {
                let n = s.read(&mut buf).await.unwrap();
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got).to_string();
                if let Some(i) = text.find("\r\n\r\n") {
                    let len: usize = text.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length: ").map(|v| v.trim().parse().unwrap())).unwrap_or(0);
                    if got.len() >= i + 4 + len {
                        break;
                    }
                }
            }
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok").await.unwrap();
            String::from_utf8_lossy(&got).to_string()
        });
        send("t", &Target::Ntfy { url: format!("http://{addr}"), topic: "alerts".into(), token: None }, &[note(true)]).await.unwrap();
        let req = server.await.unwrap();
        assert!(req.starts_with("POST /alerts HTTP/1.1"), "{req}");
        assert!(req.to_ascii_lowercase().contains("priority: high"));
        assert!(req.ends_with("web is crash looping"));
    }
}

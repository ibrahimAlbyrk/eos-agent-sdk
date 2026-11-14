//! A terminated tunnel end to end: a TLS client trusting the proxy's CA,
//! the proxy, and a local HTTPS server it verifies with its own roots.

use std::sync::Arc;

use eos_agent_core::AllowAll;
use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

use super::super::filter::HostPolicy;
use super::super::{Lease, Owner, Proxy};
use super::ca::Ca;
use crate::sandbox::credentials::{CredentialsConfig, Vault};
use crate::sandbox::violations::Violations;

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// An HTTPS server on 127.0.0.1 answering each request with its
/// `authorization` header, `|`, and its body; and the certificate to trust.
async fn upstream() -> (u16, CertificateDer<'static>) {
    let key = KeyPair::generate().expect("key");
    let cert = CertificateParams::new(vec!["127.0.0.1".into()]).expect("params").self_signed(&key).expect("cert");
    let der = cert.der().clone();
    let config = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())))
        .expect("server");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else { return };
                let service = hyper::service::service_fn(|req: http::Request<hyper::body::Incoming>| async move {
                    use http_body_util::BodyExt as _;
                    if req.uri().path() == "/slow" {
                        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                        return Ok(http::Response::new(http_body_util::Full::new(bytes::Bytes::from("late"))));
                    }
                    if req.uri().path() == "/sink" {
                        // Counted, not echoed (the measurement's uploads).
                        let mut body = req.into_body();
                        let mut n = 0usize;
                        while let Some(Ok(frame)) = body.frame().await {
                            n += frame.data_ref().map_or(0, bytes::Bytes::len);
                        }
                        return Ok::<_, std::convert::Infallible>(http::Response::new(http_body_util::Full::new(
                            bytes::Bytes::from(n.to_string()),
                        )));
                    }
                    let auth =
                        req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
                    let host = req.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
                    let body = req.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                    let text = format!("{host}|{auth}|{}", String::from_utf8_lossy(&body));
                    Ok::<_, std::convert::Infallible>(http::Response::new(http_body_util::Full::new(
                        bytes::Bytes::from(text),
                    )))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                    .await;
            });
        }
    });
    (port, der)
}

struct Setup {
    proxy: Proxy,
    _lease: Lease,
    auth: String,
    ca: Arc<Ca>,
    sentinel: String,
    _tmp: std::path::PathBuf,
}

async fn setup(inject_hosts: &str, trust: &CertificateDer<'static>) -> Setup {
    let cfg = json!({"envVars": [{"name": "TOKEN", "mode": "mask", "injectHosts": [inject_hosts]}]});
    setup_with(cfg, &[("TOKEN", "real-secret")], trust, true).await
}

async fn setup_with(
    cfg: serde_json::Value,
    vars: &[(&str, &str)],
    trust: &CertificateDer<'static>,
    terminate: bool,
) -> Setup {
    let tmp = std::fs::canonicalize(std::env::temp_dir()).expect("tmp").join(format!("eos-tls-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&tmp).expect("tmp");
    let proxy = Proxy::start(Arc::new(AllowAll), Violations::new()).await.expect("proxy");
    let mut roots = rustls::RootCertStore::empty();
    roots.add(trust.clone()).expect("root");
    let client = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    proxy.shared.upstream_tls.set(Arc::new(client)).expect("unset");
    let ca = Arc::new(Ca::new(None, &tmp).expect("ca"));
    let vault = Arc::new(Vault::default());
    let cfg: CredentialsConfig = serde_json::from_value(cfg).expect("credentials");
    let plan =
        vault.plan(&cfg, &[], |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned())).expect("plan");
    let sentinel = plan.set.first().map(|(_, v)| v.clone()).unwrap_or_default();
    let hosts = Arc::new(HostPolicy { allowed: vec!["127.0.0.1".into()], ..HostPolicy::default() });
    let lease = proxy.register(Owner {
        agent_id: "a".into(),
        session: "s".into(),
        cmd_id: "c".into(),
        hosts,
        tls: terminate.then(|| Arc::clone(&ca)),
        vault,
        plaintext_inject: false,
        sigv4: Default::default(),
    });
    let token = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("{}:{}", lease.user, lease.password),
    );
    Setup { proxy, _lease: lease, auth: format!("Proxy-Authorization: Basic {token}\r\n"), ca, sentinel, _tmp: tmp }
}

/// CONNECT, then TLS to the proxy (trusting its CA), then one request.
async fn request(s: &Setup, port: u16, req: String) -> String {
    let mut tcp = TcpStream::connect(("127.0.0.1", s.proxy.port())).await.expect("connect");
    tcp.write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n{}\r\n", s.auth).as_bytes()).await.expect("write");
    let mut buf = [0u8; 39];
    tcp.read_exact(&mut buf).await.expect("200");
    assert_eq!(&buf, b"HTTP/1.1 200 Connection Established\r\n\r\n");
    let mut roots = rustls::RootCertStore::empty();
    roots.add(s.ca.cert_der().clone()).expect("ca");
    let config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let name = ServerName::try_from("127.0.0.1").expect("name");
    let mut tls = connector.connect(name, tcp).await.expect("the proxy's leaf is trusted");
    tls.write_all(req.as_bytes()).await.expect("request");
    tls.flush().await.expect("flush");
    let mut out = Vec::new();
    let _ = tls.read_to_end(&mut out).await;
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn placeholders_become_real_values_for_their_hosts() {
    let (port, cert) = upstream().await;
    let s = setup("127.0.0.1", &cert).await;
    let body = format!("token={}", s.sentinel);
    let req = format!(
        "POST /x HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        s.sentinel,
        body.len()
    );
    let res = request(&s, port, req).await;
    assert!(res.starts_with("HTTP/1.1 200 OK\r\n"), "{res}");
    assert!(res.ends_with(&format!("127.0.0.1:{port}|Bearer real-secret|token=real-secret")), "{res}");
}

#[tokio::test]
async fn other_hosts_get_the_placeholder() {
    let (port, cert) = upstream().await;
    let s = setup("api.example.com", &cert).await;
    let req = format!("GET / HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n", s.sentinel);
    let res = request(&s, port, req).await;
    assert!(res.contains(&format!("|Bearer {}|", s.sentinel)), "{res}");
}

#[tokio::test]
async fn an_untrusted_upstream_is_a_bad_gateway() {
    let (port, _) = upstream().await;
    let (_, other) = upstream().await;
    let s = setup("127.0.0.1", &other).await;
    let res = request(&s, port, "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".into()).await;
    assert!(res.starts_with("HTTP/1.1 502 Bad Gateway\r\n") && res.ends_with("Bad Gateway"), "{res}");
    let res = request(&s, port, "GET / HTTP/1.1\r\nConnection: close\r\n\r\n".into()).await;
    assert!(res.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{res}");
    assert!(
        res.contains("x-proxy-error: blocked-by-sandbox-runtime")
            && res.ends_with("HTTP/1.1 request without a Host header\n")
    );
}

#[tokio::test]
async fn bytes_that_are_not_tls_are_tunnelled() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        if let Ok((mut s, _)) = listener.accept().await {
            let mut buf = [0u8; 5];
            let _ = s.read_exact(&mut buf).await;
            let _ = s.write_all(&buf).await;
        }
    });
    let (_, cert) = upstream().await;
    let s = setup("127.0.0.1", &cert).await;
    let mut tcp = TcpStream::connect(("127.0.0.1", s.proxy.port())).await.expect("connect");
    tcp.write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n{}\r\nhello", s.auth).as_bytes()).await.expect("write");
    let mut out = vec![0u8; 39 + 5];
    tcp.read_exact(&mut out).await.expect("echo");
    assert!(out.ends_with(b"hello"));
}

#[tokio::test]
async fn sigv4_requests_are_signed_again_with_the_real_pair() {
    use crate::sandbox::credentials::sigv4;
    let (port, cert) = upstream().await;
    let cfg = json!({"envVars": [
        {"name": "AWS_ACCESS_KEY_ID", "mode": "mask", "injectHosts": ["127.0.0.1"]},
        {"name": "AWS_SECRET_ACCESS_KEY", "mode": "mask", "injectHosts": ["127.0.0.1"]},
    ]});
    let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    let s =
        setup_with(cfg, &[("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE"), ("AWS_SECRET_ACCESS_KEY", secret)], &cert, true).await;
    let fake_key = &s.sentinel;
    let req = format!(
        "GET /?Action=ListUsers&Version=2010-05-08 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Content-Type: application/x-www-form-urlencoded; charset=utf-8\r\nX-Amz-Date: 20150830T123600Z\r\n\
         Authorization: AWS4-HMAC-SHA256 Credential={fake_key}/20150830/us-east-1/iam/aws4_request, \
         SignedHeaders=content-type;host;x-amz-date, Signature=00\r\nConnection: close\r\n\r\n"
    );
    let res = request(&s, port, req).await;
    let scope = sigv4::parse("AWS4-HMAC-SHA256 Credential=k/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=00")
    .expect("scope");
    let host = format!("127.0.0.1:{port}");
    let header = |name: &str| match name {
        "content-type" => Some(vec!["application/x-www-form-urlencoded; charset=utf-8".to_owned()]),
        "x-amz-date" => Some(vec!["20150830T123600Z".to_owned()]),
        _ => None,
    };
    let expected = sigv4::authorization(
        &sigv4::Request {
            method: "GET",
            target: "/?Action=ListUsers&Version=2010-05-08",
            host: &host,
            header: &header,
            signed_headers: &scope.signed_headers,
            amz_date: "20150830T123600Z",
            scope: &scope,
            payload_hash: &sigv4::sha256_hex(b""),
        },
        "AKIDEXAMPLE",
        secret,
    )
    .expect("signed");
    assert!(res.contains(&format!("|{expected}|")), "{res}");

    let presigned = format!(
        "GET /o?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={fake_key}%2F20150830%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Signature=00 HTTP/1.1\r\n\
         Host: x\r\nConnection: close\r\n\r\n"
    );
    let res = request(&s, port, presigned).await;
    assert!(res.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{res}");
    assert!(res.ends_with("Set credentials.sigv4.presigned to \"passthrough\" to forward it unmodified (it will fail upstream: the signature covers the masked placeholder).\n"), "{res}");
}

/// Reads until the proxy ends the connection; how long that took.
async fn until_closed(tls: &mut tokio_rustls::client::TlsStream<TcpStream>) -> std::time::Duration {
    let t = std::time::Instant::now();
    let mut buf = [0u8; 4096];
    let wait = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match tls.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });
    wait.await.expect("closed by the proxy");
    t.elapsed()
}

/// sandbox-runtime's timeouts, shortened: the first request within 300 ms,
/// 400 ms after the last response, 500 ms without progress while pending.
fn short(s: &Setup) {
    let t = super::watchdog::Timeouts {
        first_request: std::time::Duration::from_millis(300),
        keep_alive: std::time::Duration::from_millis(400),
        stalled: std::time::Duration::from_millis(500),
    };
    s.proxy.shared.tls_timeouts.set(t).expect("unset");
    // The first leaf (an RSA key, slow in a debug build) is made beforehand.
    s.ca.server_config("127.0.0.1").expect("leaf");
}

#[tokio::test]
async fn an_idle_tunnel_is_closed_as_sandbox_runtime_closes_it() {
    let (port, cert) = upstream().await;
    let s = setup("127.0.0.1", &cert).await;
    short(&s);
    let trust = s.ca.cert_der().clone();
    // No request at all: closed once the first one is overdue.
    let mut tls = open_tls(&s, port, &trust).await;
    let waited = until_closed(&mut tls).await;
    assert!(waited.as_millis() >= 200 && waited.as_millis() < 1500, "{waited:?}");
    // After a response: the keep-alive wait, not the first request's.
    let mut tls = open_tls(&s, port, &trust).await;
    get(&mut tls, port).await;
    let waited = until_closed(&mut tls).await;
    assert!(waited.as_millis() >= 300 && waited.as_millis() < 1500, "{waited:?}");
}

#[tokio::test]
async fn a_slow_upstream_is_waited_for_and_a_stalled_upload_dropped() {
    let (port, cert) = upstream().await;
    let s = setup("127.0.0.1", &cert).await;
    short(&s);
    let trust = s.ca.cert_der().clone();
    // An answer that takes longer than every timeout still arrives.
    let mut tls = open_tls(&s, port, &trust).await;
    let req = format!("GET /slow HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    tls.write_all(req.as_bytes()).await.expect("req");
    tls.flush().await.expect("flush");
    let mut out = Vec::new();
    let _ = tls.read_to_end(&mut out).await;
    let out = String::from_utf8_lossy(&out);
    assert!(out.starts_with("HTTP/1.1 200 OK\r\n") && out.ends_with("late"), "{out}");
    // A body that stops arriving: dropped once nothing moved for the stall period.
    let mut tls = open_tls(&s, port, &trust).await;
    let head = format!("POST /sink HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 1000\r\n\r\nonly a bit");
    tls.write_all(head.as_bytes()).await.expect("head");
    tls.flush().await.expect("flush");
    let waited = until_closed(&mut tls).await;
    assert!(waited.as_millis() >= 400 && waited.as_millis() < 2000, "{waited:?}");
}

fn rss_kb() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

/// CONNECT and a TLS session through the proxy, trusting `trust`.
async fn open_tls(s: &Setup, port: u16, trust: &CertificateDer<'static>) -> tokio_rustls::client::TlsStream<TcpStream> {
    let mut tcp = TcpStream::connect(("127.0.0.1", s.proxy.port())).await.expect("connect");
    tcp.write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n{}\r\n", s.auth).as_bytes()).await.expect("write");
    let mut buf = [0u8; 39];
    tcp.read_exact(&mut buf).await.expect("200");
    let mut roots = rustls::RootCertStore::empty();
    roots.add(trust.clone()).expect("root");
    let config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = ServerName::try_from("127.0.0.1").expect("name");
    tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await.expect("tls")
}

/// One kept-alive request and its whole response.
async fn get(tls: &mut tokio_rustls::client::TlsStream<TcpStream>, port: u16) {
    tls.write_all(format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes()).await.expect("req");
    // A TLS stream may keep the last records until flushed.
    tls.flush().await.expect("flush");
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = tls.read(&mut buf).await.expect("read");
        assert!(n > 0, "closed early");
        seen.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&seen);
        if let Some(end) = text.find("\r\n\r\n") {
            let len: usize = text[..end]
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length: ").map(str::to_owned))
                .and_then(|v| v.trim().parse().ok())
                .expect("length");
            if seen.len() >= end + 4 + len {
                return;
            }
        }
    }
}

/// `cargo test -p eos-agent-tools-shell --release --lib tls_cost -- --ignored --nocapture`,
/// with `TLS_COST=tunnelled` or `TLS_COST=terminated` to measure one mode in
/// a fresh process (memory freed by the first mode is reused by the second).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement, not a test"]
async fn tls_cost() {
    use std::time::Instant;
    let tmp = std::fs::canonicalize(std::env::temp_dir()).expect("tmp").join(format!("eos-tlsc-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&tmp).expect("tmp");
    let t = Instant::now();
    let ca = Ca::new(None, &tmp).expect("ca");
    println!("ephemeral CA and trust bundle: {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
    let t = Instant::now();
    ca.server_config("first.example").expect("leaf");
    println!("first leaf (RSA-2048 leaf key + certificate): {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
    let t = Instant::now();
    ca.server_config("second.example").expect("leaf");
    println!("another host's leaf: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
    drop(ca);

    let (port, cert) = upstream().await;
    // One masked variable, so a terminated tunnel scans every request for it.
    let cfg = json!({"envVars": [{"name": "TOKEN", "mode": "mask", "injectHosts": ["127.0.0.1"]}]});
    let modes: &[(&str, bool)] = match std::env::var("TLS_COST").as_deref() {
        Ok("tunnelled") => &[("tunnelled", false)],
        Ok("terminated") => &[("terminated", true)],
        _ => &[("tunnelled", false), ("terminated", true)],
    };
    for &(label, terminate) in modes {
        let s = setup_with(cfg.clone(), &[("TOKEN", "real-secret")], &cert, terminate).await;
        let trust = if terminate { s.ca.cert_der().clone() } else { cert.clone() };
        // Warm up (the first leaf, the upstream config).
        let mut warm = open_tls(&s, port, &trust).await;
        get(&mut warm, port).await;
        drop(warm);
        let n = 200;
        let t = Instant::now();
        for _ in 0..n {
            let mut tls = open_tls(&s, port, &trust).await;
            get(&mut tls, port).await;
        }
        let per = t.elapsed().as_secs_f64() * 1e3 / f64::from(n);
        let mut tls = open_tls(&s, port, &trust).await;
        let t = Instant::now();
        for _ in 0..n {
            get(&mut tls, port).await;
        }
        let kept = t.elapsed().as_secs_f64() * 1e3 / f64::from(n);
        let total: usize = 256 * 1024 * 1024;
        let head = format!("POST /sink HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {total}\r\n\r\n");
        let chunk = vec![b'x'; 256 * 1024];
        let t = Instant::now();
        tls.write_all(head.as_bytes()).await.expect("head");
        let mut sent = 0;
        while sent < total {
            tls.write_all(&chunk).await.expect("body");
            sent += chunk.len();
        }
        // Without it the last records may stay in the TLS stream while this
        // side waits for the answer: the upload never ends (the stall once
        // seen in this measurement).
        tls.flush().await.expect("flush");
        let mut answer = Vec::new();
        let mut buf = [0u8; 1024];
        while !String::from_utf8_lossy(&answer).ends_with(&total.to_string()) {
            let n = tls.read(&mut buf).await.expect("read");
            assert!(n > 0, "closed early: {}", String::from_utf8_lossy(&answer));
            answer.extend_from_slice(&buf[..n]);
        }
        let secs = t.elapsed().as_secs_f64();
        drop(tls);
        let mut held = Vec::new();
        let before = rss_kb();
        for _ in 0..200 {
            let mut tls = open_tls(&s, port, &trust).await;
            get(&mut tls, port).await;
            held.push(tls);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let after = rss_kb();
        println!(
            "{label}: new connection + one request {per:.2} ms; another request on it {kept:.3} ms; \
             {} MB upload {:.0} MB/s; 200 open connections after a request: +{} KB RSS ({:.1} KB each, \
             the test's client and server included)",
            total >> 20,
            (total >> 20) as f64 / secs,
            after.saturating_sub(before),
            after.saturating_sub(before) as f64 / 200.0
        );
        drop(held);
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

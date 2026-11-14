//! `cargo test -p eos-agent-tools-shell --release --lib proxy_cost -- --ignored --nocapture`

use std::time::Instant;

use eos_agent_core::AllowAll;

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A proxy whose one registered command may reach `allowed`, and that
/// command's `Proxy-Authorization` header.
async fn proxy_for(allowed: String) -> (Proxy, Lease, String) {
    let proxy = Proxy::start(Arc::new(AllowAll), Violations::new()).await.expect("proxy");
    let hosts = Arc::new(HostPolicy { allowed: vec![allowed], ..HostPolicy::default() });
    let lease = proxy.register(Owner {
        agent_id: "a".into(),
        session: "s".into(),
        cmd_id: "c".into(),
        hosts,
        tls: None,
        vault: Arc::default(),
        plaintext_inject: false,
        sigv4: Default::default(),
    });
    let token = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("{}:{}", lease.user, lease.password),
    );
    (proxy, lease, format!("Proxy-Authorization: Basic {token}\r\n"))
}

fn rss_kb() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement, not a test"]
async fn proxy_cost() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let up_port = upstream.local_addr().expect("addr").port();
    let held = Arc::new(Mutex::new(Vec::new()));
    let keep = Arc::clone(&held);
    tokio::spawn(async move {
        while let Ok((s, _)) = upstream.accept().await {
            lock(&keep).push(s);
        }
    });
    let (proxy, _lease, auth) = proxy_for(format!("127.0.0.1:{up_port}")).await;
    let before = rss_kb();
    let mut clients = Vec::new();
    let started = Instant::now();
    for _ in 0..1000 {
        let mut c = TcpStream::connect(("127.0.0.1", proxy.port())).await.expect("connect");
        c.write_all(format!("CONNECT 127.0.0.1:{up_port} HTTP/1.1\r\n{auth}\r\n").as_bytes()).await.expect("write");
        let mut buf = [0u8; 64];
        let n = c.read(&mut buf).await.expect("read");
        assert!(buf[..n].starts_with(b"HTTP/1.1 200"));
        clients.push(c);
    }
    let opened = started.elapsed();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = rss_kb();
    println!(
        "1000 idle tunnels: opened in {opened:?}; RSS {before} KB -> {after} KB (+{} KB, {:.1} KB per tunnel incl. \
             both sockets of the test)",
        after.saturating_sub(before),
        after.saturating_sub(before) as f64 / 1000.0
    );
    drop(clients);

    // Throughput through one tunnel.
    let sink = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let sink_port = sink.local_addr().expect("addr").port();
    let total: usize = 512 * 1024 * 1024;
    let reader = tokio::spawn(async move {
        let (mut s, _) = sink.accept().await.expect("accept");
        let mut buf = vec![0u8; 64 * 1024];
        let mut got = 0;
        while got < total {
            let n = s.read(&mut buf).await.expect("read");
            if n == 0 {
                break;
            }
            got += n;
        }
        got
    });
    let (proxy, _lease, auth) = proxy_for(format!("127.0.0.1:{sink_port}")).await;
    let mut c = TcpStream::connect(("127.0.0.1", proxy.port())).await.expect("connect");
    c.write_all(format!("CONNECT 127.0.0.1:{sink_port} HTTP/1.1\r\n{auth}\r\n").as_bytes()).await.expect("write");
    let mut head = [0u8; 64];
    let _ = c.read(&mut head).await.expect("read");
    let chunk = vec![7u8; 256 * 1024];
    let started = Instant::now();
    let mut sent = 0;
    while sent < total {
        c.write_all(&chunk).await.expect("write");
        sent += chunk.len();
    }
    let got = reader.await.expect("reader");
    let secs = started.elapsed().as_secs_f64();
    println!("one tunnel: {} MB in {secs:.2}s = {:.0} MB/s", got >> 20, (got >> 20) as f64 / secs);
}

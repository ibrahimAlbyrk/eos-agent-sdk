//! A recording proxy for the Anthropic API.
//!
//! Point a client at it (`ANTHROPIC_BASE_URL=http://127.0.0.1:7898 claude -p hi`)
//! and every exchange lands in `<out>/<seq>-<path>/`:
//! - `request.json`: method, path, headers (secrets redacted), body
//! - `response.sse`: the response body exactly as streamed
//! - `meta.json`: status, response headers, timing
//!
//! Captures are the reference for prompt and tool-definition parity. They hold
//! personal context, so they stay out of git (`fixtures/captured/`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use clap::Parser;
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:7898")]
    addr: std::net::SocketAddr,
    #[arg(long, default_value = "https://api.anthropic.com")]
    upstream: String,
    #[arg(long, default_value = "fixtures/captured")]
    out: PathBuf,
}

const REDACTED: &[&str] = &["authorization", "x-api-key", "cookie", "set-cookie", "proxy-authorization"];
// Hop-by-hop headers and ones the proxy must set itself. Dropping
// accept-encoding keeps the upstream body uncompressed, so captures are readable.
const NOT_FORWARDED: &[&str] = &["host", "connection", "content-length", "transfer-encoding", "accept-encoding"];
const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

struct Proxy {
    client: reqwest::Client,
    upstream: String,
    out: PathBuf,
    seq: AtomicU32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tokio::fs::create_dir_all(&args.out).await?;
    let proxy = Arc::new(Proxy {
        client: reqwest::Client::builder().build()?,
        upstream: args.upstream.trim_end_matches('/').to_owned(),
        seq: AtomicU32::new(next_seq(&args.out)),
        out: args.out,
    });
    let app = axum::Router::new().fallback(forward).with_state(proxy.clone());
    let listener = tokio::net::TcpListener::bind(args.addr).await?;
    println!("capturing {} → {} into {}", listener.local_addr()?, proxy.upstream, proxy.out.display());
    axum::serve(listener, app).await?;
    Ok(())
}

/// Continues numbering after captures already in the folder.
fn next_seq(out: &Path) -> u32 {
    std::fs::read_dir(out)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok()?.file_name().to_str()?.split('-').next()?.parse::<u32>().ok())
        .max()
        .map_or(1, |n| n + 1)
}

async fn forward(State(proxy): State<Arc<Proxy>>, req: Request) -> Response {
    match relay(&proxy, req).await {
        Ok(res) => res,
        Err(e) => (StatusCode::BAD_GATEWAY, format!("capture proxy: {e:#}")).into_response(),
    }
}

async fn relay(proxy: &Proxy, req: Request) -> anyhow::Result<Response> {
    let started = Instant::now();
    let (parts, body) = req.into_parts();
    let path_q = parts.uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
    let body = to_bytes(body, MAX_REQUEST_BYTES).await?;

    let seq = proxy.seq.fetch_add(1, Ordering::Relaxed);
    let dir = proxy.out.join(format!("{seq:04}-{}", slug(parts.uri.path())));
    tokio::fs::create_dir_all(&dir).await?;
    write_json(
        &dir.join("request.json"),
        &serde_json::json!({
            "method": parts.method.as_str(),
            "path": path_q,
            "headers": headers_json(&parts.headers),
            "body": body_json(&body),
        }),
    )
    .await?;

    let mut upstream = proxy.client.request(parts.method.clone(), format!("{}{path_q}", proxy.upstream));
    for (name, value) in &parts.headers {
        if !NOT_FORWARDED.contains(&name.as_str()) {
            upstream = upstream.header(name, value);
        }
    }
    let res = upstream.body(body).send().await?;
    let status = res.status();
    let res_headers = res.headers().clone();
    write_json(
        &dir.join("meta.json"),
        &serde_json::json!({
            "status": status.as_u16(),
            "headers": headers_json(&res_headers),
            "first_byte_ms": started.elapsed().as_millis() as u64,
        }),
    )
    .await?;

    // Tee: the client gets each chunk as it arrives while a writer task saves it.
    // Unbounded on purpose: a full channel must never drop part of a capture.
    let mut file = tokio::fs::File::create(dir.join("response.sse")).await?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    tokio::spawn(async move {
        while let Some(chunk) = rx.recv().await {
            if file.write_all(&chunk).await.is_err() {
                return;
            }
        }
        let _ = file.flush().await;
    });
    let body = res.bytes_stream().map(move |chunk| {
        if let Ok(bytes) = &chunk {
            let _ = tx.send(bytes.clone());
        }
        chunk
    });

    let mut out = Response::builder().status(status.as_u16());
    for (name, value) in &res_headers {
        if !NOT_FORWARDED.contains(&name.as_str()) {
            out = out.header(name, value);
        }
    }
    Ok(out.body(Body::from_stream(body))?)
}

fn slug(path: &str) -> String {
    path.trim_matches('/').replace(['/', '?', '&', '='], "_")
}

fn headers_json(headers: &HeaderMap) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (name, value) in headers {
        let shown = if REDACTED.contains(&name.as_str()) {
            "<redacted>".into()
        } else {
            value.to_str().unwrap_or("<binary>").to_owned()
        };
        map.insert(name.as_str().to_owned(), serde_json::Value::String(shown));
    }
    serde_json::Value::Object(map)
}

fn body_json(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body)
        .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(body).into_owned()))
}

async fn write_json(path: &Path, value: &serde_json::Value) -> anyhow::Result<()> {
    tokio::fs::write(path, serde_json::to_vec_pretty(value)?).await?;
    Ok(())
}

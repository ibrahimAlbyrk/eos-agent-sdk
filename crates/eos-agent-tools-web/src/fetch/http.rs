//! The HTTP side of WebFetch: one GET per hop, same-site redirects followed
//! by hand, a cap on the body and a deadline over the whole fetch.

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use reqwest::header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, LOCATION, RETRY_AFTER, USER_AGENT};
use reqwest::{Client, Url};

use super::urls::same_site;

pub(crate) const MAX_REDIRECTS: u32 = 10;

#[derive(Debug, Clone)]
pub(crate) struct Limits {
    pub request_timeout: Duration,
    pub deadline: Duration,
    pub max_body: usize,
}

#[derive(Debug)]
pub(crate) enum Outcome {
    Body {
        content_type: String,
        body: Bytes,
    },
    /// A redirect off the site, handed back to the model.
    Redirect {
        from: Url,
        to: Url,
        status: u16,
    },
    HttpError {
        status: u16,
        retry_after: Option<String>,
    },
}

#[derive(Debug)]
pub(crate) enum FetchError {
    TooManyRedirects,
    Deadline(Duration),
    TooLarge(usize),
    /// Connecting, TLS, DNS or reading failed.
    Transport(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyRedirects => write!(f, "Too many redirects (exceeded {MAX_REDIRECTS})"),
            Self::Deadline(d) => write!(f, "Fetch did not complete within the {}s deadline", d.as_secs()),
            Self::TooLarge(max) => write!(f, "maxContentLength size of {max} exceeded"),
            Self::Transport(message) => f.write_str(message),
        }
    }
}

pub(crate) fn client() -> Client {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .expect("the TLS backend initializes")
}

pub(crate) async fn fetch(client: &Client, url: Url, limits: &Limits, user_agent: &str) -> Result<Outcome, FetchError> {
    tokio::time::timeout(limits.deadline, hops(client, url, limits, user_agent))
        .await
        .unwrap_or(Err(FetchError::Deadline(limits.deadline)))
}

async fn hops(client: &Client, mut url: Url, limits: &Limits, user_agent: &str) -> Result<Outcome, FetchError> {
    for hop in 0..=MAX_REDIRECTS {
        let res = client
            .get(url.clone())
            .timeout(limits.request_timeout)
            .header(ACCEPT, "text/markdown, text/html, */*")
            .header(USER_AGENT, user_agent)
            .send()
            .await
            .map_err(transport)?;
        let status = res.status();
        if status.is_success() {
            let content_type = header(&res, CONTENT_TYPE).unwrap_or_default();
            let body = read_body(res, limits.max_body).await?;
            return Ok(Outcome::Body { content_type, body });
        }
        if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
            let next = header(&res, LOCATION).filter(|l| !l.trim().is_empty()).and_then(|l| url.join(&l).ok());
            let Some(next) = next else { return Ok(Outcome::HttpError { status: status.as_u16(), retry_after: None }) };
            if !same_site(&url, &next) {
                return Ok(Outcome::Redirect { from: url, to: next, status: status.as_u16() });
            }
            if hop == MAX_REDIRECTS {
                break;
            }
            url = next;
            continue;
        }
        let retry_after =
            header(&res, RETRY_AFTER).filter(|v| (1..=6).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_digit()));
        return Ok(Outcome::HttpError { status: status.as_u16(), retry_after });
    }
    Err(FetchError::TooManyRedirects)
}

fn header(res: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    res.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned)
}

async fn read_body(mut res: reqwest::Response, max: usize) -> Result<Bytes, FetchError> {
    let declared = header(&res, CONTENT_LENGTH).and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|len| len > max) {
        return Err(FetchError::TooLarge(max));
    }
    let mut body = BytesMut::with_capacity(declared.unwrap_or(0).min(max));
    while let Some(chunk) = res.chunk().await.map_err(transport)? {
        if body.len() + chunk.len() > max {
            return Err(FetchError::TooLarge(max));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

/// The error with its causes, which carry the useful part (DNS, TLS, …).
fn transport(e: reqwest::Error) -> FetchError {
    let mut message = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    FetchError::Transport(message)
}

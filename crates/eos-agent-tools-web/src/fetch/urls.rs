//! URL rules: what may be fetched, which redirects are followed, and which
//! hosts are preapproved documentation sites.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use reqwest::Url;

const MAX_URL_LEN: usize = 2000;
const PREAPPROVED_LIST: &str = include_str!("../../data/preapproved_hosts.txt");

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UrlProblem {
    /// `localhost` and other single-label hosts.
    Dotless,
    Invalid,
}

/// Length, credentials and host checks Claude Code runs before any request.
pub(crate) fn check(raw: &str) -> Result<Url, UrlProblem> {
    let parsed = Url::parse(raw);
    let dotless = |url: &Url| url.host_str().is_some_and(|h| !h.contains('.'));
    let url = match parsed {
        Ok(url) => url,
        Err(_) => return Err(UrlProblem::Invalid),
    };
    if raw.len() > MAX_URL_LEN || !url.username().is_empty() || url.password().is_some() {
        return Err(if dotless(&url) { UrlProblem::Dotless } else { UrlProblem::Invalid });
    }
    match url.host_str() {
        Some(host) if host.split('.').count() >= 2 => Ok(url),
        Some(_) => Err(UrlProblem::Dotless),
        None => Err(UrlProblem::Invalid),
    }
}

/// The snapshot id of a shared claude.ai chat link.
pub(crate) fn claude_share_id(url: &Url) -> Option<&str> {
    if url.host_str() != Some("claude.ai") {
        return None;
    }
    let id = url.path().strip_prefix("/share/")?.trim_end_matches('/');
    (!id.is_empty() && !id.contains('/')).then_some(id)
}

fn strip_www(host: &str) -> &str {
    host.strip_prefix("www.").unwrap_or(host)
}

/// A redirect is followed only when it stays on the same site: same scheme
/// and port, the same host up to `www.`, no credentials, and it does not
/// leave a preapproved path prefix the original URL was inside.
pub(crate) fn same_site(from: &Url, to: &Url) -> bool {
    if from.scheme() != to.scheme() || from.port() != to.port() {
        return false;
    }
    if !to.username().is_empty() || to.password().is_some() {
        return false;
    }
    let (Some(from_host), Some(to_host)) = (from.host_str(), to.host_str()) else { return false };
    let bare = strip_www(from_host);
    let www = format!("www.{bare}");
    let hosts = [from_host, bare, www.as_str()];
    if hosts.iter().any(|h| preapproved(h, from.path())) && !hosts.iter().any(|h| preapproved(h, to.path())) {
        return false;
    }
    strip_www(to_host) == bare
}

pub(crate) fn is_preapproved(url: &Url) -> bool {
    url.host_str().is_some_and(|host| preapproved(host, url.path()))
}

struct Lists {
    hosts: HashSet<&'static str>,
    prefixes: HashMap<&'static str, Vec<&'static str>>,
}

fn lists() -> &'static Lists {
    static LISTS: OnceLock<Lists> = OnceLock::new();
    LISTS.get_or_init(|| {
        let mut hosts = HashSet::new();
        let mut prefixes: HashMap<&str, Vec<&str>> = HashMap::new();
        for entry in PREAPPROVED_LIST.lines().map(str::trim).filter(|l| !l.is_empty()) {
            match entry.find('/') {
                None => {
                    hosts.insert(entry);
                }
                Some(at) => prefixes.entry(&entry[..at]).or_default().push(&entry[at..]),
            }
        }
        Lists { hosts, prefixes }
    })
}

/// Claude Code's preapproved-host rule: a listed hostname, or a listed
/// hostname + path prefix (on GitHub only an Anthropic org, or a repo's root
/// and its `main` branch).
pub(crate) fn preapproved(host: &str, path: &str) -> bool {
    let lists = lists();
    if lists.hosts.contains(host) {
        return true;
    }
    let Some(prefixes) = lists.prefixes.get(host) else { return false };
    if has_encoded_separator(path) {
        return false;
    }
    prefixes.iter().any(|prefix| {
        (path == *prefix || path.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/')))
            && match host {
                "github.com" => *prefix == "/anthropics" || github_repo_path(path),
                "raw.githubusercontent.com" => raw_main_path(path),
                _ => true,
            }
    })
}

/// `%2f`, `%5c`, `%2e`, also double-encoded (`%252f`).
fn has_encoded_separator(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.iter().enumerate().any(|(i, &b)| {
        if b != b'%' {
            return false;
        }
        let mut rest = &bytes[i + 1..];
        while rest.starts_with(b"25") {
            rest = &rest[2..];
        }
        rest.len() >= 2 && matches!(&rest[..2].to_ascii_lowercase()[..], b"2f" | b"5c" | b"2e")
    })
}

/// `/owner/repo`, `/owner/repo/`, `/owner/repo/(tree|blob)/main[/…]`.
fn github_repo_path(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let named = |i: usize| parts.get(i).is_some_and(|p| !p.is_empty());
    if !(parts.first() == Some(&"") && named(1) && named(2)) {
        return false;
    }
    match parts.len() {
        3 => true,
        4 => parts[3].is_empty(),
        _ => matches!(parts[3], "tree" | "blob") && parts.get(4) == Some(&"main"),
    }
}

/// `/owner/repo/refs/heads/main/<something>`.
fn raw_main_path(path: &str) -> bool {
    let parts: Vec<&str> = path.splitn(7, '/').collect();
    parts.len() == 7
        && parts[0].is_empty()
        && !parts[1].is_empty()
        && !parts[2].is_empty()
        && parts[3..6] == ["refs", "heads", "main"]
        && !parts[6].is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).expect("url")
    }

    #[test]
    fn checks_urls() {
        assert_eq!(check("http://localhost:3000/x"), Err(UrlProblem::Dotless));
        assert_eq!(check("https://user:pw@example.com"), Err(UrlProblem::Invalid));
        assert_eq!(check("not a url"), Err(UrlProblem::Invalid));
        assert_eq!(check(&format!("https://example.com/{}", "a".repeat(2000))), Err(UrlProblem::Invalid));
        assert!(check("https://example.com/a").is_ok());
    }

    #[test]
    fn same_site_rules() {
        assert!(same_site(&url("https://example.com/a"), &url("https://www.example.com/b")));
        assert!(!same_site(&url("https://example.com/a"), &url("https://other.com/b")));
        assert!(!same_site(&url("https://example.com/a"), &url("http://example.com/b")));
        assert!(!same_site(&url("https://example.com/a"), &url("https://example.com:8443/b")));
        // Leaving a preapproved path prefix is a cross-site move.
        assert!(!same_site(&url("https://github.com/anthropics/x"), &url("https://github.com/someone/y")));
        assert!(same_site(&url("https://github.com/anthropics/x"), &url("https://github.com/anthropics/y")));
    }

    #[test]
    fn preapproved_hosts() {
        assert!(preapproved("docs.python.org", "/3/library"));
        assert!(preapproved("github.com", "/anthropics/claude-code/issues"));
        assert!(preapproved("github.com", "/anthropic-experimental/repo/tree/main/src"));
        assert!(!preapproved("github.com", "/anthropic-experimental/repo/issues"));
        assert!(preapproved("raw.githubusercontent.com", "/anthropics/repo/refs/heads/main/README.md"));
        assert!(!preapproved("raw.githubusercontent.com", "/anthropics/repo/refs/heads/dev/README.md"));
        assert!(!preapproved("go.dev", "/doc%2f..%2fsecret"));
        assert!(preapproved("go.dev", "/doc/effective_go"));
        assert!(!preapproved("go.dev", "/docs"));
        assert!(!preapproved("example.com", "/"));
    }

    #[test]
    fn share_links() {
        assert_eq!(claude_share_id(&url("https://claude.ai/share/abc-123")), Some("abc-123"));
        assert_eq!(claude_share_id(&url("https://claude.ai/chat/abc")), None);
    }
}

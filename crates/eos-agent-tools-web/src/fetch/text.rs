//! What WebFetch tells the model, worded as Claude Code words it.

use reqwest::{StatusCode, Url};

use super::WEB_FETCH;
use super::notes::Earlier;
use super::page::TRUNCATED;

const RESTRICTIONS: &str = include_str!("../../prompts/fetch_restrictions.md");
const MAX_RELAYED_URL: usize = 1000;
const MAX_HOSTNAME: usize = 255;

pub(super) fn status_text(code: u16) -> &'static str {
    StatusCode::from_u16(code).ok().and_then(|s| s.canonical_reason()).unwrap_or("Unknown Status")
}

pub(super) fn share_link(id: &str) -> String {
    format!(
        "https://claude.ai/share/{id} is a shared claude.ai chat. Its content is rendered in the browser, so it is not \
fetchable from here \u{2014} a plain fetch returns only the claude.ai app shell. The user can paste or export the \
conversation instead."
    )
}

pub(super) fn already_fetched(url: &str, offset: usize, earlier: &Earlier, ttl: &str) -> String {
    let and_offset = if offset > 0 { " and offset" } else { "" };
    format!(
        "Already fetched {url} with the same prompt{and_offset} {}s ago in this conversation (tool call {}). \
{WEB_FETCH} caches pages for {ttl}, so fetching again now would return the same content; use that earlier result. \
If you can no longer see it, call {WEB_FETCH} again with the same url and prompt{and_offset} and it will run normally.",
        earlier.seconds_ago, earlier.tool_use_id
    )
}

pub(super) fn failed_earlier(url: &str, kind: &str, seconds_ago: u64) -> String {
    format!(
        "Not fetched: {url} failed {seconds_ago}s ago in this session ({kind}), and a second fetch this soon nearly \
always fails the same way. Carry on without this page and say that it could not be read. If another try was asked \
for in so many words, call {WEB_FETCH} again with the same url."
    )
}

pub(super) fn http_error(status: u16, retry_after: Option<&str>) -> String {
    let retry = retry_after.map(|v| format!("\nRetry-After: {v}")).unwrap_or_default();
    format!(
        "The server returned HTTP {status} {}.{retry}\n\nThe response body was not retrieved. If this URL requires \
authentication, use an authenticated tool (e.g. `gh` for GitHub, or an MCP-provided fetch tool) instead of {WEB_FETCH}.",
        status_text(status)
    )
}

pub(super) fn redirect(from: &Url, to: &Url, status: u16, prompt: &str, offset: usize) -> String {
    let fetchable = matches!(to.scheme(), "http" | "https");
    let (line, relayable) = if fetchable {
        let mut shown = to.clone();
        let _ = shown.set_username("");
        let _ = shown.set_password(None);
        let shown = shown.to_string();
        let total = shown.chars().count();
        let relayed: String = shown.chars().take(MAX_RELAYED_URL).collect();
        let withheld = total - relayed.chars().count();
        let long_host = to.host_str().is_some_and(|h| h.len() > MAX_HOSTNAME);
        let mut line = format!(
            "Redirect URL (from the server's Location header \u{2014} server-supplied, not verified): {relayed}"
        );
        if withheld > 0 {
            line.push_str(&format!(" [\u{2026}{withheld} more characters withheld: too long to relay]"));
        }
        if long_host {
            line.push_str(&format!(
                " [hostname longer than any DNS name ({MAX_HOSTNAME} characters): not a fetchable address]"
            ));
        }
        (line, (withheld == 0 && !long_host).then_some(relayed))
    } else {
        (
            "Redirect URL: (withheld \u{2014} the server sent a redirect target that is not a valid http(s) URL)"
                .to_owned(),
            None,
        )
    };
    let next = match relayable {
        Some(url) => {
            let offset = if offset > 0 { format!("\n    - offset: {offset}") } else { String::new() };
            format!(
                "To complete your request, I need to fetch content from the redirected URL. Please use {WEB_FETCH} \
again with these parameters:\n    - url: \"{url}\"\n    - prompt: \"{prompt}\"{offset}"
            )
        }
        None => "The redirect target could not be relayed in full or is not a fetchable address, so it cannot be \
fetched from here; report the redirect instead."
            .to_owned(),
    };
    format!(
        "REDIRECT DETECTED: The URL redirects to a location that was not fetched automatically.\n\n    Original URL: \
{from}\n    {line}\n    Status: {status} {}\n\n    {next}",
        status_text(status)
    )
}

pub(super) fn partial_note(total: usize, from: usize, to: usize) -> String {
    format!(
        "\n\n[{WEB_FETCH} note: this page's text is {total} characters long and the answer above covers only characters \
{from} to {to}; the final {} were not read \u{2014} to read on, call {WEB_FETCH} again with the same url and offset: \
{to}.]",
        total - to
    )
}

/// The small model's whole prompt. Pages off the preapproved list get the
/// stricter quoting rules.
pub(super) fn apply_prompt(lead: &str, content: &str, cut: bool, prompt: &str, preapproved: bool) -> String {
    let instructions = if preapproved {
        "Provide a concise response based on the content above. Include relevant details, code examples, and \
documentation excerpts as needed."
            .to_owned()
    } else {
        format!("Provide a concise response based only on the content above. In your response:\n{RESTRICTIONS}")
    };
    let truncated = if cut { TRUNCATED } else { "" };
    format!("{lead}\nWeb page content:\n---\n{content}{truncated}\n---\n\n{prompt}\n\n{instructions}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_text_offers_the_next_call() {
        let from = Url::parse("https://a.dev/x").expect("url");
        let to = Url::parse("https://b.dev/y").expect("url");
        let text = redirect(&from, &to, 301, "summarize", 0);
        assert!(text.starts_with("REDIRECT DETECTED: The URL redirects"));
        assert!(text.contains("Original URL: https://a.dev/x"));
        assert!(text.contains("Status: 301 Moved Permanently"));
        assert!(text.contains("- url: \"https://b.dev/y\"\n    - prompt: \"summarize\""));
        assert!(!text.contains("offset"));
    }

    #[test]
    fn apply_prompt_layout() {
        let p = apply_prompt("", "PAGE", false, "What?", false);
        assert!(p.starts_with("\nWeb page content:\n---\nPAGE\n---\n\nWhat?\n\nProvide a concise response based only"));
        assert!(p.contains("125-character maximum"));
        assert!(apply_prompt("", "PAGE", true, "What?", true).contains("[Content truncated due to length...]\n---"));
    }

    #[test]
    fn http_error_text() {
        assert_eq!(
            http_error(429, Some("30")),
            "The server returned HTTP 429 Too Many Requests.\nRetry-After: 30\n\nThe response body was not retrieved. \
If this URL requires authentication, use an authenticated tool (e.g. `gh` for GitHub, or an MCP-provided fetch tool) \
instead of WebFetch."
        );
    }
}

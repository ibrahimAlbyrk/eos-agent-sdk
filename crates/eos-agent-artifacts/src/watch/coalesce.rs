//! How the auto-reply work reaches the session, as Claude Code delivers it:
//! each notice is a `<task-notification>` of type `artifact-auto-react`;
//! notices of one kind on one artifact (automatic replies, automatic edits)
//! are gathered (`Cai`): the burst settles 15 s after its latest notice, and
//! at most a minute after its first, into one notification that counts the
//! threads and keeps the newest 20 details.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Instant;

use super::Inner;

/// Details one gathered notification keeps (`O`).
const KEPT: usize = 20;

/// The kinds Claude Code gathers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Family {
    AutoReplied,
    AutoEdited,
}

impl Family {
    /// Claude Code's `RAn`.
    fn summary(self, name: &str, count: usize) -> String {
        match (self, count) {
            (Self::AutoReplied, 1) => format!("Auto-replied to comment thread on Artifact: {name}"),
            (Self::AutoReplied, n) => format!("Auto-replied to {n} comment threads on Artifact: {name}"),
            // Its `xAn`.
            (Self::AutoEdited, 1) => format!("Auto-edited Artifact: {name} in response to a comment thread"),
            (Self::AutoEdited, n) => format!("Auto-edited Artifact: {name} in response to {n} comment threads"),
        }
    }
}

/// Claude Code's `Yt`.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// One notification, as Claude Code's `Gi` writes it.
pub(super) fn notification(summary: &str, detail: &str) -> String {
    format!(
        "<task-notification>\n<task-type>artifact-auto-react</task-type>\n<summary>{}</summary>\n{}\n</task-notification>",
        escape(summary),
        escape(detail)
    )
}

#[derive(Debug)]
struct Burst {
    agent: Arc<str>,
    name: String,
    threads: HashSet<String>,
    first_at: Instant,
    details: VecDeque<String>,
    dropped: usize,
    generation: u64,
}

impl Burst {
    fn text(&self, family: Family) -> String {
        let mut body = self.details.iter().map(String::as_str).collect::<Vec<_>>().join("\n\n");
        if self.dropped > 0 {
            let noun = if self.dropped == 1 { "notice" } else { "notices" };
            body.push_str(&format!(
                "\n\n({} earlier {noun} in this burst omitted \u{2014} newest {KEPT} kept.)",
                self.dropped
            ));
        }
        notification(&family.summary(&self.name, self.threads.len().max(1)), &body)
    }
}

/// The bursts being gathered, by kind and artifact.
#[derive(Debug, Default)]
pub(super) struct Coalescer {
    bursts: Mutex<HashMap<(Family, String), Burst>>,
}

impl Inner {
    /// Adds a notice to its burst; the burst goes to `agent` once it settles.
    pub(super) fn coalesce(
        self: &Arc<Self>,
        slug: &str,
        agent: &Arc<str>,
        family: Family,
        name: String,
        thread: &str,
        detail: String,
    ) {
        let (settle, longest) = self.config.notice_settle;
        let now = Instant::now();
        let key = (family, slug.to_owned());
        let (wait, generation) = {
            let mut bursts = self.coalescer.bursts.lock().unwrap_or_else(PoisonError::into_inner);
            let burst = bursts.entry(key.clone()).or_insert_with(|| Burst {
                agent: Arc::clone(agent),
                name: name.clone(),
                threads: HashSet::new(),
                first_at: now,
                details: VecDeque::new(),
                dropped: 0,
                generation: 0,
            });
            burst.name = name;
            burst.agent = Arc::clone(agent);
            burst.threads.insert(thread.to_owned());
            burst.details.push_back(detail);
            if burst.details.len() > KEPT {
                burst.details.pop_front();
                burst.dropped += 1;
            }
            burst.generation += 1;
            let left = (burst.first_at + longest).saturating_duration_since(now);
            (settle.min(left), burst.generation)
        };
        let weak: Weak<Self> = Arc::downgrade(self);
        let slug = slug.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let Some(inner) = weak.upgrade() else { return };
            let ready = {
                let mut bursts = inner.coalescer.bursts.lock().unwrap_or_else(PoisonError::into_inner);
                match bursts.get(&key) {
                    Some(b) if b.generation == generation => bursts.remove(&key),
                    _ => None,
                }
            };
            if let Some(burst) = ready {
                let text = burst.text(family);
                inner.deliver(&slug, vec![(burst.agent, text, true)]).await;
            }
        });
    }
}

/// Claude Code's `A3e`: an artifact named by its title in quotes (quotes
/// inside made plain, an `@` that could mention someone made full-width),
/// else by its link.
pub(super) fn artifact_name(title: Option<&str>, url: &str) -> String {
    let title = title.map(|t| t.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|t| !t.is_empty());
    let Some(title) = title else { return url.to_owned() };
    let plain: String = title
        .chars()
        .map(|c| match c {
            '"' | '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{FF02}' => '\'',
            c => c,
        })
        .collect();
    let mut out = String::with_capacity(plain.len());
    let mut boundary = true;
    for c in plain.chars() {
        out.push(if c == '@' && boundary { '\u{FF20}' } else { c });
        boundary = c.is_whitespace() || matches!(c, '\u{3002}' | '\u{3001}' | '\u{FF1F}' | '\u{FF01}');
    }
    format!("\"{out}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_and_names_read_as_claude_codes() {
        assert_eq!(
            notification("Comments are waiting on Artifact: \"A<b>\"", "x & y"),
            "<task-notification>\n<task-type>artifact-auto-react</task-type>\n<summary>Comments are waiting on \
             Artifact: \"A&lt;b&gt;\"</summary>\nx &amp; y\n</task-notification>"
        );
        assert_eq!(artifact_name(Some("  Q3 \"plan\"  @ada x@y "), "u"), "\"Q3 'plan' \u{FF20}ada x@y\"");
        assert_eq!(artifact_name(None, "https://claude.ai/artifact/x"), "https://claude.ai/artifact/x");
        let mut burst = Burst {
            agent: "a".into(),
            name: "\"P\"".into(),
            threads: ["t1", "t2"].iter().map(|s| (*s).to_owned()).collect(),
            first_at: Instant::now(),
            details: (0..KEPT).map(|i| format!("d{i}")).collect(),
            dropped: 1,
            generation: 0,
        };
        let text = burst.text(Family::AutoReplied);
        assert!(text.contains("<summary>Auto-replied to 2 comment threads on Artifact: \"P\"</summary>\nd0\n\nd1"));
        assert!(text.ends_with(
            "d19\n\n(1 earlier notice in this burst omitted \u{2014} newest 20 kept.)\n</task-notification>"
        ));
        burst.threads.remove("t2");
        assert!(
            burst
                .text(Family::AutoReplied)
                .contains("<summary>Auto-replied to comment thread on Artifact: \"P\"</summary>")
        );
    }
}

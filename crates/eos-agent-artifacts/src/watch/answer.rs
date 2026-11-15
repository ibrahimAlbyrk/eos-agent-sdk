//! One thread with news, handled in Claude Code's order (`Bs`): the loop
//! breaker, the pause after repeated refusals and the hourly cap; plan mode
//! (nothing is taken up, said once); the permission check (a reply with no
//! text yet: notify-only when someone would have to approve it, said once
//! for an auto-mode refusal of the user's own comment); then the thread is handed to
//! the session (it publishes from a local source), or answered: on an
//! edit-capable thread with an optional fast acknowledgement and an
//! automatic edit, else by the reply-only composer, with the page's source
//! and the thread's anchor.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};

use super::coalesce::Family;
use super::edit::{self, Decision, NotComposed, Unapplied};
use super::frame::{self, Framed, Snippet};
use super::react::{Admit, Take};
use super::replies::{self, Fresh, Reaction, Summary};
use super::source::{self, PageSource};
use super::status::{Status, Summon};
use super::{ComposeRequest, Composed, Inner, Probe, ack, anchor, notices};
use crate::client::{Deployed, PublishBody, SourceFile, error_text};

/// What a composer produced, as the flow needs it.
pub(super) enum Reply {
    Text(String),
    /// Nothing usable: `failed`/`compose_failed` (or `declined`/`usage_limited`).
    Failed {
        usage_limited: bool,
    },
    /// No model to ask.
    NoModel,
}

/// What the edit composer decided, as the flow needs it.
enum Drafted {
    Reply(String),
    Edit(Draft),
    /// No edit; the reply saying why is posted.
    NotComposed(NotComposed),
    /// No edit could be offered: the reply-only composer answers.
    Unavailable,
    UsageLimited,
    Failed,
    NoModel,
}

/// An automatic edit ready to publish.
struct Draft {
    content: String,
    reply: String,
    /// The version it was made from.
    ver: String,
    title: Option<String>,
    favicon: Option<String>,
}

/// How the next post goes: the first one answers the comments sent to
/// Claude; after the acknowledgement, one follows it.
#[derive(Debug, Clone, Default)]
pub(super) struct PostAs {
    pub(super) answers_summon: bool,
    pub(super) continues: Option<String>,
}

/// How a post went.
pub(super) enum Posted {
    Yes(Option<String>),
    /// The gate refused it.
    Refused,
    Failed,
}

impl Inner {
    fn status(&self, summon: &Summon, status: Status) {
        self.status.send(&self.client_for(&summon.slug), summon, status);
    }

    /// Claude Code's `Ce`: the session went into plan mode while the thread
    /// was being worked on — it stops there (`declined`/`plan_mode`).
    fn plan_paused(&self, agent: &str, summon: &Summon) -> bool {
        let plan = self.notifier.plan_mode(agent);
        if plan {
            self.status(summon, Status::Declined("plan_mode"));
        }
        plan
    }

    pub(super) async fn answer(&self, slug: &str, url: &str, agent: &Arc<str>, fresh: &Fresh) -> Vec<Reaction> {
        let thread = &fresh.thread;
        let activation = fresh.summons.is_empty();
        let mut summon = Summon {
            slug: slug.to_owned(),
            thread: thread.id.clone(),
            gesture_at: fresh.gesture_at.clone(),
            covers_reply_id: None,
        };
        let source = self.source_of(slug);
        let plan = self.notifier.plan_mode(agent);
        let admit = self.reacts.with(slug, |r| {
            let take = Take { fresh_human: fresh.fresh_human, own: fresh.own, handed_over: source.is_some(), plan };
            r.admit(&thread.id, take, self.config.auto_turns_per_hour, Instant::now())
        });
        let looping = match admit {
            Admit::Go { looping } => looping,
            Admit::Breaker => {
                self.status(&summon, Status::Declined("loop_guard"));
                return Vec::new();
            }
            Admit::Denied { notice } => {
                self.status(&summon, Status::Declined("blocked"));
                let text = notices::pipeline_paused(url);
                return if notice { vec![Reaction::Note(Summary::Paused, text)] } else { Vec::new() };
            }
            Admit::Cap { notice } => {
                self.status(&summon, Status::Declined("hourly_cap"));
                let text = notices::capped(url, activation);
                return if notice { vec![Reaction::Note(Summary::Waiting, text)] } else { Vec::new() };
            }
        };
        // Claude Code: in plan mode nothing is taken up; the session hears it once.
        if plan {
            let notice = self.reacts.with(slug, |r| {
                r.last_probe_allowed = false;
                !std::mem::replace(&mut r.plan_noticed, true)
            });
            self.status(&summon, Status::Declined("plan_mode"));
            let text = notices::plan_mode(url, activation);
            return if notice { vec![Reaction::Note(Summary::Waiting, text)] } else { Vec::new() };
        }
        let probe = json!({ "action": "reply", "url": url, "thread_id": thread.id, "text": "" });
        let verdict = self.notifier.probe(agent, "ArtifactComments", &probe).await;
        self.reacts.with(slug, |r| r.last_probe_allowed = verdict == Probe::Allow);
        if self.plan_paused(agent, &summon) {
            return Vec::new();
        }
        match verdict {
            Probe::Allow => {}
            Probe::Ask => {
                self.status(&summon, Status::Declined("ask_mode"));
                if self.mark_notified(slug) {
                    return vec![Reaction::Note(Summary::Waiting, notices::notify_only(url, activation))];
                }
                return Vec::new();
            }
            Probe::Deny => {
                self.status(&summon, Status::Declined("denied_by_rule"));
                return Vec::new();
            }
            Probe::DeniedByAutoMode => {
                // Said once, and only for what the user sent themselves (`ka`).
                let notice =
                    fresh.own && self.reacts.with(slug, |r| !std::mem::replace(&mut r.auto_mode_noticed, true));
                self.status(&summon, Status::Declined("denied_by_auto_mode"));
                let text = notices::auto_mode_declined(url, activation);
                return if notice { vec![Reaction::Note(Summary::Waiting, text)] } else { Vec::new() };
            }
        }
        self.status(&summon, Status::Working);
        self.reacts.with(slug, |r| r.working(&thread.id, looping));
        if let Some(source) = source {
            self.reacts.with(slug, |r| r.answered(&thread.id, Instant::now(), false));
            let text = notices::handed_over(
                url,
                &thread.id,
                fresh.summons.len(),
                &source.display().to_string(),
                thread.resolved,
            );
            return vec![Reaction::Note(Summary::Waiting, text)];
        }

        let mut out = Vec::new();
        let mut post_as = PostAs { answers_summon: !activation, continues: None };
        let edits = thread.edit_capable && !thread.resolved;
        // Claude Code's fast acknowledgement: summoned on an edit-capable thread.
        let mut ack_id = None;
        if !activation
            && edits
            && ack::enabled()
            && let Some(text) = self.fast_ack(agent, fresh).await
        {
            if self.plan_paused(agent, &summon) {
                return out;
            }
            match self.post(slug, url, agent, fresh, &text, &post_as).await {
                Posted::Yes(id) => {
                    self.reacts.with(slug, |r| r.answered(&thread.id, Instant::now(), false));
                    post_as = PostAs { answers_summon: false, continues: id.clone() };
                    if self.notifier.plan_mode(agent) {
                        let text = notices::acknowledged_in_plan_mode(url, &thread.id);
                        out.push(Reaction::Gathered(Family::AutoReplied, text));
                        return out;
                    }
                    out.push(Reaction::Gathered(Family::AutoReplied, notices::acknowledged(url, &thread.id)));
                    if id.is_some() {
                        summon.covers_reply_id = id.clone();
                        self.status(&summon, Status::Working);
                    }
                    ack_id = id;
                }
                _ if self.plan_paused(agent, &summon) => return out,
                _ => {}
            }
        }
        let ack = ack_id.as_deref();

        let reply = if edits {
            let page =
                tokio::time::timeout(source::READ_TIMEOUT, source::read(&self.client_for(slug), slug, None)).await;
            let page = page.ok().flatten();
            if self.plan_paused(agent, &summon) {
                return out;
            }
            let drafted = match (&page, self.notifier.output_cap(agent)) {
                (Some(page), Some(cap)) if editable(page, cap) => {
                    let draft = self.compose_edit(slug, agent, fresh, page, cap);
                    let drafted = tokio::time::timeout(replies::COMPOSE_DEADLINE, draft)
                        .await
                        .unwrap_or(Drafted::NotComposed(NotComposed::TimedOut));
                    if self.plan_paused(agent, &summon) {
                        return out;
                    }
                    drafted
                }
                _ => Drafted::Unavailable,
            };
            match drafted {
                Drafted::Reply(text) if !replies::hidden(&text) => Reply::Text(text),
                Drafted::Reply(_) | Drafted::Failed => Reply::Failed { usage_limited: false },
                Drafted::UsageLimited => Reply::Failed { usage_limited: true },
                Drafted::NoModel => Reply::NoModel,
                Drafted::Unavailable => self.compose_reply(slug, agent, fresh, page).await,
                Drafted::NotComposed(why) => {
                    let posted = self.post(slug, url, agent, fresh, why.reply(), &post_as).await;
                    out.extend(self.after_post(slug, &summon, &thread.id, posted, false, |id| {
                        let text = notices::edit_not_composed(url, &thread.id, why.told(), &[ack, id]);
                        Reaction::Note(Summary::AutoReplied, text)
                    }));
                    return out;
                }
                Drafted::Edit(draft) => {
                    let trigger = fresh.summons.last().cloned().unwrap_or_default();
                    let flow = Flow { slug, url, agent, fresh, summon: &summon, post_as: &post_as, ack };
                    out.extend(self.edit_and_reply(&flow, draft, &trigger).await);
                    return out;
                }
            }
        } else {
            self.compose_reply(slug, agent, fresh, None).await
        };
        let reply = match reply {
            Reply::Text(text) => text,
            Reply::NoModel => {
                out.push(Reaction::Unanswered);
                return out;
            }
            Reply::Failed { usage_limited } => {
                let status =
                    if usage_limited { Status::Declined("usage_limited") } else { Status::Failed("compose_failed") };
                self.status(&summon, status);
                return out;
            }
        };
        let posted = self.post(slug, url, agent, fresh, &reply, &post_as).await;
        out.extend(self.after_post(slug, &summon, &thread.id, posted, true, |id| {
            Reaction::Gathered(Family::AutoReplied, notices::auto_replied(url, &thread.id, &[ack, id], thread.resolved))
        }));
        out
    }

    /// Claude Code's `Un`: a post that went up is the thread's latest answer
    /// (`reset`: a success anywhere resumes paused threads) and the session
    /// is told; one that did not is said on the page.
    fn after_post(
        &self,
        slug: &str,
        summon: &Summon,
        thread: &str,
        posted: Posted,
        reset: bool,
        told: impl FnOnce(Option<&str>) -> Reaction,
    ) -> Option<Reaction> {
        match posted {
            Posted::Yes(id) => {
                self.reacts.with(slug, |r| r.answered(thread, Instant::now(), reset));
                Some(told(id.as_deref()))
            }
            Posted::Refused => {
                self.status(summon, Status::Declined("blocked"));
                None
            }
            Posted::Failed => {
                self.status(summon, Status::Failed("post_failed"));
                None
            }
        }
    }

    /// Claude Code's `Ks`/`Vs`: the acknowledgement to post, from the small
    /// fast model (`None`: no model to ask).
    async fn fast_ack(&self, agent: &str, fresh: &Fresh) -> Option<String> {
        let mut bare = fresh.clone();
        bare.thread.anchor.path = None;
        let framed = self.frame(&bare, None);
        if ack::fixed() {
            let request = ack::pick_request(&framed.text, fresh.trigger, true);
            let picked = tokio::time::timeout(ack::PICK_DEADLINE, self.notifier.compose(agent, &request)).await;
            return Some(ack::picked(picked.ok().flatten(), true));
        }
        let written = self.notifier.compose(agent, &ack::write_request(&framed.text)).await?;
        Some(ack::written(Some(written)))
    }

    /// Claude Code's `ld`: the edit composer's decision on the page's
    /// source, asked again once for a malformed answer (or a rewrite it may
    /// not make) and once for a patch that did not apply.
    async fn compose_edit(&self, slug: &str, agent: &str, fresh: &Fresh, page: &PageSource, cap: u32) -> Drafted {
        let rewrite = self
            .reacts
            .with(slug, |r| !matches!(&r.rewrite_unavailable, Some((ver, c)) if *ver == page.ver && cap <= *c));
        let framed = self.frame(fresh, Some(page));
        let nonce = self.reacts.with(slug, |r| r.fence_for(&page.ver, &[&framed.text, &page.html]));
        let source = edit::source_block(&page.html, &nonce);
        let base = edit::prompt(&framed.text, rewrite);
        let (mut extra, mut reformatted, mut retried) = (String::new(), false, false);
        loop {
            let request = ComposeRequest {
                system: edit::EDIT_SYSTEM.to_owned(),
                blocks: vec![source.clone(), format!("{base}{extra}")],
                cache_first: true,
                max_tokens: cap,
                effort: Some(("medium", cap)),
                small_model: false,
            };
            let text = match self.notifier.compose(agent, &request).await {
                None => return Drafted::NoModel,
                Some(Composed::UsageLimited) => return Drafted::UsageLimited,
                Some(Composed::Failed) => return Drafted::Failed,
                Some(Composed::Truncated) => {
                    if rewrite {
                        self.reacts.with(slug, |r| r.rewrite_unavailable = Some((page.ver.clone(), cap)));
                    }
                    return Drafted::Unavailable;
                }
                Some(Composed::Text(text)) => text.trim().to_owned(),
            };
            if text.is_empty() {
                return Drafted::Failed;
            }
            let Some(decision) = edit::decision(&text) else {
                if reformatted {
                    return Drafted::Failed;
                }
                reformatted = true;
                let Some(previous) = edit::unfenced(&text, &nonce, &framed.mark) else { return Drafted::Failed };
                extra = edit::reformat(&previous, &frame::nonce());
                continue;
            };
            let (content, reply) = match decision {
                Decision::Reply(text) if text.contains(&nonce) => return Drafted::Failed,
                Decision::Reply(text) => return Drafted::Reply(text),
                Decision::Patch { reply, .. } | Decision::Rewrite { reply, .. } if replies::hidden(&reply) => {
                    return Drafted::Failed;
                }
                Decision::Patch { edits, reply } => match edit::apply(&page.html, &edits) {
                    Ok(content) if edit::in_bounds(&content) => (content, reply),
                    Ok(_) => return Drafted::NotComposed(NotComposed::Unapplied),
                    Err(Unapplied::Noop) => return Drafted::NotComposed(NotComposed::Noop),
                    Err(miss @ (Unapplied::NotFound(op) | Unapplied::Ambiguous(op))) if !retried => {
                        retried = true;
                        let Some(find) = edit::unfenced(&edits[op].0, &nonce, &framed.mark) else {
                            return Drafted::Failed;
                        };
                        extra = edit::retry_patch(&find, matches!(miss, Unapplied::Ambiguous(_)), &frame::nonce());
                        continue;
                    }
                    Err(_) => return Drafted::NotComposed(NotComposed::Unapplied),
                },
                Decision::Rewrite { .. } if !rewrite => {
                    if reformatted {
                        return Drafted::NotComposed(NotComposed::RewriteWithheld);
                    }
                    reformatted = true;
                    extra = edit::REWRITE_WITHHELD.to_owned();
                    continue;
                }
                Decision::Rewrite { content, reply } => (content, reply),
            };
            if content.contains(&nonce) || reply.contains(&nonce) {
                return Drafted::Failed;
            }
            return Drafted::Edit(Draft {
                content,
                reply,
                ver: page.ver.clone(),
                title: page.title.clone(),
                favicon: page.favicon.clone(),
            });
        }
    }

    /// Claude Code's `pd`: publishes the edit as the thread's edit grant,
    /// posts its summary reply and resolves the thread at the new version;
    /// a refused publish is answered with Claude Code's reason.
    async fn edit_and_reply(&self, f: &Flow<'_>, draft: Draft, trigger: &str) -> Vec<Reaction> {
        let (slug, url, thread) = (f.slug, f.url, &f.fresh.thread);
        let version = match self.publish_edit(slug, url, f.agent, &thread.id, trigger, &draft).await {
            Ok(version) => version,
            Err((kind, denied)) => {
                if denied || matches!(kind, "pin_paused" | "disabled") {
                    self.reacts.with(slug, |r| r.denied(&thread.id));
                }
                if kind == "summon_foreign" {
                    // The grant is another user's: a reply only, composed as usual.
                    let text = match self.compose_reply(slug, f.agent, f.fresh, None).await {
                        Reply::Text(text) => text,
                        Reply::NoModel => return vec![Reaction::Unanswered],
                        Reply::Failed { usage_limited } => {
                            let status = if usage_limited {
                                Status::Declined("usage_limited")
                            } else {
                                Status::Failed("compose_failed")
                            };
                            self.status(f.summon, status);
                            return Vec::new();
                        }
                    };
                    let posted = self.post(slug, url, f.agent, f.fresh, &text, f.post_as).await;
                    let told = |id: Option<&str>| {
                        Reaction::Gathered(Family::AutoReplied, notices::edit_foreign(url, &thread.id, &[f.ack, id]))
                    };
                    return self.after_post(slug, f.summon, &thread.id, posted, false, told).into_iter().collect();
                }
                let posted = self.post(slug, url, f.agent, f.fresh, edit::refusal_reply(kind), f.post_as).await;
                let told = |id: Option<&str>| {
                    Reaction::Note(Summary::AutoReplied, notices::edit_refused(url, &thread.id, &[f.ack, id]))
                };
                return self.after_post(slug, f.summon, &thread.id, posted, false, told).into_iter().collect();
            }
        };
        let posted = self.post(slug, url, f.agent, f.fresh, &draft.reply, f.post_as).await;
        let Posted::Yes(id) = posted else {
            self.after_post(slug, f.summon, &thread.id, posted, true, |_| Reaction::Quiet);
            return vec![Reaction::Note(Summary::EditReview, notices::edit_reply_failed(url, &thread.id, f.ack))];
        };
        self.reacts.with(slug, |r| r.answered(&thread.id, Instant::now(), true));
        let ids = [f.ack, id.as_deref()];
        let resolved = self.client_for(slug).comment_resolve(slug, &thread.id, &version).await.is_ok();
        vec![if resolved {
            Reaction::Gathered(Family::AutoEdited, notices::auto_edited(url, &thread.id, &ids))
        } else {
            Reaction::Note(Summary::AutoEdited, notices::auto_edited_unresolved(url, &thread.id, &ids))
        }]
    }

    /// The edit, published as an `Artifact` publish the gate sees, carrying
    /// Claude Code's `auto_edit_attribution` and the version it was made
    /// from. The new version, or why not (Claude Code's failure kind, and
    /// whether the gate refused it).
    async fn publish_edit(
        &self,
        slug: &str,
        url: &str,
        agent: &str,
        thread: &str,
        trigger: &str,
        draft: &Draft,
    ) -> Result<String, (&'static str, bool)> {
        let dir = std::env::temp_dir().join(format!("autoedit-{:016x}", fastrand::u64(..)));
        let file = dir.join("artifact.html");
        let mut input = json!({ "file_path": file.display().to_string(), "url": url });
        if let Some(favicon) = &draft.favicon {
            input["favicon"] = json!(favicon);
        }
        if let Some(title) = &draft.title {
            input["title"] = json!(title);
        }
        if !self.notifier.may_call(agent, "Artifact", &input).await {
            return Err(("other", true));
        }
        let published = async {
            tokio::fs::create_dir_all(&dir).await.map_err(|_| "transient")?;
            tokio::fs::write(&file, &draft.content).await.map_err(|_| "transient")?;
            let page = SourceFile::scan(&file, "text/html", true).await.map_err(|_| "transient")?;
            let title =
                draft.title.clone().or_else(|| edit::page_title(&draft.content)).unwrap_or_else(|| "artifact".into());
            let mut meta = serde_json::Map::new();
            meta.insert("slug".into(), json!(slug));
            meta.insert("title".into(), json!(title));
            if let Some(favicon) = &draft.favicon {
                meta.insert("favicon".into(), json!(favicon));
            }
            meta.insert("baseVersion".into(), json!(draft.ver));
            meta.insert("auto_edit_attribution".into(), json!({ "thread_id": thread, "comment_id": trigger }));
            let body = PublishBody { meta, content: Some(page), files: Vec::new() };
            let (status, answer) = self.client_for(slug).deploy(body).await.map_err(|_| "transient")?;
            let why = || error_text(answer.to_string().as_bytes());
            match status {
                200..=299 => {
                    serde_json::from_value::<Deployed>(answer.clone()).map(|d| d.version).map_err(|_| "outcome_unknown")
                }
                409 => Err("superseded"),
                429 => Err("publish_capped"),
                403 => Err(edit::refusal_kind(&format!("publish denied: {}", why()))),
                _ => Err(edit::refusal_kind(&why())),
            }
        }
        .await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let version = published.map_err(|kind| (kind, false))?;
        let title = draft.title.clone().or_else(|| edit::page_title(&draft.content));
        self.own_version(slug, agent, &version, title.as_deref());
        Ok(version)
    }

    /// Claude Code's `qn`: reads the page (the thread's own page of a
    /// multi-file artifact; `page` is the main one when already read),
    /// frames the thread with its anchor, and asks the agent's model for the
    /// reply.
    pub(super) async fn compose_reply(
        &self,
        slug: &str,
        agent: &str,
        fresh: &Fresh,
        page: Option<PageSource>,
    ) -> Reply {
        let thread = &fresh.thread;
        let page = match page.filter(|_| thread.anchor.file.is_none()) {
            Some(page) => Some(page),
            None if thread.anchor.file_degraded => None,
            None => {
                let client = self.client_for(slug);
                let read = source::read(&client, slug, thread.anchor.file.as_deref());
                tokio::time::timeout(source::READ_TIMEOUT, read).await.ok().flatten()
            }
        };
        let framed = self.frame(fresh, page.as_ref());
        let mut blocks = Vec::new();
        let mut nonce = None;
        if let Some(page) = &page {
            let n = self.reacts.with(slug, |r| r.fence_for(&page.ver, &[&framed.text, &page.html]));
            blocks.push(replies::source_block(&page.html, &n));
            nonce = Some(n);
        }
        blocks.push(replies::prompt(&framed, page.is_some(), thread.resolved));
        let request = ComposeRequest {
            system: replies::REPLY_SYSTEM.to_owned(),
            blocks,
            cache_first: page.is_some(),
            max_tokens: replies::REPLY_MAX_TOKENS,
            effort: Some(("medium", replies::REPLY_EFFORT_TOKENS)),
            small_model: false,
        };
        let composed = tokio::time::timeout(replies::COMPOSE_DEADLINE, self.notifier.compose(agent, &request)).await;
        let Ok(composed) = composed else { return Reply::Failed { usage_limited: false } };
        let Some(composed) = composed else { return Reply::NoModel };
        match composed {
            Composed::Text(text) => {
                let text = text.trim();
                let echoed = nonce.as_deref().is_some_and(|n| text.contains(n));
                if text.is_empty() || echoed || replies::hidden(text) {
                    Reply::Failed { usage_limited: false }
                } else {
                    Reply::Text(replies::cut_bytes(text, replies::MAX_REPLY_BYTES))
                }
            }
            Composed::UsageLimited => Reply::Failed { usage_limited: true },
            Composed::Truncated | Composed::Failed => Reply::Failed { usage_limited: false },
        }
    }

    /// Claude Code's `an` for this thread: the page (when read) gives the
    /// anchored element's snippet and tells whether others wrote it.
    pub(super) fn frame(&self, fresh: &Fresh, page: Option<&PageSource>) -> Framed {
        let thread = &fresh.thread;
        let a = &thread.anchor;
        let snippet = match (&a.path, page) {
            (Some(path), Some(page)) if !a.path_unreadable && !a.carried => {
                anchor::snippet(&page.html, path, &a.region_kids)
                    .map(|(element, inside)| Snippet { element: Some(element), inside })
            }
            _ => None,
        };
        let viewer = fresh.viewer.as_deref();
        let others = thread.rows.iter().any(|r| viewer.is_none_or(|v| r.account != v));
        let summons: HashSet<String> = fresh.summons.iter().cloned().collect();
        frame::frame(&frame::Thread {
            rows: &thread.rows,
            anchor: a,
            trigger: fresh.trigger,
            summons: &summons,
            viewer,
            foreign: page.is_some_and(PageSource::foreign) || others,
            snippet: snippet.as_ref(),
        })
    }

    /// Posts `text` on the thread as the comments tool's `reply` (the gate
    /// sees it); a refused post counts toward the pause.
    pub(super) async fn post(
        &self,
        slug: &str,
        url: &str,
        agent: &str,
        fresh: &Fresh,
        text: &str,
        post_as: &PostAs,
    ) -> Posted {
        let thread = &fresh.thread;
        let input = json!({ "action": "reply", "url": url, "thread_id": thread.id, "text": text });
        if !self.notifier.may_call(agent, "ArtifactComments", &input).await {
            self.reacts.with(slug, |r| r.denied(&thread.id));
            return Posted::Refused;
        }
        let continues = post_as.continues.as_deref();
        match self.client_for(slug).comment_post(slug, &thread.id, text, post_as.answers_summon, continues).await {
            Ok(answer) => Posted::Yes(comment_id(&answer)),
            Err(_) => Posted::Failed,
        }
    }
}

/// What the edit flow carries from one step to the next.
struct Flow<'a> {
    slug: &'a str,
    url: &'a str,
    agent: &'a str,
    fresh: &'a Fresh,
    summon: &'a Summon,
    post_as: &'a PostAs,
    /// The acknowledgement posted first.
    ack: Option<&'a str>,
}

/// Claude Code's `dd`: the page may be edited automatically — the reader's
/// view is the live version, not a type's page, and small enough for the
/// model to write back.
fn editable(page: &PageSource, output_cap: u32) -> bool {
    page.edits_live() && !page.typed && page.html.len() <= edit::max_page_bytes(output_cap)
}

/// The posted comment's id, as the reply's answer echoes it (`comment_id`).
pub(super) fn comment_id(answer: &Value) -> Option<String> {
    let id = answer.get("comment_id").or_else(|| answer.get("id")).and_then(Value::as_str)?;
    super::thread::valid_id(id).then(|| id.to_owned())
}

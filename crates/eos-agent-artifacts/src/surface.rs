//! Opening an artifact for the person: the host's view does it, through an
//! [`ArtifactOpener`]. When it happens:
//!
//! - a top-level agent's publish that creates an artifact opens it; a
//!   republish never does;
//! - creating from an Artifact type opens the new artifact at once, or, with
//!   `auto_open: "after_first_write"`, on the first publish or `ArtifactData`
//!   write to it (by any agent of the process) — so the person does not first
//!   see it empty;
//! - `action: "open"` opens it when a top-level agent asks.
//!
//! Subagents never open an artifact on their own. Without an opener nothing
//! is opened, and `open` says so.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use eos_agent_core::{AgentOrigin, BoxFuture, ToolContext};

/// At most this many created artifacts wait for their first write.
const DEFERRED: usize = 256;

/// Why an artifact is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenReason {
    /// A publish created it (or a create from a type, opened at once).
    Created,
    /// The first write to an artifact created with `auto_open: "after_first_write"`.
    FirstWrite,
    /// The agent called `action: "open"`.
    Asked,
}

/// One artifact to show the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    /// The agent whose call opens it.
    pub agent_id: String,
    pub slug: String,
    /// Its claude.ai link.
    pub url: String,
    pub title: Option<String>,
    pub reason: OpenReason,
}

/// The host's view of artifacts (a side panel, a browser tab).
pub trait ArtifactOpener: Send + Sync {
    /// Shows `request`'s artifact to the person; false when it could not.
    fn open<'a>(&'a self, request: &'a OpenRequest) -> BoxFuture<'a, bool>;
}

/// The opener and the creations waiting for their first write, shared by
/// the Artifact and ArtifactData tools.
#[derive(Default)]
pub(crate) struct Surface {
    opener: Option<Arc<dyn ArtifactOpener>>,
    deferred: Mutex<VecDeque<String>>,
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Surface").field("opener", &self.opener.is_some()).finish_non_exhaustive()
    }
}

/// The call comes from a top-level agent (no other agent started it).
pub(crate) fn top_level(ctx: &ToolContext<'_>) -> bool {
    ctx.extensions.get::<AgentOrigin>().is_none()
}

impl Surface {
    pub(crate) fn new(opener: Option<Arc<dyn ArtifactOpener>>) -> Self {
        Self { opener, deferred: Mutex::default() }
    }

    pub(crate) fn can_open(&self) -> bool {
        self.opener.is_some()
    }

    /// Opens `slug` now; false without an opener or when it failed.
    pub(crate) async fn open(
        &self,
        ctx: &ToolContext<'_>,
        slug: &str,
        title: Option<&str>,
        reason: OpenReason,
    ) -> bool {
        let Some(opener) = &self.opener else { return false };
        let request = OpenRequest {
            agent_id: ctx.agent_id.to_owned(),
            slug: slug.to_owned(),
            url: crate::ids::viewer_url(slug),
            title: title.map(str::to_owned),
            reason,
        };
        opener.open(&request).await
    }

    /// `slug` opens on its first write instead of now.
    pub(crate) fn defer(&self, slug: &str) {
        let mut deferred = self.deferred.lock().unwrap_or_else(PoisonError::into_inner);
        deferred.retain(|s| s != slug);
        deferred.push_back(slug.to_owned());
        while deferred.len() > DEFERRED {
            deferred.pop_front();
        }
    }

    /// Whether `slug` was waiting for its first write (it no longer is).
    pub(crate) fn take_deferred(&self, slug: &str) -> bool {
        let mut deferred = self.deferred.lock().unwrap_or_else(PoisonError::into_inner);
        let before = deferred.len();
        deferred.retain(|s| s != slug);
        deferred.len() != before
    }

    /// After a successful write to `slug`: opens it when it was waiting for
    /// its first write. Returns the sentence the result carries.
    pub(crate) async fn written(&self, ctx: &ToolContext<'_>, slug: &str, title: Option<&str>) -> Option<&'static str> {
        if !self.take_deferred(slug) {
            return None;
        }
        Some(if self.open(ctx, slug, title, OpenReason::FirstWrite).await {
            " It is now open for the person."
        } else {
            " Opening it for the person failed; give them the link."
        })
    }
}

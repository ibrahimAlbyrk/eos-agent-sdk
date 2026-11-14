//! Claude Code's Artifact tools (`Artifact`, `ArtifactComments`,
//! `ArtifactData`) over claude.ai's frame API.
//!
//! The tools share one [`FrameClient`], which takes the Claude sign-in's
//! OAuth bearer from an injected [`TokenSource`]. Local paths in tool input
//! resolve through the calling agent's [`Workspace`](eos_agent_core::Workspace)
//! and never leave its roots; uploads stream from disk.
//!
//! # Shapes confirmed against the real API (2026-11-08)
//!
//! All under `https://api.anthropic.com/api/frame/` with the bearer,
//! `anthropic-beta: oauth-2025-04-20`, `x-frame-cp: go`, `x-frame-surface: code`.
//! Slugs are uuids; `claude.ai/artifact/<id>` is the uuid in base58.
//!
//! - `GET frames?limit=N[&rel=shared]` → `{frames: [{slug, title, description,
//!   icon, live, rel, updatedAt, created_at, …}], thumbsEnabled, starsEnabled}`.
//! - `GET <slug>?via=model_read` (boot) → `{ver, assetToken, subscriptionToken,
//!   subscriptionTokenExp, watchTokenRenewEnabled, files: [{path, contentType,
//!   size?, sha256?, src?: {slug, ver}}], title, perm: {role}, typeInstance,
//!   typeSlug?, seq, history, …}`; `contentVia: "frame"` only when content goes
//!   through the API.
//! - Content is not on the API: `GET https://<slug>.frame.claudeusercontent.com
//!   /_f/<ver>/_src/<path|index.html>?__frame_t=<assetToken>` is the stored source
//!   (the page as published), `/_f/<ver>/<path>` the served file (the page with
//!   the runtime added; assets at `_blob/<id>`). No bearer goes there.
//!   (`content/<slug>/…` answers a bare 404 unless the boot says `contentVia`.)
//! - `GET read/<slug>` → `{owned, public, owner_account_id, contract}`.
//! - `POST deploy/direct`: one page as `content`; several files as
//!   `manifest: {path: {content, contentType} | {sha256, contentType} | null}`
//!   with `mode: "replace"` (a new artifact, the whole set) or `"patch"` (with
//!   `slug` + `baseVersion`, or no page); `files` is refused (`bad json: unknown
//!   field "files"`). → `{slug, version, seq}`. A stale `baseVersion` → 409
//!   `{conflict: true, live, author, createdAt, liveEntry}`.
//! - `capabilities` needs `contract` (else 400 `capabilities requires
//!   contract`): `GET contract/latest` → `{version, capabilities: [name], core,
//!   features}`; `GET contract/<ver>/prompt` is the runtime guide (text).
//! - Assets: `POST blob/<slug>/agent-upload` (raw body) → `{id, url: "/_blob/<id>"}`;
//!   `POST blob/<slug>/agent-list` → `{assets: [{opaque_id, url, content_type,
//!   size_bytes, …}], usage}`; `POST blob/<slug>/<id>/agent-delete`.
//! - `POST db/agent` `{slug, verb, args}` → get `{exists, data, version,
//!   updatedAt}`, list/query `{docs: [{id, data, version, updatedAt}],
//!   nextCursor}`, writes `{version}` (batch `{results: [{version}]}`); a stale
//!   `if_version` is refused.
//! - `GET comments/<slug>?names=1` → `{threads: [{id, comments: [{id,
//!   author: {account, role}, text, created_at, to_claude_at}], resolved_at,
//!   claude_activated_at, claude_capability, anchor}], viewer, names: {account:
//!   name}}` (thread fields as Claude Code reads them; new threads need a
//!   claude.ai web session: `this comment action requires a claude.ai session`).
//! - Watching: the boot's `subscriptionToken` (about an hour) opens
//!   `wss://claude.ai/edge-api/frame-live/<slug>/ws` with the subprotocols
//!   `frame-live.v1` and the token; text frames are `{kind, ver?}`: a `ver`
//!   that went live, or `kind: "comment"` (read the threads) — see [`ArtifactWatcher`].
//! - `POST types/<type slug>/create` `{title, target_type_version: null}` →
//!   the new artifact; its boot lists the type's files with `src.slug` = the type.

mod artifact;
mod capabilities;
mod client;
mod comments;
mod data;
mod files;
mod ids;
mod pins;
mod state;
mod surface;
mod targets;
mod watch;

use std::path::PathBuf;
use std::sync::Arc;

use eos_agent_core::{TokenSource, Tool};

pub use artifact::ArtifactTool;
pub use capabilities::CapabilitiesGuide;
pub use client::{DEFAULT_BASE_URL, DEFAULT_CONTENT_HOST, FrameClient, FrameConfig, FrameError};
pub use comments::ArtifactCommentsTool;
pub use data::ArtifactDataTool;
pub use ids::{short_id, slug_from_url, viewer_url};
pub use surface::{ArtifactOpener, OpenReason, OpenRequest};
pub use targets::ArtifactSession;
pub use watch::{
    Armed, ArtifactWatcher, ComposeRequest, Composed, Probe, REPLY_MAX_TOKENS, REPLY_SYSTEM, WatchConfig,
    WatchNotifier, WatchRow,
};

/// The three tools, sharing one client (one connection pool).
pub fn tools(auth: Arc<dyn TokenSource>, config: FrameConfig) -> Result<Vec<Arc<dyn Tool>>, FrameError> {
    tools_opening(auth, config, None)
}

/// The three tools, opening artifacts for the person through `opener` (see
/// [`ArtifactOpener`] for when).
pub fn tools_opening(
    auth: Arc<dyn TokenSource>,
    config: FrameConfig,
    opener: Option<Arc<dyn ArtifactOpener>>,
) -> Result<Vec<Arc<dyn Tool>>, FrameError> {
    let client = Arc::new(FrameClient::new(auth, config)?);
    let surface = Arc::new(surface::Surface::new(opener));
    Ok(vec![
        Arc::new(ArtifactTool::new(Arc::clone(&client)).with_surface(Arc::clone(&surface))),
        Arc::new(ArtifactCommentsTool::new(Arc::clone(&client))),
        Arc::new(ArtifactDataTool::new(client).with_surface(surface)),
    ])
}

/// The three tools with artifact watching: publishes (and `ArtifactComments`
/// `watch`) follow artifacts through one [`ArtifactWatcher`] that tells
/// agents, through `notifier`, of versions published elsewhere and of
/// comments sent to Claude. The watcher is returned for the host to end an
/// agent's watches when the agent ends.
pub fn tools_watched(
    auth: Arc<dyn TokenSource>,
    config: FrameConfig,
    notifier: Arc<dyn WatchNotifier>,
    watch: WatchConfig,
) -> Result<(Vec<Arc<dyn Tool>>, ArtifactWatcher), FrameError> {
    let kit = kit(auth, config, notifier, watch, std::env::temp_dir().join("artifact-capabilities"), None)?;
    Ok((kit.tools, kit.watcher))
}

/// Everything the artifact feature brings, on one client.
pub struct ArtifactKit {
    /// `Artifact`, `ArtifactComments`, `ArtifactData`.
    pub tools: Vec<Arc<dyn Tool>>,
    pub watcher: ArtifactWatcher,
    /// Composes the bundled `artifact-capabilities` skill.
    pub capabilities: CapabilitiesGuide,
}

impl std::fmt::Debug for ArtifactKit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactKit").field("capabilities", &self.capabilities).finish_non_exhaustive()
    }
}

/// The watched tools (see [`tools_watched`]) and the `artifact-capabilities`
/// skill's composer, which writes the contract's type definitions under
/// `skill_dir`. With `opener`, artifacts open for the person as
/// [`ArtifactOpener`] describes.
pub fn kit(
    auth: Arc<dyn TokenSource>,
    config: FrameConfig,
    notifier: Arc<dyn WatchNotifier>,
    watch: WatchConfig,
    skill_dir: PathBuf,
    opener: Option<Arc<dyn ArtifactOpener>>,
) -> Result<ArtifactKit, FrameError> {
    let client = Arc::new(FrameClient::new(auth, config)?);
    let watcher = ArtifactWatcher::new(Arc::clone(&client), notifier, watch);
    let capabilities = CapabilitiesGuide::new(Arc::clone(&client), skill_dir);
    let surface = Arc::new(surface::Surface::new(opener));
    let artifact = ArtifactTool::new(Arc::clone(&client))
        .with_watcher(watcher.clone())
        .with_targets(capabilities.targets())
        .with_surface(Arc::clone(&surface));
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(artifact),
        Arc::new(ArtifactCommentsTool::new(Arc::clone(&client)).with_watcher(watcher.clone())),
        Arc::new(ArtifactDataTool::new(client).with_surface(surface)),
    ];
    Ok(ArtifactKit { tools, watcher, capabilities })
}

fn schema(json: &str) -> serde_json::Value {
    serde_json::from_str(json).expect("bundled tool schemas are valid JSON")
}

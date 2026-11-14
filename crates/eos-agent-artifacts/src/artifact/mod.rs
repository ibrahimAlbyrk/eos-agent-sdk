//! The `Artifact` tool.

mod assets;
mod browse;
mod publish;
mod quickstart;
mod restore;

use std::path::PathBuf;
use std::sync::Arc;

use eos_agent_core::{
    AgentIdent, BoxFuture, Extensions, StoredMessage, Tool, ToolContext, ToolOutput, ToolSpec, Workspace,
};
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;

use crate::client::{FrameClient, FrameError};
use crate::ids;
use crate::targets::{ContractTargets, session_of};
use crate::watch::ArtifactWatcher;

const DESCRIPTION: &str = include_str!("../../prompts/Artifact.md");
const SCHEMA: &str = include_str!("../../prompts/Artifact.schema.json");

/// The fields this client acts on; the rest of the schema (`auto_open`,
/// `prompt`, `page`, `overwrite_unread`) is accepted and has no effect here.
#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct Input {
    pub action: Option<String>,
    pub after: Option<String>,
    pub asset: Option<bool>,
    pub asset_ids: Option<Vec<String>>,
    pub capabilities: Option<serde_json::Map<String, Value>>,
    pub contract: Option<String>,
    pub description: Option<String>,
    pub design_systems: Option<bool>,
    pub favicon: Option<String>,
    pub file_path: Option<String>,
    pub file_paths: Option<Vec<String>>,
    pub files: Option<Value>,
    pub force: Option<bool>,
    pub from_url: Option<String>,
    pub icon: Option<String>,
    pub intent: Option<String>,
    pub label: Option<String>,
    pub limit: Option<u32>,
    pub out_dir: Option<String>,
    pub path: Option<String>,
    pub paths: Option<Vec<String>>,
    pub pin: Option<bool>,
    pub root: Option<String>,
    pub scope: Option<String>,
    pub title: Option<String>,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub type_query: Option<String>,
    pub type_url: Option<String>,
    pub url: Option<String>,
}

pub struct ArtifactTool {
    spec: ToolSpec,
    client: Arc<FrameClient>,
    watcher: Option<ArtifactWatcher>,
    targets: Option<Arc<ContractTargets>>,
}

impl std::fmt::Debug for ArtifactTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactTool").finish_non_exhaustive()
    }
}

impl ArtifactTool {
    pub fn new(client: Arc<FrameClient>) -> Self {
        Self {
            spec: ToolSpec::new("Artifact", DESCRIPTION.trim_end(), crate::schema(SCHEMA)),
            client,
            watcher: None,
            targets: None,
        }
    }

    /// Publishes start watching the artifact through `watcher`.
    pub fn with_watcher(mut self, watcher: ArtifactWatcher) -> Self {
        self.watcher = Some(watcher);
        self
    }

    /// Publishes, creations and reads for writing note the session's target
    /// artifact in `targets` (the `artifact-capabilities` skill reads it).
    pub(crate) fn with_targets(mut self, targets: Arc<ContractTargets>) -> Self {
        self.targets = Some(targets);
        self
    }

    async fn run(&self, input: &RawValue, ctx: ToolContext<'_>) -> Result<String, String> {
        let input: Input = serde_json::from_str(input.get()).map_err(|e| format!("invalid input: {e}"))?;
        let c = &self.client;
        let w = self.watcher.as_ref();
        let t = Target { targets: self.targets.as_deref(), ctx: &ctx };
        match input.action.as_deref().unwrap_or("publish") {
            "publish" if input.asset == Some(true) => assets::upload(c, &input, &ctx).await,
            "publish" if input.type_url.is_some() && input.url.is_none() => {
                quickstart::create_from_type(c, w, t, &input, &ctx).await
            }
            "publish" => publish::publish(c, w, t, &input, &ctx).await,
            "read" if input.url.is_none() && input.type_url.is_some() => quickstart::describe_type(c, &input).await,
            "read" => browse::read(c, w, t, &input, &ctx).await,
            "list" => browse::list(c, &input).await,
            "delete" => browse::delete(c, &input, &ctx).await,
            "pin" | "unpin" => browse::pin(c, &input).await,
            "open" => browse::open(&input),
            "quickstart" => quickstart::quickstart(c, &input).await,
            other => Err(format!("unknown action {other:?}")),
        }
    }
}

impl Tool for ArtifactTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn observable_input(&self, input: &RawValue, extensions: &Extensions) -> Option<Arc<RawValue>> {
        crate::files::with_absolute_out_dir(input, extensions)
    }

    /// Reading an artifact's files with `out_dir` saves them there.
    fn written_paths(&self, input: &RawValue, extensions: &Extensions) -> Vec<PathBuf> {
        let (Some(ws), Ok(input)) = (extensions.get::<Workspace>(), serde_json::from_str::<Input>(input.get())) else {
            return Vec::new();
        };
        let reads_files = input.path.is_some() || input.paths.as_ref().is_some_and(|p| !p.is_empty());
        match (input.action.as_deref(), &input.url, input.out_dir.as_deref()) {
            (Some("read"), Some(_), Some(dir)) if reads_files => {
                crate::files::out_dir_written(&ws, dir).into_iter().collect()
            }
            _ => Vec::new(),
        }
    }

    fn is_concurrency_safe(&self, input: &RawValue) -> bool {
        #[derive(Deserialize)]
        struct Action<'a> {
            #[serde(borrow)]
            action: Option<&'a str>,
        }
        serde_json::from_str::<Action<'_>>(input.get())
            .is_ok_and(|a| matches!(a.action, Some("read" | "list" | "quickstart")))
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let run = self.run(input, ctx);
            tokio::select! {
                out = run => match out {
                    Ok(text) => ToolOutput::text(text),
                    Err(text) => ToolOutput::error(text),
                },
                () = ctx.cancel.cancelled() => ToolOutput::error("cancelled"),
            }
        })
    }

    fn restore<'a>(&'a self, history: &'a [StoredMessage], extensions: &'a Extensions) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let published = restore::restore(history, extensions).await;
            // Claude Code re-arms the session's watches when it resumes.
            if let (Some(w), Some(AgentIdent(agent))) = (&self.watcher, extensions.get::<AgentIdent>().as_deref()) {
                w.attach(agent, extensions);
                for (slug, version) in published {
                    w.watch(agent, &slug, Some(&version), true, false);
                    w.resumed(&slug);
                }
            }
        })
    }
}

/// Where an action notes the session's target artifact (Claude Code's
/// `setArtifactContractTarget`).
#[derive(Clone, Copy)]
pub(crate) struct Target<'a> {
    targets: Option<&'a ContractTargets>,
    ctx: &'a ToolContext<'a>,
}

impl Target<'_> {
    /// `slug` becomes the target; a `contract` it was published with pins it.
    pub(crate) fn set(&self, slug: &str, contract: Option<&str>) {
        if let Some(t) = self.targets {
            t.set(&session_of(self.ctx.extensions, self.ctx.agent_id), slug, contract, false);
        }
    }
}

/// The slug named by `url`, or a plain error naming the field.
pub(crate) fn slug_of(url: Option<&str>, field: &str) -> Result<String, String> {
    let url = url.ok_or_else(|| format!("`{field}` is required for this action"))?;
    ids::slug_from_url(url).ok_or_else(|| {
        format!("{url:?} is not a claude.ai artifact link (claude.ai/artifact/… or claude.ai/code/artifact/…)")
    })
}

/// Tells the watcher (if any) which version this agent now has.
pub(crate) fn saw(w: Option<&ArtifactWatcher>, ctx: &ToolContext<'_>, slug: &str, version: &str, published: bool) {
    if let Some(w) = w {
        w.attach(ctx.agent_id, ctx.extensions);
        w.saw(ctx.agent_id, slug, version, published);
    }
}

/// A frame error as the model should read it.
pub(crate) fn explain(what: &str, e: FrameError) -> String {
    match e.status() {
        Some(401) => format!("{what} failed: Claude no longer accepts this sign-in — the user needs to sign in again."),
        Some(403) => format!("{what} was refused (HTTP 403): this account can't do that with this artifact. {e}"),
        Some(404) => {
            format!("{what} failed: not found — the artifact may have been deleted, or this account can't open it.")
        }
        Some(429) => {
            format!("{what} was rate limited (HTTP 429) — wait a little before trying again; do not retry at once.")
        }
        _ => format!("{what} failed: {e}"),
    }
}

//! The page a comment thread is on, read for Claude Code's composers (its
//! `ed`/`dd`): the live version's stored source, through the same boot and
//! content host as a read.

use std::time::Duration;

use crate::client::FrameClient;

/// Claude Code's `Zs`: the read's deadline for a reply.
pub(super) const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Pages larger than this are not read for a composer.
const MAX_BYTES: u64 = 8 << 20;

/// One page's live source.
#[derive(Debug, Clone)]
pub(super) struct PageSource {
    pub(super) html: String,
    pub(super) ver: String,
    /// The reading account owns the artifact.
    pub(super) owner: bool,
    /// The reader may write it and its view is the live version (not a
    /// shared view pinned to a fixed one).
    pub(super) live_writer: bool,
    /// Made from an Artifact type (its page is the type's).
    pub(super) typed: bool,
    pub(super) title: Option<String>,
    pub(super) favicon: Option<String>,
}

impl PageSource {
    /// Claude Code's `Ege`: someone other than the user may have written it.
    pub(super) fn foreign(&self) -> bool {
        !self.owner || self.typed
    }

    /// Claude Code's `audience`/`audienceView`: an automatic edit lands on
    /// what the reader sees.
    pub(super) fn edits_live(&self) -> bool {
        self.owner || self.live_writer
    }
}

/// The live source of the artifact's page (or of `file`), when it is HTML.
pub(super) async fn read(client: &FrameClient, slug: &str, file: Option<&str>) -> Option<PageSource> {
    let boot = client.boot(slug).await.ok()??;
    let ver = boot.ver.clone()?;
    let token = boot.asset_token.clone()?;
    let dest = std::env::temp_dir().join(format!("eos-artifact-source-{:016x}.html", fastrand::u64(..)));
    let read = client.content(&boot, slug, file.unwrap_or(""), &token, &dest, MAX_BYTES).await;
    let html = match read {
        Ok((kind, _)) if kind.as_deref().is_none_or(|k| k.starts_with("text/html")) => {
            tokio::fs::read_to_string(&dest).await.ok()
        }
        _ => None,
    };
    let _ = tokio::fs::remove_file(&dest).await;
    let html = html.filter(|h| !h.is_empty())?;
    let pinned = boot.shared.as_ref().and_then(|s| s.as_str()).is_some_and(|s| !s.is_empty());
    Some(PageSource {
        html,
        ver,
        owner: boot.role() == "owner",
        live_writer: boot.can_edit() && !pinned,
        typed: boot.type_instance == Some(true),
        title: boot.title.clone().filter(|t| !t.is_empty()),
        favicon: boot.favicon.clone().filter(|f| !f.is_empty()),
    })
}

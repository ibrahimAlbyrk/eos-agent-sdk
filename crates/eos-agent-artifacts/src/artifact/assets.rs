//! `publish` with `asset: true`: files into an artifact's asset store,
//! streamed from disk, or copied server side from another artifact.

use eos_agent_core::ToolContext;
use serde_json::Value;

use super::{Input, explain, slug_of};
use crate::client::{FrameClient, cut};
use crate::files;
use crate::ids;

const MAX_ASSET_BYTES: u64 = 15 << 20;
const MAX_FILES: usize = 25;
const MAX_COPIES: usize = 10;

pub(super) async fn upload(c: &FrameClient, input: &Input, ctx: &ToolContext<'_>) -> Result<String, String> {
    let slug = slug_of(input.url.as_deref(), "url")?;
    if input.from_url.is_some() || input.asset_ids.is_some() {
        return copy(c, &slug, input).await;
    }
    let ws = files::workspace(ctx)?;
    let paths: Vec<String> = match (&input.file_path, &input.file_paths) {
        (Some(p), None) => vec![p.clone()],
        (None, Some(ps)) if !ps.is_empty() && ps.len() <= MAX_FILES => ps.clone(),
        (None, Some(_)) => return Err(format!("`file_paths` takes 1 to {MAX_FILES} files")),
        _ => return Err("an asset upload takes `file_path` or `file_paths` (not both)".into()),
    };
    let mut lines = Vec::with_capacity(paths.len());
    for raw in &paths {
        lines.push(match upload_one(c, &ws, &slug, raw).await {
            Ok(line) => line,
            Err(e) => format!("- {raw}: not uploaded — {e}"),
        });
    }
    Ok(format!(
        "Assets for {} — reference each from the page by its url, exactly as given:\n{}",
        ids::viewer_url(&slug),
        lines.join("\n")
    ))
}

async fn upload_one(c: &FrameClient, ws: &eos_agent_core::Workspace, slug: &str, raw: &str) -> Result<String, String> {
    let path = files::existing_file(ws, raw).await?;
    let kind = files::content_type_for(&path.to_string_lossy())
        .ok_or("can't tell this file's media type from its extension")?;
    let size = tokio::fs::metadata(&path).await.map_err(|e| e.to_string())?.len();
    if size == 0 {
        return Err("the file is empty".into());
    }
    if size > MAX_ASSET_BYTES {
        return Err(format!(
            "the file exceeds the {} MiB per-asset limit — compress or split it",
            MAX_ASSET_BYTES >> 20
        ));
    }
    let answer = c.asset_upload(slug, path.clone(), kind).await.map_err(|e| explain("The upload", e))?;
    let id = answer["opaque_id"]
        .as_str()
        .ok_or("the upload probably succeeded but the reply was unreadable — retry at most once")?;
    Ok(format!("- {raw}: id {id}, url {}", asset_url(id, &answer)))
}

async fn copy(c: &FrameClient, slug: &str, input: &Input) -> Result<String, String> {
    let from = slug_of(input.from_url.as_deref(), "from_url")?;
    if from == slug {
        return Err("from_url names the destination itself — its assets are already there".into());
    }
    let ids_in = input.asset_ids.clone().unwrap_or_default();
    if ids_in.is_empty() || ids_in.len() > MAX_COPIES || !ids_in.iter().all(|i| ids::is_asset_id(i)) {
        return Err(format!("asset_ids must be 1–{MAX_COPIES} distinct asset ids"));
    }
    let answer = c.asset_copy(slug, &from, &ids_in).await.map_err(|e| explain("Copying the assets", e))?;
    let rows = answer["assets"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return Ok(format!("Copied; the server answered: {}", cut(&answer.to_string(), 2_000)));
    }
    let lines: Vec<String> = rows
        .iter()
        .filter_map(|r| r["opaque_id"].as_str().map(|id| format!("- id {id}, url {}", asset_url(id, r))))
        .collect();
    Ok(format!(
        "Copied into {} — reference each by its new url, exactly as given:\n{}",
        ids::viewer_url(slug),
        lines.join("\n")
    ))
}

/// The url an asset is served at in its artifact.
fn asset_url(id: &str, row: &Value) -> String {
    match row["url"].as_str() {
        Some(u) if u == format!("_blob/{id}") || u == format!("/_blob/{id}") => u.to_owned(),
        _ => format!("/_blob/{id}"),
    }
}

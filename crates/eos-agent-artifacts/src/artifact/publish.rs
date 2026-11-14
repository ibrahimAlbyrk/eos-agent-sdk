//! `action: "publish"`: a page (plus supporting files) to a new or existing
//! artifact, or data files to an artifact made from a type.

use std::path::{Path, PathBuf};

use eos_agent_core::{ToolContext, Workspace};
use serde_json::{Map, Value, json};
use tokio::io::AsyncReadExt;

use super::{Input, browse, explain, saw, slug_of};
use crate::client::{Deployed, Entry, FrameClient, PublishBody, SourceFile, UploadBatch, cut, error_text};
use crate::files;
use crate::ids;
use crate::state::{ArtifactState, Known};
use crate::watch::ArtifactWatcher;

const MAX_FILES: usize = 255;
const MAX_TEXT_BYTES: u64 = 16 << 20;
const MAX_BINARY_BYTES: u64 = 15 << 20;
const MAX_PUBLISH_BYTES: u64 = 64 << 20;
/// File content the deploy body itself may carry; the rest is staged first.
const INLINE_BUDGET: u64 = 12 << 20;
const UPLOAD_BATCH: u64 = 15 << 20;

enum Source {
    Local(SourceFile),
    Copy(Value),
    Remove,
}

pub(super) async fn publish(
    c: &FrameClient,
    w: Option<&ArtifactWatcher>,
    t: super::Target<'_>,
    input: &Input,
    ctx: &ToolContext<'_>,
) -> Result<String, String> {
    let ws = files::workspace(ctx)?;
    let state = ctx.extensions.get_or_default::<ArtifactState>();
    let raw = input.file_path.as_deref().ok_or("`file_path` is required to publish")?;
    let local = files::existing_file(&ws, raw).await?;
    let slug = match input.url.as_deref() {
        Some(url) => Some(slug_of(Some(url), "url")?),
        None => state.slug_for_path(&local),
    };
    let known = match &slug {
        Some(slug) => match state.get(slug) {
            Some(k) => Some(k),
            None => return Err(refuse_unread(c, w, ctx, &ws, &state, slug).await),
        },
        None => None,
    };
    let typed = known.as_ref().is_some_and(|k| k.typed);
    // Canonical like the file paths it is compared with.
    let root = match input.root.as_deref().map(|r| ws.resolve(Path::new(r))) {
        Some(r) => Some(tokio::fs::canonicalize(&r).await.map_err(|e| format!("root {}: {e}", r.display()))?),
        None => None,
    };

    let mut sources = Vec::new();
    let served = if typed { served_path(&local, root.as_deref())? } else { "index.html".to_owned() };
    sources.push((served.clone(), local.clone(), content_type(&served, None)?));
    let mut copies_and_removals = Vec::new();
    collect_files(input.files.as_ref(), root.as_deref(), &ws, &mut sources, &mut copies_and_removals).await?;
    if sources.len() + copies_and_removals.len() > MAX_FILES {
        return Err(format!("one publish sends at most {MAX_FILES} files; publish the rest to the same `url` after"));
    }

    let mut entries = Vec::with_capacity(sources.len() + copies_and_removals.len());
    let mut total = 0u64;
    for (published, path, kind) in sources {
        let file = SourceFile::scan(&path, kind, files::is_text(kind)).await?;
        let cap = if file.text { MAX_TEXT_BYTES } else { MAX_BINARY_BYTES };
        if file.size > cap {
            return Err(format!("{published} is {} bytes; the limit is {cap} for this kind of file", file.size));
        }
        total += file.size;
        entries.push((published, Source::Local(file)));
    }
    if total > MAX_PUBLISH_BYTES {
        return Err(format!("this publish sends {total} bytes; one publish takes at most {MAX_PUBLISH_BYTES}"));
    }
    entries.extend(copies_and_removals);
    if entries.iter().any(|(_, s)| matches!(s, Source::Remove)) && known.is_none() {
        return Err("A `null` entry in `files` removes a file from an existing artifact, and a first publish has nothing to remove, so nothing was published. Drop the `null` entries.".into());
    }

    let title = match input.title.clone() {
        Some(t) => t,
        None if !typed => page_title(&local)
            .await
            .or_else(|| known.as_ref().and_then(|k| k.title.clone()))
            .unwrap_or_else(|| file_stem(&local)),
        None => known.as_ref().and_then(|k| k.title.clone()).unwrap_or_else(|| file_stem(&local)),
    };
    let mut meta = Map::new();
    put(&mut meta, "title", Some(title.clone()));
    put(&mut meta, "icon", input.icon.clone().filter(|_| !typed));
    put(&mut meta, "favicon", input.favicon.clone().filter(|_| !typed));
    put(&mut meta, "label", input.label.clone());
    put(&mut meta, "description", input.description.clone());
    let contract = contract_for(c, slug.as_deref(), input).await?;
    if let Some(contract) = &contract {
        meta.insert("contract".into(), json!(contract));
    }
    if let Some(caps) = &input.capabilities {
        meta.insert("capabilities".into(), Value::Object(caps.clone()));
    }
    if input.force == Some(true) {
        meta.insert("force".into(), json!(true));
    } else if let Some(k) = &known {
        meta.insert("baseVersion".into(), json!(k.version));
    }

    let (slug, body) = plan(c, slug, meta, entries).await?;
    let (status, answer) = c.deploy(body).await.map_err(|e| explain("The publish", e))?;
    let deployed: Deployed = match status {
        200..=299 => serde_json::from_value(answer).map_err(|e| format!("the publish may have gone through, but its answer was unreadable ({e}) — check with action \"list\" before publishing again"))?,
        409 if slug.is_some() => {
            let target = slug.as_deref().unwrap_or_default();
            let saved = refuse_unread(c, w, ctx, &ws, &state, target).await;
            // `{conflict: true, live, author, createdAt, liveEntry}`
            let why = match (answer["live"].as_str(), answer["createdAt"].as_str()) {
                (Some(live), Some(at)) => format!("version {live} was published at {at}"),
                (Some(live), None) => format!("version {live} is live now"),
                _ => error_text(answer.to_string().as_bytes()),
            };
            return Err(format!("The publish was refused: the artifact changed since the version this conversation last saw ({why}). {saved}"));
        }
        413 => return Err("The publish is too large for the server, so nothing was published. Make the page or its files smaller.".into()),
        429 => return Err(format!("The publish was refused for now (HTTP 429: {}) — wait before publishing again.", cut(&answer.to_string(), 300))),
        _ => return Err(format!("The publish was refused (HTTP {status}: {}). Nothing was published.", error_text(answer.to_string().as_bytes()))),
    };

    let typed = typed || deployed.type_instance;
    state.record(&deployed.slug, Known { version: deployed.version.clone(), title: Some(title), typed });
    state.bind_path(&local, &deployed.slug);
    t.set(&deployed.slug, contract.as_deref());
    saw(w, ctx, &deployed.slug, &deployed.version, true);
    if let Some(w) = w {
        w.published_from(&deployed.slug, &local);
    }
    let url = ids::viewer_url(&deployed.slug);
    let version = version_note(deployed.seq, &deployed.version);
    let mut out = if typed {
        format!(
            "Updated the Artifact at {url}{version} with {} (served at {served}) and any `files` listed; own files not sent this time were kept. Its page comes from its Artifact type and can't be changed here. To update it again, publish to the same `url` (or the same `file_path` in this conversation).",
            local.display()
        )
    } else {
        format!(
            "Published {} at {url}{version}. To update it, publish the same `file_path` again in this conversation, or pass this `url`.",
            local.display()
        )
    };
    if deployed.unchanged {
        out.push_str(" The server already had this exact content, so the live version did not change.");
    }
    if input.pin == Some(true) {
        out.push_str(match c.favorite(&deployed.slug, true).await {
            Ok(()) => " Pinned to the person's claude.ai sidebar.",
            Err(_) => " Pinning it failed; the publish itself went through.",
        });
    }
    out.push_str("\n\n");
    out.push_str(&watch_line(w, ctx, &deployed.slug, &deployed.version));
    Ok(out)
}

/// The `contract` a publish sends, as Claude Code picks it: the one passed
/// (`latest` resolved to its version); with `capabilities`, the artifact's
/// stored contract, else the latest one (the server takes capabilities only
/// with a contract), and the declared names must be in it.
async fn contract_for(c: &FrameClient, slug: Option<&str>, input: &Input) -> Result<Option<String>, String> {
    let fetch_failed = |e| {
        explain(
            "Fetching the capability contract (a publish that declares capabilities or upgrades needs it; nothing was published)",
            e,
        )
    };
    let wanted = match input.contract.as_deref() {
        Some("latest") => Some(c.contract("latest").await.map_err(fetch_failed)?),
        Some(version) if input.capabilities.is_some() => Some(c.contract(version).await.map_err(fetch_failed)?),
        Some(version) => return Ok(Some(version.to_owned())),
        None if input.capabilities.is_none() => return Ok(None),
        None => {
            let stored = match slug {
                Some(slug) => c.read_meta(slug).await.ok().and_then(|m| m["contract"].as_str().map(str::to_owned)),
                None => None,
            };
            match stored.filter(|v| !v.is_empty() && v != "0.0.0") {
                Some(version) => match c.contract(&version).await {
                    Ok(roster) => Some(roster),
                    // Claude Code sends the stored one unchecked when it can't be read.
                    Err(_) => return Ok(Some(version)),
                },
                None => Some(c.contract("latest").await.map_err(fetch_failed)?),
            }
        }
    };
    let Some(roster) = wanted else { return Ok(None) };
    let version = roster["version"].as_str().ok_or("the capability contract named no version")?.to_owned();
    let known: Vec<&str> = roster["capabilities"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let unknown: Vec<&str> = input
        .capabilities
        .iter()
        .flat_map(|caps| caps.keys())
        .map(String::as_str)
        .filter(|name| !known.contains(name))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "Nothing was published: unknown {} {} — contract {version} supports: {}. Fix or drop the declaration.",
            if unknown.len() == 1 { "capability" } else { "capabilities" },
            unknown.join(", "),
            known.join(", ")
        ));
    }
    Ok(Some(version))
}

/// The publish result's subscription line: the publish starts (or keeps)
/// this agent's watch of the artifact.
pub(super) fn watch_line(w: Option<&ArtifactWatcher>, ctx: &ToolContext<'_>, slug: &str, version: &str) -> String {
    match w {
        Some(w) => {
            w.attach(ctx.agent_id, ctx.extensions);
            w.watch(ctx.agent_id, slug, Some(version), true, false).line()
        }
        None => "Not watching it: this session holds no artifact watches, so a republish from elsewhere or a comment sent to Claude will not reach it.".into(),
    }
}

/// Splits the files between the deploy body and `/upload` (by the inline
/// budget) and stages the latter. Returns the slug the deploy goes to.
async fn plan(
    c: &FrameClient,
    mut slug: Option<String>,
    mut meta: Map<String, Value>,
    entries: Vec<(String, Source)>,
) -> Result<(Option<String>, PublishBody), String> {
    let mut budget = INLINE_BUDGET;
    let mut staged = Vec::new();
    let mut copies = Vec::new();
    let mut files = Vec::new();
    for (published, source) in entries {
        match source {
            Source::Local(f) if f.wire_len <= budget => {
                budget -= f.wire_len;
                files.push((published, Entry::Inline(f)));
            }
            Source::Local(f) => {
                files.push((
                    published.clone(),
                    Entry::Staged { sha256: f.sha256.clone(), content_type: f.content_type.clone() },
                ));
                staged.push((published, f));
            }
            Source::Copy(from) => copies.push((published, from)),
            Source::Remove => files.push((published, Entry::Remove)),
        }
    }
    if !staged.is_empty() || !copies.is_empty() {
        let shas: Vec<String> = staged.iter().map(|(_, f)| f.sha256.clone()).collect();
        let prepared = c.prepare(slug.as_deref(), &shas).await.map_err(|e| explain("Preparing the upload", e))?;
        if slug.as_deref().is_some_and(|s| s != prepared.slug) {
            return Err("the server answered the upload preflight for a different artifact — not publishing.".into());
        }
        let target = prepared.slug.clone();
        let mut batch = UploadBatch { slug: target.clone(), files: Vec::new() };
        for (published, file) in staged.into_iter().filter(|(_, f)| prepared.missing.contains(&f.sha256)) {
            if !batch.files.is_empty() && batch.wire_len() + file.wire_len > UPLOAD_BATCH {
                let full = std::mem::replace(&mut batch, UploadBatch { slug: target.clone(), files: Vec::new() });
                upload(c, full, &target).await?;
            }
            batch.files.push((published, file));
        }
        if !batch.files.is_empty() {
            upload(c, batch, &target).await?;
        }
        if !copies.is_empty() {
            let echo = c.upload_copies(&target, &copies).await.map_err(|e| {
                explain(&format!("Copying files from another artifact (publish again to slug {target})"), e)
            })?;
            let rows = echo.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
            if rows.len() != copies.len() {
                return Err(
                    "the server staged the copied files but its answer was incomplete, so nothing was published".into(),
                );
            }
            for ((published, _), row) in copies.into_iter().zip(rows) {
                let (Some(sha256), Some(kind)) = (row["sha256"].as_str(), row["contentType"].as_str()) else {
                    return Err(format!("the server did not stage {published}: {}", cut(&row.to_string(), 200)));
                };
                files.push((published, Entry::Staged { sha256: sha256.into(), content_type: kind.into() }));
            }
        }
        slug = Some(target);
    }
    if let Some(s) = &slug {
        meta.insert("slug".into(), json!(s));
    }
    let single_page = files.len() == 1 && files[0].0 == "index.html" && matches!(files[0].1, Entry::Inline(_));
    if !single_page {
        // As Claude Code: a manifest without a page, or an update pinned to
        // the version it builds on, changes only the paths it names
        // (`patch`); otherwise it is the artifact's whole file set.
        let has_page = files.iter().any(|(path, entry)| path == "index.html" && !matches!(entry, Entry::Remove));
        let pinned = slug.is_some() && meta.contains_key("baseVersion");
        meta.insert("mode".into(), json!(if !has_page || pinned { "patch" } else { "replace" }));
    }
    let mut body = PublishBody { meta, content: None, files };
    if single_page && let Some((_, Entry::Inline(page))) = body.files.pop() {
        body.content = Some(page);
    }
    Ok((slug, body))
}

async fn upload(c: &FrameClient, batch: UploadBatch, slug: &str) -> Result<(), String> {
    c.upload(batch)
        .await
        .map(drop)
        .map_err(|e| explain(&format!("Uploading files (the publish was NOT completed; retry to slug {slug})"), e))
}

/// Reads the live version into the scratch directory and words the refusal.
async fn refuse_unread(
    c: &FrameClient,
    w: Option<&ArtifactWatcher>,
    ctx: &ToolContext<'_>,
    ws: &Workspace,
    state: &ArtifactState,
    slug: &str,
) -> String {
    match browse::fetch_live(c, w, ctx, ws, state, slug).await {
        Ok(live) => format!(
            "Nothing was published: this conversation had not seen the live version of {} ({}). It is saved at {} — build on that file (merge your changes into it) and publish again with the same `url`.",
            ids::viewer_url(slug),
            live.version,
            live.saved.display()
        ),
        Err(e) => {
            format!("Nothing was published: this conversation had not read that artifact, and reading it failed: {e}")
        }
    }
}

async fn collect_files(
    files: Option<&Value>,
    root: Option<&Path>,
    ws: &Workspace,
    sources: &mut Vec<(String, PathBuf, &'static str)>,
    other: &mut Vec<(String, Source)>,
) -> Result<(), String> {
    let source_path = |raw: &str| match root {
        Some(r) if !Path::new(raw).is_absolute() => r.join(raw).to_string_lossy().into_owned(),
        _ => raw.to_owned(),
    };
    match files {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) => {
            for item in list {
                let path = item["path"].as_str().ok_or("each `files` entry needs a `path`")?;
                let local = files::existing_file(ws, &source_path(path)).await?;
                sources.push((published(path)?, local, content_type(path, item["contentType"].as_str())?));
            }
        }
        Some(Value::Object(map)) => {
            for (key, value) in map {
                let key = published(key)?;
                match value {
                    Value::Null => other.push((key, Source::Remove)),
                    Value::String(src) => {
                        let local = files::existing_file(ws, &source_path(src)).await?;
                        let kind = content_type(&key, None)?;
                        sources.push((key, local, kind));
                    }
                    Value::Object(o) if o.contains_key("artifact") => {
                        let from_slug = slug_of(o.get("artifact").and_then(Value::as_str), "files.artifact")?;
                        let mut from =
                            json!({ "slug": from_slug, "path": o.get("path").cloned().unwrap_or(Value::Null) });
                        if let Some(ver) = o.get("ver") {
                            from["ver"] = ver.clone();
                        }
                        other.push((key, Source::Copy(from)));
                    }
                    Value::Object(o) => {
                        let src = o.get("from").and_then(Value::as_str).ok_or("a `files` object needs `from`")?;
                        let local = files::existing_file(ws, &source_path(src)).await?;
                        let kind = content_type(&key, o.get("contentType").and_then(Value::as_str))?;
                        sources.push((key, local, kind));
                    }
                    _ => return Err(format!("`files[{key:?}]` must be a path, an object or null")),
                }
            }
        }
        Some(_) => return Err("`files` must be a list or a map".into()),
    }
    Ok(())
}

fn published(path: &str) -> Result<String, String> {
    let clean = path.trim_start_matches("./");
    if clean.starts_with('/') || clean.split('/').any(|s| s.is_empty() || s == "..") {
        return Err(format!("{path:?} is not a relative published path"));
    }
    Ok(clean.to_owned())
}

fn content_type(path: &str, explicit: Option<&str>) -> Result<&'static str, String> {
    if let Some(kind) = explicit {
        return files::known_type(kind).ok_or_else(|| format!("{kind:?} is not a media type this client publishes"));
    }
    files::content_type_for(path).ok_or_else(|| format!("can't tell the media type of {path:?}: pass `contentType`"))
}

/// A data file's served path: relative to `root`, else its file name.
fn served_path(local: &Path, root: Option<&Path>) -> Result<String, String> {
    if let Some(rel) = root.and_then(|r| local.strip_prefix(r).ok()) {
        return published(&rel.to_string_lossy());
    }
    published(&local.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
}

fn put(meta: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(v) = value.filter(|v| !v.is_empty()) {
        meta.insert(key.into(), Value::String(v));
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Untitled".into())
}

pub(super) fn version_note(seq: Option<u64>, version: &str) -> String {
    match seq {
        Some(n) => format!(" (Version {n}, version id {version})"),
        None => format!(" (version {version})"),
    }
}

/// The page's `<title>`, read from its first 64 KiB only.
async fn page_title(path: &Path) -> Option<String> {
    let mut head = Vec::with_capacity(64 << 10);
    tokio::fs::File::open(path).await.ok()?.take(64 << 10).read_to_end(&mut head).await.ok()?;
    let text = String::from_utf8_lossy(&head);
    let lower = text.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open_end = start + lower[start..].find('>')? + 1;
    let close = open_end + lower[open_end..].find("</title")?;
    let title = text[open_end..close].split_whitespace().collect::<Vec<_>>().join(" ");
    let title = title.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"");
    (!title.is_empty()).then_some(title)
}

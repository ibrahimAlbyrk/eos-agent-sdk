//! The frame API's endpoints, one method each. Answers the tools only render
//! stay `serde_json::Value`; the fields the tools branch on are typed.

use std::path::{Path, PathBuf};

use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};

use super::body::raw_file;
use super::{FrameClient, FrameError, PublishBody, RequestBody, UploadBatch, encode};

/// What `GET /api/frame/{slug}?via=model_read` says about an artifact.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Boot {
    /// The live version's id.
    pub(crate) ver: Option<String>,
    /// Grants reading the artifact's content.
    pub(crate) asset_token: Option<String>,
    /// Opens the artifact's live channel (about an hour).
    pub(crate) subscription_token: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) perm: Option<Perm>,
    pub(crate) type_instance: Option<bool>,
    /// The Artifact type it was made from; files copied from it are the
    /// type's, not the artifact's own.
    pub(crate) type_slug: Option<String>,
    pub(crate) files: Option<Value>,
    /// `frame` when the content is read through the API instead of the
    /// content host.
    pub(crate) content_via: Option<String>,
    /// When the live-channel token ends (unix seconds).
    pub(crate) subscription_token_exp: Option<f64>,
    /// The live-channel token may be renewed without a new boot.
    pub(crate) watch_token_renew_enabled: Option<bool>,
    pub(crate) favicon: Option<String>,
    /// The version a non-owner's shared view is pinned to (none: the live one).
    pub(crate) shared: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Perm {
    pub(crate) role: Option<String>,
}

impl Boot {
    pub(crate) fn role(&self) -> &str {
        self.perm.as_ref().and_then(|p| p.role.as_deref()).unwrap_or("reader")
    }

    pub(crate) fn can_edit(&self) -> bool {
        matches!(self.role(), "owner" | "writer")
    }
}

/// The answer to a publish.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Deployed {
    pub(crate) slug: String,
    pub(crate) version: String,
    pub(crate) seq: Option<u64>,
    #[serde(default)]
    pub(crate) type_instance: bool,
    #[serde(default)]
    pub(crate) unchanged: bool,
}

/// The answer to `deploy/prepare`: the slug, and which of the offered
/// contents the server still lacks.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Prepared {
    pub(crate) slug: String,
    #[serde(default)]
    pub(crate) missing: Vec<String>,
}

fn content_type(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get("x-frame-asset-content-type")
        .or_else(|| headers.get("content-type"))
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn parse<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, FrameError> {
    serde_json::from_value(v).map_err(|e| FrameError::Malformed(e.to_string()))
}

impl FrameClient {
    /// The person's own artifacts, or (`shared`) those shared with them.
    pub(crate) async fn list_frames(&self, shared: bool, limit: u32) -> Result<Vec<Value>, FrameError> {
        let path = if shared {
            format!("/api/frame/frames?rel=shared&limit={limit}")
        } else {
            format!("/api/frame/frames?limit={limit}")
        };
        let body = self.call(Method::GET, &path, &RequestBody::Empty).await?;
        match body.get("frames") {
            Some(Value::Array(rows)) => Ok(rows.clone()),
            Some(Value::Null) => Ok(Vec::new()),
            _ => Err(FrameError::Malformed("no `frames` list".into())),
        }
    }

    /// `None` when the artifact doesn't exist (or isn't visible to this account).
    pub(crate) async fn boot(&self, slug: &str) -> Result<Option<Boot>, FrameError> {
        let path = format!("/api/frame/{}?via=model_read", encode(slug));
        match self.call(Method::GET, &path, &RequestBody::Empty).await {
            Ok(v) => parse(v).map(Some),
            Err(FrameError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Claude Code's boot for the live channel (`watchBoot`): the token, when
    /// it ends and whether it may be renewed.
    pub(crate) async fn watch_boot(&self, slug: &str) -> Result<Option<Boot>, FrameError> {
        let path = format!("/api/frame/{}?via=model_read&bk=probe&vn=passive", encode(slug));
        match self.call(Method::GET, &path, &RequestBody::Empty).await {
            Ok(v) => parse(v).map(Some),
            Err(FrameError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// A new live-channel token without a boot (`POST
    /// /api/frame/watch-token/<slug>`): the answer carries `ver`,
    /// `subscriptionToken` and `subscriptionTokenExp`.
    pub(crate) async fn renew_watch_token(&self, slug: &str) -> Result<Boot, FrameError> {
        let path = format!("/api/frame/watch-token/{}?vn=passive", encode(slug));
        parse(self.call(Method::POST, &path, &RequestBody::json(&json!({}))).await?)
    }

    /// Streams one published file of version `ver` (`file` = "" for the
    /// page) to `dest`; returns its content type and size. Like Claude Code:
    /// from the content host, the stored source first (the page as published,
    /// before the runtime is added) and then the served file; through the API
    /// when the boot says `contentVia: frame`.
    pub(crate) async fn content(
        &self,
        boot: &Boot,
        slug: &str,
        file: &str,
        token: &str,
        dest: &Path,
        max_bytes: u64,
    ) -> Result<(Option<String>, u64), FrameError> {
        let tail: Vec<String> = file.split('/').filter(|s| !s.is_empty()).map(encode).collect();
        let tail = tail.join("/");
        if boot.content_via.as_deref() == Some("frame") {
            let path = format!("/api/frame/content/{}/{tail}", encode(slug));
            let (_, headers, len) = self.download(&path, &[("x-frame-asset-token", token)], dest, max_bytes).await?;
            if headers.get("x-frame-content-route").and_then(|v| v.to_str().ok()) != Some("served") {
                let _ = tokio::fs::remove_file(dest).await;
                return Err(FrameError::Malformed("the content route did not serve this file".into()));
            }
            return Ok((content_type(&headers), len));
        }
        let ver = boot.ver.as_deref().ok_or_else(|| FrameError::Malformed("no live version to read".into()))?;
        let origin = self.content_origin(slug);
        let query = format!("?__frame_t={}", encode(token));
        let source =
            format!("{origin}/_f/{}/_src/{}{query}", encode(ver), if tail.is_empty() { "index.html" } else { &tail });
        let served = format!("{origin}/_f/{}/{tail}{query}", encode(ver));
        // Uploaded assets have no stored source.
        let urls = if file.starts_with("_blob/") { vec![served] } else { vec![source, served] };
        let mut last = None;
        for url in urls {
            match self.download_public(&url, dest, max_bytes).await {
                Ok((_, headers, len)) => return Ok((content_type(&headers), len)),
                Err(e @ FrameError::Status { .. }) => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| FrameError::Malformed("nothing to read".into())))
    }

    /// `POST deploy/direct`, answered with its raw status so callers can
    /// tell a conflict from a refusal.
    pub(crate) async fn deploy(&self, body: PublishBody) -> Result<(u16, Value), FrameError> {
        let (status, _, bytes) =
            self.call_raw(Method::POST, "/api/frame/deploy/direct", &body.into_request(), &[]).await?;
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        Ok((status, value))
    }

    pub(crate) async fn prepare(&self, slug: Option<&str>, shas: &[String]) -> Result<Prepared, FrameError> {
        let mut body = json!({ "shas": shas });
        if let Some(slug) = slug {
            body["slug"] = json!(slug);
        }
        parse(self.call(Method::POST, "/api/frame/deploy/prepare", &RequestBody::json(&body)).await?)
    }

    pub(crate) async fn upload(&self, batch: UploadBatch) -> Result<Value, FrameError> {
        self.call(Method::POST, "/api/frame/upload", &batch.into_request()).await
    }

    /// Stages files copied from other artifacts; the answer echoes each one's
    /// sha256 and content type.
    pub(crate) async fn upload_copies(&self, slug: &str, copies: &[(String, Value)]) -> Result<Value, FrameError> {
        let files: Vec<Value> = copies.iter().map(|(path, from)| json!({ "path": path, "from": from })).collect();
        let body = json!({ "slug": slug, "files": files });
        self.call(Method::POST, "/api/frame/upload", &RequestBody::json(&body)).await
    }

    pub(crate) async fn delete(&self, slug: &str) -> Result<(), FrameError> {
        self.call(Method::DELETE, &format!("/api/frame/{}", encode(slug)), &RequestBody::Empty).await.map(drop)
    }

    pub(crate) async fn favorite(&self, slug: &str, on: bool) -> Result<(), FrameError> {
        let method = if on { Method::POST } else { Method::DELETE };
        let body = if on { RequestBody::json(&json!({})) } else { RequestBody::Empty };
        self.call(method, &format!("/api/frame/favorite/{}", encode(slug)), &body).await.map(drop)
    }

    /// The type catalog; `first_party` keeps Anthropic's own types only.
    pub(crate) async fn types(&self, query: Option<&str>, first_party: bool) -> Result<Value, FrameError> {
        let mut params = Vec::new();
        if first_party {
            params.push("scopes=anthropic".to_owned());
        }
        if let Some(q) = query.filter(|q| !q.is_empty()) {
            params.push(format!("q={}", encode(q)));
        }
        let path = if params.is_empty() {
            "/api/frame/types".to_owned()
        } else {
            format!("/api/frame/types?{}", params.join("&"))
        };
        self.call(Method::GET, &path, &RequestBody::Empty).await
    }

    pub(crate) async fn type_info(&self, slug: &str) -> Result<Value, FrameError> {
        self.call(Method::GET, &format!("/api/frame/types/{}", encode(slug)), &RequestBody::Empty).await
    }

    /// One of a type's own files as text (`{path, text, truncated}`).
    pub(crate) async fn type_file(&self, slug: &str, file: &str) -> Result<Value, FrameError> {
        let tail: Vec<String> = file.split('/').map(encode).collect();
        let path = format!("/api/frame/types/{}/files/{}", encode(slug), tail.join("/"));
        self.call(Method::GET, &path, &RequestBody::Empty).await
    }

    pub(crate) async fn type_instances(&self, slug: &str) -> Result<Value, FrameError> {
        let path = format!("/api/frame/types/{}/instances?limit=200", encode(slug));
        self.call(Method::GET, &path, &RequestBody::Empty).await
    }

    pub(crate) async fn create_from_type(&self, type_slug: &str, title: Option<&str>) -> Result<Value, FrameError> {
        let mut body = json!({ "target_type_version": null });
        if let Some(t) = title.filter(|t| !t.is_empty()) {
            body["title"] = json!(t);
        }
        let path = format!("/api/frame/types/{}/create", encode(type_slug));
        self.call(Method::POST, &path, &RequestBody::json(&body)).await
    }

    pub(crate) async fn asset_upload(
        &self,
        slug: &str,
        file: PathBuf,
        content_type: &str,
    ) -> Result<Value, FrameError> {
        let path = format!("/api/frame/blob/{}/agent-upload", encode(slug));
        self.call(Method::POST, &path, &raw_file(file, content_type)).await
    }

    pub(crate) async fn asset_list(&self, slug: &str, after: Option<&str>) -> Result<Value, FrameError> {
        let mut body = json!({ "limit": 50 });
        if let Some(a) = after {
            body["after"] = json!(a);
        }
        let path = format!("/api/frame/blob/{}/agent-list", encode(slug));
        self.call(Method::POST, &path, &RequestBody::json(&body)).await
    }

    pub(crate) async fn asset_delete(&self, slug: &str, id: &str) -> Result<Value, FrameError> {
        let path = format!("/api/frame/blob/{}/{}/agent-delete", encode(slug), encode(id));
        self.call(Method::POST, &path, &RequestBody::json(&json!({}))).await
    }

    pub(crate) async fn asset_copy(&self, slug: &str, from: &str, ids: &[String]) -> Result<Value, FrameError> {
        let path = format!("/api/frame/blob/{}/agent-copy", encode(slug));
        self.call(Method::POST, &path, &RequestBody::json(&json!({ "from": from, "ids": ids }))).await
    }

    pub(crate) async fn comments(&self, slug: &str) -> Result<Value, FrameError> {
        let path = format!("/api/frame/comments/{}?names=1", encode(slug));
        self.call(Method::GET, &path, &RequestBody::Empty).await
    }

    /// An automatic reply: one that answers the comments sent to Claude
    /// (`answers_summon`), or one that follows the acknowledgement it posted
    /// (`continues_reply_id`), as Claude Code posts them.
    pub(crate) async fn comment_post(
        &self,
        slug: &str,
        thread: &str,
        text: &str,
        answers_summon: bool,
        continues: Option<&str>,
    ) -> Result<Value, FrameError> {
        let path = format!("/api/frame/comments/{}/{}", encode(slug), encode(thread));
        let mut body = json!({ "text": text });
        if answers_summon {
            body["answers_summon"] = json!(true);
        } else if let Some(id) = continues {
            body["continues_reply_id"] = json!(id);
        }
        self.call(Method::POST, &path, &RequestBody::json(&body)).await
    }

    /// What happened to a comment sent to Claude; the answer's status.
    pub(crate) async fn summon_status(&self, slug: &str, thread: &str, body: &Value) -> Result<u16, FrameError> {
        let path = format!("/api/frame/comments/{}/{}/summon-status", encode(slug), encode(thread));
        let (status, _, _) = self.call_raw(Method::POST, &path, &RequestBody::json(body), &[]).await?;
        Ok(status)
    }

    pub(crate) async fn comment_reply(&self, slug: &str, thread: &str, text: &str) -> Result<Value, FrameError> {
        let path = format!("/api/frame/comments/{}/{}", encode(slug), encode(thread));
        self.call(Method::POST, &path, &RequestBody::json(&json!({ "text": text }))).await
    }

    pub(crate) async fn comment_resolve(
        &self,
        slug: &str,
        thread: &str,
        after_version: &str,
    ) -> Result<Value, FrameError> {
        let path = format!("/api/frame/comments/{}/{}/resolve", encode(slug), encode(thread));
        let body = json!({ "resolved": true, "after_version": after_version });
        self.call(Method::POST, &path, &RequestBody::json(&body)).await
    }

    /// The shared database: `{slug, verb, args, asLevel?}`, any status back.
    pub(crate) async fn db(&self, body: &Value) -> Result<(u16, Value), FrameError> {
        let (status, _, bytes) =
            self.call_raw(Method::POST, "/api/frame/db/agent", &RequestBody::json(body), &[]).await?;
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        Ok((status, value))
    }

    /// The runtime contract: `latest`, or one version. `{version,
    /// capabilities: [name], core, features}`.
    pub(crate) async fn contract(&self, version: &str) -> Result<Value, FrameError> {
        let path = format!("/api/frame/contract/{}", encode(version));
        self.call(Method::GET, &path, &RequestBody::Empty).await
    }

    /// What a publish needs to know about an existing artifact:
    /// `{owned, public, contract, …}`.
    pub(crate) async fn read_meta(&self, slug: &str) -> Result<Value, FrameError> {
        self.call(Method::GET, &format!("/api/frame/read/{}", encode(slug)), &RequestBody::Empty).await
    }

    pub(crate) async fn profiles(&self, slug: &str, ids: &[String]) -> Result<Value, FrameError> {
        let path = format!("/api/frame/user/profiles/{}", encode(slug));
        self.call(Method::POST, &path, &RequestBody::json(&json!({ "ids": ids }))).await
    }
}

//! `ListMcpResourcesTool` and `ReadMcpResourceTool`, as Claude Code defines them.

use std::sync::{Arc, Weak};

use eos_agent_core::{BoxFuture, Tool, ToolContext, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};

use crate::content::save_blob_at;
use crate::server::{self, Server};
use crate::tool::{blob_dir, normalize};

const LIST_NAME: &str = "ListMcpResourcesTool";
const READ_NAME: &str = "ReadMcpResourceTool";
const SCHEMA_URL: &str = "https://json-schema.org/draft/2020-12/schema";

pub(crate) struct ListResources {
    spec: ToolSpec,
    servers: Arc<[Weak<Server>]>,
}

pub(crate) struct ReadResource {
    spec: ToolSpec,
    servers: Arc<[Weak<Server>]>,
}

impl std::fmt::Debug for ListResources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(LIST_NAME)
    }
}

impl std::fmt::Debug for ReadResource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(READ_NAME)
    }
}

impl ListResources {
    pub(crate) fn new(servers: Arc<[Weak<Server>]>) -> Self {
        let schema = json!({
            "$schema": SCHEMA_URL,
            "additionalProperties": false,
            "properties": {"server": {"description": "Optional server name to filter resources by", "type": "string"}},
            "type": "object",
        });
        let spec = ToolSpec::new(LIST_NAME, include_str!("../prompts/list_mcp_resources.md"), schema)
            .deferred()
            .with_search_hint("list resources from connected MCP servers");
        Self { spec, servers }
    }
}

impl ReadResource {
    pub(crate) fn new(servers: Arc<[Weak<Server>]>) -> Self {
        let schema = json!({
            "$schema": SCHEMA_URL,
            "additionalProperties": false,
            "properties": {
                "server": {"description": "The MCP server name", "type": "string"},
                "uri": {"description": "The resource URI to read", "type": "string"},
            },
            "required": ["server", "uri"],
            "type": "object",
        });
        let spec = ToolSpec::new(READ_NAME, include_str!("../prompts/read_mcp_resource.md"), schema)
            .deferred()
            .with_search_hint("read a specific MCP resource by URI");
        Self { spec, servers }
    }
}

fn live(servers: &[Weak<Server>]) -> Vec<Arc<Server>> {
    servers.iter().filter_map(Weak::upgrade).collect()
}

/// Exact name first, then the normalized one (`my server` ↔ `my_server`).
fn find<'a>(servers: &'a [Arc<Server>], name: &str) -> Vec<&'a Arc<Server>> {
    let exact: Vec<_> = servers.iter().filter(|s| s.config.name == name).collect();
    if !exact.is_empty() {
        return exact;
    }
    let wanted = normalize(name);
    servers.iter().filter(|s| normalize(&s.config.name) == wanted).collect()
}

fn not_found(name: &str, servers: &[Arc<Server>]) -> ToolOutput {
    let names: Vec<&str> = servers.iter().map(|s| s.config.name.as_str()).collect();
    ToolOutput::error(format!("Server \"{name}\" not found. Available servers: {}", names.join(", ")))
}

impl Tool for ListResources {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, _input: &RawValue) -> bool {
        true
    }

    fn call<'a>(&'a self, input: &'a RawValue, _ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        #[derive(Deserialize)]
        struct Input {
            server: Option<String>,
        }
        Box::pin(async move {
            let input: Input = match serde_json::from_str(input.get()) {
                Ok(input) => input,
                Err(e) => return ToolOutput::error(format!("Invalid input: {e}")),
            };
            let servers = live(&self.servers);
            let chosen: Vec<&Arc<Server>> = match &input.server {
                Some(name) => {
                    let found = find(&servers, name);
                    if found.is_empty() {
                        return not_found(name, &servers);
                    }
                    found
                }
                None => servers.iter().collect(),
            };
            let lists = futures_util::future::join_all(chosen.iter().map(|s| async move {
                if !s.catalog().resources {
                    return Vec::new();
                }
                // A server that fails to list contributes nothing, as in Claude Code.
                let mut listed = s.list_resources().await.unwrap_or_default();
                listed.retain(|r| !r.get("uri").and_then(Value::as_str).is_some_and(|u| u.starts_with("ui://")));
                for resource in &mut listed {
                    if let Value::Object(map) = resource {
                        map.insert("server".into(), Value::String(s.config.name.clone()));
                    }
                }
                listed
            }))
            .await;
            let all: Vec<Value> = lists.into_iter().flatten().collect();
            if all.is_empty() {
                ToolOutput::text(
                    "No resources found. MCP servers may still provide tools even if they have no resources.",
                )
            } else {
                ToolOutput::text(Value::Array(all).to_string())
            }
        })
    }
}

impl Tool for ReadResource {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, _input: &RawValue) -> bool {
        true
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        #[derive(Deserialize)]
        struct Input {
            server: String,
            uri: String,
        }
        Box::pin(async move {
            let input: Input = match serde_json::from_str(input.get()) {
                Ok(input) => input,
                Err(e) => return ToolOutput::error(format!("Invalid input: {e}")),
            };
            let servers = live(&self.servers);
            let Some(server) = find(&servers, &input.server).into_iter().next() else {
                return not_found(&input.server, &servers);
            };
            let name = &server.config.name;
            if !server.catalog().resources {
                return ToolOutput::error(format!("Server \"{name}\" does not support resources"));
            }
            let result = match server.read_resource(&input.uri).await {
                Ok(result) => result,
                // These two come back as the result's `error`, not as a failed call.
                Err(e) if server::is_method_not_found(&e) => {
                    return ToolOutput::text(format!(
                        "Server \"{name}\" advertises resource support but does not implement resource reads."
                    ));
                }
                Err(e) if server::is_not_found(&e) => {
                    return ToolOutput::text(format!(
                        "Resource not found: {} \u{2014} it may have been deleted or the URI is stale. Re-run \
                         {LIST_NAME} to refresh.",
                        input.uri
                    ));
                }
                Err(e) => return ToolOutput::error(e.message),
            };
            let dir = blob_dir(&ctx);
            let mut contents = Vec::new();
            for item in result.get("contents").and_then(Value::as_array).into_iter().flatten() {
                contents.push(read_item(name, item, &dir).await);
            }
            ToolOutput::text(json!({"contents": contents}).to_string())
        })
    }
}

/// One entry of `contents`: text kept, a blob saved to disk with a note.
async fn read_item(server: &str, item: &Value, dir: &std::path::Path) -> Value {
    let field = |key: &str| item.get(key).and_then(Value::as_str);
    let uri = field("uri").unwrap_or_default();
    let mut out = Map::new();
    out.insert("uri".into(), uri.into());
    if let Some(mime) = field("mimeType") {
        out.insert("mimeType".into(), mime.into());
    }
    if let Some(text) = field("text") {
        out.insert("text".into(), text.into());
    } else if let Some(blob) = field("blob") {
        let prefix = format!("[Resource from {server} at {uri}] ");
        let (note, path) = save_blob_at(dir, server, blob, field("mimeType").unwrap_or_default(), &prefix).await;
        if let Some(path) = path {
            out.insert("blobSavedTo".into(), path.display().to_string().into());
        }
        out.insert("text".into(), note.into());
    }
    Value::Object(out)
}

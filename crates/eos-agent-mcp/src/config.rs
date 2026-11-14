//! Claude Code's `mcpServers` JSON, parsed into typed server configs.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Map, Value};

/// One configured server. Two configs that compare equal share one connection
/// (unless `shared` is off), so the derive covers every field that changes
/// what is started or reached.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct McpServerConfig {
    /// The key in `mcpServers`; tools are named `mcp__<name>__<tool>`.
    pub name: String,
    pub transport: McpTransport,
    /// One instance for every agent using this config (default). Off: each
    /// toolset gets its own, for servers that keep per-session state.
    pub shared: bool,
    /// Send every tool of this server with its full definition instead of
    /// deferring it behind tool search.
    pub always_load: bool,
    /// Limit for one tool call (Claude Code's per-server `timeout`, ms).
    pub tool_timeout: Option<Duration>,
    /// How the config wrote what a sign-in's storage key hashes.
    pub key_hints: KeyHints,
}

/// What Claude Code's sign-in storage key hashes as the config wrote it:
/// the `type` (`http`, `streamable-http`, `sse`) and the headers' order
/// (documents read for MCP configs note it as `__headerOrder`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct KeyHints {
    pub type_name: Option<String>,
    pub header_order: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    /// Streamable HTTP (`"type": "http"` or `"streamable-http"`).
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        oauth: Option<OAuthConfig>,
    },
    /// The legacy HTTP+SSE transport (our own small client; rmcp has none).
    Sse {
        url: String,
        headers: BTreeMap<String, String>,
        oauth: Option<OAuthConfig>,
    },
}

/// A remote server's `oauth` settings (Claude Code's schema). Without them a
/// server that answers 401 is still signed in to, through discovery and
/// dynamic client registration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct OAuthConfig {
    /// A client registered ahead of time; its secret, when it has one, is
    /// read from the credential store (`mcpOAuthClientConfig`).
    pub client_id: Option<String>,
    /// A fixed callback port, for providers that need the exact redirect URI
    /// registered (`http://localhost:<port>/callback`).
    pub callback_port: Option<u16>,
    /// The authorization server's metadata document (https), skipping discovery.
    pub auth_server_metadata_url: Option<String>,
    /// Scopes to ask for, space-separated, instead of what the server offers.
    pub scopes: Option<String>,
    /// Cross-app access (Claude Code `oauth.xaa`). Parsed, not supported.
    pub xaa: bool,
}

impl McpTransport {
    pub fn is_stdio(&self) -> bool {
        matches!(self, Self::Stdio { .. })
    }

    /// The remote server's URL (`None` for stdio).
    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Http { url, .. } | Self::Sse { url, .. } => Some(url),
            Self::Stdio { .. } => None,
        }
    }
}

/// A config entry that could not be used. The other servers still load.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("MCP server \"{server}\": {message}")]
pub struct ConfigError {
    pub server: String,
    pub message: String,
}

/// Every usable server of one or more config documents, plus the entries that
/// were left out and why.
#[derive(Debug, Clone, Default)]
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
    pub errors: Vec<ConfigError>,
}

/// Where a config document came from, ordered from lowest to highest
/// precedence as Claude Code merges them (2.1.319): claude.ai connectors,
/// plugins, user (`~/.claude.json`), project (`.mcp.json`, approved entries
/// only), local (per-project entry in `~/.claude.json`), managed settings,
/// dynamic (`--mcp-config` / SDK `mcpServers`). A later scope replaces a
/// server of the same name. When an enterprise `managed-mcp.json` exists it
/// takes exclusive control: only managed and enterprise servers load, the
/// enterprise ones winning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum McpScope {
    ClaudeAi,
    Plugin,
    User,
    Project,
    Local,
    Managed,
    Dynamic,
    Enterprise,
}

impl McpConfig {
    /// Parses `{"mcpServers": {...}}` or the bare `{name: config}` map.
    pub fn from_json(doc: &Value) -> Self {
        let mut out = Self::default();
        for (name, entry) in servers_of(doc) {
            match parse_server(name, entry) {
                Ok(server) => out.servers.push(server),
                Err(message) => out.errors.push(ConfigError { server: name.clone(), message }),
            }
        }
        out
    }

    /// Merges documents from several scopes (see [`McpScope`] for the order).
    pub fn merge(docs: &[(McpScope, &Value)]) -> Self {
        let exclusive = docs.iter().any(|(scope, _)| *scope == McpScope::Enterprise);
        let mut ordered: Vec<&(McpScope, &Value)> = docs
            .iter()
            .filter(|(scope, _)| !exclusive || matches!(scope, McpScope::Managed | McpScope::Enterprise))
            .collect();
        ordered.sort_by_key(|(scope, _)| *scope);

        let mut out = Self::default();
        for (_, doc) in ordered {
            let parsed = Self::from_json(doc);
            for server in parsed.servers {
                // Like `Object.assign`: a later scope's entry keeps the first one's position.
                match out.servers.iter_mut().find(|s| s.name == server.name) {
                    Some(slot) => *slot = server,
                    None => out.servers.push(server),
                }
            }
            out.errors.extend(parsed.errors);
        }
        out
    }
}

impl McpServerConfig {
    pub fn stdio(name: &str, command: &str, args: &[&str]) -> Self {
        Self::new(
            name,
            McpTransport::Stdio {
                command: command.into(),
                args: args.iter().map(|a| (*a).to_owned()).collect(),
                env: BTreeMap::new(),
                cwd: None,
            },
        )
    }

    pub fn http(name: &str, url: &str) -> Self {
        Self::new(name, McpTransport::Http { url: url.into(), headers: BTreeMap::new(), oauth: None })
    }

    pub fn new(name: &str, transport: McpTransport) -> Self {
        Self {
            name: name.into(),
            transport,
            shared: true,
            always_load: false,
            tool_timeout: None,
            key_hints: KeyHints::default(),
        }
    }

    /// Expands `${VAR}` and `${VAR:-default}` in the command, args, env
    /// values, URL and header values, as Claude Code does for every scope but
    /// managed ones. Returns the names that had no value (left as written).
    pub fn expand_vars(&mut self, lookup: impl Fn(&str) -> Option<String>) -> Vec<String> {
        let mut missing = Vec::new();
        let mut fill = |s: &mut String| *s = expand(s, &lookup, &mut missing);
        match &mut self.transport {
            McpTransport::Stdio { command, args, env, .. } => {
                fill(command);
                args.iter_mut().for_each(&mut fill);
                env.values_mut().for_each(&mut fill);
            }
            McpTransport::Http { url, headers, .. } | McpTransport::Sse { url, headers, .. } => {
                fill(url);
                headers.values_mut().for_each(&mut fill);
            }
        }
        missing.sort();
        missing.dedup();
        missing
    }
}

/// Where the compat layer notes a `headers` object's written key order.
const HEADER_ORDER: &str = "__headerOrder";

fn servers_of(doc: &Value) -> impl Iterator<Item = (&String, &Value)> {
    let map = match doc.get("mcpServers") {
        Some(Value::Object(inner)) => Some(inner),
        Some(_) => None,
        None => doc.as_object(),
    };
    map.into_iter().flat_map(Map::iter)
}

fn parse_server(name: &str, entry: &Value) -> Result<McpServerConfig, String> {
    if name.is_empty() {
        return Err("the server name is empty".into());
    }
    let obj = entry.as_object().ok_or("the entry is not an object")?;
    let kind = match obj.get("type") {
        None | Some(Value::Null) => "stdio",
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err("\"type\" is not a string".into()),
    };
    let transport = match kind {
        "stdio" => McpTransport::Stdio {
            command: string(obj, "command")?.ok_or("\"command\" is required")?,
            args: strings(obj, "args")?,
            env: string_map(obj, "env")?,
            cwd: string(obj, "cwd")?.map(PathBuf::from),
        },
        "http" | "streamable-http" => {
            McpTransport::Http { url: url(obj)?, headers: string_map(obj, "headers")?, oauth: oauth(obj)? }
        }
        "sse" => McpTransport::Sse { url: url(obj)?, headers: string_map(obj, "headers")?, oauth: oauth(obj)? },
        other => return Err(format!("transport type \"{other}\" is not supported")),
    };
    let tool_timeout = match obj.get("timeout") {
        None | Some(Value::Null) => None,
        Some(v) => {
            Some(Duration::from_millis(v.as_u64().filter(|ms| *ms >= 1000).ok_or("\"timeout\" must be ≥ 1000 ms")?))
        }
    };
    Ok(McpServerConfig {
        name: name.into(),
        transport,
        shared: flag(obj, "shared")?.unwrap_or(true),
        always_load: flag(obj, "alwaysLoad")?.unwrap_or(false),
        tool_timeout,
        key_hints: KeyHints {
            type_name: obj.get("type").and_then(Value::as_str).map(str::to_owned),
            header_order: strings(obj, HEADER_ORDER).unwrap_or_default(),
        },
    })
}

fn url(obj: &Map<String, Value>) -> Result<String, String> {
    let url = string(obj, "url")?.ok_or("\"url\" is required")?;
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(url)
    } else {
        Err(format!("\"url\" must be an http(s) URL, got \"{url}\""))
    }
}

/// Claude Code's checks: `authServerMetadataUrl` is https and at most 4096
/// characters, `scopes` is not empty, `callbackPort` is a positive integer.
fn oauth(obj: &Map<String, Value>) -> Result<Option<OAuthConfig>, String> {
    let o = match obj.get("oauth") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(o)) => o,
        Some(_) => return Err("\"oauth\" is not an object".into()),
    };
    let callback_port = match o.get("callbackPort") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|p| *p > 0)
                .and_then(|p| u16::try_from(p).ok())
                .ok_or("\"oauth.callbackPort\" must be a port number")?,
        ),
    };
    let metadata = string(o, "authServerMetadataUrl")?;
    if let Some(url) = &metadata {
        if url.len() > 4096 {
            return Err("authServerMetadataUrl is too long".into());
        }
        if !url.starts_with("https://") {
            return Err("authServerMetadataUrl must use https://".into());
        }
    }
    let scopes = string(o, "scopes")?;
    if scopes.as_deref() == Some("") {
        return Err("\"oauth.scopes\" is empty".into());
    }
    Ok(Some(OAuthConfig {
        client_id: string(o, "clientId")?,
        callback_port,
        auth_server_metadata_url: metadata,
        scopes,
        xaa: o.get("xaa").is_some_and(|v| !v.is_null() && v != &Value::Bool(false)),
    }))
}

fn string(obj: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("\"{key}\" is not a string")),
    }
}

fn flag(obj: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("\"{key}\" is not a boolean")),
    }
}

fn strings(obj: &Map<String, Value>, key: &str) -> Result<Vec<String>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map(str::to_owned).ok_or_else(|| format!("\"{key}\" holds a non-string")))
            .collect(),
        Some(_) => Err(format!("\"{key}\" is not an array")),
    }
}

fn string_map(obj: &Map<String, Value>, key: &str) -> Result<BTreeMap<String, String>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(k, v)| match v {
                Value::String(s) => Ok((k.clone(), s.clone())),
                _ => Err(format!("\"{key}.{k}\" is not a string")),
            })
            .collect(),
        Some(_) => Err(format!("\"{key}\" is not an object")),
    }
}

fn expand(s: &str, lookup: &impl Fn(&str) -> Option<String>, missing: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        let Some(len) = rest[start..].find('}') else { break };
        out.push_str(&rest[..start]);
        let inner = &rest[start + 2..start + len];
        let (var, default) = match inner.split_once(":-") {
            Some((var, default)) => (var, Some(default)),
            None => (inner, None),
        };
        match lookup(var).or_else(|| default.map(str::to_owned)) {
            Some(value) => out.push_str(&value),
            None => {
                missing.push(var.to_owned());
                out.push_str(&rest[start..=start + len]);
            }
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_every_transport() {
        let cfg = McpConfig::from_json(&json!({"mcpServers": {
            "fs": {"command": "npx", "args": ["-y", "server"], "env": {"A": "1"}, "shared": false},
            "web": {"type": "http", "url": "https://x.dev/mcp", "headers": {"Authorization": "Bearer t"}, "alwaysLoad": true},
            "old": {"type": "sse", "url": "http://localhost:9/sse", "timeout": 5000},
            "ide": {"type": "ws-ide", "url": "ws://x"},
            "bad": {"type": "stdio"},
        }}));
        let get = |name: &str| cfg.servers.iter().find(|s| s.name == name).expect("parsed");
        assert_eq!(cfg.servers.len(), 3);
        assert!(!get("fs").shared && get("fs").transport.is_stdio());
        assert!(
            get("web").always_load
                && matches!(&get("web").transport, McpTransport::Http { headers, .. } if headers.len() == 1)
        );
        assert_eq!(get("old").tool_timeout, Some(Duration::from_secs(5)));
        let failed: Vec<&str> = cfg.errors.iter().map(|e| e.server.as_str()).collect();
        assert_eq!(failed, ["bad", "ide"]);
    }

    #[test]
    fn parses_oauth_settings() {
        let cfg = McpConfig::from_json(&json!({
            "a": {"type": "http", "url": "https://a.dev/mcp", "oauth": {"clientId": "c", "callbackPort": 8080,
                  "authServerMetadataUrl": "https://as.dev/.well-known/oauth-authorization-server", "scopes": "read write"}},
            "b": {"type": "http", "url": "https://b.dev/mcp", "oauth": {"authServerMetadataUrl": "http://as.dev/m"}},
            "c": {"type": "sse", "url": "https://c.dev/sse", "oauth": {"callbackPort": 0}},
        }));
        let McpTransport::Http { oauth: Some(o), .. } = &cfg.servers[0].transport else { panic!("oauth") };
        assert_eq!(
            (o.client_id.as_deref(), o.callback_port, o.scopes.as_deref()),
            (Some("c"), Some(8080), Some("read write"))
        );
        let failed: Vec<&str> = cfg.errors.iter().map(|e| e.server.as_str()).collect();
        assert_eq!(failed, ["b", "c"]);
    }

    #[test]
    fn later_scopes_win_and_enterprise_is_exclusive() {
        let user = json!({"a": {"command": "user-a"}, "b": {"command": "user-b"}});
        let local = json!({"mcpServers": {"a": {"command": "local-a"}}});
        let merged = McpConfig::merge(&[(McpScope::Local, &local), (McpScope::User, &user)]);
        let names: Vec<(&str, &McpTransport)> =
            merged.servers.iter().map(|s| (s.name.as_str(), &s.transport)).collect();
        assert_eq!(names.len(), 2);
        assert!(matches!(names[0], ("a", McpTransport::Stdio { command, .. }) if command == "local-a"));

        let enterprise = json!({"e": {"command": "corp"}});
        let merged = McpConfig::merge(&[(McpScope::User, &user), (McpScope::Enterprise, &enterprise)]);
        assert_eq!(merged.servers.len(), 1);
        assert_eq!(merged.servers[0].name, "e");
    }

    #[test]
    fn expands_variables() {
        let mut cfg = McpServerConfig::stdio("s", "${BIN}", &["--root=${ROOT:-/tmp}", "${NOPE}"]);
        let missing = cfg.expand_vars(|v| (v == "BIN").then(|| "node".to_owned()));
        assert_eq!(missing, ["NOPE"]);
        let McpTransport::Stdio { command, args, .. } = &cfg.transport else { unreachable!() };
        assert_eq!(command, "node");
        assert_eq!(args, &["--root=/tmp", "${NOPE}"]);
    }
}

use std::collections::HashMap;
use std::process::Command;

use anyhow::{Context, bail};
use serde::Deserialize;

/// Dependency direction: types → core → kits → adapters → engine → sdk → host.
/// A crate may only depend (normally) on crates in a lower layer, so adapters
/// never reach into each other and the core never sees an adapter. The sdk
/// composes everything below it; hosts (CLI, server) build on it. A kit holds
/// I/O plumbing several adapters share (the core does no I/O, and adapters may
/// not depend on each other).
const LAYERS: &[(&str, Layer)] = &[
    ("eos-agent-types", Layer::Types),
    ("eos-agent-core", Layer::Core),
    ("eos-agent-oauth", Layer::Kit),
    ("eos-agent-http", Layer::Kit),
    ("eos-agent-prompt", Layer::Adapter),
    ("eos-agent-anthropic", Layer::Adapter),
    ("eos-agent-openai", Layer::Adapter),
    ("eos-agent-tools-fs", Layer::Adapter),
    ("eos-agent-tools-shell", Layer::Adapter),
    ("eos-agent-tools-web", Layer::Adapter),
    ("eos-agent-tools-session", Layer::Adapter),
    ("eos-agent-agents", Layer::Adapter),
    ("eos-agent-skills", Layer::Adapter),
    ("eos-agent-mcp", Layer::Adapter),
    ("eos-agent-artifacts", Layer::Adapter),
    ("eos-agent-compat", Layer::Adapter),
    ("eos-agent-workflow", Layer::Adapter),
    ("eos-agent-engine", Layer::Engine),
    ("eos-agent-sdk", Layer::Sdk),
    ("eos-agent-server", Layer::Host),
    ("eos-agent-cli", Layer::Host),
    // Test and dev tooling: never a normal dependency of a library crate.
    ("eos-agent-testkit", Layer::Support),
    ("eos-agent-mock-api", Layer::Support),
    ("eos-agent-capture", Layer::Support),
    ("eos-agent-scale-bench", Layer::Support),
    ("xtask", Layer::Support),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Layer {
    Types,
    Core,
    Kit,
    Adapter,
    Engine,
    Sdk,
    Host,
    Support,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    dependencies: Vec<Dep>,
}

#[derive(Deserialize)]
struct Dep {
    name: String,
    kind: Option<String>,
}

pub(crate) fn check() -> anyhow::Result<()> {
    let out = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .context("running cargo metadata")?;
    if !out.status.success() {
        bail!("cargo metadata failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let meta: Metadata = serde_json::from_slice(&out.stdout)?;
    let layers: HashMap<&str, Layer> = LAYERS.iter().copied().collect();

    let mut problems = Vec::new();
    for pkg in meta.packages.iter().filter(|p| meta.workspace_members.contains(&p.id)) {
        let Some(&own) = layers.get(pkg.name.as_str()) else {
            problems.push(format!("{}: not listed in xtask LAYERS", pkg.name));
            continue;
        };
        for dep in &pkg.dependencies {
            let Some(&theirs) = layers.get(dep.name.as_str()) else { continue };
            if let Some(why) = violation(own, theirs, dep.kind.as_deref()) {
                problems.push(format!("{} → {}: {why}", pkg.name, dep.name));
            }
        }
    }

    if problems.is_empty() {
        println!("layers ok");
        Ok(())
    } else {
        bail!("dependency direction violated:\n  {}", problems.join("\n  "))
    }
}

fn violation(own: Layer, theirs: Layer, kind: Option<&str>) -> Option<&'static str> {
    let dev = matches!(kind, Some("dev" | "build"));
    match (own, theirs) {
        // Tooling may use anything; anything may use tooling in tests.
        (Layer::Support, _) => None,
        (_, Layer::Support) if dev => None,
        (_, Layer::Support) => Some("test tooling as a normal dependency"),
        _ if theirs < own => None,
        // Tests may wire adapters together (e.g. a tool test with the fs adapter).
        _ if dev && theirs <= own => None,
        _ => Some("depends on the same or a higher layer"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_rules() {
        assert_eq!(violation(Layer::Core, Layer::Types, None), None);
        assert!(violation(Layer::Core, Layer::Adapter, None).is_some());
        assert_eq!(violation(Layer::Adapter, Layer::Kit, None), None);
        assert!(violation(Layer::Kit, Layer::Adapter, None).is_some());
        assert!(violation(Layer::Core, Layer::Kit, None).is_some());
        assert!(violation(Layer::Adapter, Layer::Adapter, None).is_some());
        assert_eq!(violation(Layer::Adapter, Layer::Adapter, Some("dev")), None);
        assert!(violation(Layer::Adapter, Layer::Support, None).is_some());
        assert_eq!(violation(Layer::Adapter, Layer::Support, Some("dev")), None);
        assert_eq!(violation(Layer::Host, Layer::Engine, None), None);
        assert_eq!(violation(Layer::Host, Layer::Sdk, None), None);
        assert!(violation(Layer::Sdk, Layer::Host, None).is_some());
    }
}

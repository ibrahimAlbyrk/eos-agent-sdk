//! The model catalog a ChatGPT sign-in sees (`GET /v1/models`): which
//! slugs to offer, their context windows and reasoning levels. Kept on disk
//! beside the sign-in and refreshed at most every ten minutes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use eos_agent_oauth::now_ms;
use serde_json::Value;

use crate::OpenAiClient;

/// A model id written `openai/<slug>` runs on OpenAI whatever its slug.
pub const PREFIX: &str = "openai/";
/// The alias for the catalog's first listed model.
pub const ALIAS: &str = "gpt";
/// How old a catalog may get before it is fetched again.
pub const MAX_AGE: Duration = Duration::from_secs(600);
/// When the catalog does not say.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 272_000;
/// Reasoning levels, lowest first.
const LEVELS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];

/// Whether `id` names an OpenAI model: `gpt-*`, `o<digit>*`, `codex-*`,
/// `chatgpt-*`, the `gpt` alias, or anything written `openai/…`.
pub fn is_openai_model(id: &str) -> bool {
    id == ALIAS
        || id.starts_with(PREFIX)
        || id.starts_with("gpt-")
        || id.starts_with("codex-")
        || id.starts_with("chatgpt-")
        || id.strip_prefix('o').is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
}

/// `id` without the `openai/` prefix.
pub fn slug(id: &str) -> &str {
    id.strip_prefix(PREFIX).unwrap_or(id)
}

/// One model as the catalog describes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelInfo {
    pub slug: String,
    pub display_name: Option<String>,
    /// Meant to be offered (`visibility: "list"`).
    pub listed: bool,
    pub context_window: Option<u64>,
    pub default_effort: Option<String>,
    /// Reasoning levels it takes (empty: unknown).
    pub efforts: Vec<String>,
    /// `false` when the catalog says it gives no reasoning summaries.
    pub summaries: bool,
}

/// The catalog as fetched.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub models: Vec<ModelInfo>,
    /// Unix ms (0: never fetched).
    pub fetched_at: u64,
}

impl Catalog {
    /// Reads `{"models":[…]}` (or an API-style `{"data":[…]}`), keeping the server's order.
    pub fn parse(doc: &Value) -> Self {
        let list = doc["models"].as_array().or_else(|| doc["data"].as_array()).cloned().unwrap_or_default();
        let models = list.iter().filter_map(model).collect();
        Self { models, fetched_at: doc["fetched_at"].as_u64().unwrap_or(0) }
    }

    pub fn get(&self, slug: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|m| m.slug == slug)
    }

    /// The first listed model: what `gpt` means.
    pub fn default_model(&self) -> Option<&ModelInfo> {
        self.models.iter().find(|m| m.listed)
    }

    /// A small listed model for side calls (WebFetch's summaries), else the default.
    pub fn small_model(&self) -> Option<&ModelInfo> {
        let small = |m: &&ModelInfo| m.listed && ["mini", "nano", "luna"].iter().any(|s| m.slug.contains(s));
        self.models.iter().find(small).or_else(|| self.default_model())
    }

    /// The reasoning effort to send `slug`: `wanted` (else its default)
    /// lowered to the nearest level it takes; passed through when unknown.
    pub fn effort(&self, slug: &str, wanted: Option<&str>) -> Option<String> {
        let model = self.get(slug);
        let wanted = wanted.or(model.and_then(|m| m.default_effort.as_deref()))?;
        let Some(model) = model.filter(|m| !m.efforts.is_empty()) else { return Some(wanted.to_owned()) };
        nearest_effort(&model.efforts.iter().map(String::as_str).collect::<Vec<_>>(), wanted)
    }

    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        Some(Self::parse(&serde_json::from_str(&text).ok()?))
    }

    fn save(&self, raw: &Value, path: &Path) {
        let mut doc = raw.clone();
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("fetched_at".into(), self.fetched_at.into());
        }
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, doc.to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

fn model(v: &Value) -> Option<ModelInfo> {
    let slug = v["slug"].as_str().or_else(|| v["id"].as_str())?.to_owned();
    let efforts = v["supported_reasoning_levels"]
        .as_array()
        .map(|levels| {
            levels.iter().filter_map(|l| l.as_str().or_else(|| l["effort"].as_str()).map(str::to_owned)).collect()
        })
        .unwrap_or_default();
    Some(ModelInfo {
        display_name: v["display_name"].as_str().map(str::to_owned),
        listed: v["visibility"].as_str().is_none_or(|vis| vis == "list"),
        context_window: v["context_window"].as_u64().or_else(|| v["max_context_window"].as_u64()),
        default_effort: v["default_reasoning_level"].as_str().map(str::to_owned),
        efforts,
        summaries: v["supports_reasoning_summaries"].as_bool() != Some(false),
        slug,
    })
}

/// The catalog one process shares: read from disk, fetched when missing or
/// older than [`MAX_AGE`] (in the background once there is one).
pub struct ModelCatalog {
    client: Arc<OpenAiClient>,
    path: Option<PathBuf>,
    current: Mutex<Arc<Catalog>>,
    refreshing: AtomicBool,
}

impl std::fmt::Debug for ModelCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelCatalog").field("path", &self.path).finish_non_exhaustive()
    }
}

impl ModelCatalog {
    /// `cache`: where the catalog is kept between runs (the sign-in's store dir).
    pub fn new(client: Arc<OpenAiClient>, cache: Option<&Path>) -> Arc<Self> {
        let path = cache.map(|dir| dir.join("models_cache.json"));
        let loaded = path.as_deref().and_then(Catalog::load).unwrap_or_default();
        Arc::new(Self { client, path, current: Mutex::new(Arc::new(loaded)), refreshing: AtomicBool::new(false) })
    }

    /// The catalog as it stands (possibly empty).
    pub fn snapshot(&self) -> Arc<Catalog> {
        Arc::clone(&self.current.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Fetches now unless the one held is fresh. A failed fetch keeps the old one.
    pub async fn ensure(&self) -> Arc<Catalog> {
        let held = self.snapshot();
        if !held.models.is_empty() && now_ms().saturating_sub(held.fetched_at) < MAX_AGE.as_millis() as u64 {
            return held;
        }
        self.fetch().await.unwrap_or(held)
    }

    /// Refreshes in the background when the catalog is stale.
    pub fn refresh_if_stale(self: &Arc<Self>) {
        let held = self.snapshot();
        if now_ms().saturating_sub(held.fetched_at) < MAX_AGE.as_millis() as u64
            || self.refreshing.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let _ = this.fetch().await;
            this.refreshing.store(false, Ordering::Release);
        });
    }

    async fn fetch(&self) -> Option<Arc<Catalog>> {
        let raw = self.client.list_models().await.ok()?;
        let mut catalog = Catalog::parse(&raw);
        if catalog.models.is_empty() {
            return None;
        }
        catalog.fetched_at = now_ms();
        if let Some(path) = &self.path {
            catalog.save(&raw, path);
        }
        let catalog = Arc::new(catalog);
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Arc::clone(&catalog);
        Some(catalog)
    }
}

/// `wanted` when `levels` has it, else the nearest level below it, else
/// the least (`None`: there are no levels).
pub fn nearest_effort(levels: &[&str], wanted: &str) -> Option<String> {
    if levels.contains(&wanted) {
        return Some(wanted.to_owned());
    }
    let rank = |e: &str| LEVELS.iter().position(|l| *l == e);
    let target = rank(wanted).unwrap_or(LEVELS.len());
    let below = levels.iter().filter(|e| rank(e).is_some_and(|r| r <= target)).max_by_key(|e| rank(e));
    below.or_else(|| levels.iter().min_by_key(|e| rank(e).unwrap_or(usize::MAX))).map(|e| (*e).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openai_ids() {
        for id in ["gpt", "gpt-5.5", "gpt-6-sol", "o3", "o4-mini", "codex-mini", "openai/anything"] {
            assert!(is_openai_model(id), "{id}");
        }
        for id in ["claude-opus-5-5", "opus", "haiku", "other", "o", "orca-1"] {
            assert!(!is_openai_model(id), "{id}");
        }
        assert_eq!(slug("openai/x"), "x");
    }

    #[test]
    fn catalog_order_levels_and_picks() {
        let c = Catalog::parse(&json!({"models": [
            {"slug": "gpt-hidden", "visibility": "hide"},
            {"slug": "gpt-6-sol", "display_name": "GPT-6 Sol", "visibility": "list", "context_window": 272000,
             "default_reasoning_level": "medium",
             "supported_reasoning_levels": [{"effort": "low"}, {"effort": "medium"}, {"effort": "high"}, {"effort": "xhigh"}]},
            {"slug": "gpt-6-luna", "visibility": "list", "supported_reasoning_levels": ["low", "medium"],
             "supports_reasoning_summaries": false},
        ]}));
        assert_eq!(c.default_model().map(|m| m.slug.as_str()), Some("gpt-6-sol"));
        assert_eq!(c.small_model().map(|m| m.slug.as_str()), Some("gpt-6-luna"));
        assert_eq!(c.effort("gpt-6-sol", None).as_deref(), Some("medium"));
        assert_eq!(c.effort("gpt-6-sol", Some("max")).as_deref(), Some("xhigh"));
        assert_eq!(c.effort("gpt-6-luna", Some("high")).as_deref(), Some("medium"));
        assert_eq!(c.effort("gpt-6-luna", Some("minimal")).as_deref(), Some("low"));
        assert_eq!(c.effort("unknown", Some("max")).as_deref(), Some("max"));
        assert_eq!(c.effort("unknown", None), None);
        assert!(!c.get("gpt-6-luna").expect("listed").summaries);
        assert_eq!(c.get("gpt-6-sol").and_then(|m| m.context_window), Some(272_000));
    }
}

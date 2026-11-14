//! Reminder sources: files that changed on disk since the agent saw them,
//! and the date rolling over during a session.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::UNIX_EPOCH;

use eos_agent_core::{BoxFuture, Extensions, FileStamp, FileStates, ReminderSource, content_hash};

use crate::library::{Fragment, Prompts, system_reminder};

/// Files above this size are reported as changed without hashing them.
const MAX_HASHED_BYTES: u64 = 16 * 1024 * 1024;

/// The local date as Claude Code prints it (`YYYY-MM-DD`).
pub fn local_date() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Tells the agent once about each file it read or wrote that changed on disk
/// since. Costs one `stat` per tracked file (the tracked set is bounded);
/// content is hashed only when the size or time differ, to tell a touched
/// file from a changed one.
#[derive(Debug, Clone)]
pub struct FileChangeReminders {
    template: Arc<str>,
}

impl FileChangeReminders {
    pub fn new(prompts: &Prompts) -> Self {
        Self { template: prompts.get(Fragment::FileChanged).into() }
    }
}

impl ReminderSource for FileChangeReminders {
    fn collect<'a>(&'a self, extensions: &'a Extensions) -> BoxFuture<'a, Vec<String>> {
        Box::pin(async move {
            let Some(states) = extensions.get::<FileStates>() else { return Vec::new() };
            let changed = tokio::task::spawn_blocking(move || changed_files(&states)).await.unwrap_or_default();
            changed
                .iter()
                .map(|path| {
                    let path = path.display().to_string();
                    system_reminder(&crate::library::render(&self.template, &[("PATH", &path)]))
                })
                .collect()
        })
    }
}

/// Checks every tracked file and re-stamps the changed ones, so each change
/// is reported once. Vanished files are dropped silently.
fn changed_files(states: &FileStates) -> Vec<PathBuf> {
    let mut changed = Vec::new();
    for (path, stamp) in states.snapshot() {
        let meta = match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => m,
            Ok(_) | Err(_) => {
                states.forget(&path);
                continue;
            }
        };
        let modified_ms =
            meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64);
        if modified_ms == stamp.modified_ms && meta.len() == stamp.len {
            continue;
        }
        let hash = if meta.len() <= MAX_HASHED_BYTES { hash_of(&path) } else { None };
        if hash == Some(stamp.hash) && meta.len() == stamp.len {
            states.record(path, FileStamp { modified_ms, ..stamp });
            continue;
        }
        // The agent has not seen this version, so the new stamp is partial.
        states.record(path.clone(), FileStamp { modified_ms, len: meta.len(), hash: hash.unwrap_or(0), partial: true });
        changed.push(path);
    }
    changed.sort();
    changed
}

fn hash_of(path: &Path) -> Option<u64> {
    std::fs::read(path).ok().map(|bytes| content_hash(&bytes))
}

/// The date the agent was last told, per agent.
#[derive(Debug, Default)]
struct KnownDate(Mutex<Option<String>>);

type Today = Arc<dyn Fn() -> String + Send + Sync>;

/// Tells the agent when the local date changed since it was last told (the
/// first check only records the date the session started with).
#[derive(Clone)]
pub struct DateChangeReminder {
    template: Arc<str>,
    today: Today,
}

impl std::fmt::Debug for DateChangeReminder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DateChangeReminder").finish_non_exhaustive()
    }
}

impl DateChangeReminder {
    pub fn new(prompts: &Prompts) -> Self {
        Self { template: prompts.get(Fragment::DateChanged).into(), today: Arc::new(local_date) }
    }

    /// Takes the date from `today` instead of the local clock.
    pub fn with_today(mut self, today: impl Fn() -> String + Send + Sync + 'static) -> Self {
        self.today = Arc::new(today);
        self
    }
}

impl ReminderSource for DateChangeReminder {
    fn collect<'a>(&'a self, extensions: &'a Extensions) -> BoxFuture<'a, Vec<String>> {
        let today = (self.today)();
        let known = extensions.get_or_default::<KnownDate>();
        let mut known = known.0.lock().unwrap_or_else(PoisonError::into_inner);
        let note = match known.as_deref() {
            Some(date) if date != today => {
                vec![system_reminder(&crate::library::render(&self.template, &[("DATE", &today)]))]
            }
            _ => Vec::new(),
        };
        *known = Some(today);
        Box::pin(std::future::ready(note))
    }
}

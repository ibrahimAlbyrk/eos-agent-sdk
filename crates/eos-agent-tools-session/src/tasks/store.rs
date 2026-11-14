//! Task lists on disk, in Claude Code's layout: `<root>/<listId>/<id>.json`
//! (pretty JSON), ids counting up from 1 and never reused (a
//! `.highwatermark` file remembers the highest deleted one).

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const HIGH_WATER_MARK: &str = ".highwatermark";
const STRIPES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

/// One task as Claude Code stores it (same fields, same order).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub subject: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default)]
    pub blocks: Vec<String>,
    #[serde(default)]
    pub blocked_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

/// Where task lists live. Clones share the locks, so one store per process
/// keeps concurrent agents on the same list from interleaving their writes.
/// (Other processes are not locked out, unlike Claude Code's lock files;
/// ids are still claimed with exclusive creates.)
#[derive(Debug, Clone)]
pub struct TaskStore {
    root: PathBuf,
    locks: Arc<[Mutex<()>; STRIPES]>,
}

impl TaskStore {
    /// Claude Code uses `~/.claude/tasks`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), locks: Arc::new(std::array::from_fn(|_| Mutex::new(()))) }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Exclusive access to one list for a sequence of operations (blocking).
    pub fn lock(&self, list_id: &str) -> LockedList<'_> {
        let dir = self.root.join(sanitize(list_id));
        let stripe =
            dir.as_os_str().as_encoded_bytes().iter().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(*b as usize));
        let guard = self.locks[stripe % STRIPES].lock().unwrap_or_else(PoisonError::into_inner);
        LockedList { dir, _guard: guard }
    }
}

/// A task list held for exclusive use.
#[derive(Debug)]
pub struct LockedList<'a> {
    dir: PathBuf,
    _guard: MutexGuard<'a, ()>,
}

impl LockedList<'_> {
    pub fn get(&self, id: &str) -> io::Result<Option<Task>> {
        match std::fs::read(self.file(id)) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::InvalidInput) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Every readable task, in id order.
    pub fn all(&self) -> io::Result<Vec<Task>> {
        let mut tasks: Vec<Task> = self.ids()?.iter().filter_map(|id| self.get(id).ok().flatten()).collect();
        tasks.sort_by_key(|t| t.id.parse::<u64>().unwrap_or(u64::MAX));
        Ok(tasks)
    }

    /// Stores `task` under the next free id and returns it.
    pub fn create(&self, mut task: Task) -> io::Result<Task> {
        std::fs::create_dir_all(&self.dir)?;
        let highest = self.ids()?.iter().filter_map(|id| id.parse::<u64>().ok()).max().unwrap_or(0);
        let mut next = highest.max(self.high_water_mark()) + 1;
        loop {
            task.id = next.to_string();
            let file = std::fs::OpenOptions::new().write(true).create_new(true).open(self.file(&task.id));
            match file {
                Ok(mut f) => {
                    io::Write::write_all(&mut f, &to_json(&task)?)?;
                    return Ok(task);
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => next += 1,
                Err(e) => return Err(e),
            }
        }
    }

    pub fn put(&self, task: &Task) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!(".{}.json.tmp", sanitize(&task.id)));
        std::fs::write(&tmp, to_json(task)?)?;
        std::fs::rename(tmp, self.file(&task.id))
    }

    /// Deletes a task and every reference to it. Returns whether it existed.
    pub fn remove(&self, id: &str) -> io::Result<bool> {
        if let Ok(n) = id.parse::<u64>()
            && n > self.high_water_mark()
        {
            std::fs::create_dir_all(&self.dir)?;
            std::fs::write(self.dir.join(HIGH_WATER_MARK), n.to_string())?;
        }
        match std::fs::remove_file(self.file(id)) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        }
        for mut task in self.all()? {
            let before = task.blocks.len() + task.blocked_by.len();
            task.blocks.retain(|b| b != id);
            task.blocked_by.retain(|b| b != id);
            if task.blocks.len() + task.blocked_by.len() != before {
                self.put(&task)?;
            }
        }
        Ok(true)
    }

    /// Records that `blocker` must finish before `blocked` can start.
    /// Returns false when either task is missing.
    pub fn link(&self, blocker: &str, blocked: &str) -> io::Result<bool> {
        let (Some(mut from), Some(mut to)) = (self.get(blocker)?, self.get(blocked)?) else { return Ok(false) };
        if !from.blocks.iter().any(|b| b == blocked) {
            from.blocks.push(blocked.to_owned());
            self.put(&from)?;
        }
        if !to.blocked_by.iter().any(|b| b == blocker) {
            to.blocked_by.push(blocker.to_owned());
            self.put(&to)?;
        }
        Ok(true)
    }

    fn file(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{}.json", sanitize(id)))
    }

    fn ids(&self) -> io::Result<Vec<String>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".json")).map(str::to_owned))
            .filter(|id| !id.starts_with('.'))
            .collect())
    }

    fn high_water_mark(&self) -> u64 {
        std::fs::read_to_string(self.dir.join(HIGH_WATER_MARK)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
    }
}

fn to_json(task: &Task) -> io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(task).map_err(io::Error::other)
}

/// Claude Code's file-name rule for list and task ids.
fn sanitize(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' }).collect()
}

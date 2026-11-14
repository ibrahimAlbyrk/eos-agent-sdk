//! Claude Code's nudge back to the todo or task list after it has gone
//! unused for a while.

use std::sync::atomic::{AtomicU32, Ordering};

use eos_agent_core::{BoxFuture, Extensions, ReminderSource};

use crate::tasks::{TaskListId, TaskStore};
use crate::todo::TodoList;

/// Remind after this many model turns without a list update…
const TURNS_SINCE_USE: u32 = 10;
/// …and at most once per this many turns.
const TURNS_BETWEEN_REMINDERS: u32 = 10;

/// Model turns since the list was last updated and since the last nudge.
#[derive(Debug, Default)]
struct Counters {
    since_use: AtomicU32,
    since_reminder: AtomicU32,
}

pub(crate) fn mark_used(extensions: &Extensions) {
    extensions.get_or_default::<Counters>().since_use.store(0, Ordering::Relaxed);
}

/// Which list the agent keeps.
#[derive(Debug, Clone)]
pub enum NudgeKind {
    Todos,
    Tasks(TaskStore),
}

/// A reminder source: give it to agents that have TodoWrite (`Todos`) or the
/// Task tools (`Tasks`). Each source call is one model turn.
#[derive(Debug, Clone)]
pub struct TrackingNudge {
    kind: NudgeKind,
}

impl TrackingNudge {
    pub fn new(kind: NudgeKind) -> Self {
        Self { kind }
    }
}

impl ReminderSource for TrackingNudge {
    fn collect<'a>(&'a self, extensions: &'a Extensions) -> BoxFuture<'a, Vec<String>> {
        let counters = extensions.get_or_default::<Counters>();
        let due = counters.since_use.load(Ordering::Relaxed) >= TURNS_SINCE_USE
            && counters.since_reminder.load(Ordering::Relaxed) >= TURNS_BETWEEN_REMINDERS;
        if due {
            counters.since_reminder.store(0, Ordering::Relaxed);
        }
        counters.since_use.fetch_add(1, Ordering::Relaxed);
        counters.since_reminder.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            if !due {
                return Vec::new();
            }
            let text = match &self.kind {
                NudgeKind::Todos => todo_text(&extensions.get_or_default::<TodoList>().items()),
                NudgeKind::Tasks(store) => task_text(store, extensions.get::<TaskListId>()).await,
            };
            vec![crate::system_reminder(&text)]
        })
    }
}

fn todo_text(todos: &[crate::todo::Todo]) -> String {
    let mut text = include_str!("../prompts/TodoReminder.md").to_owned();
    if !todos.is_empty() {
        let lines: Vec<String> = todos
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{}. [{}] {}", i + 1, t.status.as_str(), t.content))
            .collect();
        text.push_str(&format!("\n\nHere are the existing contents of your todo list:\n\n[{}]", lines.join("\n")));
    }
    text
}

async fn task_text(store: &TaskStore, list: Option<std::sync::Arc<TaskListId>>) -> String {
    let mut text = include_str!("../prompts/TaskReminder.md").to_owned();
    let Some(list) = list else { return text };
    let store = store.clone();
    let tasks =
        tokio::task::spawn_blocking(move || store.lock(&list.0).all().unwrap_or_default()).await.unwrap_or_default();
    if !tasks.is_empty() {
        let lines: Vec<String> =
            tasks.iter().map(|t| format!("#{}. [{}] {}", t.id, t.status.as_str(), t.subject)).collect();
        text.push_str(&format!("\n\nHere are the existing tasks:\n\n{}", lines.join("\n")));
    }
    text
}

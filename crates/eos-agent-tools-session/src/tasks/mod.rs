//! Claude Code's Task tools: TaskCreate, TaskGet, TaskUpdate and TaskList
//! over a task list kept on disk.

mod store;

use std::collections::HashSet;
use std::io;

use eos_agent_core::{BoxFuture, ToolContext, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value};

pub use store::{LockedList, Task, TaskStatus, TaskStore};

use crate::nudge;

/// The task list an agent works on, set in its extensions to share one list
/// between agents (Claude Code shares the session's list with subagents).
/// Without it, each agent has its own list named after its id.
#[derive(Debug, Clone)]
pub struct TaskListId(pub String);

/// Recorded on first use, so the task reminder can find the list too.
fn list_id(ctx: &ToolContext<'_>) -> String {
    ctx.extensions.get_or_insert_with(|| TaskListId(ctx.agent_id.to_owned())).0.clone()
}

/// Runs a sequence of list operations on a blocking thread, holding the list.
async fn with_list<T: Send + 'static>(
    store: &TaskStore,
    list: String,
    work: impl FnOnce(&LockedList<'_>) -> io::Result<T> + Send + 'static,
) -> Result<T, ToolOutput> {
    let store = store.clone();
    match tokio::task::spawn_blocking(move || work(&store.lock(&list))).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(ToolOutput::error(format!("Task list could not be read or written: {e}"))),
        Err(e) => Err(ToolOutput::error(format!("Task list operation failed: {e}"))),
    }
}

macro_rules! task_tool {
    ($name:ident, $file:literal, $hint:literal) => {
        #[derive(Debug)]
        pub struct $name {
            spec: ToolSpec,
            store: TaskStore,
        }

        impl $name {
            pub fn new(store: TaskStore) -> Self {
                let spec = crate::spec(
                    $file,
                    include_str!(concat!("../../prompts/", $file, ".md")),
                    include_str!(concat!("../../prompts/", $file, ".schema.json")),
                )
                .with_search_hint($hint);
                Self { spec, store }
            }

            /// Sent with `defer_loading`, as Claude Code does when tool search is on.
            pub fn deferred(mut self) -> Self {
                self.spec = self.spec.deferred();
                self
            }
        }
    };
}

task_tool!(TaskCreate, "TaskCreate", "create a task in the task list");
task_tool!(TaskGet, "TaskGet", "retrieve a task by ID");
task_tool!(TaskUpdate, "TaskUpdate", "update a task");
task_tool!(TaskList, "TaskList", "list all tasks");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateInput {
    subject: String,
    description: String,
    active_form: Option<String>,
    metadata: Option<Map<String, Value>>,
}

impl eos_agent_core::Tool for TaskCreate {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let input: CreateInput = match crate::parse(input) {
                Ok(i) => i,
                Err(e) => return e,
            };
            let task = Task {
                id: String::new(),
                subject: input.subject,
                description: input.description,
                active_form: input.active_form,
                status: TaskStatus::Pending,
                owner: None,
                blocks: Vec::new(),
                blocked_by: Vec::new(),
                metadata: input.metadata,
            };
            match with_list(&self.store, list_id(&ctx), move |list| list.create(task)).await {
                Ok(task) => {
                    nudge::mark_used(ctx.extensions);
                    ToolOutput::text(format!("Task #{} created successfully: {}", task.id, task.subject))
                }
                Err(e) => e,
            }
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GetInput {
    #[serde(alias = "id", alias = "task_id")]
    task_id: String,
}

impl eos_agent_core::Tool for TaskGet {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, _: &RawValue) -> bool {
        true
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let GetInput { task_id } = match crate::parse(input) {
                Ok(i) => i,
                Err(e) => return e,
            };
            match with_list(&self.store, list_id(&ctx), move |list| list.get(&task_id)).await {
                Ok(Some(task)) => ToolOutput::text(describe(&task)),
                Ok(None) => ToolOutput::text("Task not found"),
                Err(e) => e,
            }
        })
    }
}

fn describe(task: &Task) -> String {
    let mut lines = vec![
        format!("Task #{}: {}", task.id, task.subject),
        format!("Status: {}", task.status.as_str()),
        format!("Description: {}", task.description),
    ];
    if !task.blocked_by.is_empty() {
        lines.push(format!("Blocked by: {}", refs(&task.blocked_by)));
    }
    if !task.blocks.is_empty() {
        lines.push(format!("Blocks: {}", refs(&task.blocks)));
    }
    lines.join("\n")
}

fn refs(ids: &[String]) -> String {
    ids.iter().map(|id| format!("#{id}")).collect::<Vec<_>>().join(", ")
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum NewStatus {
    Pending,
    InProgress,
    Completed,
    Deleted,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateInput {
    #[serde(alias = "id", alias = "task_id")]
    task_id: String,
    subject: Option<String>,
    description: Option<String>,
    #[serde(alias = "active_form")]
    active_form: Option<String>,
    status: Option<NewStatus>,
    add_blocks: Option<Vec<String>>,
    add_blocked_by: Option<Vec<String>>,
    owner: Option<String>,
    metadata: Option<Map<String, Value>>,
}

enum Updated {
    Missing,
    Deleted(bool),
    Changed(Vec<&'static str>),
}

impl eos_agent_core::Tool for TaskUpdate {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, _: &RawValue) -> bool {
        true
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let input: UpdateInput = match crate::parse(input) {
                Ok(i) => i,
                Err(e) => return e,
            };
            let id = input.task_id.clone();
            let outcome = match with_list(&self.store, list_id(&ctx), move |list| update(list, input)).await {
                Ok(o) => o,
                Err(e) => return e,
            };
            nudge::mark_used(ctx.extensions);
            ToolOutput::text(match outcome {
                Updated::Missing => "Task not found".to_owned(),
                Updated::Deleted(false) => "Failed to delete task".to_owned(),
                Updated::Deleted(true) => format!("Updated task #{id} deleted"),
                Updated::Changed(fields) => format!("Updated task #{id} {}", fields.join(", ")),
            })
        })
    }
}

fn update(list: &LockedList<'_>, input: UpdateInput) -> io::Result<Updated> {
    let Some(task) = list.get(&input.task_id)? else { return Ok(Updated::Missing) };
    let mut next = task.clone();
    let mut fields = Vec::new();
    if let Some(subject) = input.subject.filter(|s| *s != task.subject) {
        next.subject = subject;
        fields.push("subject");
    }
    if let Some(description) = input.description.filter(|d| *d != task.description) {
        next.description = description;
        fields.push("description");
    }
    if let Some(form) = input.active_form.filter(|f| Some(f) != task.active_form.as_ref()) {
        next.active_form = Some(form);
        fields.push("activeForm");
    }
    if let Some(owner) = input.owner.filter(|o| Some(o) != task.owner.as_ref()) {
        next.owner = Some(owner);
        fields.push("owner");
    }
    if let Some(changes) = input.metadata {
        let mut merged = task.metadata.clone().unwrap_or_default();
        for (key, value) in changes {
            if value.is_null() {
                merged.remove(&key);
            } else {
                merged.insert(key, value);
            }
        }
        next.metadata = Some(merged);
        fields.push("metadata");
    }
    let status = match input.status {
        Some(NewStatus::Deleted) => return list.remove(&input.task_id).map(Updated::Deleted),
        Some(NewStatus::Pending) => Some(TaskStatus::Pending),
        Some(NewStatus::InProgress) => Some(TaskStatus::InProgress),
        Some(NewStatus::Completed) => Some(TaskStatus::Completed),
        None => None,
    };
    if let Some(status) = status.filter(|s| *s != task.status) {
        next.status = status;
        fields.push("status");
    }
    if !fields.is_empty() {
        list.put(&next)?;
    }
    let new_blocks: Vec<String> =
        input.add_blocks.unwrap_or_default().into_iter().filter(|b| !task.blocks.contains(b)).collect();
    for blocked in &new_blocks {
        list.link(&task.id, blocked)?;
    }
    if !new_blocks.is_empty() {
        fields.push("blocks");
    }
    let new_blockers: Vec<String> =
        input.add_blocked_by.unwrap_or_default().into_iter().filter(|b| !task.blocked_by.contains(b)).collect();
    for blocker in &new_blockers {
        list.link(blocker, &task.id)?;
    }
    if !new_blockers.is_empty() {
        fields.push("blockedBy");
    }
    Ok(Updated::Changed(fields))
}

impl eos_agent_core::Tool for TaskList {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, _: &RawValue) -> bool {
        true
    }

    fn call<'a>(&'a self, _input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            match with_list(&self.store, list_id(&ctx), |list| list.all()).await {
                Ok(tasks) => ToolOutput::text(summarize(&tasks)),
                Err(e) => e,
            }
        })
    }
}

/// TaskList's summary: open blockers only, internal tasks hidden.
fn summarize(tasks: &[Task]) -> String {
    let visible: Vec<&Task> = tasks.iter().filter(|t| !is_internal(t)).collect();
    if visible.is_empty() {
        return "No tasks found".to_owned();
    }
    let done: HashSet<&str> =
        visible.iter().filter(|t| t.status == TaskStatus::Completed).map(|t| t.id.as_str()).collect();
    visible
        .iter()
        .map(|t| {
            let owner = t.owner.as_ref().map(|o| format!(" ({o})")).unwrap_or_default();
            let open: Vec<String> = t.blocked_by.iter().filter(|b| !done.contains(b.as_str())).cloned().collect();
            let blocked = if open.is_empty() { String::new() } else { format!(" [blocked by {}]", refs(&open)) };
            format!("#{} [{}] {}{owner}{blocked}", t.id, t.status.as_str(), t.subject)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_internal(task: &Task) -> bool {
    task.metadata
        .as_ref()
        .and_then(|m| m.get("_internal"))
        .is_some_and(|v| !matches!(v, Value::Null | Value::Bool(false)))
}

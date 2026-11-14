//! Stopping a background task by a handle an agent or the host has for it:
//! a command or monitor, a workflow run, or a subagent, the one way for the
//! TaskStop tool and for [`Sdk::stop_task`](crate::Sdk::stop_task).
//!
//! A handle is a task id (a subagent's agent id, the `<task-id>` of its
//! notifications), else the name a subagent was started with, exactly or
//! as written another way (case, spaces). Workflow agents and subagents no
//! longer listed have no handle. What is found is stopped only while it
//! runs (a subagent also while it waits on its own background work) and,
//! for a subagent asking, only when it is its own.

use std::sync::Arc;
use std::time::Duration;

use eos_agent_core::{
    BackgroundTask, BackgroundTasks, Reminders, StopOrigin, StopRefused, SubagentInfo, TaskStatus, TaskStopped, Wake,
};
use eos_agent_engine::AgentHandle;
use eos_agent_tools_shell::ShellTasks;
use unicode_normalization::UnicodeNormalization;

use crate::registry::{Claim, Stop};
use crate::services::Services;

/// How long a printed name or description may be.
const SHOWN_CHARS: usize = 160;

enum Found {
    Shell { owner: Arc<str>, shells: Arc<ShellTasks> },
    Job { owner: Arc<str>, task: Arc<BackgroundTask> },
    Agent { id: String, description: String },
}

pub(super) async fn stop_task(
    services: &Arc<Services>,
    caller: &str,
    handle: &str,
    origin: StopOrigin,
) -> Result<TaskStopped, StopRefused> {
    let registry = &services.registry;
    let scope: Arc<str> = match origin {
        StopOrigin::Agent => registry.root(caller).unwrap_or_else(|| Arc::from(caller)),
        StopOrigin::Host => Arc::from(caller),
    };
    let tree = registry.tree_ids(&scope);
    let agents: Vec<SubagentInfo> = registry
        .session_agents(&scope)
        .into_iter()
        .filter(|a| !a.workflow && !a.delisted && tree.iter().any(|id| id.as_ref() == a.agent_id))
        .collect();
    let Some(found) = find(services, &tree, &agents, handle) else {
        return Err(StopRefused::NotFound(not_found(services, handle, &agents, caller)));
    };
    // Anyone may stop for the host and for the session's top-level agent.
    let any = origin == StopOrigin::Host || registry.is_top_level(caller);
    let not_running = |id: &str, status| StopRefused::NotRunning { id: shown_as(handle, id), status };
    let not_owner = |id: &str, owner: &str| {
        let owner = if registry.is_top_level(owner) { "main session" } else { owner };
        StopRefused::NotOwner(format!(
            "Task {} is owned by {}; agent {} cannot stop it.",
            shown_as(handle, id),
            clip(owner),
            clip(caller)
        ))
    };
    match found {
        Found::Shell { owner, shells } => {
            let (status, _) = shells.describe(handle).unwrap_or(("killed", String::new()));
            if status != "running" {
                return Err(not_running(handle, status));
            }
            if !any && owner.as_ref() != caller {
                return Err(not_owner(handle, &owner));
            }
            match shells.stop(handle).await {
                Some(Ok(command)) => Ok(stopped(handle, "local_bash", command, &owner)),
                Some(Err(status)) => Err(not_running(handle, status)),
                None => Err(not_running(handle, "killed")),
            }
        }
        Found::Job { owner, task } => {
            if task.status() != TaskStatus::Running {
                return Err(not_running(&task.id, task.status().as_str()));
            }
            if !any && owner.as_ref() != caller {
                return Err(not_owner(&task.id, &owner));
            }
            task.stop().await;
            Ok(stopped(&task.id, &task.task_type, task.description.clone(), &owner))
        }
        Found::Agent { id, description } => {
            registry.stoppable(&id).map_err(|status| not_running(&id, status))?;
            // A subagent's own task is itself.
            if !any && id != caller {
                return Err(not_owner(&id, &id));
            }
            let stop = Stop { origin, quiet: false };
            stop_agent(services, &id, stop, id != caller).await.map_err(|status| not_running(&id, status))?;
            Ok(stopped(&id, "local_agent", description, &id))
        }
    }
}

fn stopped(id: &str, task_type: &str, description: String, owner: &str) -> TaskStopped {
    TaskStopped { task_id: id.to_owned(), task_type: task_type.to_owned(), description, owner: owner.to_owned() }
}

/// The task `handle` names: a task id first, then a subagent's name.
fn find(services: &Services, tree: &[Arc<str>], agents: &[SubagentInfo], handle: &str) -> Option<Found> {
    let registry = &services.registry;
    for owner in tree {
        let Some(agent) = registry.handle(owner) else { continue };
        let extensions = agent.extensions();
        if let Some(shells) = extensions.get::<ShellTasks>()
            && shells.describe(handle).is_some()
        {
            return Some(Found::Shell { owner: Arc::clone(owner), shells });
        }
        if let Some(task) = extensions.get::<BackgroundTasks>().and_then(|t| t.get(handle)) {
            return Some(Found::Job { owner: Arc::clone(owner), task });
        }
    }
    let agent = agents
        .iter()
        .find(|a| a.agent_id == handle)
        .or_else(|| agents.iter().find(|a| a.name.as_deref() == Some(handle)))
        .or_else(|| {
            let wanted = normalized(handle);
            agents.iter().find(|a| a.name.as_deref().is_some_and(|n| normalized(n) == wanted))
        })?;
    Some(Found::Agent { id: agent.agent_id.clone(), description: agent.description.clone() })
}

/// Stops the subagent `id` (see [`Subagents::stop_task`](eos_agent_core::Subagents::stop_task));
/// `wait`: until its turn has ended (not when it stops itself). Else how its
/// last run ended.
pub(super) async fn stop_agent(services: &Arc<Services>, id: &str, stop: Stop, wait: bool) -> Result<(), &'static str> {
    let registry = &services.registry;
    match registry.claim_stop(id, stop) {
        Claim::Ended(status) => Err(status),
        Claim::Running(handle) => {
            let Some(handle) = handle else { return Ok(()) };
            handle.interrupt();
            if !wait {
                return Ok(());
            }
            // A run claimed for it may not have begun its turn yet.
            while registry.child(id).is_some_and(|i| i.running) {
                handle.interrupt();
                handle.wait_idle().await;
                if registry.child(id).is_some_and(|i| i.running) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            end_work(services, id, &handle).await;
            Ok(())
        }
        Claim::Waiting(handle) => {
            if let Some(handle) = &handle {
                end_work(services, id, handle).await;
            }
            // What it started stops with it, without a word to it.
            for below in registry.tree_ids(id).into_iter().skip(1) {
                if registry.child(&below).is_some_and(|i| i.workflow) {
                    continue;
                }
                let quiet = Stop { origin: stop.origin, quiet: true };
                let _ = Box::pin(stop_agent(services, &below, quiet, true)).await;
            }
            if let Some(info) = registry.child(id) {
                info.tracker.announce_end("killed").await;
            }
            Ok(())
        }
    }
}

/// Ends what a stopped subagent left running (its commands, monitors and
/// workflow runs) and what was waiting for it to read: notes of its own
/// work are dropped, a subagent's report goes to its session instead.
pub(super) async fn end_work(services: &Arc<Services>, id: &str, handle: &AgentHandle) {
    let extensions = handle.extensions();
    if let Some(shells) = extensions.get::<ShellTasks>() {
        for task in shells.running() {
            let _ = shells.stop(&task).await;
        }
    }
    if let Some(tasks) = extensions.get::<BackgroundTasks>() {
        for task in tasks.all_running() {
            task.stop().await;
        }
    }
    if let Some(wake) = extensions.get::<Wake>() {
        wake.clear();
    }
    let notes = extensions.get_or_default::<Reminders>().take();
    let registry = &services.registry;
    let children = registry.children(id);
    let main = registry.root(id).and_then(|root| registry.handle(&root));
    for note in notes {
        let from_child = notified_task(&note).is_some_and(|task| children.iter().any(|c| c.as_ref() == task));
        if let (true, Some(main)) = (from_child, &main) {
            let _ = main.notify(note, true).await;
        }
    }
}

/// The task a `<task-notification>` is about.
fn notified_task(note: &str) -> Option<&str> {
    let rest = note.strip_prefix("<task-notification>\n<task-id>")?;
    rest.split_once("</task-id>").map(|(id, _)| id)
}

/// The handle as the reply names it: the id, or `handle (id)` when the
/// handle was a name.
fn shown_as(handle: &str, id: &str) -> String {
    if handle == id { id.to_owned() } else { format!("{} ({id})", clip(handle)) }
}

/// The answer for a handle that names nothing: a near name, the names and
/// the unnamed background agents that can be stopped.
fn not_found(services: &Services, handle: &str, agents: &[SubagentInfo], caller: &str) -> String {
    let registry = &services.registry;
    let live: Vec<&SubagentInfo> = agents.iter().filter(|a| registry.stoppable(&a.agent_id).is_ok()).collect();
    let names: Vec<&str> = live.iter().filter_map(|a| a.name.as_deref()).collect();
    let mut out = format!("No task found with ID: {}", clip(handle));
    if let Some(near) = nearest(handle, &names) {
        out += &format!(". Did you mean: {}?", clip(near));
    }
    if !names.is_empty() {
        out += &format!(". Running named agents: {}", names.iter().map(|n| clip(n)).collect::<Vec<_>>().join(", "));
    }
    let unnamed: Vec<String> = live
        .iter()
        .filter(|a| a.name.is_none() && a.agent_id != caller)
        .filter(|a| registry.child(&a.agent_id).is_some_and(|i| i.background))
        .map(|a| {
            if a.description.is_empty() {
                a.agent_id.clone()
            } else {
                format!("{} ({})", a.agent_id, clip(&a.description))
            }
        })
        .collect();
    if !unnamed.is_empty() {
        out += &format!(". Running background agents: {}", unnamed.join(", "));
    }
    out
}

/// The name closest to `handle` within two edits (as written another way).
fn nearest<'a>(handle: &str, names: &[&'a str]) -> Option<&'a str> {
    let wanted: Vec<char> = normalized(handle).chars().collect();
    let mut best: Option<(usize, &str)> = None;
    for name in names {
        let candidate: Vec<char> = normalized(name).chars().collect();
        if candidate.len().abs_diff(wanted.len()) > 2 {
            continue;
        }
        let distance = edits(&wanted, &candidate);
        if distance <= 2 && best.is_none_or(|(d, _)| distance < d) {
            best = Some((distance, name));
        }
    }
    best.map(|(_, name)| name)
}

/// Edits between `a` and `b`: insertions, deletions, substitutions and
/// swaps of two neighbours.
fn edits(a: &[char], b: &[char]) -> usize {
    let mut rows = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in rows[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut d = (rows[i - 1][j] + 1).min(rows[i][j - 1] + 1).min(rows[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(rows[i - 2][j - 2] + 1);
            }
            rows[i][j] = d;
        }
    }
    rows[a.len()][b.len()]
}

/// A name as compared: compatibility-normalized, invisible characters
/// dropped, trimmed, lower case, each run of spaces one `-`.
fn normalized(name: &str) -> String {
    let kept: String = name.nfkc().filter(|c| c.is_whitespace() || !(c.is_control() || is_format(*c))).collect();
    kept.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join("-")
}

fn is_format(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}')
}

/// A name or description as printed: control characters dropped, spaces
/// collapsed, at most 160 characters.
fn clip(text: &str) -> String {
    let kept: String = text.chars().filter(|c| c.is_whitespace() || !(c.is_control() || is_format(*c))).collect();
    let one_line = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= SHOWN_CHARS {
        return one_line;
    }
    let mut cut: String = one_line.chars().take(SHOWN_CHARS - 1).collect();
    cut.push('\u{2026}');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_compare_as_written_another_way() {
        assert_eq!(normalized("  Scout  Two\u{200B} "), "scout-two");
        assert_eq!(normalized("ｓｃｏｕｔ"), "scout");
    }

    #[test]
    fn a_near_name_is_suggested_within_two_edits() {
        let names = ["scout-name", "reviewer"];
        assert_eq!(nearest("scout-nmae", &names), Some("scout-name"));
        assert_eq!(nearest("Scout-Nam", &names), Some("scout-name"));
        assert_eq!(nearest("scout", &names), None);
        assert_eq!(edits(&['a', 'b'], &['b', 'a']), 1);
    }

    #[test]
    fn a_notification_names_its_task() {
        assert_eq!(notified_task("<task-notification>\n<task-id>a12</task-id>\n<status>"), Some("a12"));
        assert_eq!(notified_task("Background command done"), None);
    }

    #[test]
    fn long_text_is_clipped() {
        assert_eq!(clip("a\n b\u{7}"), "a b");
        assert_eq!(clip(&"x".repeat(200)).chars().count(), SHOWN_CHARS);
    }
}

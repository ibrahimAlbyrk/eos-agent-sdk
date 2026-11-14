//! A subagent's own background work (a command, a monitor, a subagent it
//! started) leaves it a note; an idle subagent is woken for it through the
//! registry, so the turn is reported to its parent like any background run
//! (a second notification under the same task id).

use std::sync::Arc;

use eos_agent_core::{Extensions, Reminders, TaskNotices, Wake};
use eos_agent_types::ContentBlock;

use super::{restart, run_in_background};
use crate::services::Services;

/// Gives the subagent `id` (whose extensions these are) its wake.
pub(super) fn install(services: &Arc<Services>, id: &str, extensions: &Extensions) {
    let services = Arc::downgrade(services);
    let id = id.to_owned();
    extensions.insert(Wake::new(move || {
        if let Some(services) = services.upgrade() {
            wake(&services, &id);
        }
    }));
}

/// Runs a turn of the subagent `id` on its unread notes, in the background,
/// if it is idle and was not stopped. A working subagent reads them with
/// its next request, or is woken again when its run ends.
pub(super) fn wake(services: &Arc<Services>, id: &str) {
    let Some((handle, tracker)) = services.registry.start_run(id, true) else { return };
    let ext = handle.extensions();
    let notes =
        if ext.get::<Wake>().is_some_and(|w| w.take()) { ext.get_or_default::<Reminders>().take() } else { Vec::new() };
    if notes.is_empty() {
        // Read by a turn that slipped in between: nothing to run.
        let generation = services.registry.set_running(id, false);
        services.release_later(id, generation);
        return;
    }
    let input = notes.into_iter().map(|text| ContentBlock::Text { text, citations: None }).collect();
    tokio::spawn(run_in_background(Arc::clone(services), handle, tracker, input, None));
}

/// Tells `id`'s parent how its run went (`text`, a `<task-notification>`).
/// A top-level parent takes it up in a turn of its own. A subagent parent
/// reads it as a note of its own background work, which wakes it; one that
/// was let go is started again from its transcript first, as a message
/// would restart it. Only a parent that has ended (or whose transcript
/// cannot be read) never hears.
pub(super) async fn report_to_parent(services: &Arc<Services>, id: &str, text: String) {
    let registry = &services.registry;
    let Some(parent) = registry.parent(id) else { return };
    if registry.is_top_level(&parent) {
        if let Some(main) = registry.handle(&parent) {
            let _ = main.notify(text, true).await;
        }
        return;
    }
    let handle = match registry.handle(&parent) {
        Some(handle) => handle,
        None if registry.is_released(&parent) => match restart(services, &parent).await {
            Ok(handle) => handle,
            Err(_) => return,
        },
        None => return,
    };
    TaskNotices::of(handle.extensions()).push(text);
}

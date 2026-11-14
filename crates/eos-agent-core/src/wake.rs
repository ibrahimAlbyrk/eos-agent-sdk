//! Waking an idle agent when its own background work (a command, a monitor,
//! a subagent it started) leaves it a note, as Claude Code does.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::Extensions;
use crate::workspace::Reminders;

/// An agent's wake hook, kept in its [`Extensions`] by whoever runs the
/// agent. A note pushed to the agent's [`Reminders`] followed by
/// [`wake`](Self::wake) asks for a turn; the host runs one only when the
/// agent is idle and the note is still unread. A request that reads the
/// agent's notes clears it ([`clear`](Self::clear)).
pub struct Wake {
    wanted: AtomicBool,
    hook: Box<dyn Fn() + Send + Sync>,
}

impl std::fmt::Debug for Wake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wake").field("wanted", &self.is_wanted()).finish_non_exhaustive()
    }
}

impl Wake {
    /// `hook` must not block: it hands the wake to the agent's host.
    pub fn new(hook: impl Fn() + Send + Sync + 'static) -> Self {
        Self { wanted: AtomicBool::new(false), hook: Box::new(hook) }
    }

    /// A note is waiting: asks for a turn.
    pub fn wake(&self) {
        self.wanted.store(true, Ordering::SeqCst);
        (self.hook)();
    }

    /// A note arrived that no request has read yet.
    pub fn is_wanted(&self) -> bool {
        self.wanted.load(Ordering::SeqCst)
    }

    /// Takes the request: true when a note was waiting.
    pub fn take(&self) -> bool {
        self.wanted.swap(false, Ordering::SeqCst)
    }

    /// The agent's notes are being read now.
    pub fn clear(&self) {
        self.wanted.store(false, Ordering::SeqCst);
    }
}

/// How a background task tells its agent something: a note with the next
/// request, which wakes the agent if it sits idle.
#[derive(Debug, Clone)]
pub struct TaskNotices {
    reminders: Arc<Reminders>,
    wake: Option<Arc<Wake>>,
}

impl TaskNotices {
    /// The notices of the agent owning `extensions` (taken when the task
    /// starts: the agent's host installs its wake before its first turn).
    pub fn of(extensions: &Extensions) -> Self {
        Self { reminders: extensions.get_or_default::<Reminders>(), wake: extensions.get::<Wake>() }
    }

    pub fn push(&self, text: impl Into<String>) {
        self.reminders.push(text);
        if let Some(wake) = &self.wake {
            wake.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn a_notice_asks_for_a_turn_until_read() {
        let ext = Extensions::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        ext.insert(Wake::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        TaskNotices::of(&ext).push("done");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let wake = ext.get::<Wake>().expect("wake");
        assert!(wake.is_wanted());
        wake.clear();
        assert!(!wake.take());
        assert_eq!(ext.get_or_default::<Reminders>().take(), vec!["done".to_owned()]);
    }

    #[test]
    fn without_a_hook_a_notice_only_waits() {
        let ext = Extensions::default();
        TaskNotices::of(&ext).push("done");
        assert!(!ext.get_or_default::<Reminders>().is_empty());
    }
}

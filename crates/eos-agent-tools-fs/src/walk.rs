//! A parallel directory walk where each thread fills its own state, merged
//! once per thread at the end (no lock per entry).

use std::sync::{Mutex, PoisonError};

use eos_agent_core::CancellationToken;
use ignore::{DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkBuilder, WalkState};

/// Walk threads per search; many agents search at once, so keep it modest.
pub(crate) fn threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get().min(4))
}

pub(crate) trait Merge: Default + Send {
    fn merge(&mut self, other: Self);
}

/// Visits every entry `walk` yields; stops early on cancel. Returns the
/// merged state and whether the walk was cancelled.
pub(crate) fn walk<S, F>(walk: &WalkBuilder, cancel: &CancellationToken, visit: F) -> (S, bool)
where
    S: Merge,
    F: Fn(&mut S, DirEntry) + Sync,
{
    let shared = Mutex::new(S::default());
    let mut builder = Builder { shared: &shared, cancel, visit: &visit };
    walk.build_parallel().visit(&mut builder);
    (shared.into_inner().unwrap_or_else(PoisonError::into_inner), cancel.is_cancelled())
}

struct Builder<'s, S, F> {
    shared: &'s Mutex<S>,
    cancel: &'s CancellationToken,
    visit: &'s F,
}

impl<'s, S: Merge, F: Fn(&mut S, DirEntry) + Sync> ParallelVisitorBuilder<'s> for Builder<'s, S, F> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(Visitor { local: S::default(), shared: self.shared, cancel: self.cancel, visit: self.visit })
    }
}

struct Visitor<'s, S: Merge, F> {
    local: S,
    shared: &'s Mutex<S>,
    cancel: &'s CancellationToken,
    visit: &'s F,
}

impl<S: Merge, F: Fn(&mut S, DirEntry) + Sync> ParallelVisitor for Visitor<'_, S, F> {
    fn visit(&mut self, entry: Result<DirEntry, ignore::Error>) -> WalkState {
        if self.cancel.is_cancelled() {
            return WalkState::Quit;
        }
        if let Ok(entry) = entry {
            (self.visit)(&mut self.local, entry);
        }
        WalkState::Continue
    }
}

impl<S: Merge, F> Drop for Visitor<'_, S, F> {
    fn drop(&mut self) {
        let local = std::mem::take(&mut self.local);
        self.shared.lock().unwrap_or_else(PoisonError::into_inner).merge(local);
    }
}

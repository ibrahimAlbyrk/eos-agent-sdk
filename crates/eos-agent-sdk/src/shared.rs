//! One copy of each session context for every agent that has the same one.
//!
//! Agents of one project, model and tool set get the same environment note
//! (with the skill and agent listings) and the same instruction files, often
//! tens of kilobytes. Each context is built as before and then looked up by
//! content: an identical one already held by a live agent is used instead,
//! so the model receives exactly the same bytes and the process keeps one
//! copy. Entries are weak: once no agent (and no conversation) holds a text,
//! it is freed.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use bytes::Bytes;
use eos_agent_core::{SessionContext, StoredMessage};
use eos_agent_types::ContentBlock;

#[derive(Debug, Default)]
pub(crate) struct SharedContexts {
    notes: Mutex<Pool<[u8]>>,
    prefixes: Mutex<Pool<[ContentBlock]>>,
}

impl SharedContexts {
    /// `context` with its prefix and notes replaced by the copies live agents
    /// already hold, where they are the same.
    pub(crate) fn share(&self, context: SessionContext) -> SessionContext {
        let SessionContext { user_prefix, system_notes, servers } = context;
        let user_prefix = if user_prefix.is_empty() {
            user_prefix
        } else {
            let key = hash(user_prefix.iter().map(text_of));
            let mut pool = lock(&self.prefixes);
            match pool.find(key, |held| same_blocks(held, &user_prefix)) {
                Some(held) => held,
                None => pool.insert(key, user_prefix),
            }
        };
        let system_notes = system_notes.into_iter().map(|note| self.note(note)).collect();
        SessionContext { user_prefix, system_notes, servers }
    }

    fn note(&self, note: StoredMessage) -> StoredMessage {
        let json = &note.json()[..];
        let key = hash([json]);
        let mut pool = lock(&self.notes);
        let bytes = match pool.find(key, |held| held == json) {
            Some(held) => held,
            None => pool.insert(key, Arc::from(json)),
        };
        drop(pool);
        // The conversation keeps the shared bytes alive (and findable) after
        // the first turn, so later agents still share them.
        StoredMessage::from_wire(Bytes::from_owner(Owner(bytes))).unwrap_or(note)
    }
}

/// Holds a shared buffer for [`Bytes`].
struct Owner(Arc<[u8]>);

impl AsRef<[u8]> for Owner {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Weakly held values by content hash. Dead entries are swept whenever the
/// table has doubled since the last sweep, so it stays proportional to the
/// texts alive.
#[derive(Debug)]
struct Pool<T: ?Sized> {
    entries: HashMap<u64, Vec<Weak<T>>>,
    swept_at: usize,
}

impl<T: ?Sized> Default for Pool<T> {
    fn default() -> Self {
        Self { entries: HashMap::new(), swept_at: 0 }
    }
}

impl<T: ?Sized> Pool<T> {
    fn find(&self, key: u64, same: impl Fn(&T) -> bool) -> Option<Arc<T>> {
        self.entries.get(&key)?.iter().filter_map(Weak::upgrade).find(|held| same(held))
    }

    fn insert(&mut self, key: u64, value: Arc<T>) -> Arc<T> {
        let bucket = self.entries.entry(key).or_default();
        bucket.retain(|w| w.strong_count() > 0);
        bucket.push(Arc::downgrade(&value));
        if self.entries.len() > 2 * self.swept_at + 16 {
            self.entries.retain(|_, bucket| {
                bucket.retain(|w| w.strong_count() > 0);
                !bucket.is_empty()
            });
            self.swept_at = self.entries.len();
        }
        value
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.values().map(|b| b.iter().filter(|w| w.strong_count() > 0).count()).sum()
    }
}

fn text_of(block: &ContentBlock) -> &[u8] {
    match block {
        ContentBlock::Text { text, .. } => text.as_bytes(),
        _ => &[],
    }
}

/// Prefixes are text blocks; anything else is never taken for another's.
fn same_blocks(a: &[ContentBlock], b: &[ContentBlock]) -> bool {
    let plain = |block: &ContentBlock| matches!(block, ContentBlock::Text { citations: None, .. });
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| plain(x) && plain(y) && text_of(x) == text_of(y))
}

fn hash<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> u64 {
    let mut hasher = DefaultHasher::new();
    for part in parts {
        part.hash(&mut hasher);
    }
    hasher.finish()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(prefix: &str, note: &str) -> SessionContext {
        SessionContext {
            user_prefix: vec![ContentBlock::Text { text: prefix.into(), citations: None }].into(),
            system_notes: vec![StoredMessage::system_text(note)],
            ..SessionContext::default()
        }
    }

    #[test]
    fn equal_contexts_share_one_copy() {
        let shared = SharedContexts::default();
        let a = shared.share(context("claude.md", "environment"));
        let b = shared.share(context("claude.md", "environment"));
        let c = shared.share(context("claude.md", "another environment"));
        assert!(Arc::ptr_eq(&a.user_prefix, &b.user_prefix));
        assert_eq!(a.system_notes[0].json().as_ptr(), b.system_notes[0].json().as_ptr());
        assert_ne!(a.system_notes[0].json().as_ptr(), c.system_notes[0].json().as_ptr());
        assert_eq!(c.note_texts(), vec!["another environment".to_owned()]);
        assert_eq!(a.system_notes[0].json(), context("x", "environment").system_notes[0].json(), "same bytes");
    }

    #[test]
    fn a_note_outlives_the_context_in_conversations() {
        let shared = SharedContexts::default();
        let note = shared.share(context("p", "environment")).system_notes[0].clone();
        let later = shared.share(context("p", "environment"));
        assert_eq!(note.json().as_ptr(), later.system_notes[0].json().as_ptr());
        drop((note, later));
        assert_eq!(lock(&shared.notes).len(), 0, "freed once nobody holds it");
        assert_eq!(lock(&shared.prefixes).len(), 0);
    }
}

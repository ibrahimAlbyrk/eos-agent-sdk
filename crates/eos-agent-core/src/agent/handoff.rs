//! The conversation as the provider of the next request can read it.
//!
//! A model's reasoning comes back signed or encrypted by its provider, and no
//! other provider accepts it. The conversation keeps every message as it was
//! written; a request is built through [`for_provider`], which leaves out the
//! reasoning of messages another provider wrote. Going back to the provider
//! that wrote them sends them whole again.

use eos_agent_types::{ContentBlock, Role};

use crate::auth::ProviderId;
use crate::message::StoredMessage;

/// `history` for a request to `provider`; `None` when it can go as it is.
pub(super) fn for_provider(history: &[StoredMessage], provider: &ProviderId) -> Option<Vec<StoredMessage>> {
    let foreign = |m: &StoredMessage| m.origin().is_some_and(|o| o.provider != *provider) && has_reasoning(m);
    if !history.iter().any(foreign) {
        return None;
    }
    Some(history.iter().filter_map(|m| if foreign(m) { without_reasoning(m) } else { Some(m.clone()) }).collect())
}

fn has_reasoning(message: &StoredMessage) -> bool {
    let json = &message.json()[..];
    let holds = |needle: &[u8]| json.windows(needle.len()).any(|w| w == needle);
    message.role() == Role::Assistant && (holds(br#""type":"thinking""#) || holds(br#""type":"redacted_thinking""#))
}

/// The message without its reasoning (`None`: it was nothing else).
fn without_reasoning(message: &StoredMessage) -> Option<StoredMessage> {
    let Ok(mut blocks) = message.blocks() else { return Some(message.clone()) };
    blocks.retain(|b| !matches!(b, ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }));
    let kept = StoredMessage::new(message.role(), &blocks).ok()?;
    Some(
        kept.with_recorded_ms(message.recorded_ms())
            .with_meta(message.is_meta())
            .with_origin(message.origin().cloned()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Origin;

    fn text(text: &str) -> ContentBlock {
        ContentBlock::Text { text: text.into(), citations: None }
    }

    fn thinking() -> ContentBlock {
        ContentBlock::Thinking { thinking: "hm".into(), signature: "sig".into() }
    }

    fn reply(provider: &str, blocks: &[ContentBlock]) -> StoredMessage {
        let origin = Origin { provider: ProviderId::new(provider), model: "m".into() };
        StoredMessage::new(Role::Assistant, blocks).expect("message").with_origin(Some(origin))
    }

    #[test]
    fn reasoning_goes_only_to_the_provider_that_wrote_it() {
        let history = vec![
            StoredMessage::user_text("hi"),
            reply("a", &[thinking(), text("from a")]),
            StoredMessage::user_text("again"),
            reply("b", &[thinking(), text("from b")]),
            reply("a", &[thinking()]),
        ];
        let a = ProviderId::new("a");
        let for_a = for_provider(&history, &a).expect("b's reasoning is left out");
        let kinds = |m: &StoredMessage| -> Vec<String> {
            let blocks = m.blocks().expect("blocks");
            blocks.iter().filter_map(ContentBlock::kind).map(str::to_owned).collect()
        };
        assert_eq!(for_a.len(), 5);
        assert_eq!(for_a[1].json().as_ptr(), history[1].json().as_ptr(), "its own messages go as they were written");
        assert_eq!(kinds(&for_a[3]), ["text"]);
        assert_eq!(kinds(&for_a[4]), ["thinking"]);

        let for_b = for_provider(&history, &ProviderId::new("b")).expect("a's reasoning is left out");
        assert_eq!(for_b.len(), 4, "a message of reasoning only is left out whole");
        assert_eq!(kinds(&for_b[1]), ["text"]);
        assert_eq!(for_b[3].json().as_ptr(), history[3].json().as_ptr());
    }

    #[test]
    fn a_conversation_of_one_provider_goes_as_it_is() {
        let history = vec![StoredMessage::user_text("hi"), reply("a", &[thinking(), text("ok")])];
        assert!(for_provider(&history, &ProviderId::new("a")).is_none());
        // Who wrote a message is unknown: nothing is taken from it.
        let unsigned = vec![StoredMessage::new(Role::Assistant, &[thinking(), text("ok")]).expect("message")];
        assert!(for_provider(&unsigned, &ProviderId::new("b")).is_none());
    }
}

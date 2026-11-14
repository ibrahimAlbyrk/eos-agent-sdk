//! Claude Code's `/btw`: a question answered from an agent's conversation
//! by a request of its own, which the agent never sees.

use eos_agent_core::{Assembler, ModelError, ModelRequest, RetryPolicy, StoredMessage};
use eos_agent_engine::{AgentHandle, Gone};
use eos_agent_prompt::Fragment;
use eos_agent_types::{ContentBlock, Role, StreamEvent};
use futures_util::StreamExt;

use crate::services::Services;

/// Transient failures a side question tries again after.
const RETRIES: u32 = 2;

#[derive(Debug, thiserror::Error)]
pub enum SideError {
    #[error(transparent)]
    Gone(#[from] Gone),
    #[error("could not read the conversation: {0}")]
    Store(#[from] std::io::Error),
    #[error(transparent)]
    Model(#[from] ModelError),
    /// The model wrote no text (Claude Code shows "No answer available for
    /// this side question").
    #[error("no answer available for this side question")]
    NoAnswer,
}

pub(crate) async fn ask(handle: &AgentHandle, services: &Services, question: &str) -> Result<String, SideError> {
    let mut request = handle.context_request().await??;
    // Claude Code's wording, as two text blocks of one user message. The
    // agent's system prompt, tools and messages stay as they are, so the
    // request reads the agent's prompt cache; no tool it names is run.
    let reminder = format!("{}\n\n", services.prompts.get(Fragment::SideQuestion).trim_end());
    let blocks = [
        ContentBlock::Text { text: reminder, citations: None },
        ContentBlock::Text { text: question.to_owned(), citations: None },
    ];
    let message = StoredMessage::new(Role::User, &blocks).map_err(|e| ModelError::Invalid(e.to_string()))?;
    request.push_user(message.json().clone());

    let retry = RetryPolicy::default();
    let mut attempt = 0;
    let content = loop {
        attempt += 1;
        match answer(services, &request).await {
            Ok(content) => break content,
            Err(error) if error.is_retryable() && attempt <= RETRIES => {
                tokio::time::sleep(retry.delay(attempt, error.retry_after())).await;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let texts: Vec<&str> = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if texts.is_empty() {
        return Err(SideError::NoAnswer);
    }
    Ok(texts.join("\n\n"))
}

async fn answer(services: &Services, request: &ModelRequest) -> Result<Vec<ContentBlock>, ModelError> {
    let mut events = services.engine.model().stream(request).await?;
    let mut assembler = Assembler::default();
    while let Some(event) = events.next().await {
        let event = event?;
        if let StreamEvent::Error(e) = &event {
            return Err(ModelError::Api { kind: e.kind.clone(), message: e.message.clone() });
        }
        let _ = assembler.push(event);
    }
    let done = assembler.finish();
    if !done.complete {
        return Err(ModelError::Transient("stream ended before message_stop".into()));
    }
    Ok(done.content)
}

//! Claude Code's `AskUserQuestion`, answered through a host-provided
//! [`UserPrompter`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use eos_agent_core::{BoxFuture, Tool, ToolContext, ToolOutput, ToolSpec};
use eos_agent_types::ContentBlock;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

const NO_USER: &str = "No user is available to answer questions in this session. Proceed using your best judgment \
based on the context so far.";
const NOT_UNIQUE: &str = "Question texts must be unique, option labels must be unique within each question";
/// Claude Code's words for a call the user turned away without a word.
pub const REJECTED: &str = "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it \
was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell \
you how to proceed.";
/// Claude Code's lead for a turned-away call the user said something about.
pub const REJECTED_SAYING: &str = "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. \
if it was a file edit, the new_string was NOT written to the file). To tell you how to proceed, the user said:\n";
/// An answer slot the user filled only with notes.
const NOTES_ONLY: &str = "(notes only)";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Annotation {
    /// The preview of the chosen option.
    pub preview: Option<String>,
    pub notes: Option<String>,
}

/// The user's reply, keyed by question text.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    /// The chosen label, or free text; several labels joined with ", ".
    pub answers: HashMap<String, String>,
    pub annotations: HashMap<String, Annotation>,
    /// Text the user wrote instead of answering the questions.
    pub response: Option<String>,
    /// The dialog answered by itself after this long without the user
    /// (Claude Code's `afkTimeoutMs`); `answers` holds what was chosen by then.
    pub afk_timeout_ms: Option<u64>,
    /// Images the user pasted into an answer; they go into the conversation
    /// beside the result (Claude Code's `contentBlocks`).
    pub images: Vec<ContentBlock>,
}

/// Why the questions came back without a reply.
#[derive(Debug, Clone)]
pub enum Refusal {
    /// The user turned the questions away without a word (Esc, or Cancel on
    /// the review): Claude Code rejects the call and its turn ends.
    Cancelled,
    /// The user turned them away saying `text` ("Chat about this"), with
    /// images they pasted: the call is rejected, the model reads it and goes on.
    Said { text: String, images: Vec<ContentBlock> },
    /// Nobody could answer; the model reads why.
    Unanswered(String),
}

impl From<String> for Refusal {
    fn from(reason: String) -> Self {
        Self::Unanswered(reason)
    }
}

/// The host side of AskUserQuestion: shows the questions to a person and
/// returns their reply, or why there is none. `tool_use_id` is the
/// AskUserQuestion call that asks.
pub trait UserPrompter: Send + Sync + std::fmt::Debug {
    fn ask<'a>(
        &'a self,
        agent_id: &'a str,
        tool_use_id: &'a str,
        questions: &'a [Question],
    ) -> BoxFuture<'a, Result<Reply, Refusal>>;
}

/// For sessions with nobody to ask.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoUser;

impl UserPrompter for NoUser {
    fn ask<'a>(&'a self, _: &'a str, _: &'a str, _: &'a [Question]) -> BoxFuture<'a, Result<Reply, Refusal>> {
        Box::pin(std::future::ready(Err(Refusal::Unanswered(NO_USER.to_owned()))))
    }
}

#[derive(Deserialize)]
struct Input {
    questions: Vec<Question>,
}

#[derive(Debug)]
pub struct AskUserQuestion {
    spec: ToolSpec,
    prompter: Arc<dyn UserPrompter>,
}

impl AskUserQuestion {
    pub fn new(prompter: Arc<dyn UserPrompter>) -> Self {
        let spec = crate::spec(
            "AskUserQuestion",
            include_str!("../prompts/AskUserQuestion.md"),
            include_str!("../prompts/AskUserQuestion.schema.json"),
        )
        .with_search_hint("prompt the user with a multiple-choice question");
        Self { spec, prompter }
    }
}

impl Tool for AskUserQuestion {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, _: &RawValue) -> bool {
        true
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let Input { questions } = match crate::parse(input) {
                Ok(i) => i,
                Err(e) => return e,
            };
            if let Some(problem) = invalid(&questions) {
                return ToolOutput::error(format!("<tool_use_error>InputValidationError: {problem}</tool_use_error>"));
            }
            match self.prompter.ask(ctx.agent_id, ctx.tool_use_id, &questions).await {
                Ok(reply) => ToolOutput::text(render(&questions, &reply)).with_beside(reply.images),
                // Claude Code aborts the turn on a rejection with nothing said
                // (its `cancelAndAbort`), after recording this result.
                Err(Refusal::Cancelled) => {
                    ctx.cancel.cancel();
                    ToolOutput::error(REJECTED)
                }
                Err(Refusal::Said { text, images }) => {
                    ToolOutput::error(format!("{REJECTED_SAYING}{text}")).with_beside(images)
                }
                Err(Refusal::Unanswered(reason)) => ToolOutput::error(reason),
            }
        })
    }
}

fn invalid(questions: &[Question]) -> Option<&'static str> {
    if questions.is_empty() || questions.len() > 4 {
        return Some("questions: must have 1-4 questions");
    }
    if questions.iter().any(|q| q.options.len() < 2 || q.options.len() > 4) {
        return Some("options: each question must have 2-4 options");
    }
    let texts: HashSet<&str> = questions.iter().map(|q| q.question.as_str()).collect();
    let labels_unique =
        questions.iter().all(|q| q.options.iter().map(|o| &o.label).collect::<HashSet<_>>().len() == q.options.len());
    (texts.len() != questions.len() || !labels_unique).then_some(NOT_UNIQUE)
}

/// Claude Code's wording for the reply the model reads.
fn render(questions: &[Question], reply: &Reply) -> String {
    let answered: Vec<String> = questions
        .iter()
        .filter_map(|q| {
            let answer = reply.answers.get(&q.question).filter(|a| !a.is_empty() && *a != NOTES_ONLY);
            let note = reply.annotations.get(&q.question);
            if answer.is_none() && note.and_then(|n| n.notes.as_ref()).is_none() {
                return None;
            }
            let mut parts = vec![match answer {
                Some(a) => format!("\"{}\"=\"{a}\"", q.question),
                None => format!("\"{}\"=(no option selected)", q.question),
            }];
            if let Some(preview) = note.and_then(|n| n.preview.as_ref()) {
                parts.push(format!("selected preview:\n{preview}"));
            }
            if let Some(notes) = note.and_then(|n| n.notes.as_ref()) {
                parts.push(format!("notes: {notes}"));
            }
            Some(parts.join(" "))
        })
        .collect();
    let answered = answered.join(", ");
    if let Some(ms) = reply.afk_timeout_ms {
        let away = format!(
            "No response after {}s \u{2014} the user may be away from keyboard. Proceed using your best judgment \
based on the context so far; you can re-ask this question later if it's still relevant.",
            ms.saturating_add(500) / 1000
        );
        return if answered.is_empty() {
            away
        } else {
            format!("{away}\n\nBefore going idle the user had selected: {answered}.")
        };
    }
    if let Some(response) = reply.response.as_deref().filter(|r| !r.trim().is_empty()) {
        format!("The user responded: {response}")
    } else if answered.is_empty() {
        "The user did not answer the questions.".to_owned()
    } else if questions.iter().all(|q| chose_offered(q, reply)) {
        format!("Your questions have been answered: {answered}. You can now continue with these answers in mind.")
    } else {
        format!(
            "The user answered: {answered}. Read the answers carefully \u{2014} they may request clarification, \
changes, or that you not proceed \u{2014} and follow what they actually say."
        )
    }
}

/// The answer is one of the offered labels (several, for multi-select) and
/// carries no notes, so the model can take it at face value.
fn chose_offered(question: &Question, reply: &Reply) -> bool {
    if reply.annotations.get(&question.question).and_then(|a| a.notes.as_ref()).is_some() {
        return false;
    }
    let Some(answer) = reply.answers.get(&question.question).filter(|a| !a.is_empty() && *a != NOTES_ONLY) else {
        return true;
    };
    let offered = |label: &str| question.options.iter().any(|o| o.label == label);
    offered(answer) || (question.multi_select && answer.split(", ").all(offered))
}

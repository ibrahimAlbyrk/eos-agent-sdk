use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eos_agent_core::{BoxFuture, CancellationToken, Extensions, ReminderSource, Tool, ToolContext, ToolOutput};
use eos_agent_tools_session::{
    Annotation, AskUserQuestion, DeferredTool, NoUser, NudgeKind, Question, Refusal, Reply, StructuredOutput,
    StructuredResult, TaskCreate, TaskGet, TaskList, TaskListId, TaskStore, TaskUpdate, TodoList, TodoStatus,
    TodoWrite, ToolSearch, TrackingNudge, UserPrompter,
};
use eos_agent_types::{ContentBlock, ToolResultContent};
use serde_json::value::RawValue;

async fn call_as(tool: &dyn Tool, agent: &str, ext: &Extensions, input: &str) -> ToolOutput {
    let raw = RawValue::from_string(input.to_owned()).expect("json");
    let cancel = CancellationToken::new();
    tool.call(
        &raw,
        ToolContext { agent_id: agent, tool_use_id: "t1", cancel: &cancel, extensions: ext, transcript: &[] },
    )
    .await
}

async fn call(tool: &dyn Tool, ext: &Extensions, input: &str) -> ToolOutput {
    call_as(tool, "agent-1", ext, input).await
}

fn text(out: &ToolOutput) -> &str {
    match &out.content {
        ToolResultContent::Text(t) => t,
        ToolResultContent::Blocks(_) => panic!("expected text"),
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("eos-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn todo_write_keeps_the_list_per_agent() {
    let tool = TodoWrite::new();
    let ext = Extensions::default();
    let out = call(
        &tool,
        &ext,
        r#"{"todos":[{"content":"Run tests","status":"in_progress","activeForm":"Running tests"},
                     {"content":"Build","status":"pending","activeForm":"Building"}]}"#,
    )
    .await;
    assert!(!out.is_error);
    assert!(text(&out).starts_with("Todos have been modified successfully."));
    let items = ext.get_or_default::<TodoList>().items();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].status, TodoStatus::InProgress);

    let done = r#"{"todos":[{"content":"Run tests","status":"completed","activeForm":"Running tests"}]}"#;
    assert!(!call(&tool, &ext, done).await.is_error);
    assert!(ext.get_or_default::<TodoList>().items().is_empty(), "an all-completed list is cleared");

    let bad = call(&tool, &ext, r#"{"todos":[{"content":"","status":"pending","activeForm":"x"}]}"#).await;
    assert!(bad.is_error && text(&bad).contains("Content cannot be empty"));
    let v: serde_json::Value = serde_json::from_slice(TodoWrite::new().deferred().spec().json()).expect("json");
    assert_eq!(v["defer_loading"], true);
    assert_eq!(v["input_schema"]["required"], serde_json::json!(["todos"]));
}

#[tokio::test]
async fn tasks_persist_with_dependencies() {
    let dir = TempDir::new("tasks");
    let store = TaskStore::new(&dir.0);
    let (create, get, update, list) = (
        TaskCreate::new(store.clone()),
        TaskGet::new(store.clone()),
        TaskUpdate::new(store.clone()),
        TaskList::new(store),
    );
    let ext = Extensions::default();

    for (subject, n) in [("Design", 1), ("Build", 2), ("Ship", 3)] {
        let input = format!(r#"{{"subject":"{subject}","description":"{subject} it"}}"#);
        assert_eq!(text(&call(&create, &ext, &input).await), format!("Task #{n} created successfully: {subject}"));
    }
    let saved = std::fs::read_to_string(dir.0.join("agent-1/1.json")).expect("task file");
    assert_eq!(
        saved,
        "{\n  \"id\": \"1\",\n  \"subject\": \"Design\",\n  \"description\": \"Design it\",\n  \"status\": \"pending\",\n  \
\"blocks\": [],\n  \"blockedBy\": []\n}"
    );

    let out = call(&update, &ext, r#"{"taskId":"2","addBlockedBy":["1"],"status":"in_progress"}"#).await;
    assert_eq!(text(&out), "Updated task #2 status, blockedBy");
    assert_eq!(
        text(&call(&get, &ext, r#"{"taskId":"2"}"#).await),
        "Task #2: Build\nStatus: in_progress\nDescription: Build it\nBlocked by: #1"
    );
    assert!(text(&call(&get, &ext, r#"{"taskId":"1"}"#).await).ends_with("Blocks: #2"));
    assert_eq!(
        text(&call(&list, &ext, "{}").await),
        "#1 [pending] Design\n#2 [in_progress] Build [blocked by #1]\n#3 [pending] Ship"
    );

    call(&update, &ext, r#"{"taskId":"1","status":"completed","owner":"me"}"#).await;
    assert!(text(&call(&list, &ext, "{}").await).contains("#1 [completed] Design (me)\n#2 [in_progress] Build\n"));

    assert_eq!(text(&call(&update, &ext, r#"{"taskId":"3","status":"deleted"}"#).await), "Updated task #3 deleted");
    assert_eq!(
        text(&call(&create, &ext, r#"{"subject":"Again","description":"d"}"#).await),
        "Task #4 created successfully: Again",
        "deleted ids are not reused"
    );
    call(&update, &ext, r#"{"taskId":"1","status":"deleted"}"#).await;
    assert_eq!(
        text(&call(&get, &ext, r#"{"taskId":"2"}"#).await),
        "Task #2: Build\nStatus: in_progress\nDescription: Build it"
    );
    assert_eq!(text(&call(&get, &ext, r#"{"taskId":"9"}"#).await), "Task not found");
    assert_eq!(text(&call(&update, &ext, r#"{"taskId":"9","subject":"x"}"#).await), "Task not found");

    // Another agent sees its own list unless the host shares one.
    let other = Extensions::default();
    assert_eq!(text(&call_as(&list, "agent-2", &other, "{}").await), "No tasks found");
    other.insert(TaskListId("agent-1".into()));
    assert!(text(&call_as(&list, "agent-2", &other, "{}").await).starts_with("#2 [in_progress] Build"));
}

#[tokio::test]
async fn nudge_after_ten_quiet_turns_at_most_every_ten() {
    let nudge = TrackingNudge::new(NudgeKind::Todos);
    let ext = Extensions::default();
    let mut fired = Vec::new();
    for turn in 0..25 {
        if turn == 12 {
            let input = r#"{"todos":[{"content":"Ship","status":"pending","activeForm":"Shipping"}]}"#;
            call(&TodoWrite::new(), &ext, input).await;
        }
        let notes = nudge.collect(&ext).await;
        if !notes.is_empty() {
            fired.push((turn, notes));
        }
    }
    let turns: Vec<usize> = fired.iter().map(|(t, _)| *t).collect();
    assert_eq!(turns, vec![10, 22]);
    assert!(fired[0].1[0].starts_with("<system-reminder>\nThe TodoWrite tool hasn't been used recently."));
    assert!(
        fired[1].1[0]
            .ends_with("Here are the existing contents of your todo list:\n\n[1. [pending] Ship]\n</system-reminder>")
    );
}

#[tokio::test]
async fn task_nudge_lists_open_tasks() {
    let dir = TempDir::new("nudge");
    let store = TaskStore::new(&dir.0);
    let ext = Extensions::default();
    call(&TaskCreate::new(store.clone()), &ext, r#"{"subject":"Write docs","description":"d"}"#).await;
    let nudge = TrackingNudge::new(NudgeKind::Tasks(store));
    let notes: Vec<String> = {
        let mut all = Vec::new();
        for _ in 0..11 {
            all.extend(nudge.collect(&ext).await);
        }
        all
    };
    assert_eq!(notes.len(), 1);
    assert!(notes[0].contains("consider using TaskCreate to add new tasks"));
    assert!(notes[0].ends_with("Here are the existing tasks:\n\n#1. [pending] Write docs\n</system-reminder>"));
}

fn deferred(name: &str, description: &str, hint: Option<&str>) -> DeferredTool {
    DeferredTool { name: name.into(), description: description.into(), search_hint: hint.map(Into::into) }
}

fn catalog() -> ToolSearch {
    ToolSearch::new(vec![
        deferred("NotebookEdit", "Replace the contents of a cell in a Jupyter notebook.", None),
        deferred("WebFetch", "Fetches content from a URL.", Some("fetch and extract content from a URL")),
        deferred("WebSearch", "Search the web and use the results.", None),
        deferred("CronCreate", "Create a job.", Some("schedule a recurring or one-shot prompt")),
        deferred("mcp__slack__send_message", "Send a message to a Slack channel.", None),
        deferred("mcp__slack__list_channels", "List channels.", None),
        deferred("mcp__github__create_issue", "Open an issue.", None),
    ])
}

#[test]
fn tool_search_query_forms() {
    let search = catalog();
    assert_eq!(search.search("select:WebFetch,NotebookEdit,Missing", 5), ["WebFetch", "NotebookEdit"]);
    assert_eq!(search.search("SELECT: webfetch , send_message", 5), ["WebFetch", "mcp__slack__send_message"]);
    assert_eq!(search.search("notebook jupyter", 5), ["NotebookEdit"]);
    assert_eq!(search.search("+slack send", 5), ["mcp__slack__send_message", "mcp__slack__list_channels"]);
    assert_eq!(search.search("web", 5), ["WebSearch", "WebFetch"], "its description says web too");
    assert_eq!(search.search("web", 1), ["WebSearch"]);
    assert_eq!(search.search("schedule", 5), ["CronCreate"], "found through its search hint");
    assert_eq!(search.search("webSEARCH", 5), ["WebSearch"], "exact name wins");
    assert_eq!(search.search("mcp__github", 5), ["mcp__github__create_issue"]);
    assert!(search.search("+nothing web", 5).is_empty());
}

#[tokio::test]
async fn tool_search_answers_with_tool_references() {
    let search = catalog();
    let out = call(&search, &Extensions::default(), r#"{"query":"select:WebFetch,WebSearch","max_results":5}"#).await;
    let ToolResultContent::Blocks(blocks) = &out.content else { panic!("expected blocks") };
    let raw: Vec<&str> = blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Unknown(raw) => raw.get(),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        raw,
        [r#"{"tool_name":"WebFetch","type":"tool_reference"}"#, r#"{"tool_name":"WebSearch","type":"tool_reference"}"#]
    );
    let none = call(&search, &Extensions::default(), r#"{"query":"zzz"}"#).await;
    assert_eq!(text(&none), "No matching deferred tools found");

    let spec = DeferredTool::from_spec(TodoWrite::new().spec());
    assert_eq!(spec.search_hint.as_deref(), Some("manage the session task checklist"));
    assert!(spec.description.starts_with("Use this tool to create and manage a structured task list"));
}

#[tokio::test]
async fn structured_output_checks_the_schema() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"answer": {"type": "string"}, "score": {"type": "number"}},
        "required": ["answer"],
        "additionalProperties": false
    });
    let tool = StructuredOutput::new(schema.clone());
    let v: serde_json::Value = serde_json::from_slice(tool.spec().json()).expect("json");
    assert_eq!(v["input_schema"], schema);
    let ext = Extensions::default();
    let bad = call(&tool, &ext, r#"{"score":"high"}"#).await;
    assert!(bad.is_error);
    assert_eq!(
        text(&bad),
        "Output does not match required schema: root: must have required property 'answer', /score: must be number"
    );
    assert!(ext.get_or_default::<StructuredResult>().get().is_none());
    let ok = call(&tool, &ext, r#"{"answer":"42","score":0.9}"#).await;
    assert_eq!(text(&ok), "Structured output provided successfully");
    assert_eq!(
        ext.get_or_default::<StructuredResult>().take(),
        Some(serde_json::json!({"answer": "42", "score": 0.9}))
    );
}

#[derive(Debug)]
struct Scripted {
    reply: Reply,
    asked: Mutex<Vec<(String, String, Vec<Question>)>>,
}

impl UserPrompter for Scripted {
    fn ask<'a>(
        &'a self,
        agent_id: &'a str,
        tool_use_id: &'a str,
        questions: &'a [Question],
    ) -> BoxFuture<'a, Result<Reply, Refusal>> {
        self.asked.lock().expect("lock").push((agent_id.to_owned(), tool_use_id.to_owned(), questions.to_vec()));
        Box::pin(std::future::ready(Ok(self.reply.clone())))
    }
}

const QUESTIONS: &str = r#"{"questions":[
    {"question":"Which library?","header":"Library","multiSelect":false,
     "options":[{"label":"date-fns","description":"small"},{"label":"luxon","description":"zones"}]},
    {"question":"Which features?","header":"Features","multiSelect":true,
     "options":[{"label":"Dark mode","description":"d"},{"label":"Sync","description":"s"}]}]}"#;

fn prompter(answers: &[(&str, &str)], annotations: &[(&str, Annotation)], response: Option<&str>) -> Arc<Scripted> {
    Arc::new(Scripted {
        reply: Reply {
            answers: answers.iter().map(|(q, a)| ((*q).to_owned(), (*a).to_owned())).collect(),
            annotations: annotations.iter().map(|(q, a)| ((*q).to_owned(), a.clone())).collect(),
            response: response.map(Into::into),
            ..Reply::default()
        },
        asked: Mutex::new(Vec::new()),
    })
}

#[tokio::test]
async fn ask_user_question_goes_through_the_prompter() {
    let ext = Extensions::default();
    let fake = prompter(&[("Which library?", "luxon"), ("Which features?", "Dark mode, Sync")], &[], None);
    let out = call(&AskUserQuestion::new(fake.clone()), &ext, QUESTIONS).await;
    assert_eq!(
        text(&out),
        "Your questions have been answered: \"Which library?\"=\"luxon\", \"Which features?\"=\"Dark mode, Sync\". \
You can now continue with these answers in mind."
    );
    let (agent, asking, asked) = fake.asked.lock().expect("lock")[0].clone();
    assert_eq!((agent.as_str(), asking.as_str()), ("agent-1", "t1"), "the asking call reaches the host");
    assert!(asked[1].multi_select);

    let notes = Annotation { preview: None, notes: Some("only if cheap".into()) };
    let free = prompter(&[("Which library?", "neither, use std")], &[("Which features?", notes)], None);
    assert_eq!(
        text(&call(&AskUserQuestion::new(free), &ext, QUESTIONS).await),
        "The user answered: \"Which library?\"=\"neither, use std\", \"Which features?\"=(no option selected) notes: \
only if cheap. Read the answers carefully — they may request clarification, changes, or that you not proceed — and \
follow what they actually say."
    );
    let typed = prompter(&[], &[], Some("Let's talk first"));
    assert_eq!(
        text(&call(&AskUserQuestion::new(typed), &ext, QUESTIONS).await),
        "The user responded: Let's talk first"
    );
    let silent = prompter(&[], &[], None);
    assert_eq!(
        text(&call(&AskUserQuestion::new(silent), &ext, QUESTIONS).await),
        "The user did not answer the questions."
    );

    let nobody = call(&AskUserQuestion::new(Arc::new(NoUser)), &ext, QUESTIONS).await;
    assert!(nobody.is_error && text(&nobody).starts_with("No user is available to answer questions"));

    let one_option = r#"{"questions":[{"question":"Q?","header":"H","options":[{"label":"a","description":"d"}]}]}"#;
    assert!(call(&AskUserQuestion::new(Arc::new(NoUser)), &ext, one_option).await.is_error);
}

#[derive(Debug)]
struct Refusing(Refusal);

impl UserPrompter for Refusing {
    fn ask<'a>(&'a self, _: &'a str, _: &'a str, _: &'a [Question]) -> BoxFuture<'a, Result<Reply, Refusal>> {
        Box::pin(std::future::ready(Err(self.0.clone())))
    }
}

fn json(blocks: &[ContentBlock]) -> String {
    serde_json::to_string(blocks).expect("json")
}

fn image() -> ContentBlock {
    serde_json::from_value(serde_json::json!({
        "type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}
    }))
    .expect("image block")
}

/// Claude Code's Esc rejects the call and ends the turn (its
/// `cancelAndAbort`); with words said ("Chat about this") the model reads
/// them and goes on; pasted images go beside the result.
#[tokio::test]
async fn ask_user_question_refusals_and_images() {
    let ext = Extensions::default();
    let raw = RawValue::from_string(QUESTIONS.to_owned()).expect("json");
    let cancel = CancellationToken::new();
    let tool = AskUserQuestion::new(Arc::new(Refusing(Refusal::Cancelled)));
    let ctx =
        ToolContext { agent_id: "agent-1", tool_use_id: "t1", cancel: &cancel, extensions: &ext, transcript: &[] };
    let out = tool.call(&raw, ctx).await;
    assert!(out.is_error && cancel.is_cancelled(), "the turn is stopped");
    assert_eq!(text(&out), eos_agent_tools_session::REJECTED);

    let said = Refusal::Said { text: "wait".into(), images: vec![image()] };
    let out = call(&AskUserQuestion::new(Arc::new(Refusing(said))), &ext, QUESTIONS).await;
    assert!(out.is_error);
    assert_eq!(text(&out), format!("{}wait", eos_agent_tools_session::REJECTED_SAYING));
    assert_eq!(json(&out.beside), json(&[image()]));

    let mut reply = Reply { images: vec![image()], ..Reply::default() };
    reply.answers.insert("Which library?".into(), "luxon (Image attached)".into());
    let fake = Arc::new(Scripted { reply, asked: Mutex::new(Vec::new()) });
    let out = call(&AskUserQuestion::new(fake), &ext, QUESTIONS).await;
    assert!(!out.is_error);
    assert_eq!(json(&out.beside), json(&[image()]));
}

/// Claude Code's idle auto-continue: what the model reads when the dialog
/// answered by itself.
#[tokio::test]
async fn ask_user_question_after_the_user_went_idle() {
    let ext = Extensions::default();
    let away = "No response after 60s \u{2014} the user may be away from keyboard. Proceed using your best judgment \
based on the context so far; you can re-ask this question later if it's still relevant.";
    let idle = |answers: &[(&str, &str)]| {
        let mut reply = Reply { afk_timeout_ms: Some(60_000), ..Reply::default() };
        reply.answers = answers.iter().map(|(q, a)| ((*q).to_owned(), (*a).to_owned())).collect();
        Arc::new(Scripted { reply, asked: Mutex::new(Vec::new()) })
    };
    assert_eq!(text(&call(&AskUserQuestion::new(idle(&[])), &ext, QUESTIONS).await), away);
    assert_eq!(
        text(&call(&AskUserQuestion::new(idle(&[("Which library?", "luxon")])), &ext, QUESTIONS).await),
        format!("{away}\n\nBefore going idle the user had selected: \"Which library?\"=\"luxon\".")
    );
}

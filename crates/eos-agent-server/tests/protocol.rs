mod common;

use std::time::Duration;

use common::Client;
use eos_agent_mock_api::{MockApi, Reply};
use serde_json::{Value, json};

async fn mock(replies: impl IntoIterator<Item = Reply>) -> MockApi {
    MockApi::start(replies).await.expect("mock")
}

/// Every text in the request's messages, joined.
fn request_text(mock: &MockApi, index: usize) -> String {
    mock.requests()[index].json()["messages"].to_string()
}

#[tokio::test]
async fn a_turn_streams_its_events_before_the_response() {
    let mock = mock([Reply::text("hello there")]).await;
    let mut client = Client::start("turn");
    let init = client.initialize(&mock.base_url(), json!({"permissions": "allow"})).await;
    assert_eq!(init["protocolVersion"], 1);
    let again = client.call("initialize", json!({})).await.expect_err("only once");
    assert_eq!(again["code"], -32600);

    let agent = client.spawn_agent().await;
    let done = client.call("agent.send", json!({"agentId": agent, "text": "hi"})).await.expect("turn");
    assert_eq!(done, json!({"outcome": "finished", "stopReason": "end_turn"}));
    assert_eq!(client.text_of(&agent), "hello there");
    let kinds: Vec<&str> = client.events.iter().filter_map(|e| e["event"]["type"].as_str()).collect();
    assert_eq!(kinds.first(), Some(&"message"), "the user's message comes first: {kinds:?}");
    assert_eq!(kinds.last(), Some(&"turnEnded"), "{kinds:?}");
    assert!(kinds.contains(&"requestStarted") && kinds.contains(&"usage"));
    let assistant = client.events.iter().find(|e| e["event"]["message"]["role"] == "assistant").expect("reply");
    assert_eq!(assistant["event"]["message"]["content"][0]["text"], "hello there");
    assert!(client.events.iter().all(|e| e.get("parentId").is_none()));

    let list = client.call("agent.list", json!({})).await.expect("list");
    assert_eq!(list.as_array().map(Vec::len), Some(1));
    assert_eq!(list[0]["agentId"], agent.as_str());
    assert_eq!(list[0]["busy"], false);
    client.call("agent.close", json!({"agentId": agent})).await.expect("closed");
    let list = client.call("agent.list", json!({})).await.expect("list");
    assert_eq!(list, json!([]));
    client.close().await;
}

#[tokio::test]
async fn permissions_and_questions_go_to_the_client() {
    let questions = r#"{"questions":[{"question":"Which one?","header":"Pick","multiSelect":false,
        "options":[{"label":"A","description":"first"},{"label":"B","description":"second"}]}]}"#;
    let mock = mock([
        Reply::tool_use("toolu_w", "Write", r#"{"file_path":"/tmp/eos-server-never.txt","content":"x"}"#),
        Reply::tool_use("toolu_q", "AskUserQuestion", questions),
        Reply::text("ok"),
    ])
    .await;
    let mut client = Client::start("ask");
    client.initialize(&mock.base_url(), json!({})).await;
    client.answer = Box::new(|method, params| match method {
        "permission.decide" if params["tool"] == "Write" => json!({"decision": "deny", "message": "No writes today."}),
        "permission.decide" => json!({"decision": "allow"}),
        "user.ask" => json!({"answers": {params["questions"][0]["question"].as_str().unwrap_or(""): "A"}}),
        _ => Value::Null,
    });
    let agent = client.spawn_agent().await;
    client.call("agent.send", json!({"agentId": agent, "text": "go"})).await.expect("turn");

    let methods: Vec<&str> = client.asked.iter().filter_map(|m| m["method"].as_str()).collect();
    assert_eq!(methods, ["permission.decide", "permission.decide", "user.ask"]);
    let decide = &client.asked[0]["params"];
    assert_eq!(decide["agentId"], agent.as_str());
    assert_eq!(decide["toolUseId"], "toolu_w");
    assert_eq!(decide["input"]["content"], "x");
    assert_eq!(decide["sandboxed"], false);
    assert_eq!(decide["rootAgentId"], agent.as_str());
    assert_eq!((&decide["subagent"], &decide["workflow"]), (&json!(false), &json!(false)));
    assert!(decide.get("parentAgentId").is_none(), "a top-level agent has no parent");
    assert!(request_text(&mock, 1).contains("No writes today."));
    assert!(!std::path::Path::new("/tmp/eos-server-never.txt").exists());
    assert!(request_text(&mock, 2).contains(r#"\"Which one?\"=\"A\""#), "{}", request_text(&mock, 2));
    client.close().await;
}

#[tokio::test]
async fn sandboxed_commands_are_not_asked_about() {
    if !std::path::Path::new("/usr/bin/sandbox-exec").exists() {
        return;
    }
    let mock = mock([
        Reply::tool_use("toolu_1", "Bash", r#"{"command":"echo inside"}"#),
        Reply::tool_use("toolu_2", "Bash", r#"{"command":"echo outside","dangerouslyDisableSandbox":true}"#),
        Reply::text("ok"),
    ])
    .await;
    let mut client = Client::start("sandbox");
    client.initialize(&mock.base_url(), json!({"sandbox": {"enabled": true}})).await;
    client.answer = Box::new(|_, _| json!({"decision": "allow"}));
    let agent = client.spawn_agent().await;
    client.call("agent.send", json!({"agentId": agent, "text": "go"})).await.expect("turn");
    let asked: Vec<&Value> = client.asked.iter().filter(|m| m["method"] == "permission.decide").collect();
    assert_eq!(asked.len(), 1, "only the unsandboxed command is asked about");
    assert_eq!(asked[0]["params"]["toolUseId"], "toolu_2");
    assert_eq!(asked[0]["params"]["sandboxed"], false);
    assert!(request_text(&mock, 1).contains("inside"));
    client.close().await;
}

#[tokio::test]
async fn subagent_events_arrive_tagged_with_their_parent() {
    let call = r#"{"description":"Look","prompt":"List files","subagent_type":"Explore","run_in_background":false}"#;
    let mock =
        mock([Reply::tool_use("toolu_a", "Agent", call), Reply::text("two files"), Reply::text("it found two")]).await;
    let mut client = Client::start("sub");
    client.initialize(&mock.base_url(), json!({"permissions": "allow"})).await;
    let agent = client.spawn_agent().await;
    client.call("agent.send", json!({"agentId": agent, "text": "look"})).await.expect("turn");

    let sub: Vec<&Value> = client.events.iter().filter(|e| e["agentId"] != agent.as_str()).collect();
    assert!(!sub.is_empty(), "the subagent's events reached the client");
    assert!(sub.iter().all(|e| e["parentId"] == agent.as_str()));
    let child = sub[0]["agentId"].as_str().expect("id").to_owned();
    assert_eq!(client.text_of(&child), "two files");
    assert_eq!(client.text_of(&agent), "it found two");
    let list = client.call("agent.list", json!({})).await.expect("list");
    let listed = list.as_array().expect("array").iter().find(|a| a["agentId"] == child.as_str()).expect("listed");
    assert_eq!(listed["parentId"], agent.as_str());
    assert_eq!(listed["agentType"], "Explore");
    client.close().await;
}

#[tokio::test]
async fn interrupt_compact_notify_and_errors() {
    let mock = mock([
        Reply::text("a long answer").slowed(Duration::from_millis(300)),
        Reply::text("hello"),
        Reply::text("the summary"),
        Reply::text("woken"),
    ])
    .await;
    let mut client = Client::start("control");
    let early = client.call("agent.list", json!({})).await.expect_err("not initialized");
    assert_eq!(early["code"], -32002);
    client.initialize(&mock.base_url(), json!({"permissions": "allow"})).await;
    let unknown = client.call("agent.fly", json!({})).await.expect_err("no such method");
    assert_eq!(unknown["code"], -32601);
    let missing = client.call("agent.send", json!({"agentId": "nope", "text": "x"})).await.expect_err("no agent");
    assert_eq!(missing["code"], -32001);
    let bad = client.call("agent.spawn", json!({"cwd": 3})).await.expect_err("bad params");
    assert_eq!(bad["code"], -32602);

    let agent = client.spawn_agent().await;
    let turn = client.start_call("agent.send", json!({"agentId": agent, "text": "talk"})).await;
    client.until_event(|e| e["event"]["type"] == "requestStarted").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    client.call("agent.interrupt", json!({"agentId": agent})).await.expect("interrupted");
    assert_eq!(client.wait(turn).await.expect("turn"), json!({"outcome": "interrupted"}));
    assert!(client.events.iter().any(|e| e["event"]["type"] == "interrupted"));

    client
        .call("agent.send", json!({"agentId": agent, "content": [{"type": "text", "text": "hi"}]}))
        .await
        .expect("turn");
    let compacted = client.call("agent.compact", json!({"agentId": agent, "focus": "names"})).await.expect("compacted");
    assert_eq!(compacted, json!({"compacted": true}));
    assert!(client.events.iter().any(|e| e["event"]["type"] == "compacted"));

    client.call("agent.notify", json!({"agentId": agent, "text": "build finished", "wake": true})).await.expect("ok");
    client.until_event(|e| e["event"]["type"] == "textDelta" && e["event"]["text"] == "woken").await;
    let woke = request_text(&mock, 3);
    assert!(
        woke.contains("build finished") && woke.contains("the summary"),
        "the summary replaced the history: {woke}"
    );
    client.close().await;
}

#[tokio::test]
async fn resume_continues_a_recorded_conversation() {
    let mock = mock([Reply::text("first"), Reply::text("second")]).await;
    let mut client = Client::start("resume");
    let transcripts = client.root.join("transcripts");
    client.initialize(&mock.base_url(), json!({"permissions": "allow", "transcripts": transcripts})).await;
    let agent = client.spawn_agent().await;
    client.call("agent.send", json!({"agentId": agent, "text": "remember 42"})).await.expect("turn");
    client.call("agent.close", json!({"agentId": agent})).await.expect("closed");

    let project = client.project();
    let resumed = client.call("agent.spawn", json!({"cwd": project, "mcp": false, "id": agent, "resume": true})).await;
    assert_eq!(resumed.expect("resumed")["agentId"], agent.as_str());
    client.call("agent.send", json!({"agentId": agent, "text": "what number?"})).await.expect("turn");
    let text = request_text(&mock, 1);
    assert!(text.contains("remember 42") && text.contains("what number?"), "{text}");
    client.close().await;
}

#[tokio::test]
async fn the_artifact_tools_can_have_their_own_credential() {
    let has_artifacts = |mock: &MockApi| {
        mock.requests()[0].json()["tools"].as_array().expect("tools").iter().any(|t| t["name"] == "Artifact")
    };
    for (extra, on) in [(json!({}), false), (json!({"artifactsAuth": {"authTokenEnv": "TEST_SIGN_IN"}}), true)] {
        let mock = mock([Reply::text("hi")]).await;
        let mut client = Client::start("artifacts-auth");
        let mut params = json!({"permissions": "allow"});
        params.as_object_mut().expect("object").extend(extra.as_object().expect("object").clone());
        client.initialize(&mock.base_url(), params).await;
        let agent = client.spawn_agent().await;
        client.call("agent.send", json!({"agentId": agent, "text": "hi"})).await.expect("turn");
        assert_eq!(has_artifacts(&mock), on, "{extra}");
        let request = &mock.requests()[0];
        assert_eq!(
            request.headers.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("k"),
            "the model keeps its key"
        );
        client.close().await;
    }

    let mut client = Client::start("artifacts-auth-unset");
    let unset = client.call("initialize", json!({"artifactsAuth": {"authTokenEnv": "NOT_SET"}})).await;
    assert!(
        unset.expect_err("an unset variable is refused")["message"].as_str().is_some_and(|m| m.contains("NOT_SET"))
    );
    client.close().await;
}

#[tokio::test]
async fn openai_models_run_on_a_chatgpt_store() {
    let mock = mock([Reply::text("from claude"), eos_agent_mock_api::Reply::responses_text("from gpt")]).await;
    mock.set_models(json!({"models": [{"slug": "gpt-6-sol", "visibility": "list"}]}));
    let mut client = Client::start("openai");
    // A signed-in store whose token is good for an hour.
    let store = client.root.join("chatgpt");
    std::fs::create_dir_all(&store).expect("dir");
    let expires =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("time").as_millis() + 3_600_000;
    let record = json!({"issuer": "https://auth.openai.com", "subject": "u", "client_id": "oaiapp_1",
        "ext_agent_host_id": "urn:uuid:h", "access_token": "plan-token", "refresh_token": "r", "expires_at": expires as u64,
        "scopes": ["chatgpt.tokens.use.direct"], "saved_at": 1});
    std::fs::write(store.join("siwc.json"), record.to_string()).expect("record");
    let auth = json!([{"apiKeyEnv": "TEST_KEY"}, {"openaiStore": store}]);
    let openai = format!("{}/v1", mock.base_url());
    client.initialize(&mock.base_url(), json!({"permissions": "allow", "auth": auth, "openaiBaseUrl": openai})).await;

    let claude = client.spawn_agent().await;
    client.call("agent.send", json!({"agentId": claude, "text": "hi"})).await.expect("claude turn");
    let project = client.project();
    let spawned =
        client.call("agent.spawn", json!({"cwd": project, "mcp": false, "model": "gpt"})).await.expect("spawned");
    let gpt = spawned["agentId"].as_str().expect("id").to_owned();
    client.call("agent.send", json!({"agentId": gpt, "text": "hi"})).await.expect("gpt turn");
    assert_eq!(client.text_of(&gpt), "from gpt");
    let sent = mock.requests();
    assert_eq!((sent[0].path.as_str(), sent[1].path.as_str()), ("/v1/messages", "/v1/responses"));
    assert_eq!(sent[1].headers["authorization"], "Bearer plan-token");
    assert_eq!(sent[1].json()["model"], "gpt-6-sol");
    let list = client.call("agent.list", json!({})).await.expect("list");
    assert!(list.as_array().expect("agents").iter().any(|a| a["model"] == "gpt-6-sol"));
    client.close().await;
}

#[tokio::test]
async fn an_allow_may_carry_the_input_to_run_with() {
    let call = r#"{"description":"Look","prompt":"List files","run_in_background":false}"#;
    let mock = mock([
        Reply::tool_use("toolu_a", "Agent", call),
        Reply::tool_use("toolu_g", "Glob", r#"{"pattern":"*.nothing"}"#),
        Reply::text("found"),
        Reply::text("done"),
    ])
    .await;
    let mut client = Client::start("updated-input");
    client.initialize(&mock.base_url(), json!({})).await;
    client.answer = Box::new(|_, params| match params["tool"].as_str() {
        Some("Glob") => json!({"decision": "allow", "updatedInput": {"pattern": "*.here"}}),
        _ => json!({"decision": "allow"}),
    });
    let agent = client.spawn_agent().await;
    std::fs::write(client.project().join("it.here"), "x").expect("file");
    client.call("agent.send", json!({"agentId": agent, "text": "look"})).await.expect("turn");

    let glob = client.asked.iter().find(|m| m["params"]["tool"] == "Glob").expect("asked about Glob");
    let params = &glob["params"];
    assert_eq!(params["rootAgentId"], agent.as_str());
    assert_eq!(params["parentAgentId"], agent.as_str());
    assert_eq!(params["parentToolUseId"], "toolu_a");
    assert_eq!(params["subagent"], true);
    assert_ne!(params["agentId"], agent.as_str());
    assert!(request_text(&mock, 2).contains("it.here"), "the tool ran with the client's input");
    client.close().await;
}

#[tokio::test]
async fn a_client_changes_a_live_agent_and_asks_one_shot_questions() {
    let mock = mock([
        Reply::text("one"),
        Reply::text("two"),
        Reply::tool_use("toolu_b", "Bash", r#"{"command":"sleep 30","run_in_background":true}"#),
        Reply::text("started"),
        Reply::text("A title"),
        Reply::tool_use("toolu_s", "StructuredOutput", r#"{"ok":true}"#),
    ])
    .await;
    let mut client = Client::start("host-api");
    client.initialize(&mock.base_url(), json!({"permissions": "allow", "model": "claude-opus-5-5"})).await;
    let skill = client.project().join(".claude/skills/review");
    std::fs::create_dir_all(&skill).expect("dir");
    std::fs::write(skill.join("SKILL.md"), "---\ndescription: Review it\nargument-hint: \"[pr]\"\n---\nReview.")
        .expect("skill");
    let agent = client.spawn_agent().await;
    client.call("agent.send", json!({"agentId": agent, "text": "prompt-alpha"})).await.expect("turn");

    // Model and effort hold for later turns.
    let set = client.call("agent.setModel", json!({"agentId": agent, "model": "haiku", "effort": "low"})).await;
    assert_eq!(set.expect("set"), json!({"model": "claude-haiku-5-5", "effort": "low"}));
    let other = client.call("agent.setModel", json!({"agentId": agent, "model": "gpt-5"})).await.expect_err("provider");
    assert_eq!(other["code"], -32602);

    // Back to the start: the first turn is dropped.
    client.call("agent.rewind", json!({"agentId": agent, "keep": 0})).await.expect("rewound");
    client.call("agent.send", json!({"agentId": agent, "text": "prompt-beta"})).await.expect("turn");
    let body = mock.requests()[1].json();
    assert_eq!(body["model"], "claude-haiku-5-5");
    assert_eq!(body["output_config"]["effort"], "low");
    assert!(request_text(&mock, 1).contains("prompt-beta"));
    assert!(!request_text(&mock, 1).contains("prompt-alpha"), "the rewound turn is gone");

    let commands = client.call("agent.commands", json!({"agentId": agent})).await.expect("commands");
    let review = commands.as_array().expect("array").iter().find(|c| c["name"] == "review").expect("listed");
    assert_eq!(review["argumentHint"], "[pr]");
    assert_eq!((&review["source"], &review["userInvocable"]), (&json!("project"), &json!(true)));

    client.call("agent.send", json!({"agentId": agent, "text": "run it"})).await.expect("turn");
    // The task's id as its `task.event` announced it.
    let started = client.tasks.iter().find(|t| t["event"]["type"] == "started").expect("announced");
    assert_eq!(started["event"]["taskType"], "local_bash");
    let id = started["taskId"].as_str().expect("id").to_owned();
    let stopped = client.call("task.stop", json!({"agentId": agent, "taskId": id})).await.expect("stopped");
    let stopped_json = json!({"agentId": agent, "taskId": id, "taskType": "local_bash", "description": "sleep 30"});
    assert_eq!(stopped, stopped_json);
    let again = client.call("task.stop", json!({"agentId": agent, "taskId": id})).await.expect_err("ended");
    assert_eq!(again["code"], -32600);

    let title = client.call("complete", json!({"prompt": "Name it", "system": "Titles only.", "model": "haiku"})).await;
    let title = title.expect("answered");
    assert_eq!((&title["text"], &title["model"]), (&json!("A title"), &json!("claude-haiku-5-5")));
    assert_eq!(title["usage"]["output_tokens"], 5);
    let schema = json!({"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"]});
    let judged = client.call("complete", json!({"prompt": "Judge it", "schema": schema})).await.expect("answered");
    assert_eq!(judged["structured"], json!({"ok": true}));
    client.close().await;
}

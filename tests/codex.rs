//! Opt-in integration with the installed Codex CLI and a local deterministic Responses server.
use axum::{Router, body::Bytes, http::HeaderMap, routing::post};
use codex_turn_proxy::cli;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn sse_event(kind: &str, value: Value) -> String {
    format!("event: {kind}\ndata: {value}\n\n")
}
fn reply(command: Option<&str>, step: usize) -> String {
    let tool = command.is_some();
    let item = if let Some(command) = command {
        json!({"type":"function_call","id":format!("fc_{step}"),"call_id":format!("call_{step}"),"name":"exec_command",
        "arguments":json!({"cmd":command,"max_output_tokens":12000}).to_string()})
    } else {
        json!({"type":"message","id":"msg_1","role":"assistant","status":"completed","phase":"final_answer",
            "content":[{"type":"output_text","text":"OFFLINE_OK","annotations":[]}]})
    };
    let response_id = if tool { "resp_1" } else { "resp_2" };
    let mut text = sse_event(
        "response.created",
        json!({"type":"response.created","response":{"id":response_id,"status":"in_progress","output":[]}}),
    );
    text += &sse_event(
        "response.output_item.added",
        json!({"type":"response.output_item.added","output_index":0,"item":item}),
    );
    if !tool {
        text += &sse_event(
            "response.output_text.delta",
            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_1","delta":"OFFLINE_OK"}),
        );
    }
    text += &sse_event(
        "response.output_item.done",
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
    );
    text += &sse_event(
        "response.completed",
        json!({"type":"response.completed","response":{"id":response_id,"status":"completed","output":[item],
        "usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}}}),
    );
    text
}
fn read_rollouts(path: &Path, results: &mut Vec<Value>) {
    if !path.exists() {
        return;
    }
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            read_rollouts(&path, results);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            for line in fs::read_to_string(&path).unwrap().lines() {
                if let Ok(value) = serde_json::from_str(line) {
                    results.push(value);
                }
            }
        }
    }
}
#[tokio::test]
#[ignore = "requires an installed codex CLI; uses only a local mock upstream"]
async fn test_codex_native_hook_previews_first_then_reads_original_as_new_result() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mock = Router::new().route(
        "/v1/responses",
        post(move |headers: HeaderMap, body: Bytes| {
            let tx = tx.clone();
            async move {
                let input: Value = serde_json::from_slice(&body).unwrap();
                let outputs: Vec<_> = input["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|item| item["type"] == "function_call_output")
                    .collect();
                let command = match outputs.len() {
                    0 => Some("printf 'native-evidence\\n%.0s' {1..600}".to_owned()),
                    1 => {
                        let text = outputs[0]["output"].as_str().unwrap();
                        let id = text
                            .lines()
                            .find_map(|line| line.strip_prefix("ID: "))
                            .expect("first tool result must contain a persisted ID");
                        Some(format!(
                            "{} read {id} 1 2000",
                            cli::quote(env!("CARGO_BIN_EXE_codex-turn-proxy"))
                        ))
                    }
                    _ => None,
                };
                let step = outputs.len();
                tx.send((headers, input)).unwrap();
                (
                    [("content-type", "text/event-stream")],
                    reply(command.as_deref(), step),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}/v1", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("codex");
    fs::create_dir(&home).unwrap();
    let binary = env!("CARGO_BIN_EXE_codex-turn-proxy");
    fs::write(home.join("hooks.json"),json!({"hooks":{"PreToolUse":[{"matcher":"^Bash$","hooks":[{"type":"command","command":format!("{} hook",cli::quote(binary)),"timeout":10}]}]}}).to_string()).unwrap();
    fs::write(
        home.join("config.toml"),
        format!(
            r#"
model = "gpt-5.4"
model_provider = "offline"
approval_policy = "never"
sandbox_mode = "workspace-write"
[features]
code_mode = false
code_mode_only = false
hooks = true
[model_providers.offline]
name = "offline"
base_url = "{upstream}"
env_key = "OFFLINE_TEST_KEY"
wire_api = "responses"
supports_websockets = false
request_max_retries = 0
stream_max_retries = 0
[shell_environment_policy]
inherit = "all"
"#
        ),
    )
    .unwrap();
    // Only this child uses the isolated home. The user's configuration is never modified.
    let mut child = Command::new("codex")
        .args([
            "exec",
            "--skip-git-repo-check",
            "--dangerously-bypass-hook-trust",
            "--json",
            "Run the provided local evidence command, then report completion.",
        ])
        .current_dir(temp.path())
        .env("CODEX_HOME", &home)
        .env("OFFLINE_TEST_KEY", "offline-not-a-real-key")
        .env("OUTPUT_STORE_DIR", temp.path().join("store"))
        .stdin(Stdio::null())
        .stdout(fs::File::create(temp.path().join("stdout")).unwrap())
        .stderr(fs::File::create(temp.path().join("stderr")).unwrap())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(40) {
            child.kill().unwrap();
            let _ = child.wait();
            panic!(
                "Codex timed out: {}",
                fs::read_to_string(temp.path().join("stderr")).unwrap()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let stderr = fs::read_to_string(temp.path().join("stderr")).unwrap();
    let stdout = fs::read_to_string(temp.path().join("stdout")).unwrap();
    assert!(status.success(), "{stderr}\n{stdout}");
    let mut requests = Vec::new();
    while let Ok(request) = rx.try_recv() {
        requests.push(request);
    }
    assert!(
        requests.len() >= 2,
        "{stderr}\n{stdout}\nrequests={requests:?}"
    );
    assert_eq!(requests.len(), 3, "{stderr}\n{stdout}");
    let first_followup = &requests[1].1;
    let full = "native-evidence\n".repeat(600);
    assert!(first_followup.to_string().contains("<persisted-output>"));
    assert!(!first_followup.to_string().contains("codex_tool_evidence"));
    let first_tools: Vec<_> = first_followup["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .collect();
    assert_eq!(first_tools.len(), 1);
    assert!(!first_tools[0]["output"].as_str().unwrap().contains(&full));
    let last = &requests[2].1;
    let last_tools: Vec<_> = last["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .collect();
    assert_eq!(last_tools.len(), 2);
    assert_eq!(first_tools[0], last_tools[0], "original card changed");
    assert!(
        last_tools[1]["output"].as_str().unwrap().contains(&full),
        "read did not return original: {last}"
    );
    assert!(
        !last_tools[1]["output"]
            .as_str()
            .unwrap()
            .contains("<persisted-output>")
    );
    let old_input = first_followup["input"].as_array().unwrap();
    assert_eq!(
        &last["input"].as_array().unwrap()[..old_input.len()],
        old_input.as_slice()
    );
    let mut rollouts = Vec::new();
    read_rollouts(&home.join("sessions"), &mut rollouts);
    let tool_outputs: Vec<&Value> = rollouts
        .iter()
        .filter(|v| v["type"] == "response_item" && v["payload"]["type"] == "function_call_output")
        .collect();
    assert!(
        !tool_outputs.is_empty(),
        "no tool outputs in rollout: {rollouts:?}"
    );
    assert!(
        tool_outputs.iter().any(|v| v["payload"]["output"]
            .to_string()
            .contains("<persisted-output>")),
        "{tool_outputs:?}"
    );
    assert!(
        tool_outputs
            .iter()
            .any(|v| v["payload"]["output"].as_str().unwrap().contains(&full))
    );
    eprintln!(
        "native Codex verified: preview first; explicit CLI read appends original; prior input prefix unchanged"
    );
    mock_task.abort();
}

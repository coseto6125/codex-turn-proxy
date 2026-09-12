use axum::{
    Router,
    body::{Body, Bytes},
    http::HeaderMap,
    response::Response,
    routing::post,
};
use codex_turn_proxy::{Config, Proxy, cli};
use serde_json::{Value, json};
use std::{process::Command, sync::Arc, time::Duration};
const TOKEN: &str = "test-proxy-token-at-least-32-characters";
fn headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("thread-id", "thread-a".parse().unwrap());
    headers.insert("x-proxy-token", TOKEN.parse().unwrap());
    headers
}
#[test]
fn test_chained_hook_preserves_previous_transform_and_deny() {
    let event = json!({"hook_event_name":"PreToolUse","tool_name":"Bash","session_id":"thread-a","tool_use_id":"a", "tool_input":{"command":"original"}});
    let previous = json!({"hookSpecificOutput":{"permissionDecision":"allow","additionalContext":"existing context","updatedInput":{"command":"printf transformed"}}});
    let argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!(
            "cat >/dev/null; printf %s {}",
            cli::quote(&previous.to_string())
        ),
    ];
    let result =
        cli::chained_hook(event.clone(), std::path::Path::new("/bin/echo"), &argv).unwrap();
    assert_eq!(
        result["hookSpecificOutput"]["additionalContext"],
        "existing context"
    );
    assert!(
        result["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap()
            .contains("printf transformed")
    );
    let deny = json!({"hookSpecificOutput":{"permissionDecision":"deny"}});
    let argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!(
            "cat >/dev/null; printf %s {}",
            cli::quote(&deny.to_string())
        ),
    ];
    assert_eq!(
        cli::chained_hook(event, std::path::Path::new("/bin/echo"), &argv).unwrap(),
        deny
    );
}
#[test]
fn test_hook_wrapper_preserves_input_and_shell_quoting() {
    let command = "printf '%s' 'a b'";
    let event = json!({"hook_event_name":"PreToolUse","tool_name":"Bash","session_id":"thread-a","tool_use_id":"a",
        "tool_input":{"command":command,"cwd":"/tmp","timeout":1000}});
    let output = cli::hook(event.clone(), std::path::Path::new("/bin/echo")).unwrap();
    let input = &output["hookSpecificOutput"]["updatedInput"];
    assert_eq!(input["cwd"], "/tmp");
    assert_eq!(input["timeout"], 1000);
    assert_eq!(output["hookSpecificOutput"]["permissionDecision"], "allow");
    let actual = Command::new("bash")
        .args(["-c", input["command"].as_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(actual.stdout).unwrap().trim_end(),
        format!("capture thread-a a {command}")
    );
    let mut again = event;
    again["tool_input"] = input.clone();
    assert_eq!(
        cli::hook(again, std::path::Path::new("/bin/echo")).unwrap(),
        json!({})
    );
}

async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, task)
}
#[tokio::test]
async fn test_models_forward_preserves_query_and_auth_without_local_token() {
    let upstream = Router::new().route(
        "/v1/models",
        axum::routing::get(
            |uri: axum::extract::OriginalUri, headers: HeaderMap| async move {
                assert_eq!(uri.query(), Some("client_version=0.154.0"));
                assert_eq!(headers["authorization"], "Bearer test");
                assert!(!headers.contains_key("x-proxy-token"));
                axum::Json(json!({"models":[]}))
            },
        ),
    );
    let (upstream_url, upstream_task) = serve(upstream).await;
    let proxy = Proxy::new(Config::new(format!("{upstream_url}/v1"), TOKEN.into())).unwrap();
    let (url, task) = serve(proxy.router()).await;
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{url}/v1/models"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let response = client
        .get(format!("{url}/v1/models?client_version=0.154.0"))
        .header("x-proxy-token", TOKEN)
        .bearer_auth("test")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"models":[]})
    );
    task.abort();
    upstream_task.abort();
}
#[tokio::test]
async fn test_http_forward_preserves_request_bytes_strips_token_and_streams_without_buffering() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let gate = Arc::new(tokio::sync::Notify::new());
    let handler_gate = gate.clone();
    let upstream = Router::new().route(
        "/v1/responses",
        post(move |headers: HeaderMap, body: Bytes| {
            let tx = tx.clone();
            let gate = handler_gate.clone();
            async move {
                tx.send((headers, body)).await.unwrap();
                let stream = futures_util::stream::unfold(0, move |step| {
                    let gate = gate.clone();
                    async move {
                        match step {
                            0 => Some((
                                Ok::<_, std::io::Error>(Bytes::from_static(b"data: first\n\n")),
                                1,
                            )),
                            1 => {
                                gate.notified().await;
                                Some((Ok(Bytes::from_static(b"data: last\n\n")), 2))
                            }
                            _ => None,
                        }
                    }
                });
                Response::builder()
                    .status(201)
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
        }),
    );
    let (upstream_url, upstream_task) = serve(upstream).await;
    let proxy = Proxy::new(Config::new(format!("{upstream_url}/v1"), TOKEN.into())).unwrap();
    let original = Bytes::from_static(b"{ \"previous_response_id\": \"older\", \"input\": [{\"type\":\"function_call_output\",\"output\":\"<persisted-output>preview</persisted-output>\"}] }\n");
    let (url, task) = serve(proxy.router()).await;
    let client = reqwest::Client::new();
    let denied = client
        .post(format!("{url}/v1/responses"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let mut h = headers();
    h.insert("authorization", "Bearer fake-upstream-key".parse().unwrap());
    let mut response = client
        .post(format!("{url}/v1/responses"))
        .headers(h)
        .body(original.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    assert_eq!(response.headers()["x-proxy-injected"], "0");
    let (seen, body) = rx.recv().await.unwrap();
    assert!(!seen.contains_key("x-proxy-token"));
    assert_eq!(seen["authorization"], "Bearer fake-upstream-key");
    assert_eq!(body, original);
    let first = tokio::time::timeout(Duration::from_secs(2), response.chunk())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(&first[..], b"data: first\n\n");
    gate.notify_one();
    assert_eq!(response.bytes().await.unwrap(), "data: last\n\n");
    task.abort();
    upstream_task.abort();
}

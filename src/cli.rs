use crate::store::{self, MAX_OUTPUT_BYTES, READ_LINES, Store};
use serde_json::{Value, json};
use std::{
    env,
    io::{self, BufRead, Read, Write},
    os::{
        fd::OwnedFd,
        unix::{net::UnixStream, process::ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

type CliResult<T> = Result<T, Box<dyn std::error::Error>>;
#[derive(Default, serde::Deserialize)]
pub struct ClientSettings {
    pub output_dir: Option<PathBuf>,
    pub pre_hook_argv: Vec<String>,
}
pub fn client_settings() -> CliResult<ClientSettings> {
    let path = env::current_exe()?.with_file_name("client.json");
    match std::fs::File::open(path) {
        Ok(file) => Ok(serde_json::from_reader(file)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ClientSettings::default()),
        Err(error) => Err(error.into()),
    }
}

/// Run the existing transformer first; one hook owns the final updatedInput.
pub fn chained_hook(mut event: Value, binary: &Path, argv: &[String]) -> CliResult<Value> {
    let reading = is_read_command(
        event
            .pointer("/tool_input/command")
            .and_then(Value::as_str)
            .unwrap_or(""),
        binary,
    );
    if argv.is_empty()
        || event["hook_event_name"] != "PreToolUse"
        || event["tool_name"] != "Bash"
        || event
            .pointer("/tool_input/command")
            .and_then(Value::as_str)
            .is_some_and(|s| s.starts_with("# codex-output-capture-v2\n"))
    {
        return hook(event, binary);
    }
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    serde_json::to_writer(child.stdin.take().ok_or("hook stdin missing")?, &event)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err("preceding hook failed".into());
    }
    let mut preceding: Value = serde_json::from_slice(&output.stdout)?;
    if preceding["continue"] == false
        || preceding
            .pointer("/hookSpecificOutput/permissionDecision")
            .and_then(Value::as_str)
            .is_some_and(|s| matches!(s, "deny" | "ask"))
    {
        return Ok(preceding);
    }
    if let Some(input) = preceding.pointer("/hookSpecificOutput/updatedInput") {
        event["tool_input"] = input.clone();
    }
    let wrapped = hook_with_mode(event, binary, reading)?;
    if let Some(fields) = preceding
        .get_mut("hookSpecificOutput")
        .and_then(Value::as_object_mut)
    {
        fields.extend(
            wrapped["hookSpecificOutput"]
                .as_object()
                .ok_or("invalid wrapped hook")?
                .clone(),
        );
        Ok(preceding)
    } else {
        Ok(wrapped)
    }
}
pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub fn hook(event: Value, binary: &Path) -> CliResult<Value> {
    let reading = is_read_command(
        event
            .pointer("/tool_input/command")
            .and_then(Value::as_str)
            .unwrap_or(""),
        binary,
    );
    hook_with_mode(event, binary, reading)
}

/// Exempt only a standalone read invocation, never a compound shell command.
pub fn is_read_command(command: &str, binary: &Path) -> bool {
    let Some(words) = shlex::split(command) else {
        return false;
    };
    let words = if words.starts_with(&["rtk".into(), "proxy".into()]) {
        &words[2..]
    } else {
        &words[..]
    };
    matches!(words.len(), 3..=5)
        && words[0] == binary.to_string_lossy()
        && matches!(words[1].as_str(), "read" | "expand")
        && store::valid_id(&words[2])
        && words[3..]
            .iter()
            .all(|word| word.parse::<usize>().is_ok_and(|n| n > 0))
        && !command.contains(['$', '`', '\n', '#'])
}

fn hook_with_mode(event: Value, binary: &Path, reading: bool) -> CliResult<Value> {
    if event["hook_event_name"] != "PreToolUse" || event["tool_name"] != "Bash" {
        return Ok(json!({}));
    }
    let Some(command) = event.pointer("/tool_input/command").and_then(Value::as_str) else {
        return Ok(json!({}));
    };
    if command.starts_with("# codex-output-capture-v2\n") {
        return Ok(json!({}));
    }
    let session = event["session_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("hook session_id is required")?;
    let tool_id = event["tool_use_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("hook tool_use_id is required")?;
    let binary = binary.to_str().ok_or("binary path must be UTF-8")?;
    let mut input = event["tool_input"].clone();
    // Preserve tool permissions, cwd, timeout, and every other canonical Bash field.
    input["command"] = json!(format!(
        "# codex-output-capture-v2\n{} {} {} {} {}",
        quote(binary),
        if reading { "capture-read" } else { "capture" },
        quote(session),
        quote(tool_id),
        quote(command)
    ));
    // Codex 0.154 requires this wire value to accept updatedInput. Its registry still sends
    // the rewritten invocation through the normal tool handler and sandbox checks.
    Ok(
        json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":input}}),
    )
}

pub fn output_store() -> CliResult<Store> {
    let root = env::var_os("OUTPUT_STORE_DIR")
        .map(PathBuf::from)
        .or(client_settings()?.output_dir)
        .unwrap_or_else(store::default_root);
    let number = |key: &str, default: u64| -> CliResult<u64> {
        Ok(env::var(key).map_or_else(|_| Ok(default), |value| value.parse())?)
    };
    Ok(Store::new(
        root,
        Duration::from_secs(number("STORE_IDLE_SECONDS", 900)?),
        number("MAX_STORE_BYTES", 256 * 1024 * 1024)?,
        usize::try_from(number("MAX_STORE_ENTRIES", 8192)?)?,
    )?)
}

/// Merge stdout/stderr through one OS socket; keep at most the item limit before passing through.
/// This wrapper changes buffering, but never reruns a failed command or replaces its exit status.
pub fn capture(
    session: &str,
    tool_use_id: &str,
    command: &str,
    threshold: usize,
) -> CliResult<i32> {
    let (mut reader, writer) = UnixStream::pair()?;
    let mut child = Command::new("/bin/bash")
        .args(["-c", command])
        .stdin(Stdio::inherit())
        .stdout(Stdio::from(OwnedFd::from(writer.try_clone()?)))
        .stderr(Stdio::from(OwnedFd::from(writer)))
        .spawn()?;
    let mut data = Vec::new();
    Read::by_ref(&mut reader)
        .take((MAX_OUTPUT_BYTES + 1) as u64)
        .read_to_end(&mut data)?;
    let overflow = data.len() > MAX_OUTPUT_BYTES;
    if overflow {
        let mut out = io::stdout().lock();
        out.write_all(&data)?;
        io::copy(&mut reader, &mut out)?;
    }
    let status = child.wait()?;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
    if overflow {
        return Ok(code);
    }
    if data.len() < threshold
        || String::from_utf8_lossy(&data)
            .chars()
            .take(store::PREVIEW_CHARS + 1)
            .count()
            <= store::PREVIEW_CHARS
    {
        io::stdout().lock().write_all(&data)?;
        return Ok(code);
    }
    let saved = output_store().and_then(|store| {
        Ok(store.persist(session, tool_use_id, code, &data, &env::current_exe()?)?)
    });
    match saved {
        Ok(card) if card.len() < data.len() => io::stdout().lock().write_all(card.as_bytes())?,
        Ok(_) => io::stdout().lock().write_all(&data)?,
        Err(error) => {
            eprintln!("codex-turn-proxy: persistence failed; returning original output ({error})");
            io::stdout().lock().write_all(&data)?;
        }
    }
    Ok(code)
}

// Optional compatibility adapter. Reads return text directly; no request injection occurs.
pub fn mcp_response(request: &Value) -> Option<Value> {
    let id = request.get("id")?.clone();
    let result = match request["method"].as_str().unwrap_or("") {
        "initialize" => json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},
            "serverInfo":{"name":"codex-turn-proxy","version":env!("CARGO_PKG_VERSION")}}),
        "ping" => json!({}),
        "tools/list" => json!({"tools":[{"name":"expand",
            "description":"Read saved tool output when its preview lacks needed evidence. Pass its ID. Returns up to 2000 lines directly.",
            "inputSchema":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}}]}),
        "tools/call"
            if request.pointer("/params/name").and_then(Value::as_str) == Some("expand") =>
        {
            match request
                .pointer("/params/arguments/id")
                .and_then(Value::as_str)
            {
                Some(id) => match output_store().and_then(|store| {
                    let mut output = Vec::new();
                    store.read(id, 1, READ_LINES, &mut output)?;
                    Ok(String::from_utf8_lossy(&output).into_owned())
                }) {
                    Ok(result) => {
                        json!({"content":[{"type":"text","text":result}],"isError":false})
                    }
                    Err(error) => {
                        json!({"content":[{"type":"text","text":error.to_string()}],"isError":true})
                    }
                },
                None => {
                    return Some(
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"id is required"}}),
                    );
                }
            }
        }
        _ => {
            return Some(
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method or tool not found"}}),
            );
        }
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}
pub fn mcp() -> CliResult<()> {
    let mut input = io::stdin().lock();
    let mut out = io::stdout().lock();
    loop {
        let mut line = String::new();
        let read = input.by_ref().take(65537).read_line(&mut line)?;
        if read == 0 {
            break;
        }
        if read > 65536 {
            return Err("MCP message exceeds limit".into());
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(request) => mcp_response(&request),
            Err(_) => Some(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"invalid JSON"}}),
            ),
        };
        if let Some(reply) = reply {
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
    }
    Ok(())
}

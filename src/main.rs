use codex_turn_proxy::cli;
use serde_json::{Value, json};
use std::env;

const USAGE: &str = "usage: codex-turn-proxy [hook | hook-config | mcp | read ID [OFFSET LIMIT] | expand ID | cleanup | capture SESSION TOOL_USE_ID COMMAND]";

fn number(name: &str, default: usize) -> Result<usize, Box<dyn std::error::Error>> {
    Ok(env::var(name).map_or_else(|_| Ok(default), |s| s.parse())?)
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("capture" | "capture-read") if args.len() == 4 => {
            std::process::exit(cli::capture(
                &args[1],
                &args[2],
                &args[3],
                if args[0] == "capture-read" {
                    usize::MAX
                } else {
                    number("MIN_OUTPUT_BYTES", 4096)?
                },
            )?);
        }
        Some("read" | "expand") if (2..=4).contains(&args.len()) => {
            let offset = args.get(2).map_or(Ok(1), |value| value.parse::<usize>())?;
            let limit = args
                .get(3)
                .map_or(Ok(codex_turn_proxy::store::READ_LINES), |value| {
                    value.parse::<usize>()
                })?;
            cli::output_store()?.read(&args[1], offset, limit, &mut std::io::stdout().lock())?;
        }
        Some("cleanup") => {
            println!(
                "{}",
                cli::output_store()?.purge_expired(std::time::SystemTime::now())?
            );
        }
        Some("hook") => {
            let event: Value = serde_json::from_reader(std::io::stdin().lock())?;
            serde_json::to_writer(
                std::io::stdout().lock(),
                &cli::chained_hook(
                    event,
                    &env::current_exe()?,
                    &cli::client_settings()?.pre_hook_argv,
                )?,
            )?;
        }
        Some("hook-config") => {
            let binary = env::current_exe()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"hooks":{"PreToolUse":[{
                    "matcher":"^Bash$", "hooks":[{"type":"command",
                        "command":format!("{} hook",cli::quote(&binary.to_string_lossy())),"timeout":10}]
                }]}}))?
            );
        }
        Some("mcp") => cli::mcp()?,
        Some("serve" | "healthcheck") => {
            return Err("HTTP proxy support was removed. Use the local hook and keep Codex connected directly to its provider.".into());
        }
        Some("--help" | "-h") | None => println!("{USAGE}"),
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

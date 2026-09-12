use codex_turn_proxy::{Config, Proxy, cli};
use serde_json::{Value, json};
use std::env;

fn number(name: &str, default: usize) -> Result<usize, Box<dyn std::error::Error>> {
    Ok(env::var(name).map_or_else(|_| Ok(default), |s| s.parse())?)
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("capture" | "capture-read") if args.len() == 4 => {
            std::process::exit(cli::capture(&args[1], &args[2], &args[3], if args[0] == "capture-read" { usize::MAX } else { number("MIN_OUTPUT_BYTES", 4096)? })?);
        }
        Some("read" | "expand") if (2..=4).contains(&args.len()) => {
            let offset = args.get(2).map_or(Ok(1), |value| value.parse::<usize>())?;
            let limit = args.get(3).map_or(Ok(codex_turn_proxy::store::READ_LINES), |value| value.parse::<usize>())?;
            cli::output_store()?.read(&args[1], offset, limit, &mut std::io::stdout().lock())?;
        }
        Some("cleanup") => {
            println!("{}", cli::output_store()?.purge_expired(std::time::SystemTime::now())?);
        }
        Some("hook") => {
            let event: Value = serde_json::from_reader(std::io::stdin().lock())?;
            serde_json::to_writer(std::io::stdout().lock(), &cli::chained_hook(event, &env::current_exe()?, &cli::client_settings()?.pre_hook_argv)?)?;
        }
        Some("hook-config") => {
            let binary = env::current_exe()?;
            println!("{}", serde_json::to_string_pretty(&json!({"hooks":{"PreToolUse":[{
                "matcher":"^Bash$", "hooks":[{"type":"command",
                    "command":format!("{} hook",cli::quote(&binary.to_string_lossy())),"timeout":10}]
            }]}}))?);
        }
        Some("mcp") => cli::mcp()?,
        Some("healthcheck") => {
            reqwest::blocking::Client::new().get("http://127.0.0.1:8787/healthz")
                .timeout(std::time::Duration::from_secs(3)).send()?.error_for_status()?;
        }
        Some("serve") | None => {
            let config = Config::new(env::var("UPSTREAM_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into()),
                env::var("PROXY_TOKEN").map_err(|_| "PROXY_TOKEN is required")?);
            let address = env::var("LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
            tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
                let proxy = Proxy::new(config).map_err(|e| e.1)?;
                let cleanup = cli::output_store()?;
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                    loop {
                        interval.tick().await;
                        let store = cleanup.clone();
                        match tokio::task::spawn_blocking(move || store.purge_expired(std::time::SystemTime::now())).await {
                            Ok(Ok(_)) => (),
                            result => eprintln!("output cleanup failed: {result:?}"),
                        }
                    }
                });
                let listener = tokio::net::TcpListener::bind(&address).await?;
                eprintln!("codex-turn-proxy listening on {address}");
                axum::serve(listener, proxy.router()).with_graceful_shutdown(async {
                    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
                    tokio::select! { _ = tokio::signal::ctrl_c() => (), _ = terminate.recv() => () }
                }).await?;
                Ok::<(), Box<dyn std::error::Error>>(())
            })?;
        }
        _ => return Err("usage: codex-turn-proxy [serve | hook | hook-config | mcp | read ID [OFFSET LIMIT] | expand ID | cleanup | capture SESSION TOOL_USE_ID COMMAND]".into()),
    }
    Ok(())
}

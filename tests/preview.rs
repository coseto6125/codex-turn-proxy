use codex_turn_proxy::{cli, store::Store};
use std::{fs, process::Command};
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    time::{Duration, SystemTime},
};

fn store(root: &Path, bytes: u64, entries: usize) -> Store {
    Store::new(root.join("store"), Duration::from_secs(900), bytes, entries).unwrap()
}
fn id(card: &str) -> &str {
    card.lines()
        .find_map(|line| line.strip_prefix("ID: "))
        .unwrap()
}

#[test]
fn test_store_immutable_ids_session_isolation_and_restart_reads() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path(), 1024, 10);
    let binary = Path::new("/bin/helper");
    let card = store
        .persist("s", "a", 7, b"one\r\ntwo\nlast", binary)
        .unwrap();
    assert_eq!(
        store
            .persist("s", "a", 7, b"one\r\ntwo\nlast", binary)
            .unwrap(),
        card
    );
    assert!(store.persist("s", "a", 7, b"different", binary).is_err());
    let other = store
        .persist("other-session", "a", 0, b"other", binary)
        .unwrap();
    assert_ne!(id(&card), id(&other));
    let reopened = Store::new(
        root.path().join("store"),
        Duration::from_secs(900),
        1024,
        10,
    )
    .unwrap();
    let mut text = Vec::new();
    reopened.read(id(&card), 2, 2000, &mut text).unwrap();
    assert_eq!(text, b"two\nlast");
    assert_eq!(
        fs::metadata(store.path(id(&card)).unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(store.read("../../secret", 1, 1, &mut Vec::new()).is_err());
    assert!(store.read(id(&card), 0, 1, &mut Vec::new()).is_err());
    assert!(store.read(id(&card), 1, 2001, &mut Vec::new()).is_err());
}

#[test]
fn test_store_expiration_and_capacity_delete_files_without_changing_cards() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path(), 6, 2);
    let binary = Path::new("/bin/helper");
    let a = store.persist("s", "a", 0, b"aaa", binary).unwrap();
    let b = store.persist("s", "b", 0, b"bbb", binary).unwrap();
    fs::File::open(store.path(id(&a)).unwrap())
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(10))
        .unwrap();
    store.persist("s", "c", 0, b"ccc", binary).unwrap();
    assert!(!store.path(id(&a)).unwrap().exists());
    assert!(store.path(id(&b)).unwrap().exists());
    assert_eq!(
        store
            .purge_expired(SystemTime::now() + Duration::from_secs(901))
            .unwrap(),
        2
    );
    assert!(store.read(id(&b), 1, 1, &mut Vec::new()).is_err());
    fs::write(
        root.path().join("store/.pending-interrupted"),
        "partial original",
    )
    .unwrap();
    fs::write(root.path().join("store/unrelated"), "keep").unwrap();
    assert_eq!(store.purge_expired(SystemTime::now()).unwrap(), 1);
    assert!(root.path().join("store/unrelated").exists());
}

#[test]
fn test_store_concurrent_duplicate_writers_keep_one_complete_original() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path(), 10000, 10);
    let cards: Vec<_> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    store
                        .persist(
                            "s",
                            "same",
                            0,
                            b"complete original",
                            Path::new("/bin/helper"),
                        )
                        .unwrap()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect()
    });
    assert!(cards.iter().all(|card| card == &cards[0]));
    let mut text = Vec::new();
    store.read(id(&cards[0]), 1, 2000, &mut text).unwrap();
    assert_eq!(text, b"complete original");
}

#[test]
fn test_cli_small_output_remains_inline_with_exit_status() {
    let result = Command::new(env!("CARGO_BIN_EXE_codex-turn-proxy"))
        .args(["capture", "s", "short", "printf small; exit 7"])
        .output()
        .unwrap();
    assert_eq!(result.stdout, b"small");
    assert_eq!(result.status.code(), Some(7));
}

#[test]
fn test_cli_multibyte_output_covered_by_preview_stays_inline() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let raw = "中".repeat(2000);
    fs::write(&source, &raw).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_codex-turn-proxy"))
        .args([
            "capture",
            "s",
            "unicode",
            &format!("cat '{}'", source.display()),
        ])
        .env("OUTPUT_STORE_DIR", root.path().join("store"))
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(result.stdout, raw.as_bytes());
    assert!(!root.path().join("store").exists());
}

#[test]
fn test_store_rejects_symlinks_and_public_directory() {
    let root = tempfile::tempdir().unwrap();
    let private = root.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Store::new(private.clone(), Duration::from_secs(1), 100, 1).is_err());
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let link = root.path().join("link");
    symlink(&private, &link).unwrap();
    assert!(Store::new(link, Duration::from_secs(1), 100, 1).is_err());
    let store = store(root.path(), 100, 1);
    symlink(
        root.path().join("secret"),
        store.path(&"a".repeat(64)).unwrap(),
    )
    .unwrap();
    assert!(store.read(&"a".repeat(64), 1, 1, &mut Vec::new()).is_err());
}

#[test]
fn test_store_preview_handles_unicode_and_binary_bytes() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path(), 10000, 10);
    let data = "中".repeat(2600);
    let card = store
        .persist("s", "unicode", 0, data.as_bytes(), Path::new("/bin/helper"))
        .unwrap();
    assert!(card.contains(&"中".repeat(2000)));
    assert!(!card.contains(&"中".repeat(2001)));
    let card = store
        .persist("s", "binary", 0, b"\xff\0\n", Path::new("/bin/helper"))
        .unwrap();
    let mut read = Vec::new();
    store.read(id(&card), 1, 2000, &mut read).unwrap();
    assert_eq!(read, b"\xff\0\n");
}

#[test]
fn test_hook_read_exemption_rejects_compound_commands() {
    let binary = Path::new("/bin/helper");
    let command = format!("'/bin/helper' read {} 1 2000", "a".repeat(64));
    assert!(cli::is_read_command(&command, binary));
    assert!(cli::is_read_command(
        &format!("rtk proxy {command}"),
        binary
    ));
    for suffix in [
        "; seq 10000",
        " && seq 10000",
        " | cat",
        " > /tmp/file",
        " # comment",
    ] {
        assert!(
            !cli::is_read_command(&format!("{command}{suffix}"), binary),
            "{suffix}"
        );
    }
    assert!(!cli::is_read_command(
        &command.replace("read", "capture"),
        binary
    ));
}

#[test]
fn test_cli_read_through_hook_returns_full_text_without_recapture() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path(), 100000, 10);
    let binary = Path::new(env!("CARGO_BIN_EXE_codex-turn-proxy"));
    let raw = "evidence line\n".repeat(600);
    let card = store.persist("s", "a", 0, raw.as_bytes(), binary).unwrap();
    let command = format!(
        "{} read {} 1 2000",
        cli::quote(&binary.to_string_lossy()),
        id(&card)
    );
    let event = serde_json::json!({"hook_event_name":"PreToolUse","tool_name":"Bash","session_id":"s","tool_use_id":"read", "tool_input":{"command":command}});
    let wrapped = cli::hook(event, binary).unwrap();
    let result = Command::new("/bin/bash")
        .args([
            "-c",
            wrapped["hookSpecificOutput"]["updatedInput"]["command"]
                .as_str()
                .unwrap(),
        ])
        .env("OUTPUT_STORE_DIR", root.path().join("store"))
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(result.stdout, raw.as_bytes());
}

#[test]
fn test_cli_persistence_failure_preserves_output_and_exit_status() {
    let root = tempfile::tempdir().unwrap();
    let invalid = root.path().join("not-a-directory");
    fs::write(&invalid, "occupied").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_codex-turn-proxy"))
        .args([
            "capture",
            "s",
            "a",
            "printf 'unchanged%.0s' {1..1000}; exit 9",
        ])
        .env("OUTPUT_STORE_DIR", invalid)
        .env("MIN_OUTPUT_BYTES", "1")
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(9));
    assert_eq!(result.stdout, "unchanged".repeat(1000).as_bytes());
    assert!(String::from_utf8_lossy(&result.stderr).contains("persistence failed"));
}

#[test]
fn test_capture_offline_returns_preview_and_persists_original() {
    let root = tempfile::tempdir().unwrap();
    let raw = "line of evidence\n".repeat(600);
    let input = root.path().join("input");
    fs::write(&input, &raw).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_codex-turn-proxy"))
        .args([
            "capture",
            "session",
            "tool",
            &format!("cat '{}'", input.display()),
        ])
        .env("OUTPUT_STORE_DIR", root.path().join("store"))
        .env("PROXY_URL", "http://127.0.0.1:1")
        .env("PROXY_TOKEN", "offline-unused-token-at-least-32-chars")
        .output()
        .unwrap();
    assert!(result.status.success());
    let card = String::from_utf8(result.stdout).unwrap();
    assert!(card.starts_with("<persisted-output>"), "{card}");
    assert!(card.contains("line of evidence"));
    assert!(!card.contains(&raw));
    let files: Vec<_> = fs::read_dir(root.path().join("store"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .collect();
    assert_eq!(files.len(), 1);
    assert_eq!(fs::read_to_string(&files[0]).unwrap(), raw);
}

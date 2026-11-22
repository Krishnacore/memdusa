//! End-to-end `memdusa sync` against a real `memdusa serve` subprocess:
//! opt-in, one-shot push/pull, listing/status, and API-key login.

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

use memdusa_core::model::{MemoryQuery, NewMemory};
use memdusa_core::store::Store;

fn cli(dir: &Path, db: &str, server: Option<(&str, &str)>, args: &[&str]) -> String {
    let mut c = Command::new(env!("CARGO_BIN_EXE_memdusa"));
    c.args(["--database-url", db]).args(args).env("MEMDUSA_HOME", dir).env("HOME", dir);
    c.env_remove("MEMDUSA_URL").env_remove("MEMDUSA_TOKEN");
    if let Some((url, token)) = server {
        c.env("MEMDUSA_URL", url).env("MEMDUSA_TOKEN", token);
    }
    let out = c.output().unwrap();
    assert!(out.status.success(), "cli {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn spawn_server(dir: &Path, db: &str, port: u16) -> Child {
    let mut c = Command::new(env!("CARGO_BIN_EXE_memdusa"));
    c.args([
        "--database-url",
        db,
        "serve",
        "--addr",
        &format!("127.0.0.1:{port}"),
        "--token",
        "root-token",
        "--no-auto-prune",
        "--no-metrics",
    ])
    .env("MEMDUSA_HOME", dir)
    .env("HOME", dir)
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    c.spawn().unwrap()
}

fn wait_ready(port: u16) {
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        sleep(Duration::from_millis(25));
    }
    panic!("server on {port} never came up");
}

fn scratch(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf, String, String) {
    let root = std::env::temp_dir().join(format!("memdusa-sync-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let server_dir = root.join("server");
    let client_dir = root.join("client");
    std::fs::create_dir_all(&server_dir).unwrap();
    std::fs::create_dir_all(&client_dir).unwrap();
    let server_db = memdusa_core::store::sqlite_url(&server_dir.join("server.db"));
    let local_db = memdusa_core::store::sqlite_url(&client_dir.join("local.db"));
    (root, server_dir, client_dir, server_db, local_db)
}

fn login_key(dir: &Path, server_db: &str) -> String {
    let created = cli(dir, server_db, None, &["user", "add", "alice", "--key-name", "laptop"]);
    created.lines().map(str::trim).find(|l| l.starts_with("mdk_")).expect("the new key is printed once").to_owned()
}

#[tokio::test]
async fn one_shot_sync_respects_opt_in_and_exposes_status() {
    let (root, server_dir, client_dir, server_db, local_db) = scratch("cli");
    let port = free_port();
    let mut server = spawn_server(&server_dir, &server_db, port);
    wait_ready(port);
    let url = format!("http://127.0.0.1:{port}");
    let key = login_key(&server_dir, &server_db);
    let login = Some((url.as_str(), key.as_str()));
    let opted = format!("sync-cli/{}", memdusa_core::model::new_id());
    let unopted = format!("sync-cli/{}", memdusa_core::model::new_id());

    // Two local projects with memories; only one is opted in.
    cli(&client_dir, &local_db, None, &["--project", &opted, "remember", "opted project fact"]);
    cli(&client_dir, &local_db, None, &["--project", &unopted, "remember", "secret local fact"]);
    let added = cli(&client_dir, &local_db, login, &["sync", "add", &opted]);
    assert!(added.contains("initial sync"), "{added}");
    assert!(added.contains("pushed 1"), "{added}");

    let server_store = Store::connect(&server_db).await.unwrap();
    let in_server =
        server_store.memory_search(MemoryQuery { project: Some(opted.clone()), ..Default::default() }).await.unwrap();
    assert_eq!(in_server.len(), 1, "the opted-in project reached the server");
    let leaked =
        server_store.memory_search(MemoryQuery { project: Some(unopted.clone()), ..Default::default() }).await.unwrap();
    assert!(leaked.is_empty(), "an unsynced project never leaves the machine");

    // A server-side memory flows back on a one-shot run.
    server_store
        .memory_save(NewMemory {
            project: Some(opted.clone()),
            content: "fact from the team server".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let out = cli(&client_dir, &local_db, login, &["sync"]);
    assert!(out.contains(&format!("{opted}: pushed 0, pulled 1")), "{out}");

    let local_store = Store::connect(&local_db).await.unwrap();
    let pulled = local_store
        .memory_search(MemoryQuery {
            project: Some(opted.clone()),
            query: Some("team server".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(pulled.len(), 1, "the server's memory arrived locally: {pulled:?}");

    let listing = cli(&client_dir, &local_db, login, &["sync", "list"]);
    assert!(listing.contains(&opted) && !listing.contains(&unopted), "{listing}");
    let status = cli(&client_dir, &local_db, login, &["sync", "status"]);
    assert!(status.contains("pending local changes: 0"), "{status}");
    assert!(status.contains("last error: none"), "{status}");
    assert!(!status.contains("last ok: never"), "{status}");

    cli(&client_dir, &local_db, login, &["sync", "remove", &opted]);
    let listing = cli(&client_dir, &local_db, login, &["sync", "list"]);
    assert!(!listing.contains(&opted), "{listing}");

    server.kill().unwrap();
    let _ = server.wait();
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn globals_are_opted_in_separately() {
    let (root, server_dir, client_dir, server_db, local_db) = scratch("globals");
    let port = free_port();
    let mut server = spawn_server(&server_dir, &server_db, port);
    wait_ready(port);
    let url = format!("http://127.0.0.1:{port}");
    let key = login_key(&server_dir, &server_db);
    let login = Some((url.as_str(), key.as_str()));

    cli(&client_dir, &local_db, None, &["remember", "private global fact", "--global"]);
    let out = cli(&client_dir, &local_db, login, &["sync", "add", "--globals"]);
    assert!(out.contains("initial sync"), "{out}");
    let listing = cli(&client_dir, &local_db, login, &["sync", "list"]);
    assert!(listing.contains("globals"), "{listing}");

    let server_store = Store::connect(&server_db).await.unwrap();
    let found = server_store
        .memory_search(MemoryQuery {
            user: Some("alice".into()),
            query: Some("private global fact".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].user.as_deref(), Some("alice"), "globals stay private to their owner");

    server.kill().unwrap();
    let _ = server.wait();
    let _ = std::fs::remove_dir_all(&root);
}

//! `memdusa remember` says whether it saved, merged into a near-duplicate, or refreshed an identical memory.

use std::process::Command;

fn remember(dir: &std::path::Path, text: &str) -> String {
    // sqlite_url, not "sqlite://{path}": a Windows path's "C:" parses as host:port
    let db = memdusa_core::store::sqlite_url(&dir.join("t.db"));
    let out = Command::new(env!("CARGO_BIN_EXE_memdusa"))
        .args(["--database-url", &db, "--project", "demo", "remember", text])
        .env("MEMDUSA_HOME", dir)
        .env("HOME", dir)
        .env_remove("MEMDUSA_URL")
        .env_remove("MEMDUSA_TOKEN")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn remember_reports_new_merged_and_refreshed() {
    let dir = std::env::temp_dir().join(format!("memdusa-remember-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let first = remember(&dir, "the billing api stores money as integer cents everywhere");
    assert!(first.starts_with("saved "), "{first}");
    let id = first.trim_start_matches("saved ");

    std::thread::sleep(std::time::Duration::from_millis(5));
    let same = remember(&dir, "the billing api stores money as integer cents everywhere");
    assert_eq!(same, format!("already saved {id} (refreshed)"));

    let near = remember(&dir, "the billing api stores money as integer cents everywhere now");
    assert!(near.starts_with(&format!("updated existing {id}")), "{near}");

    let other = remember(&dir, "deploys happen on fridays");
    assert!(other.starts_with("saved ") && !other.contains(id), "{other}");

    let _ = std::fs::remove_dir_all(&dir);
}

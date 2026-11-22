//! Hermes runs hook subprocesses from its own install dir, so the payload's
//! `cwd` (and `TERMINAL_CWD`) say nothing about the project the agent works
//! in. Hermes records the real workspace per session in
//! `$HERMES_HOME/state.db` (`sessions.id`, `sessions.cwd`); read it from there.
//! Every failure (no db, locked, different schema) just means "no answer".

use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::ConnectOptions;
use sqlx::sqlite::SqliteConnectOptions;

/// `$HERMES_HOME`, default `~/.hermes`.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HERMES_HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::config::home_dir().ok().map(|h| h.join(".hermes")))
}

/// `sessions.cwd` for `session_id`, read-only (Hermes writes the db concurrently in WAL mode).
async fn session_cwd(db: &Path, session_id: &str) -> Option<String> {
    if !db.is_file() {
        return None;
    }
    let lookup = async {
        let mut conn = SqliteConnectOptions::new()
            .filename(db)
            .read_only(true)
            .create_if_missing(false)
            .busy_timeout(Duration::from_millis(300))
            .connect()
            .await
            .ok()?;
        sqlx::query_scalar::<_, Option<String>>("SELECT cwd FROM sessions WHERE id = ?")
            .bind(session_id)
            .fetch_optional(&mut conn)
            .await
            .ok()
            .flatten()
            .flatten()
    };
    tokio::time::timeout(Duration::from_millis(1500), lookup).await.ok().flatten().filter(|c| !c.trim().is_empty())
}

fn inside(dir: &Path, root: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(dir).starts_with(canon(root))
}

/// The directory the Hermes session really works in:
/// 1. `sessions.cwd` from Hermes' state.db,
/// 2. else the payload cwd, unless it is inside Hermes' install dir,
/// 3. else `None` (the caller falls back to `MEMDUSA_PROJECT` / its usual detection).
pub async fn resolve_cwd(home: &Path, session_id: Option<&str>, payload_cwd: Option<&str>) -> Option<String> {
    if let Some(id) = session_id.filter(|s| !s.is_empty())
        && let Some(cwd) = session_cwd(&home.join("state.db"), id).await
    {
        return Some(cwd);
    }
    payload_cwd.filter(|c| !c.is_empty() && !inside(Path::new(c), &home.join("hermes-agent"))).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Connection;

    fn temp_home() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("memdusa-hermes-{}", memdusa_core::model::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn make_db(home: &Path, schema: &'static str, rows: &[(&str, &str)]) {
        let mut conn = SqliteConnectOptions::new()
            .filename(home.join("state.db"))
            .create_if_missing(true)
            .connect()
            .await
            .unwrap();
        sqlx::query(schema).execute(&mut conn).await.unwrap();
        for (id, cwd) in rows {
            sqlx::query("INSERT INTO sessions (id, cwd) VALUES (?, ?)")
                .bind(*id)
                .bind(*cwd)
                .execute(&mut conn)
                .await
                .unwrap();
        }
        conn.close().await.unwrap();
    }

    #[tokio::test]
    async fn session_row_wins_over_the_payload_cwd() {
        let home = temp_home();
        make_db(&home, "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, extra TEXT)", &[("s1", "/work/app")])
            .await;
        let install = home.join("hermes-agent");
        let got = resolve_cwd(&home, Some("s1"), Some(&install.display().to_string())).await;
        assert_eq!(got.as_deref(), Some("/work/app"));
    }

    #[tokio::test]
    async fn missing_row_falls_back_to_a_payload_cwd_outside_the_install_dir() {
        let home = temp_home();
        make_db(&home, "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT)", &[("other", "/work/other")]).await;
        assert_eq!(resolve_cwd(&home, Some("nope"), Some("/work/real")).await.as_deref(), Some("/work/real"));
        // the payload cwd is Hermes' own dir (or below): no answer, caller uses its fallback
        let install = home.join("hermes-agent");
        std::fs::create_dir_all(install.join("sub")).unwrap();
        assert_eq!(resolve_cwd(&home, Some("nope"), Some(&install.display().to_string())).await, None);
        assert_eq!(resolve_cwd(&home, Some("nope"), Some(&install.join("sub").display().to_string())).await, None);
        assert_eq!(resolve_cwd(&home, None, None).await, None);
    }

    #[tokio::test]
    async fn missing_db_or_wrong_schema_never_fails() {
        let home = temp_home();
        assert_eq!(resolve_cwd(&home, Some("s1"), Some("/work/x")).await.as_deref(), Some("/work/x"));
        assert!(!home.join("state.db").exists(), "must not create the database");

        let other = temp_home();
        let mut conn = SqliteConnectOptions::new()
            .filename(other.join("state.db"))
            .create_if_missing(true)
            .connect()
            .await
            .unwrap();
        sqlx::query("CREATE TABLE sessions (id TEXT)").execute(&mut conn).await.unwrap();
        conn.close().await.unwrap();
        assert_eq!(resolve_cwd(&other, Some("s1"), Some("/work/y")).await.as_deref(), Some("/work/y"));
        // a blank cwd column is no answer either
        let blank = temp_home();
        make_db(&blank, "CREATE TABLE sessions (id TEXT, cwd TEXT)", &[("s1", " ")]).await;
        assert_eq!(resolve_cwd(&blank, Some("s1"), Some("/work/z")).await.as_deref(), Some("/work/z"));
    }

    #[tokio::test]
    async fn reads_while_hermes_holds_the_db_open_in_wal_mode() {
        let home = temp_home();
        let mut writer = SqliteConnectOptions::new()
            .filename(home.join("state.db"))
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .connect()
            .await
            .unwrap();
        sqlx::query("CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT)").execute(&mut writer).await.unwrap();
        sqlx::query("INSERT INTO sessions VALUES ('w1', '/work/wal')").execute(&mut writer).await.unwrap();
        assert_eq!(resolve_cwd(&home, Some("w1"), None).await.as_deref(), Some("/work/wal"));
        writer.close().await.unwrap();
    }
}

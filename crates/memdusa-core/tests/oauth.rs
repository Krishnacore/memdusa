//! OAuth storage (H2a). Runs against SQLite always and Postgres when
//! `MEMDUSA_TEST_POSTGRES_URL` is set. Names are unique per test because
//! Postgres tests share one database.

use memdusa_core::Store;
use memdusa_core::auth::generate_secret;
use memdusa_core::model::*;
use memdusa_core::oauth::*;
use memdusa_core::store::sqlite_url;

async fn stores() -> Vec<(String, Store)> {
    let dir = std::env::temp_dir().join(format!("memdusa-oauth-store-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut out = vec![("sqlite".to_string(), Store::connect(&sqlite_url(&dir.join("t.db"))).await.unwrap())];
    if let Ok(url) = std::env::var("MEMDUSA_TEST_POSTGRES_URL") {
        out.push(("postgres".into(), Store::connect(&url).await.unwrap()));
    }
    out
}

#[tokio::test]
async fn clients_transactions_codes_and_grants() {
    for (name, s) in stores().await {
        let user = format!("u{}", new_id());
        s.user_create(&user, false).await.unwrap();
        let client = OAuthClient {
            id: new_id(),
            client_id: format!("client-{}", new_id()),
            name: "Test Client".into(),
            redirect_uris: vec!["http://127.0.0.1:4242/cb".into()],
            grant_types: vec!["authorization_code".into(), "refresh_token".into()],
            created_at: now_ms(),
        };
        s.oauth_client_create(&client).await.unwrap();
        assert_eq!(s.oauth_client_get(&client.client_id).await.unwrap().unwrap().name, "Test Client");

        let txn = OAuthTransaction {
            csrf: new_id(),
            client_id: client.client_id.clone(),
            redirect_uri: client.redirect_uris[0].clone(),
            code_challenge: "c".repeat(43),
            scope: "memdusa".into(),
            resource: None,
            state: "s".into(),
            user_name: None,
            created_at: now_ms(),
            upstream: None,
            signin_method: "api_key".into(),
        };
        s.oauth_txn_create(&txn).await.unwrap();
        assert_eq!(s.oauth_txn_get(&txn.csrf).await.unwrap().unwrap().state, "s");
        s.oauth_txn_set_user(&txn.csrf, &user).await.unwrap();
        assert_eq!(s.oauth_txn_get(&txn.csrf).await.unwrap().unwrap().user_name.as_deref(), Some(user.as_str()));

        let first = s.oauth_grant_find_or_create(&user, &client.client_id, "memdusa", None, "api_key").await.unwrap();
        let second = s.oauth_grant_find_or_create(&user, &client.client_id, "memdusa", None, "api_key").await.unwrap();
        assert_eq!(first.id, second.id, "{name}: re-sign-in reuses the grant");

        let secret = generate_secret(CODE_PREFIX).unwrap();
        let code = OAuthCode {
            id: new_id(),
            secret: secret.clone(),
            client_id: client.client_id.clone(),
            redirect_uri: client.redirect_uris[0].clone(),
            code_challenge: txn.code_challenge.clone(),
            scope: "memdusa".into(),
            resource: None,
            grant_id: first.id.clone(),
            expires_at: now_ms() + 60_000,
            used_at: None,
            created_at: now_ms(),
        };
        s.oauth_code_create(&code).await.unwrap();
        let stored = s.oauth_code_get(&secret).await.unwrap().unwrap();
        assert_eq!(stored.grant_id, first.id, "{name}: code bound to grant");
        assert!(s.oauth_code_claim(&secret, now_ms()).await.unwrap(), "{name}: first claim");
        assert!(!s.oauth_code_claim(&secret, now_ms()).await.unwrap(), "{name}: replay refused");
        s.oauth_txn_delete(&txn.csrf).await.unwrap();
        assert!(s.oauth_txn_get(&txn.csrf).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn tokens_authenticate_rotate_and_revoke() {
    for (name, s) in stores().await {
        let user = format!("u{}", new_id());
        s.user_create(&user, false).await.unwrap();
        let grant = s.oauth_grant_find_or_create(&user, "client", "memdusa", None, "api_key").await.unwrap();
        let access = generate_secret(ACCESS_PREFIX).unwrap();
        let refresh = generate_secret(REFRESH_PREFIX).unwrap();
        let now = now_ms();
        for (secret, kind) in [(&access, TokenKind::Access), (&refresh, TokenKind::Refresh)] {
            s.oauth_token_create(&OAuthToken {
                id: new_id(),
                secret: secret.clone(),
                grant_id: grant.id.clone(),
                kind,
                prefix: secret[..12].into(),
                scope: "memdusa".into(),
                resource: None,
                expires_at: now + 3_600_000,
                rotated_from: None,
                revoked_at: None,
                last_used_at: None,
                created_at: now,
            })
            .await
            .unwrap();
        }
        let auth = s.authenticate_access(&access, now).await.unwrap().unwrap();
        assert_eq!(auth.identity.user.as_deref(), Some(user.as_str()));
        assert!(!auth.read_only, "{name}: full scope");
        assert!(s.authenticate_access("mdk_not-an-oauth-token", now).await.unwrap().is_none());

        // Revoking the grant kills the whole chain and authentication.
        s.oauth_grant_revoke(&grant.id, now + 1).await.unwrap();
        assert!(s.authenticate_access(&access, now + 2).await.unwrap().is_none(), "{name}: revocation immediate");
        let tokens = s.oauth_tokens_for_grant(&grant.id).await.unwrap();
        assert!(tokens.iter().all(|t| t.revoked_at.is_some()));
    }
}

#[tokio::test]
async fn read_only_scope_and_sessions() {
    for (name, s) in stores().await {
        let user = format!("u{}", new_id());
        s.user_create(&user, false).await.unwrap();
        let grant =
            s.oauth_grant_find_or_create(&user, "client", "memdusa:read", Some("res"), "api_key").await.unwrap();
        let secret = generate_secret(ACCESS_PREFIX).unwrap();
        s.oauth_token_create(&OAuthToken {
            id: new_id(),
            secret: secret.clone(),
            grant_id: grant.id.clone(),
            kind: TokenKind::Access,
            prefix: secret[..12].into(),
            scope: "memdusa:read".into(),
            resource: Some("res".into()),
            expires_at: now_ms() + 3_600_000,
            rotated_from: None,
            revoked_at: None,
            last_used_at: None,
            created_at: now_ms(),
        })
        .await
        .unwrap();
        assert!(s.authenticate_access(&secret, now_ms()).await.unwrap().unwrap().read_only, "{name}: read-only");

        let sessions = s.oauth_sessions_list(Some(&user)).await.unwrap();
        assert_eq!(sessions.len(), 1, "{name}: one session");
        assert_eq!(sessions[0].resource.as_deref(), Some("res"));
        let revoked = s.oauth_session_revoke(&sessions[0].id, Some(&user), now_ms()).await.unwrap();
        assert_eq!(revoked, 1, "{name}: revoked");
        assert!(s.oauth_sessions_list(Some(&user)).await.unwrap().is_empty());
        assert_eq!(
            s.oauth_session_revoke(&sessions[0].id, Some(&user), now_ms()).await.unwrap(),
            0,
            "{name}: idempotent"
        );
    }
}

#[tokio::test]
async fn identities_link_bind_and_unlink() {
    for (name, s) in stores().await {
        let user = format!("u{}", new_id()).replace('-', "");
        s.user_create(&user, false).await.unwrap();

        // Admin link is stored pending until the first successful sign-in.
        let pending = s.identity_insert(&user, "github", "pending:octocat", None, Some("octocat")).await.unwrap();
        assert!(pending.pending());
        assert_eq!(pending.handle(), "octocat");
        assert_eq!(s.identity_by_subject("github", "424242").await.unwrap().map(|i| i.user_name), None);
        assert_eq!(s.identity_pending("github", "octocat").await.unwrap().unwrap().user_name, user);

        s.identity_bind_pending(&pending, "424242").await.unwrap();
        let bound = s.identity_by_subject("github", "424242").await.unwrap().unwrap();
        assert_eq!(bound.user_name, user);
        assert!(!bound.pending());
        assert_eq!(bound.subject, "424242");

        // Duplicate real subjects are refused, and unlink removes by subject
        // or by pending handle.
        assert!(s.identity_insert(&user, "github", "424242", None, None).await.is_err());
        s.identity_insert(&user, "google", "pending:alice@acme.com", Some("alice@acme.com"), None).await.unwrap();
        assert_eq!(s.identity_list(&user).await.unwrap().len(), 2);
        assert_eq!(s.identity_unlink(&user, "google", "alice@acme.com").await.unwrap(), 1);
        assert_eq!(s.identity_unlink(&user, "github", "424242").await.unwrap(), 1);
        assert!(s.identity_list(&user).await.unwrap().is_empty(), "{name}: all links removed");
    }
}

#[tokio::test]
async fn auto_provision_user_names_are_sanitized_and_unique() {
    for (_name, s) in stores().await {
        let first = s.user_create_unique("Alice.O@example").await.unwrap();
        assert_eq!(first.name, "alice.o-example");
        assert!(!first.admin);
        let second = s.user_create_unique("Alice.O@example").await.unwrap();
        assert_eq!(second.name, "alice.o-example-2");
        let third = s.user_create_unique("Alice.O@example").await.unwrap();
        assert_eq!(third.name, "alice.o-example-3");
    }
}

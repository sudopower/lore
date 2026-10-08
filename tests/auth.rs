mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{body_json, create_tenant_key, test_router, test_state};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(b) = bearer {
        req = req.header("authorization", format!("Bearer {b}"));
    }
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    (status, body_json(res).await)
}

fn app(pool: PgPool) -> Router {
    test_router(test_state(pool, &std::env::temp_dir()))
}

#[sqlx::test(migrations = "./migrations")]
async fn tenant_key_and_me(pool: PgPool) {
    let app = app(pool);
    let (status, tenant) = send(
        &app,
        "POST",
        "/v1/admin/tenants",
        Some("t"),
        Some(json!({"name": "demo"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(tenant["name"], "demo");
    let tenant_id = tenant["id"].as_str().unwrap();

    let (status, key) = send(
        &app,
        "POST",
        &format!("/v1/admin/tenants/{tenant_id}/keys"),
        Some("t"),
        Some(json!({"scopes": ["read"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let plaintext = key["key"].as_str().unwrap();
    assert!(plaintext.starts_with("lk_"));
    assert_eq!(key["prefix"], plaintext[..11]);
    assert_eq!(key["scopes"], json!(["read"]));

    let (status, me) = send(&app, "GET", "/v1/me", Some(plaintext), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["tenant_id"], tenant_id);
    assert_eq!(me["tenant_name"], "demo");
    assert_eq!(me["scopes"], json!(["read"]));

    let (status, list) = send(&app, "GET", "/v1/admin/tenants", Some("t"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["tenants"][0]["name"], "demo");
    assert!(list["tenants"][0]["created_at"].is_string());
}

#[sqlx::test(migrations = "./migrations")]
async fn key_scopes_default_to_both(pool: PgPool) {
    let app = app(pool);
    let (tenant, _) = create_tenant_key(&app, "demo", &["read"]).await;
    // No body at all, and an empty object, both mean the defaults.
    for body in [None, Some(json!({}))] {
        let (status, key) = send(
            &app,
            "POST",
            &format!("/v1/admin/tenants/{tenant}/keys"),
            Some("t"),
            body,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(key["scopes"], json!(["ingest", "read"]));
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn bad_requests_are_400(pool: PgPool) {
    let app = app(pool);
    for name in ["", "Demo", "has space", "under_score", &"a".repeat(65)] {
        let (status, body) = send(
            &app,
            "POST",
            "/v1/admin/tenants",
            Some("t"),
            Some(json!({"name": name})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "name {name:?}");
        assert!(body["error"].is_string());
    }
    let (status, _) = send(
        &app,
        "POST",
        "/v1/admin/tenants",
        Some("t"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (tenant, _) = create_tenant_key(&app, "demo", &["read"]).await;
    for scopes in [json!(["root"]), json!([])] {
        let (status, _) = send(
            &app,
            "POST",
            &format!("/v1/admin/tenants/{tenant}/keys"),
            Some("t"),
            Some(json!({"scopes": scopes})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "scopes {scopes}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn wrong_admin_token_is_401(pool: PgPool) {
    let app = app(pool);
    for bearer in [Some("wrong"), Some(""), Some("tt"), None] {
        let (status, _) = send(&app, "GET", "/v1/admin/tenants", bearer, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "bearer {bearer:?}");
    }
    let (status, _) = send(
        &app,
        "POST",
        "/v1/admin/tenants",
        Some("wrong"),
        Some(json!({"name": "demo"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A tenant key is not an admin token.
    let (_, key) = create_tenant_key(&app, "demo", &["read", "ingest"]).await;
    let (status, _) = send(&app, "GET", "/v1/admin/tenants", Some(&key), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_api_disabled_is_403(pool: PgPool) {
    let mut state = test_state(pool, &std::env::temp_dir());
    let mut config = (*state.config).clone();
    config.admin_token = None;
    state.config = std::sync::Arc::new(config);
    let app = test_router(state);
    for bearer in [Some("t"), None] {
        let (status, body) = send(&app, "GET", "/v1/admin/tenants", bearer, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "bearer {bearer:?}");
        assert_eq!(body["error"], "admin API disabled");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn unknown_or_malformed_bearer_is_401(pool: PgPool) {
    let app = app(pool);
    let (_, key) = create_tenant_key(&app, "demo", &["read"]).await;
    let unknown = format!("lk_{}", "0".repeat(40));
    let truncated = key[..key.len() - 1].to_string();
    for bearer in [
        None,
        Some("nonsense"),
        Some(unknown.as_str()),
        Some(truncated.as_str()),
        Some("t"),
    ] {
        let (status, body) = send(&app, "GET", "/v1/me", bearer, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "bearer {bearer:?}");
        assert_eq!(body["error"], "unauthorized");
    }
    // A key under another scheme is not accepted either.
    let req = Request::get("/v1/me")
        .header("authorization", format!("Basic {key}"))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn revoked_key_is_401(pool: PgPool) {
    let app = app(pool);
    let (tenant, key) = create_tenant_key(&app, "demo", &["read"]).await;
    let (status, _) = send(&app, "GET", "/v1/me", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK);

    let (_, list) = send(
        &app,
        "GET",
        &format!("/v1/admin/tenants/{tenant}/keys"),
        Some("t"),
        None,
    )
    .await;
    let key_id = list["keys"][0]["id"].as_str().unwrap().to_string();

    let (status, _) = send(
        &app,
        "DELETE",
        &format!("/v1/admin/keys/{key_id}"),
        Some("t"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(&app, "GET", "/v1/me", Some(&key), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Revoking again is fine, and the list shows when it was revoked.
    let (status, _) = send(
        &app,
        "DELETE",
        &format!("/v1/admin/keys/{key_id}"),
        Some("t"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = send(
        &app,
        "GET",
        &format!("/v1/admin/tenants/{tenant}/keys"),
        Some("t"),
        None,
    )
    .await;
    assert!(list["keys"][0]["revoked_at"].is_string());
}

#[sqlx::test(migrations = "./migrations")]
async fn revoke_needs_admin_and_a_real_key(pool: PgPool) {
    let app = app(pool);
    let (_, key) = create_tenant_key(&app, "demo", &["read"]).await;
    let missing = uuid::Uuid::new_v4();
    let (status, _) = send(
        &app,
        "DELETE",
        &format!("/v1/admin/keys/{missing}"),
        Some("t"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &app,
        "DELETE",
        &format!("/v1/admin/keys/{missing}"),
        Some("wrong"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The key still works after the failed attempts.
    let (status, _) = send(&app, "GET", "/v1/me", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "./migrations")]
async fn duplicate_tenant_is_409(pool: PgPool) {
    let app = app(pool);
    create_tenant_key(&app, "demo", &["read"]).await;
    let (status, body) = send(
        &app,
        "POST",
        "/v1/admin/tenants",
        Some("t"),
        Some(json!({"name": "demo"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].is_string());
}

#[sqlx::test(migrations = "./migrations")]
async fn unknown_tenant_is_404(pool: PgPool) {
    let app = app(pool);
    let missing = uuid::Uuid::new_v4();
    let (status, _) = send(
        &app,
        "POST",
        &format!("/v1/admin/tenants/{missing}/keys"),
        Some("t"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &app,
        "GET",
        &format!("/v1/admin/tenants/{missing}/keys"),
        Some("t"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn key_list_shows_prefix_not_hash(pool: PgPool) {
    let app = app(pool.clone());
    let (tenant, key) = create_tenant_key(&app, "demo", &["ingest", "read"]).await;
    let (status, list) = send(
        &app,
        "GET",
        &format!("/v1/admin/tenants/{tenant}/keys"),
        Some("t"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let item = &list["keys"][0];
    assert_eq!(item["prefix"], key[..11]);
    assert_eq!(item["scopes"], json!(["ingest", "read"]));
    assert!(item["revoked_at"].is_null());
    assert!(item["created_at"].is_string());

    let raw = list.to_string();
    let stored: String = sqlx::query_scalar("SELECT key_hash FROM api_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!raw.contains(&stored), "hash leaked: {raw}");
    assert!(!raw.contains("key_hash"));
    assert!(!raw.contains(&key));
    // Only the hash is stored, never the plaintext.
    assert_ne!(stored, key);
    assert_eq!(stored, lore::auth::hash_key(&key));
}

#[sqlx::test(migrations = "./migrations")]
async fn keys_are_scoped_to_their_tenant(pool: PgPool) {
    let app = app(pool);
    let (_, a) = create_tenant_key(&app, "alpha", &["read"]).await;
    let (_, b) = create_tenant_key(&app, "beta", &["ingest"]).await;
    let (_, me_a) = send(&app, "GET", "/v1/me", Some(&a), None).await;
    let (_, me_b) = send(&app, "GET", "/v1/me", Some(&b), None).await;
    assert_eq!(me_a["tenant_name"], "alpha");
    assert_eq!(me_b["tenant_name"], "beta");
    assert_eq!(me_b["scopes"], json!(["ingest"]));
}

#[sqlx::test(migrations = "./migrations")]
async fn caller_from_headers_reads_the_key(pool: PgPool) {
    use axum::http::HeaderMap;
    use lore::auth::{Scope, caller_from_headers};

    let state = test_state(pool, &std::env::temp_dir());
    let app = test_router(state.clone());
    let (tenant, key) = create_tenant_key(&app, "demo", &["ingest"]).await;

    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {key}").parse().unwrap());
    let caller = caller_from_headers(&state, &headers).await.unwrap();
    assert_eq!(caller.tenant_id, tenant);
    assert_eq!(caller.scopes, vec![Scope::Ingest]);
    assert!(caller.require(Scope::Ingest).is_ok());
    assert_eq!(
        caller.require(Scope::Read).unwrap_err().status(),
        StatusCode::FORBIDDEN
    );

    assert!(
        caller_from_headers(&state, &HeaderMap::new())
            .await
            .is_err()
    );
}

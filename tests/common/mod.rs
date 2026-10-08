#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use lore::config::Config;
use lore::http::{AppState, router};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

pub fn test_config(data_dir: &Path) -> Config {
    Config::from_lookup(|k| match k {
        "LORE_DATA_DIR" => Some(data_dir.to_string_lossy().into_owned()),
        "LORE_ADMIN_TOKEN" => Some("t".to_string()),
        _ => None,
    })
    .expect("test config is valid")
}

pub fn test_state(pool: PgPool, data_dir: &Path) -> AppState {
    AppState {
        pool,
        config: Arc::new(test_config(data_dir)),
    }
}

pub fn test_router(state: AppState) -> Router {
    router(state)
}

/// Creates a tenant and one key with the given scopes through the admin routes (admin token
/// `"t"`). Returns the tenant id and the plaintext key.
pub async fn create_tenant_key(router: &Router, name: &str, scopes: &[&str]) -> (Uuid, String) {
    let req = Request::post("/v1/admin/tenants")
        .header("authorization", "Bearer t")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": name }).to_string()))
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED, "create tenant {name}");
    let tenant: Value = body_json(res).await;
    let tenant_id: Uuid = tenant["id"].as_str().unwrap().parse().unwrap();

    let req = Request::post(format!("/v1/admin/tenants/{tenant_id}/keys"))
        .header("authorization", "Bearer t")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "scopes": scopes }).to_string()))
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED, "create key for {name}");
    let key: Value = body_json(res).await;
    (tenant_id, key["key"].as_str().unwrap().to_string())
}

pub async fn body_json(res: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

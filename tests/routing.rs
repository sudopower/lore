mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{create_tenant_key, test_router, test_state};
use sqlx::PgPool;
use tower::ServiceExt;

async fn status(app: &axum::Router, method: &str, path: &str, key: Option<&str>) -> StatusCode {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(key) = key {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    let req = req.body(Body::empty()).unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

#[sqlx::test(migrations = "./migrations")]
async fn unknown_paths_are_404_with_or_without_a_key(pool: PgPool) {
    let app = test_router(test_state(pool, &std::env::temp_dir()));
    let (_, key) = create_tenant_key(&app, "acme", &["read"]).await;

    assert_eq!(
        status(&app, "GET", "/nope", None).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status(&app, "GET", "/nope", Some(&key)).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status(&app, "POST", "/nope/deeper", Some("lk_bad")).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status(&app, "GET", "/v1/nope", None).await,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn mcp_and_api_routes_still_require_a_key(pool: PgPool) {
    let app = test_router(test_state(pool, &std::env::temp_dir()));
    let (_, key) = create_tenant_key(&app, "acme", &["read"]).await;

    assert_eq!(
        status(&app, "POST", "/mcp", None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(&app, "GET", "/mcp", None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(&app, "POST", "/mcp", Some("lk_bad")).await,
        StatusCode::UNAUTHORIZED
    );
    // Sub-paths of /mcp are still behind the key.
    assert_eq!(
        status(&app, "POST", "/mcp/x", None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(&app, "GET", "/mcp/x/y", None).await,
        StatusCode::UNAUTHORIZED
    );
    // A known route with a key is not refused for the key.
    assert_eq!(
        status(&app, "GET", "/v1/me", Some(&key)).await,
        StatusCode::OK
    );
    // A known route without a key is 401.
    assert_eq!(
        status(&app, "GET", "/v1/me", None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(&app, "GET", "/v1/search?q=x", None).await,
        StatusCode::UNAUTHORIZED
    );
}

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use lore::http::router;
use sqlx::PgPool;
use tower::ServiceExt;

async fn get(app: axum::Router, path: &str) -> (StatusCode, String) {
    let res = app
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[sqlx::test(migrations = "./migrations")]
async fn healthz_and_livez(pool: PgPool) {
    let app = router(common::test_state(pool, &std::env::temp_dir()));

    let (status, body) = get(app.clone(), "/healthz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"status":"ok","db":"ok"}"#);

    let (status, body) = get(app, "/livez").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"status":"ok"}"#);
}

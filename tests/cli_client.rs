mod common;

use common::{test_router, test_state};
use lore::cli::client::{ApiError, Auth, Client};
use serde_json::{Value, json};
use sqlx::PgPool;

/// Serves the router on a random local port and returns its base URL.
async fn serve(pool: PgPool) -> String {
    let app = test_router(test_state(pool, &std::env::temp_dir()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[sqlx::test(migrations = "./migrations")]
async fn client_walks_through_tenant_key_me_and_revoke(pool: PgPool) {
    let base = serve(pool).await;
    let admin = Client::new(&base, None, Some("t".into())).unwrap();

    let tenant: Value = admin
        .post_json("/v1/admin/tenants", Auth::Admin, &json!({"name": "demo"}))
        .await
        .unwrap();
    let tenant_id = tenant["id"].as_str().unwrap().to_string();
    let key: Value = admin
        .post_json(
            &format!("/v1/admin/tenants/{tenant_id}/keys"),
            Auth::Admin,
            &json!({}),
        )
        .await
        .unwrap();
    let plaintext = key["key"].as_str().unwrap().to_string();

    let user = Client::new(&format!("{base}/"), Some(plaintext), None).unwrap();
    let me: Value = user.get_json("/v1/me", Auth::Key).await.unwrap();
    assert_eq!(me["tenant_name"], "demo");

    admin
        .delete(
            &format!("/v1/admin/keys/{}", key["id"].as_str().unwrap()),
            Auth::Admin,
        )
        .await
        .unwrap();
    let err = user
        .get_json::<Value>("/v1/me", Auth::Key)
        .await
        .unwrap_err();
    let api = err.downcast_ref::<ApiError>().expect("an ApiError");
    assert_eq!(api.status, 401);
    assert_eq!(api.message, "unauthorized");
    assert_eq!(err.to_string(), "server returned 401: unauthorized");
}

#[sqlx::test(migrations = "./migrations")]
async fn client_reports_the_servers_error_message(pool: PgPool) {
    let base = serve(pool).await;
    let admin = Client::new(&base, None, Some("t".into())).unwrap();
    let err = admin
        .post_json::<_, Value>(
            "/v1/admin/tenants",
            Auth::Admin,
            &json!({"name": "Bad Name"}),
        )
        .await
        .unwrap_err();
    let api = err.downcast_ref::<ApiError>().expect("an ApiError");
    assert_eq!(api.status, 400);
    assert!(api.message.contains("tenant name"), "{}", api.message);

    let wrong = Client::new(&base, None, Some("nope".into())).unwrap();
    let err = wrong
        .get_json::<Value>("/v1/admin/tenants", Auth::Admin)
        .await
        .unwrap_err();
    assert_eq!(err.downcast_ref::<ApiError>().unwrap().status, 401);
}

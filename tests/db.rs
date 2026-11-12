mod common;

use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};
use lore::auth::caller_from_headers;
use sqlx::{ConnectOptions, PgPool};
use uuid::Uuid;

#[sqlx::test(migrations = "./migrations")]
async fn production_pool_has_a_ready_spare_for_auth_during_the_first_worker_poll(pool: PgPool) {
    let data = tempfile::tempdir().unwrap();
    let setup = lore::http::router(common::test_state(pool.clone(), data.path()));
    let (tenant, key) = common::create_tenant_key(&setup, "startup", &["read"]).await;
    let application = format!("startup-{}", Uuid::new_v4());
    let mut url = pool.connect_options().to_url_lossy();
    // The lossy conversion omits application_name; add the observer tag to the actual URL.
    url.query_pairs_mut()
        .append_pair("application_name", &application);
    let production = lore::db::connect(url.as_str()).await.unwrap();
    assert_eq!(production.options().get_max_connections(), 10);
    assert_eq!(production.options().get_min_connections(), 2);
    assert_eq!(production.size(), 2);
    let count_backends = || {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname=current_database() AND application_name=$1",
        )
        .bind(&application)
        .fetch_one(&pool)
    };
    assert_eq!(
        count_backends().await.unwrap(),
        2,
        "both backends are already live"
    );

    // Hold the first poll's connection across authentication, without a timing-dependent sleep.
    let mut worker = production.acquire().await.unwrap();
    let due: i64 = sqlx::query_scalar("SELECT count(*) FROM session_queue")
        .fetch_one(&mut *worker)
        .await
        .unwrap();
    assert_eq!(due, 0);
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {key}")).unwrap(),
    );
    let state = common::test_state(production.clone(), data.path());
    let caller = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        caller_from_headers(&state, &headers),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(caller.tenant_id, tenant);
    assert_eq!(
        production.size(),
        2,
        "authentication must reuse the ready spare"
    );
    assert_eq!(
        count_backends().await.unwrap(),
        2,
        "no third backend was opened"
    );
    drop(worker);
    production.close().await;
}

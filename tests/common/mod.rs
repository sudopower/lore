#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use lore::config::Config;
use lore::http::AppState;
use sqlx::PgPool;

pub fn test_config(data_dir: &Path) -> Config {
    Config::from_lookup(|k| match k {
        "LORE_DATA_DIR" => Some(data_dir.to_string_lossy().into_owned()),
        "LORE_ADMIN_TOKEN" => Some("test-admin".to_string()),
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

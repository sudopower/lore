use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::{Admin, Scope, generate_key};
use crate::error::AppError;
use crate::http::{AppJson, AppState};

#[derive(Deserialize)]
pub struct CreateTenant {
    name: String,
}

#[derive(Serialize)]
pub struct TenantCreated {
    id: Uuid,
    name: String,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct TenantItem {
    id: Uuid,
    name: String,
    created_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct TenantList {
    tenants: Vec<TenantItem>,
}

#[derive(Deserialize, Default)]
struct CreateKey {
    scopes: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct KeyCreated {
    id: Uuid,
    key: String,
    prefix: String,
    scopes: Vec<Scope>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct KeyItem {
    id: Uuid,
    prefix: String,
    scopes: Vec<String>,
    created_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct KeyList {
    keys: Vec<KeyItem>,
}

/// 1 to 64 characters of `[a-z0-9-]`.
fn valid_tenant_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub async fn create_tenant(
    _admin: Admin,
    State(state): State<AppState>,
    AppJson(req): AppJson<CreateTenant>,
) -> Result<(StatusCode, Json<TenantCreated>), AppError> {
    if !valid_tenant_name(&req.name) {
        return Err(AppError::BadRequest(
            "tenant name must be 1 to 64 characters of a-z, 0-9 and -".to_string(),
        ));
    }
    let id = Uuid::new_v4();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO tenants (id, name) VALUES ($1, $2) ON CONFLICT (name) DO NOTHING RETURNING id",
    )
    .bind(id)
    .bind(&req.name)
    .fetch_optional(&state.pool)
    .await?;
    match inserted {
        Some(id) => Ok((
            StatusCode::CREATED,
            Json(TenantCreated { id, name: req.name }),
        )),
        None => Err(AppError::Conflict(format!(
            "tenant {:?} already exists",
            req.name
        ))),
    }
}

pub async fn list_tenants(
    _admin: Admin,
    State(state): State<AppState>,
) -> Result<Json<TenantList>, AppError> {
    let tenants: Vec<TenantItem> =
        sqlx::query_as("SELECT id, name, created_at FROM tenants ORDER BY created_at, name")
            .fetch_all(&state.pool)
            .await?;
    Ok(Json(TenantList { tenants }))
}

/// Parses the optional scopes of a key request. Missing means both scopes. An empty list or an
/// unknown scope is a 400. Duplicates are dropped.
fn parse_scopes(requested: Option<Vec<String>>) -> Result<Vec<Scope>, AppError> {
    let Some(names) = requested else {
        return Ok(vec![Scope::Ingest, Scope::Read]);
    };
    if names.is_empty() {
        return Err(AppError::BadRequest("scopes must not be empty".to_string()));
    }
    let mut scopes = Vec::new();
    for name in &names {
        let s = Scope::parse(name)?;
        if !scopes.contains(&s) {
            scopes.push(s);
        }
    }
    Ok(scopes)
}

pub async fn create_key(
    _admin: Admin,
    State(state): State<AppState>,
    Path(tenant_id): Path<Uuid>,
    body: Bytes,
) -> Result<(StatusCode, Json<KeyCreated>), AppError> {
    // An empty body means "defaults", so `curl -X POST` works without `-d '{}'`.
    let req: CreateKey = if body.iter().all(u8::is_ascii_whitespace) {
        CreateKey::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| AppError::BadRequest(format!("invalid JSON body: {e}")))?
    };
    let scopes = parse_scopes(req.scopes)?;
    let scope_names: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();

    let key = generate_key();
    let id = Uuid::new_v4();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO api_keys (id, tenant_id, prefix, key_hash, scopes) \
         SELECT $1, t.id, $3, $4, $5 FROM tenants t WHERE t.id = $2 \
         RETURNING id",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(&key.prefix)
    .bind(&key.hash)
    .bind(&scope_names)
    .fetch_optional(&state.pool)
    .await?;
    let id = inserted.ok_or(AppError::NotFound)?;
    Ok((
        StatusCode::CREATED,
        Json(KeyCreated {
            id,
            key: key.plaintext,
            prefix: key.prefix,
            scopes,
        }),
    ))
}

pub async fn list_keys(
    _admin: Admin,
    State(state): State<AppState>,
    Path(tenant_id): Path<Uuid>,
) -> Result<Json<KeyList>, AppError> {
    let exists: Option<Uuid> = sqlx::query_scalar("SELECT id FROM tenants WHERE id = $1")
        .bind(tenant_id)
        .fetch_optional(&state.pool)
        .await?;
    if exists.is_none() {
        return Err(AppError::NotFound);
    }
    let keys: Vec<KeyItem> = sqlx::query_as(
        "SELECT id, prefix, scopes, created_at, revoked_at FROM api_keys \
         WHERE tenant_id = $1 ORDER BY created_at, id",
    )
    .bind(tenant_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(KeyList { keys }))
}

/// Revokes a key. Revoking an already revoked key keeps the first `revoked_at` and is a 204.
pub async fn revoke_key(
    _admin: Admin,
    State(state): State<AppState>,
    Path(key_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let done =
        sqlx::query("UPDATE api_keys SET revoked_at = COALESCE(revoked_at, now()) WHERE id = $1")
            .bind(key_id)
            .execute(&state.pool)
            .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_names() {
        assert!(valid_tenant_name("demo"));
        assert!(valid_tenant_name("a"));
        assert!(valid_tenant_name("glass-flow-2"));
        assert!(valid_tenant_name(&"a".repeat(64)));
        assert!(!valid_tenant_name(""));
        assert!(!valid_tenant_name(&"a".repeat(65)));
        assert!(!valid_tenant_name("Demo"));
        assert!(!valid_tenant_name("de mo"));
        assert!(!valid_tenant_name("de_mo"));
        assert!(!valid_tenant_name("démo"));
    }

    #[test]
    fn scope_defaults_and_validation() {
        assert_eq!(
            parse_scopes(None).unwrap(),
            vec![Scope::Ingest, Scope::Read]
        );
        assert_eq!(
            parse_scopes(Some(vec!["read".into(), "read".into()])).unwrap(),
            vec![Scope::Read]
        );
        assert!(parse_scopes(Some(vec![])).is_err());
        assert!(parse_scopes(Some(vec!["root".into()])).is_err());
    }
}

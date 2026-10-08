use std::fmt;

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use rand::RngCore;
use rand::rngs::OsRng;
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::error::AppError;
use crate::http::AppState;

/// Every API key starts with this.
pub const KEY_PREFIX: &str = "lk_";
/// Length of the stored, displayable prefix: `lk_` plus 8 hex characters.
const DISPLAY_PREFIX_LEN: usize = 11;
/// Random bytes in a key (40 hex characters).
const KEY_BYTES: usize = 20;

/// A freshly generated key. The plaintext is shown once and never stored.
pub struct GeneratedKey {
    pub plaintext: String,
    pub prefix: String,
    pub hash: String,
}

/// Generates `lk_` followed by 40 lowercase hex characters from the OS random source.
pub fn generate_key() -> GeneratedKey {
    let mut bytes = [0u8; KEY_BYTES];
    OsRng.fill_bytes(&mut bytes);
    let plaintext = format!("{KEY_PREFIX}{}", hex::encode(bytes));
    let prefix = plaintext[..DISPLAY_PREFIX_LEN].to_string();
    let hash = hash_key(&plaintext);
    GeneratedKey {
        plaintext,
        prefix,
        hash,
    }
}

/// Lowercase hex SHA-256 of a key. This is what `api_keys.key_hash` holds.
pub fn hash_key(plaintext: &str) -> String {
    hex::encode(Sha256::digest(plaintext.as_bytes()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Ingest,
    Read,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Ingest => "ingest",
            Scope::Read => "read",
        }
    }

    /// Unknown scope names are a 400.
    pub fn parse(s: &str) -> Result<Scope, AppError> {
        match s {
            "ingest" => Ok(Scope::Ingest),
            "read" => Ok(Scope::Read),
            other => Err(AppError::BadRequest(format!("unknown scope {other:?}"))),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The authenticated tenant key behind a request.
#[derive(Debug, Clone)]
pub struct Caller {
    pub tenant_id: Uuid,
    pub key_id: Uuid,
    pub scopes: Vec<Scope>,
}

impl Caller {
    pub fn require(&self, s: Scope) -> Result<(), AppError> {
        if self.scopes.contains(&s) {
            Ok(())
        } else {
            Err(AppError::Forbidden(format!("missing scope {s}")))
        }
    }
}

/// Returns the token of an `Authorization: Bearer <token>` header, if there is one.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Resolves the tenant key in the `Authorization` header. A missing, malformed, unknown or
/// revoked key is `Unauthorized`. The MCP middleware reuses this.
pub async fn caller_from_headers(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Caller, AppError> {
    let token = bearer_token(headers).ok_or(AppError::Unauthorized)?;
    if !token.starts_with(KEY_PREFIX) {
        return Err(AppError::Unauthorized);
    }
    let row: Option<(Uuid, Uuid, Vec<String>)> = sqlx::query_as(
        "SELECT id, tenant_id, scopes FROM api_keys WHERE key_hash = $1 AND revoked_at IS NULL",
    )
    .bind(hash_key(token))
    .fetch_optional(&state.pool)
    .await?;
    let (key_id, tenant_id, scopes) = row.ok_or(AppError::Unauthorized)?;
    let scopes = scopes
        .iter()
        .map(|s| Scope::parse(s))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("stored key has bad scope: {e}")))?;
    Ok(Caller {
        tenant_id,
        key_id,
        scopes,
    })
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        caller_from_headers(state, &parts.headers).await
    }
}

/// Compares two tokens in constant time. Both sides are hashed first, so the comparison
/// takes the same time whatever their lengths.
fn tokens_match(expected: &str, provided: &str) -> bool {
    let a = Sha256::digest(expected.as_bytes());
    let b = Sha256::digest(provided.as_bytes());
    a.ct_eq(&b).into()
}

/// Extractor for the admin API: the bearer token must equal `LORE_ADMIN_TOKEN`.
pub struct Admin;

impl FromRequestParts<AppState> for Admin {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let expected = state
            .config
            .admin_token
            .as_deref()
            .ok_or_else(|| AppError::Forbidden("admin API disabled".to_string()))?;
        let provided = bearer_token(&parts.headers).ok_or(AppError::Unauthorized)?;
        if tokens_match(expected, provided) {
            Ok(Admin)
        } else {
            Err(AppError::Unauthorized)
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn generated_key_format() {
        let k = generate_key();
        assert_eq!(k.plaintext.len(), 43);
        assert!(k.plaintext.starts_with("lk_"));
        assert!(
            k.plaintext[3..]
                .chars()
                .all(|c| matches!(c, '0'..='9' | 'a'..='f'))
        );
        assert_eq!(k.prefix, &k.plaintext[..11]);
        assert_eq!(k.hash, hash_key(&k.plaintext));
        assert_eq!(k.hash.len(), 64);
        assert!(!k.hash.contains(&k.plaintext));
    }

    #[test]
    fn generated_keys_differ() {
        assert_ne!(generate_key().plaintext, generate_key().plaintext);
    }

    #[test]
    fn hash_is_sha256_hex() {
        assert_eq!(
            hash_key("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn scope_parse_and_display() {
        assert_eq!(Scope::parse("ingest").unwrap(), Scope::Ingest);
        assert_eq!(Scope::parse("read").unwrap(), Scope::Read);
        assert_eq!(Scope::Ingest.to_string(), "ingest");
        assert_eq!(Scope::Read.to_string(), "read");
        let err = Scope::parse("admin").unwrap_err();
        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(Scope::parse("Read").unwrap_err().status().as_u16(), 400);
    }

    #[test]
    fn require_checks_scope() {
        let c = Caller {
            tenant_id: Uuid::nil(),
            key_id: Uuid::nil(),
            scopes: vec![Scope::Read],
        };
        assert!(c.require(Scope::Read).is_ok());
        match c.require(Scope::Ingest) {
            Err(AppError::Forbidden(m)) => assert_eq!(m, "missing scope ingest"),
            other => panic!("expected Forbidden, got {other:?}"),
        }
        let none = Caller {
            scopes: vec![],
            ..c
        };
        assert!(none.require(Scope::Read).is_err());
    }

    #[test]
    fn bearer_parsing() {
        let h = |v: &str| {
            let mut m = HeaderMap::new();
            m.insert(AUTHORIZATION, HeaderValue::from_str(v).unwrap());
            m
        };
        assert_eq!(bearer_token(&h("Bearer abc")), Some("abc"));
        assert_eq!(bearer_token(&h("bearer abc")), Some("abc"));
        assert_eq!(bearer_token(&h("Basic abc")), None);
        assert_eq!(bearer_token(&h("Bearer ")), None);
        assert_eq!(bearer_token(&h("abc")), None);
        assert_eq!(bearer_token(&HeaderMap::new()), None);
    }

    #[test]
    fn token_comparison() {
        assert!(tokens_match("secret", "secret"));
        assert!(!tokens_match("secret", "secreT"));
        assert!(!tokens_match("secret", "secre"));
        assert!(!tokens_match("secret", "secret2"));
        assert!(!tokens_match("secret", ""));
    }
}

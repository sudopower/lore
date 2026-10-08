use std::path::PathBuf;

use anyhow::{Result, bail};

/// Runtime configuration. All settings come from `LORE_*` environment variables.
/// Every later ticket adds its variables here, and to the README table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub database_url: String,
    pub listen_addr: String,
    pub data_dir: PathBuf,
    pub admin_token: Option<String>,
    pub log: String,
    pub log_format: LogFormat,
    pub embedder: EmbedderKind,
    pub embed_model: String,
    /// Largest accepted git bundle upload, in bytes.
    pub max_bundle_bytes: u64,
    pub version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedderKind {
    Fastembed,
    Hash,
}

/// The only embedding model v1 supports.
pub const EMBED_MODEL: &str = "bge-small-en-v1.5";

/// Default `LORE_MAX_BUNDLE_BYTES`: 512 MiB.
pub const DEFAULT_MAX_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(f: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let get = |key: &str, default: &str| f(key).unwrap_or_else(|| default.to_string());

        let log_format = match get("LORE_LOG_FORMAT", "text").as_str() {
            "text" => LogFormat::Text,
            "json" => LogFormat::Json,
            other => bail!("LORE_LOG_FORMAT must be \"text\" or \"json\", got {other:?}"),
        };

        let embedder = match get("LORE_EMBEDDER", "fastembed").as_str() {
            "fastembed" => EmbedderKind::Fastembed,
            "hash" => EmbedderKind::Hash,
            other => bail!("LORE_EMBEDDER must be \"fastembed\" or \"hash\", got {other:?}"),
        };
        let embed_model = get("LORE_EMBED_MODEL", EMBED_MODEL);
        if embed_model != EMBED_MODEL {
            bail!(
                "LORE_EMBED_MODEL must be {EMBED_MODEL:?} (the only supported model), got {embed_model:?}"
            );
        }

        let max_bundle_bytes = match f("LORE_MAX_BUNDLE_BYTES") {
            None => DEFAULT_MAX_BUNDLE_BYTES,
            Some(v) => match v.trim().parse::<u64>() {
                Ok(n) if n > 0 => n,
                _ => bail!("LORE_MAX_BUNDLE_BYTES must be a positive integer, got {v:?}"),
            },
        };

        Ok(Self {
            database_url: get(
                "LORE_DATABASE_URL",
                "postgres://lore:lore@localhost:5434/lore",
            ),
            listen_addr: get("LORE_LISTEN_ADDR", "0.0.0.0:8080"),
            data_dir: PathBuf::from(get("LORE_DATA_DIR", "./data")),
            admin_token: f("LORE_ADMIN_TOKEN").filter(|t| !t.is_empty()),
            log: get("LORE_LOG", "info"),
            log_format,
            embedder,
            embed_model,
            max_bundle_bytes,
            version: get("LORE_VERSION", env!("CARGO_PKG_VERSION")),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> Result<Config> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn defaults() {
        let c = lookup(&[]).unwrap();
        assert_eq!(c.database_url, "postgres://lore:lore@localhost:5434/lore");
        assert_eq!(c.listen_addr, "0.0.0.0:8080");
        assert_eq!(c.data_dir, PathBuf::from("./data"));
        assert_eq!(c.admin_token, None);
        assert_eq!(c.log, "info");
        assert_eq!(c.log_format, LogFormat::Text);
        assert_eq!(c.embedder, EmbedderKind::Fastembed);
        assert_eq!(c.embed_model, "bge-small-en-v1.5");
        assert_eq!(c.max_bundle_bytes, 512 * 1024 * 1024);
        assert_eq!(c.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn overrides_are_read() {
        let c = lookup(&[
            ("LORE_DATABASE_URL", "postgres://x/y"),
            ("LORE_LISTEN_ADDR", "127.0.0.1:9"),
            ("LORE_DATA_DIR", "/tmp/d"),
            ("LORE_ADMIN_TOKEN", "secret"),
            ("LORE_LOG", "debug"),
            ("LORE_LOG_FORMAT", "json"),
            ("LORE_EMBEDDER", "hash"),
            ("LORE_EMBED_MODEL", "bge-small-en-v1.5"),
            ("LORE_MAX_BUNDLE_BYTES", "1048576"),
            ("LORE_VERSION", "9.9.9"),
        ])
        .unwrap();
        assert_eq!(c.database_url, "postgres://x/y");
        assert_eq!(c.listen_addr, "127.0.0.1:9");
        assert_eq!(c.data_dir, PathBuf::from("/tmp/d"));
        assert_eq!(c.admin_token.as_deref(), Some("secret"));
        assert_eq!(c.log, "debug");
        assert_eq!(c.log_format, LogFormat::Json);
        assert_eq!(c.embedder, EmbedderKind::Hash);
        assert_eq!(c.max_bundle_bytes, 1_048_576);
        assert_eq!(c.version, "9.9.9");
    }

    #[test]
    fn empty_admin_token_is_none() {
        let c = lookup(&[("LORE_ADMIN_TOKEN", "")]).unwrap();
        assert_eq!(c.admin_token, None);
    }

    #[test]
    fn bad_log_format_is_an_error() {
        let err = lookup(&[("LORE_LOG_FORMAT", "yaml")]).unwrap_err();
        assert!(err.to_string().contains("LORE_LOG_FORMAT"), "{err}");
    }

    #[test]
    fn bad_embedder_is_an_error() {
        let err = lookup(&[("LORE_EMBEDDER", "openai")]).unwrap_err();
        assert!(err.to_string().contains("LORE_EMBEDDER"), "{err}");
    }

    #[test]
    fn unknown_embed_model_is_an_error() {
        let err = lookup(&[("LORE_EMBED_MODEL", "all-MiniLM-L6-v2")]).unwrap_err();
        assert!(err.to_string().contains("LORE_EMBED_MODEL"), "{err}");
    }

    #[test]
    fn bad_max_bundle_bytes_is_an_error() {
        for bad in ["", "0", "-1", "big", "1.5"] {
            let err = lookup(&[("LORE_MAX_BUNDLE_BYTES", bad)]).unwrap_err();
            assert!(err.to_string().contains("LORE_MAX_BUNDLE_BYTES"), "{err}");
        }
    }
}

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::{Body, Method, RequestBuilder, Response};
use serde::de::DeserializeOwned;

/// Bundle uploads can be slow, so a request may take up to 15 minutes.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Which bearer token a request carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// The tenant API key (`--key`).
    Key,
    /// The admin token (`--admin-token`).
    Admin,
}

/// A non-2xx answer from the server, with the message from its `error` field.
#[derive(Debug, thiserror::Error)]
#[error("server returned {status}: {message}")]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

/// HTTP client for a lore server. The flags `--url`, `--key` and `--admin-token` are global,
/// so every subcommand builds one of these from them.
pub struct Client {
    base: String,
    key: Option<String>,
    admin: Option<String>,
    http: reqwest::Client,
}

impl Client {
    pub fn new(base: &str, key: Option<String>, admin: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building the HTTP client")?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            key: key.filter(|k| !k.is_empty()),
            admin: admin.filter(|k| !k.is_empty()),
            http,
        })
    }

    fn request(&self, method: Method, path: &str, auth: Auth) -> Result<RequestBuilder> {
        let token = match auth {
            Auth::Key => self.key.as_deref().ok_or_else(|| {
                anyhow!("this command needs an API key: pass --key or set LORE_KEY")
            })?,
            Auth::Admin => self.admin.as_deref().ok_or_else(|| {
                anyhow!(
                    "this command needs the admin token: pass --admin-token or set LORE_ADMIN_TOKEN"
                )
            })?,
        };
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        Ok(self.http.request(method, url).bearer_auth(token))
    }

    async fn send(req: RequestBuilder) -> Result<Response> {
        let res = req.send().await.context("sending the request")?;
        let status = res.status();
        if status.is_success() {
            return Ok(res);
        }
        let text = res.text().await.unwrap_or_default();
        Err(ApiError {
            status: status.as_u16(),
            message: error_message(&text),
        }
        .into())
    }

    async fn json<T: DeserializeOwned>(res: Response) -> Result<T> {
        res.json().await.context("reading the server's JSON reply")
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str, auth: Auth) -> Result<T> {
        let res = Self::send(self.request(Method::GET, path, auth)?).await?;
        Self::json(res).await
    }

    pub async fn post_json<B, T>(&self, path: &str, auth: Auth, body: &B) -> Result<T>
    where
        B: serde::Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let res = Self::send(self.request(Method::POST, path, auth)?.json(body)).await?;
        Self::json(res).await
    }

    pub async fn delete(&self, path: &str, auth: Auth) -> Result<()> {
        Self::send(self.request(Method::DELETE, path, auth)?).await?;
        Ok(())
    }

    /// Uploads a file as the raw request body with `PUT`, streaming it from disk.
    pub async fn put_file<T: DeserializeOwned>(
        &self,
        path: &str,
        auth: Auth,
        file: &Path,
        content_type: &str,
    ) -> Result<T> {
        let f = tokio::fs::File::open(file)
            .await
            .with_context(|| format!("opening {}", file.display()))?;
        let len = f
            .metadata()
            .await
            .with_context(|| format!("reading {}", file.display()))?
            .len();
        let req = self
            .request(Method::PUT, path, auth)?
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(Body::from(f));
        let res = Self::send(req).await?;
        Self::json(res).await
    }
}

/// The `error` field of the server's JSON error body, or the raw text when it is not one.
fn error_message(body: &str) -> String {
    #[derive(serde::Deserialize)]
    struct ErrorBody {
        error: String,
    }
    match serde_json::from_str::<ErrorBody>(body) {
        Ok(b) => b.error,
        Err(_) => body.trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_message_prefers_the_error_field() {
        assert_eq!(error_message(r#"{"error":"not found"}"#), "not found");
        assert_eq!(error_message("plain text\n"), "plain text");
        assert_eq!(error_message(""), "");
    }

    #[test]
    fn base_url_loses_trailing_slash() {
        let c = Client::new("http://x:1///", None, None).unwrap();
        assert_eq!(c.base, "http://x:1");
    }

    #[test]
    fn missing_tokens_are_reported() {
        let c = Client::new("http://x", None, Some(String::new())).unwrap();
        let e = c.request(Method::GET, "/v1/me", Auth::Key).unwrap_err();
        assert!(e.to_string().contains("--key"), "{e}");
        let e = c.request(Method::GET, "/v1/me", Auth::Admin).unwrap_err();
        assert!(e.to_string().contains("--admin-token"), "{e}");
    }
}

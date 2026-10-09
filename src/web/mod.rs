//! `GET /`: the built-in web page, one self-contained HTML file. No key needed: the page asks
//! for one and sends it to the API itself.

use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};

const INDEX_HTML: &str = include_str!("index.html");

pub async fn index() -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        INDEX_HTML,
    )
        .into_response()
}

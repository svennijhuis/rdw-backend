//! Error types and Accept-header-aware response rendering.
//!
//! Rendering is split into a pure function (`render`) that returns a plain
//! struct, and a thin Axum conversion at the call site, so the rendering
//! logic itself can be unit tested without spinning up a server.

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Unauthorized,
    RateLimited {
        retry_after_secs: u64,
    },
    Busy,
    BadGateway(String),
    GatewayTimeout(String),
    /// The staged export is uncompressed-too-large for a client that did not
    /// send `Accept-Encoding: gzip`. Always rendered as plain text (see
    /// `render`), regardless of the request's `Accept` header, so a bare
    /// `curl` or script always gets an actionable, machine-parseable message.
    NotAcceptable(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::RateLimited { .. } | ApiError::Busy => StatusCode::TOO_MANY_REQUESTS,
            ApiError::BadGateway(_) => StatusCode::BAD_GATEWAY,
            ApiError::GatewayTimeout(_) => StatusCode::GATEWAY_TIMEOUT,
            ApiError::NotAcceptable(_) => StatusCode::NOT_ACCEPTABLE,
        }
    }

    pub fn message(&self) -> String {
        match self {
            ApiError::BadRequest(msg) => format!("Bad request: {msg}"),
            ApiError::Unauthorized => "Unauthorized: missing or invalid API key".to_string(),
            ApiError::RateLimited { .. } => "Rate limit exceeded".to_string(),
            ApiError::Busy => {
                "Another export is already in progress; try again shortly".to_string()
            }
            ApiError::BadGateway(msg) => format!("Upstream RDW error: {msg}"),
            ApiError::GatewayTimeout(msg) => format!("Upstream RDW timeout: {msg}"),
            ApiError::NotAcceptable(msg) => msg.clone(),
        }
    }

    fn retry_after_secs(&self) -> Option<u64> {
        match self {
            ApiError::RateLimited { retry_after_secs } => Some(*retry_after_secs),
            ApiError::Busy => Some(5),
            _ => None,
        }
    }
}

pub struct RenderedError {
    pub status: StatusCode,
    pub content_type: &'static str,
    pub body: String,
    pub retry_after_secs: Option<u64>,
}

/// Decide whether the client prefers an HTML error page. Any parse trouble
/// (missing header, unknown media type, malformed value) falls back to
/// plain text gracefully rather than erroring.
fn prefers_html(accept: Option<&str>) -> bool {
    let Some(value) = accept else {
        return false;
    };
    value
        .split(',')
        .any(|part| part.trim().to_ascii_lowercase().starts_with("text/html"))
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Render an `ApiError` as either a small HTML error page or plain text,
/// based on the request's `Accept` header.
pub fn render(err: &ApiError, accept: Option<&str>) -> RenderedError {
    let status = err.status();
    let message = err.message();
    let retry_after_secs = err.retry_after_secs();

    // A 406 must always be plain text with a copy-pasteable retry command,
    // even when the request's Accept header prefers HTML: a bare `curl`
    // (the exact client this error targets) never sends `Accept: text/html`,
    // but a browser navigating directly to the URL might, and an HTML page
    // here would hide the actionable message this error exists to deliver.
    if matches!(err, ApiError::NotAcceptable(_)) {
        return RenderedError {
            status,
            content_type: "text/plain; charset=utf-8",
            body: message,
            retry_after_secs,
        };
    }

    if prefers_html(accept) {
        let body = format!(
            "<!DOCTYPE html><html><head><title>{code}</title></head><body><h1>{code} {reason}</h1><p>{msg}</p></body></html>",
            code = status.as_u16(),
            reason = status.canonical_reason().unwrap_or(""),
            msg = escape_html(&message),
        );
        RenderedError {
            status,
            content_type: "text/html; charset=utf-8",
            body,
            retry_after_secs,
        }
    } else {
        RenderedError {
            status,
            content_type: "text/plain; charset=utf-8",
            body: message,
            retry_after_secs,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // No request Accept header available here; used only for paths
        // that build the error before headers are read. Handlers that
        // have already read the request prefer `render` directly.
        render_to_response(&self, None)
    }
}

pub fn render_to_response(err: &ApiError, accept: Option<&str>) -> Response {
    let rendered = render(err, accept);
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(rendered.content_type) {
        headers.insert(axum::http::header::CONTENT_TYPE, value);
    }
    if let Some(secs) = rendered.retry_after_secs {
        if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
            headers.insert(axum::http::header::RETRY_AFTER, value);
        }
    }
    (rendered.status, headers, rendered.body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_plain_text_accept_returns_plain_body() {
        let rendered = render(
            &ApiError::BadRequest("bad brand".to_string()),
            Some("text/plain"),
        );
        assert_eq!(rendered.status, StatusCode::BAD_REQUEST);
        assert_eq!(rendered.content_type, "text/plain; charset=utf-8");
        assert!(rendered.body.contains("bad brand"));
    }

    #[test]
    fn happy_path_html_accept_returns_html_body() {
        let rendered = render(
            &ApiError::BadRequest("bad brand".to_string()),
            Some("text/html"),
        );
        assert_eq!(rendered.content_type, "text/html; charset=utf-8");
        assert!(rendered.body.starts_with("<!DOCTYPE html>"));
        assert!(rendered.body.contains("bad brand"));
    }

    #[test]
    fn edge_missing_accept_header_defaults_to_plain_text() {
        let rendered = render(&ApiError::Unauthorized, None);
        assert_eq!(rendered.content_type, "text/plain; charset=utf-8");
    }

    #[test]
    fn edge_json_accept_is_not_supported_and_falls_back_to_plain_text() {
        let rendered = render(&ApiError::Unauthorized, Some("application/json"));
        assert_eq!(rendered.content_type, "text/plain; charset=utf-8");
    }

    #[test]
    fn failure_malformed_accept_header_falls_back_to_plain_text_gracefully() {
        let rendered = render(&ApiError::Unauthorized, Some(",,,not a media type;;;"));
        assert_eq!(rendered.content_type, "text/plain; charset=utf-8");
    }

    #[test]
    fn rate_limited_sets_retry_after_seconds() {
        let rendered = render(
            &ApiError::RateLimited {
                retry_after_secs: 42,
            },
            None,
        );
        assert_eq!(rendered.retry_after_secs, Some(42));
    }

    #[test]
    fn happy_path_not_acceptable_is_406_with_actionable_plain_text() {
        let rendered = render(
            &ApiError::NotAcceptable(
                "Staged data exceeds 50 MB; please retry with Accept-Encoding: gzip".to_string(),
            ),
            None,
        );
        assert_eq!(rendered.status, StatusCode::NOT_ACCEPTABLE);
        assert_eq!(rendered.content_type, "text/plain; charset=utf-8");
        assert!(rendered.body.contains("Accept-Encoding: gzip"));
    }

    #[test]
    fn edge_not_acceptable_stays_plain_text_even_when_html_is_preferred() {
        // The message must remain copy-pasteable for a CLI tool even if a
        // browser (which sends `Accept: text/html`) is the one that hits it.
        let rendered = render(
            &ApiError::NotAcceptable("please retry with Accept-Encoding: gzip".to_string()),
            Some("text/html"),
        );
        assert_eq!(rendered.content_type, "text/plain; charset=utf-8");
        assert!(!rendered.body.starts_with("<!DOCTYPE"));
    }

    #[test]
    fn html_message_is_escaped_against_reflected_input() {
        let rendered = render(
            &ApiError::BadRequest("<script>alert(1)</script>".to_string()),
            Some("text/html"),
        );
        assert!(!rendered.body.contains("<script>"));
        assert!(rendered.body.contains("&lt;script&gt;"));
    }
}

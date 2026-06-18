//! Request ID propagation middleware for the Tent of Trials backend.
//!
//! This module implements middleware that ensures every HTTP request has a
//! unique identifier that flows through both responses and log output, enabling
//! correlation across distributed services.
//!
//! # Behavior
//!
//! * If the inbound request carries an `X-Request-Id` header whose value is
//!   non-empty and shorter than 128 characters, that value is used verbatim.
//! * Otherwise a fresh UUID v4 is generated and used instead.
//! * The resolved request ID is:
//!   - Attached to the request via an extension so downstream handlers can read it.
//!   - Included in every response as the `X-Request-Id` header.
//!   - Recorded on the current tracing span so every log line emitted while
//!     processing the request is automatically tagged.

use axum::{
    extract::Request,
    middleware::Next,
    response::Response,
};
use tracing::Span;
use uuid::Uuid;

/// Maximum allowed length for a request ID value (in characters).
pub const MAX_REQUEST_ID_LEN: usize = 128;

/// The HTTP header name used for request ID propagation.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Type-safe wrapper stored in [`Request`] extensions so downstream handlers
/// can retrieve the resolved request ID without re-parsing headers.
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RequestId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<RequestId> for String {
    fn from(id: RequestId) -> String {
        id.0
    }
}

/// Returns `true` when `value` is a valid request ID: non-empty and shorter
/// than [`MAX_REQUEST_ID_LEN`] characters.
pub fn is_valid_request_id(value: &str) -> bool {
    !value.is_empty() && value.len() < MAX_REQUEST_ID_LEN
}

/// Resolve the request ID from an optional header value.
///
/// Returns a tuple of `(request_id, source)` where `source` is `"header"` or
/// `"generated"`, useful for structured logging.
fn resolve_request_id(header_value: Option<&str>) -> (String, &'static str) {
    match header_value {
        Some(v) if is_valid_request_id(v) => (v.to_string(), "header"),
        _ => (Uuid::new_v4().to_string(), "generated"),
    }
}

/// Axum middleware that propagates a request ID through every request/response
/// cycle and into the structured log output.
///
/// The middleware:
/// 1. Reads the `X-Request-Id` header from the inbound request.
/// 2. Validates it (non-empty, < 128 chars).
/// 3. Generates a UUID v4 when the header is missing or invalid.
/// 4. Stores the final ID in [`RequestId`] request extensions.
/// 5. Records the ID on the current tracing span for log correlation.
/// 6. Appends `X-Request-Id` to the outbound response.
pub async fn request_id_middleware(
    mut request: Request,
    next: Next,
) -> Response {
    let header_value = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok());

    let (request_id, source) = resolve_request_id(header_value);

    tracing::info!(
        request_id = %request_id,
        source = %source,
        "request received"
    );

    // Store in extensions so handlers / other middleware can read it.
    request.extensions_mut().insert(RequestId(request_id.clone()));

    // Attach to the current tracing span so every child log line inherits it.
    Span::current().record("request_id", request_id.as_str());

    let mut response = next.run(request).await;

    response.headers_mut().insert(
        REQUEST_ID_HEADER,
        request_id.parse().expect("request ID is valid header value"),
    );

    response
}

/// Build an [`axum::Router`] with the request ID middleware applied to all
/// routes. This is the recommended entry-point for wiring up HTTP endpoints.
pub fn build_router() -> axum::Router {
    use axum::routing::get;

    axum::Router::new()
        .route("/health", get(health_handler))
        .layer(axum::middleware::from_fn(request_id_middleware))
}

/// Minimal health endpoint that echoes the request ID. Downstream services and
/// load balancers can use this to verify end-to-end request ID propagation.
async fn health_handler(
    axum::extract::Extension(request_id): axum::extract::Extension<RequestId>,
) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "ok",
        "request_id": request_id.0,
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{self, HeaderValue, StatusCode};
    use axum::Router;
    use axum::routing::get;
    use tower::ServiceExt;

    /// Build a test router with the middleware applied and a simple handler
    /// that returns 200 OK with no body (the middleware still adds the header).
    fn test_router() -> Router {
        Router::new()
            .route("/", get(|| async { StatusCode::OK }))
            .layer(axum::middleware::from_fn(request_id_middleware))
    }

    #[test]
    fn test_valid_request_id_accepts_normal_values() {
        assert!(is_valid_request_id("abc-123"));
        assert!(is_valid_request_id("req_abc123"));
        assert!(is_valid_request_id(&"x".repeat(127)));
    }

    #[test]
    fn test_valid_request_id_rejects_empty_string() {
        assert!(!is_valid_request_id(""));
    }

    #[test]
    fn test_valid_request_id_rejects_overlong_values() {
        assert!(!is_valid_request_id(&"a".repeat(128)));
        assert!(!is_valid_request_id(&"a".repeat(256)));
    }

    #[test]
    fn test_resolve_request_id_with_valid_header() {
        let (id, source) = resolve_request_id(Some("my-custom-id"));
        assert_eq!(id, "my-custom-id");
        assert_eq!(source, "header");
    }

    #[test]
    fn test_resolve_request_id_with_missing_header() {
        let (id, source) = resolve_request_id(None);
        assert_eq!(source, "generated");
        // UUID v4 format: 8-4-4-4-12 = 36 chars
        assert_eq!(id.len(), 36);
        assert!(Uuid::parse_str(&id).is_ok(), "generated ID is a valid UUID");
    }

    #[test]
    fn test_resolve_request_id_with_empty_header() {
        let (id, source) = resolve_request_id(Some(""));
        assert_eq!(source, "generated");
        assert!(Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn test_resolve_request_id_with_overlong_header() {
        let long_value = "a".repeat(200);
        let (id, source) = resolve_request_id(Some(&long_value));
        assert_eq!(source, "generated");
        assert!(Uuid::parse_str(&id).is_ok());
    }

    #[tokio::test]
    async fn test_middleware_echoes_provided_request_id() {
        let app = test_router();

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header(REQUEST_ID_HEADER, "my-request-42")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let header = response
            .headers()
            .get(REQUEST_ID_HEADER)
            .expect("response must contain X-Request-Id");
        assert_eq!(header.to_str().unwrap(), "my-request-42");
    }

    #[tokio::test]
    async fn test_middleware_generates_id_when_header_missing() {
        let app = test_router();

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let header = response
            .headers()
            .get(REQUEST_ID_HEADER)
            .expect("response must contain X-Request-Id when header is missing");
        let value = header.to_str().unwrap();
        assert!(
            Uuid::parse_str(value).is_ok(),
            "generated request ID should be a valid UUID v4, got: {value}"
        );
    }

    #[tokio::test]
    async fn test_middleware_generates_id_for_empty_header() {
        let app = test_router();

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header(REQUEST_ID_HEADER, "")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let header = response
            .headers()
            .get(REQUEST_ID_HEADER)
            .expect("response must contain X-Request-Id for empty header");
        let value = header.to_str().unwrap();
        assert!(
            Uuid::parse_str(value).is_ok(),
            "empty header should trigger UUID generation, got: {value}"
        );
    }

    #[tokio::test]
    async fn test_middleware_generates_id_for_overlong_header() {
        let app = test_router();
        let long_id = "x".repeat(200);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header(REQUEST_ID_HEADER, long_id.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let header = response
            .headers()
            .get(REQUEST_ID_HEADER)
            .expect("response must contain X-Request-Id for overlong header");
        let value = header.to_str().unwrap();
        assert!(
            Uuid::parse_str(value).is_ok(),
            "overlong header should trigger UUID generation, got: {value}"
        );
    }

    #[tokio::test]
    async fn test_middleware_preserves_non_ascii_header_gracefully() {
        let app = test_router();

        // A header value with non-ASCII bytes is invalid HTTP and to_str()
        // will fail, so the middleware should generate a UUID instead.
        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header(
                        REQUEST_ID_HEADER,
                        HeaderValue::from_bytes(b"valid-id-99").unwrap(),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let header = response
            .headers()
            .get(REQUEST_ID_HEADER)
            .expect("response must contain X-Request-Id");
        assert_eq!(header.to_str().unwrap(), "valid-id-99");
    }

    #[tokio::test]
    async fn test_request_id_stored_in_extensions() {
        use axum::extract::Extension;

        let app = Router::new()
            .route(
                "/",
                get(|Extension(rid): Extension<RequestId>| async move {
                    rid.0
                }),
            )
            .layer(axum::middleware::from_fn(request_id_middleware));

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header(REQUEST_ID_HEADER, "ext-test-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body_bytes, "ext-test-id");
    }
}

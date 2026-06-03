//! Simple API key middleware for Phase 1-3.

use axum::{extract::Request, http::StatusCode, middleware::Next, response::Response};

pub async fn require_api_key(
    expected_key: String,
) -> impl Fn(Request, Next) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response, StatusCode>> + Send>>
    + Clone {
    move |req, next| {
        let expected = expected_key.clone();
        Box::pin(async move {
            let header = req
                .headers()
                .get("x-edge-api-key")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if header != expected {
                return Err(StatusCode::UNAUTHORIZED);
            }
            Ok(next.run(req).await)
        })
    }
}

use crate::AppState;
use axum::{
    body::Body,
    extract::State,
    http::{header::AUTHORIZATION, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

fn bearer_token_matches(header_value: &str, expected_token: &str) -> bool {
    let Some(provided_token) = header_value.strip_prefix("Bearer ") else {
        return false;
    };

    let provided = provided_token.as_bytes();
    let expected = expected_token.as_bytes();
    let mut difference = provided.len() ^ expected.len();
    let comparison_len = provided.len().max(expected.len());

    for index in 0..comparison_len {
        difference |= usize::from(
            provided.get(index).copied().unwrap_or(0) ^ expected.get(index).copied().unwrap_or(0),
        );
    }

    difference == 0
}

pub(crate) async fn require_authentication(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let Some(expected_token) = state.auth_token.as_deref() else {
        return next.run(request).await;
    };

    let authorized = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| bearer_token_matches(value, expected_token));

    if authorized {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, "Missing or invalid bearer token").into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::bearer_token_matches;

    #[test]
    fn bearer_tokens_require_the_exact_scheme_and_value() {
        assert!(bearer_token_matches("Bearer secret", "secret"));
        assert!(!bearer_token_matches("Bearer secrets", "secret"));
        assert!(!bearer_token_matches("bearer secret", "secret"));
    }
}

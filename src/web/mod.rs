use crate::proxy::ConfigHandle;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use parking_lot::Mutex;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

mod assets;
mod auth;
mod user;

#[derive(Clone)]
struct WebState {
    config: ConfigHandle,
    sessions: Arc<Mutex<HashMap<String, UserSession>>>,
}

#[derive(Clone)]
struct UserSession {
    api_key_id: String,
    expires_at: Instant,
}

#[derive(Debug)]
struct WebError {
    status: StatusCode,
    message: String,
}

impl WebError {
    fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "ok": false,
                "error": self.message,
            })),
        )
            .into_response()
    }
}

pub(crate) fn router(config: ConfigHandle) -> Router {
    let state = WebState {
        config,
        sessions: Arc::new(Mutex::new(HashMap::new())),
    };
    Router::new()
        .route("/", get(assets::index))
        .route("/assets/app.css", get(assets::css))
        .route("/assets/app.js", get(assets::javascript))
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route("/api/session", get(auth::session))
        .route("/api/dashboard", get(user::dashboard))
        .with_state(state)
}

fn session_ttl(config: &crate::config::AppConfig) -> Duration {
    Duration::from_secs(
        config
            .web
            .session_ttl_hours
            .clamp(1, 24 * 30)
            .saturating_mul(60 * 60),
    )
}

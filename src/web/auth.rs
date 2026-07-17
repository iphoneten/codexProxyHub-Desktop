use super::{session_ttl, UserSession, WebError, WebState};
use crate::{config::ApiKeyConfig, proxy};
use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Instant;
use uuid::Uuid;

const SESSION_COOKIE: &str = "routehub_user_session";

#[derive(Deserialize)]
pub(super) struct LoginRequest {
    api_key: String,
}

#[derive(Clone, Serialize)]
pub(super) struct UserProfile {
    #[serde(skip_serializing)]
    pub(super) api_key_id: String,
    pub(super) name: String,
    pub(super) max_concurrency: usize,
    pub(super) allowed_models: Vec<String>,
    pub(super) allowed_providers: Vec<String>,
}

pub(super) async fn login(
    State(state): State<WebState>,
    Json(payload): Json<LoginRequest>,
) -> Result<Response, WebError> {
    let cfg = state.config.read().clone();
    if !cfg.auth.enabled {
        return Err(WebError::unauthorized("API Key 鉴权未启用"));
    }
    let token = payload.api_key.trim();
    let key = cfg
        .auth
        .api_keys
        .iter()
        .find(|key| key.enabled && key.key == token)
        .ok_or_else(|| WebError::unauthorized("API Key 无效或已停用"))?;
    let profile = profile_for_key(&cfg, key);
    let session_token = Uuid::new_v4().to_string();
    let ttl = session_ttl(&cfg);
    state.sessions.lock().insert(
        session_token.clone(),
        UserSession {
            api_key_id: profile.api_key_id.clone(),
            expires_at: Instant::now() + ttl,
        },
    );

    let mut response = Json(json!({"ok": true, "user": profile})).into_response();
    let cookie = format!(
        "{SESSION_COOKIE}={session_token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        ttl.as_secs()
    );
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|_| WebError::internal("创建登录会话失败"))?,
    );
    Ok(response)
}

pub(super) async fn logout(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    if let Some(token) = session_token(&headers) {
        state.sessions.lock().remove(token);
    }
    let mut response = Json(json!({"ok": true})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "routehub_user_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
        ),
    );
    Ok(response)
}

pub(super) async fn session(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, WebError> {
    let profile = authorized_user(&state, &headers)?;
    Ok(Json(json!({"ok": true, "user": profile})))
}

pub(super) fn authorized_user(
    state: &WebState,
    headers: &HeaderMap,
) -> Result<UserProfile, WebError> {
    let token =
        session_token(headers).ok_or_else(|| WebError::unauthorized("请先使用 API Key 登录"))?;
    let api_key_id = {
        let mut sessions = state.sessions.lock();
        sessions.retain(|_, session| session.expires_at > Instant::now());
        sessions
            .get(token)
            .map(|session| session.api_key_id.clone())
            .ok_or_else(|| WebError::unauthorized("登录会话已过期"))?
    };
    let cfg = state.config.read().clone();
    if !cfg.auth.enabled {
        return Err(WebError::unauthorized("API Key 鉴权未启用"));
    }
    let key = cfg
        .auth
        .api_keys
        .iter()
        .find(|key| key.enabled && proxy::api_key_id(&key.key) == api_key_id)
        .ok_or_else(|| WebError::unauthorized("API Key 已停用或删除"))?;
    Ok(profile_for_key(&cfg, key))
}

fn profile_for_key(config: &crate::config::AppConfig, key: &ApiKeyConfig) -> UserProfile {
    UserProfile {
        api_key_id: proxy::api_key_id(&key.key),
        name: key.name.clone(),
        max_concurrency: key
            .max_concurrency
            .or(config.auth.max_concurrency_per_key)
            .unwrap_or(5),
        allowed_models: key.allowed_models.clone(),
        allowed_providers: key.allowed_providers.clone(),
    }
}

fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|cookie| cookie.strip_prefix(&format!("{SESSION_COOKIE}=")))
        .filter(|value| !value.is_empty())
}

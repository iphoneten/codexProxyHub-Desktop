use super::*;

pub(super) fn authorize_and_acquire(
    state: &AppState,
    config: &AppConfig,
    headers: &HeaderMap,
) -> Result<AuthAccess, ProxyError> {
    if !config.auth.enabled {
        return Ok(AuthAccess {
            permit: state.acquire_api_key_permit("__auth_disabled__", 1_000_000)?,
            key_name: String::new(),
            allowed_models: Vec::new(),
            allowed_providers: Vec::new(),
        });
    }
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or_default();

    if let Some(key) = config
        .auth
        .api_keys
        .iter()
        .find(|k| k.enabled && k.key == token)
    {
        let limit = key
            .max_concurrency
            .or(config.auth.max_concurrency_per_key)
            .unwrap_or(5);
        Ok(AuthAccess {
            permit: state.acquire_api_key_permit(token, limit)?,
            key_name: key.name.clone(),
            allowed_models: key.allowed_models.clone(),
            allowed_providers: key.allowed_providers.clone(),
        })
    } else {
        Err(ProxyError::new(
            StatusCode::UNAUTHORIZED,
            "无效或缺失 API Key",
        ))
    }
}

pub(super) fn ensure_api_key_allows_model(
    auth: &AuthAccess,
    model: &str,
) -> Result<(), ProxyError> {
    if api_key_allows_model(auth, model) {
        return Ok(());
    }
    Err(ProxyError::new(
        StatusCode::FORBIDDEN,
        format!("API Key 不允许访问模型 '{model}'"),
    ))
}

pub(super) fn api_key_allows_model(auth: &AuthAccess, model: &str) -> bool {
    auth.allowed_models.is_empty()
        || auth.allowed_models.iter().any(|allowed| {
            allowed.trim() == "*"
                || normalize_model(allowed.trim()) == normalize_model(model.trim())
        })
}

pub(super) fn api_key_allows_provider(allowed_providers: &[String], provider_name: &str) -> bool {
    allowed_providers.is_empty()
        || allowed_providers
            .iter()
            .any(|allowed| allowed.trim() == "*" || allowed.trim() == provider_name.trim())
}

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
            daily_token_limit: None,
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
            daily_token_limit: key.daily_token_limit,
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

pub(super) fn enforce_daily_token_limit(
    config: &AppConfig,
    auth: &AuthAccess,
    api_key_id: &str,
) -> Result<(), ProxyError> {
    let Some(limit) = auth.daily_token_limit else {
        return Ok(());
    };
    if limit == 0 {
        return Ok(());
    }
    let used = crate::proxy::usage_log::read_api_key_today_tokens(
        config.usage_log_sqlite_path(),
        api_key_id,
    )
    .map_err(|err| ProxyError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    if used as u64 >= limit {
        return Err(ProxyError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!("API Key 今日 Token 用量已达上限 ({used}/{limit})"),
        ));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> AppState {
        AppState {
            config: Arc::new(RwLock::new(Arc::new(
                AppConfig::load("config.example.yaml").unwrap(),
            ))),
            clients: Arc::new(Mutex::new(HashMap::new())),
            counters: Arc::new(Mutex::new(HashMap::new())),
            keepalive_headers: Arc::new(Mutex::new(HashMap::new())),
            api_key_limiters: Arc::new(Mutex::new(HashMap::new())),
            provider_circuits: Arc::new(Mutex::new(HashMap::new())),
            provider_loads: Arc::new(Mutex::new(HashMap::new())),
            provider_statuses: Arc::new(RwLock::new(HashMap::new())),
            usage_injection: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[test]
    fn api_key_daily_token_limit_rejects_when_today_usage_reaches_limit() {
        let path = std::env::temp_dir().join(format!(
            "routehub-daily-limit-{}.sqlite3",
            Uuid::new_v4().simple()
        ));
        let cfg: AppConfig = serde_yaml::from_str(&format!(
            r#"
usage_log:
  sqlite_path: {}
providers: []
"#,
            path.display()
        ))
        .unwrap();
        let token = "sk-daily-limit";
        let key_id = api_key_id(token);
        usage_log::log_usage_sqlite_for_key(
            path.clone(),
            &chrono::Local::now().format("%Y-%m-%d 12:00:00").to_string(),
            "responses",
            &key_id,
            "daily",
            "ok",
            "test",
            "gpt-test",
            "gpt-test",
            100,
            None,
            "",
            6,
            4,
            "upstream_or_unknown",
        )
        .unwrap();
        let state = test_state();
        let auth = AuthAccess {
            permit: state.acquire_api_key_permit(token, 5).unwrap(),
            key_name: "daily".to_string(),
            daily_token_limit: Some(10),
            allowed_models: Vec::new(),
            allowed_providers: Vec::new(),
        };

        let err = enforce_daily_token_limit(&cfg, &auth, &key_id).unwrap_err();
        assert_eq!(err.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(err.message.contains("今日 Token 用量已达上限"));

        let _ = std::fs::remove_file(path);
    }
}

use super::*;

pub(super) async fn keepalive_loop(
    state: AppState,
    status: KeepaliveStatusHandle,
    mut stop: oneshot::Receiver<()>,
) {
    let mut last_sent: HashMap<String, Instant> = HashMap::new();
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }

        let config = state.snapshot();
        let mut active = HashSet::new();
        for provider in config
            .providers
            .iter()
            .filter(|provider| provider.enabled && provider.persist_keepalive)
        {
            active.insert(provider.name.clone());
            let interval = provider.persist_keepalive_interval.max(5);
            if last_sent
                .get(&provider.name)
                .is_some_and(|sent| sent.elapsed() < Duration::from_secs(interval))
            {
                continue;
            }

            last_sent.insert(provider.name.clone(), Instant::now());
            let state = state.clone();
            let provider = provider.clone();
            let status = Arc::clone(&status);
            tokio::spawn(async move {
                let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                {
                    let mut statuses = status.write();
                    let item = statuses.entry(provider.name.clone()).or_default();
                    item.last_attempt = Some(now);
                }
                let result = send_provider_keepalive(&state, &provider).await;
                let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                let mut statuses = status.write();
                let item = statuses.entry(provider.name.clone()).or_default();
                match result {
                    Ok(()) => {
                        item.last_success = Some(now);
                        item.last_error = None;
                    }
                    Err(err) => {
                        item.last_error = Some(err.message);
                    }
                }
            });
        }
        last_sent.retain(|name, _| active.contains(name));
    }
}

pub(super) async fn send_provider_keepalive(
    state: &AppState,
    provider: &ProviderConfig,
) -> Result<(), ProxyError> {
    let model = provider
        .persist_keepalive_model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .or_else(|| provider.models.first().map(String::as_str))
        .ok_or_else(|| ProxyError::new(StatusCode::BAD_REQUEST, "保活渠道未配置模型"))?
        .to_string();
    let prompt = if provider.persist_keepalive_prompt.trim().is_empty() {
        "Hi"
    } else {
        provider.persist_keepalive_prompt.trim()
    };
    let (path, body) = build_keepalive_request(provider, &model, prompt)?;
    let request_headers = state
        .cached_keepalive_headers(provider)
        .filter(has_codex_session_headers)
        .or_else(|| {
            if provider_keepalive_requires_client_headers(provider) {
                None
            } else {
                Some(keepalive_request_headers())
            }
        })
        .ok_or_else(|| {
            ProxyError::new(
                StatusCode::BAD_REQUEST,
                "等待真实 Codex 请求头，先通过该渠道完成一次请求后会自动保活",
            )
        })?;

    let client = state.client_for_provider(provider);
    let _ = if is_google_native_provider(provider) {
        send_json_to_provider_with_headers(
            &client,
            provider,
            &path,
            google_ai_headers(provider, &request_headers, false),
            body,
        )
        .await?
    } else {
        send_json_to_provider(&client, provider, &path, &request_headers, body).await?
    };
    Ok(())
}

pub(super) fn build_keepalive_request(
    provider: &ProviderConfig,
    model: &str,
    prompt: &str,
) -> Result<(String, Value), ProxyError> {
    if provider.provider_type == "anthropic" {
        let mut body = json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": 1,
            "stream": false
        });
        apply_model_mapping(provider, &mut body);
        let body = crate::anthropic::openai_to_anthropic_request(&body).map_err(|err| {
            ProxyError::new(StatusCode::BAD_REQUEST, format!("保活请求翻译失败: {err}"))
        })?;
        return Ok(("/messages".to_string(), body));
    }

    if is_google_native_provider(provider) {
        let mut body = json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": 1,
            "stream": false
        });
        apply_model_mapping(provider, &mut body);
        let request_model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(model)
            .to_string();
        let body = crate::google_ai::openai_to_google_request(&body).map_err(|err| {
            ProxyError::new(StatusCode::BAD_REQUEST, format!("保活请求翻译失败: {err}"))
        })?;
        return Ok((
            crate::google_ai::generate_content_path(&request_model, false),
            body,
        ));
    }

    if provider_keepalive_prefers_responses(provider) {
        let chat_body = json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": 1,
            "stream": false
        });
        let mut body =
            crate::responses_api::chat_to_responses_request(&chat_body).map_err(|err| {
                ProxyError::new(StatusCode::BAD_REQUEST, format!("保活请求翻译失败: {err}"))
            })?;
        apply_model_mapping(provider, &mut body);
        return Ok(("/responses".to_string(), body));
    }

    let mut body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 1,
        "stream": false
    });
    apply_model_mapping(provider, &mut body);
    Ok(("/chat/completions".to_string(), body))
}

pub(super) fn keepalive_request_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    let request_id = format!("req_{}", Uuid::new_v4().simple());
    let session_id = Uuid::new_v4().to_string();
    insert_header(&mut headers, "x-request-id", &request_id);
    insert_header(&mut headers, "session_id", &session_id);
    insert_header(&mut headers, "conversation_id", &session_id);
    insert_header(&mut headers, "openai-client", "codex_cli_rs");
    insert_header(&mut headers, "x-stainless-runtime", "rust");
    insert_header(&mut headers, "x-codex-keepalive", "true");
    headers
}

pub(super) fn provider_keepalive_requires_client_headers(provider: &ProviderConfig) -> bool {
    provider_keepalive_prefers_responses(provider)
        && (provider.provider_type == "codex_only"
            || provider
                .extra_headers
                .get("originator")
                .is_some_and(|value| value.to_ascii_lowercase().contains("codex"))
            || provider
                .models
                .iter()
                .any(|model| model.to_ascii_lowercase().contains("codex")))
}

pub(super) fn has_codex_session_headers(headers: &HeaderMap) -> bool {
    headers.contains_key("session_id") || headers.contains_key("conversation_id")
}

pub(super) fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        http::header::HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        headers.insert(name, value);
    }
}

pub(super) fn provider_keepalive_prefers_responses(provider: &ProviderConfig) -> bool {
    let supports_responses =
        !matches!(provider.capabilities.get("supports_responses"), Some(false));
    let supports_chat = !matches!(provider.capabilities.get("supports_chat"), Some(false));

    provider.provider_type == "codex_only"
        || provider.responses_mode == "native"
        || (provider.responses_mode == "auto" && supports_responses && !supports_chat)
}

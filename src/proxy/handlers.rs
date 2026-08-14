use super::*;

pub(super) async fn index(State(_state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "name": "RouteHub",
        "ok": true,
        "endpoints": [
            "/health",
            "/v1/models",
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/embeddings",
            "/v1/responses",
            "/v1/messages",
            "/v1/messages/count_tokens"
        ]
    }))
}

pub(super) async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let cfg = state.snapshot();
    Json(json!({
        "ok": true,
        "providers": cfg.providers.iter().filter(|p| p.enabled).count(),
        "models": collect_models(&cfg, &[]).len(),
    }))
}

pub(super) async fn list_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    let data: Vec<Value> = collect_models(&cfg, &auth.allowed_providers)
        .into_iter()
        .filter(|model| api_key_allows_model(&auth, model))
        .map(|id| json!({"id": id, "object": "model", "created": 0, "owned_by": "route-hub"}))
        .collect();
    Ok(Json(json!({"object": "list", "data": data})))
}

pub(super) async fn get_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(model): Path<String>,
) -> Result<impl IntoResponse, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    if collect_models(&cfg, &auth.allowed_providers).contains(&model) {
        ensure_api_key_allows_model(&auth, &model)?;
        Ok(Json(
            json!({"id": model, "object": "model", "created": 0, "owned_by": "route-hub"}),
        ))
    } else {
        Err(ProxyError::new(StatusCode::NOT_FOUND, "模型不存在"))
    }
}

pub(super) async fn chat_completions(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    let api_key_id = request_api_key_id(&cfg, &headers);
    let model = body_model(&body)?;
    ensure_api_key_allows_model(&auth, &model)?;
    enforce_daily_token_limit(&cfg, &auth, &api_key_id)?;
    forward_openai(
        state,
        ctx,
        headers,
        body,
        "/chat/completions",
        "chat",
        Some(auth.permit),
        api_key_id,
        auth.key_name,
        auth.allowed_providers,
    )
    .await
}

pub(super) async fn completions(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    let api_key_id = request_api_key_id(&cfg, &headers);
    let model = body_model(&body)?;
    ensure_api_key_allows_model(&auth, &model)?;
    enforce_daily_token_limit(&cfg, &auth, &api_key_id)?;
    forward_openai(
        state,
        ctx,
        headers,
        body,
        "/completions",
        "completion",
        Some(auth.permit),
        api_key_id,
        auth.key_name,
        auth.allowed_providers,
    )
    .await
}

pub(super) async fn embeddings(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    let api_key_id = request_api_key_id(&cfg, &headers);
    let model = body_model(&body)?;
    ensure_api_key_allows_model(&auth, &model)?;
    enforce_daily_token_limit(&cfg, &auth, &api_key_id)?;
    forward_openai(
        state,
        ctx,
        headers,
        body,
        "/embeddings",
        "embedding",
        Some(auth.permit),
        api_key_id,
        auth.key_name,
        auth.allowed_providers,
    )
    .await
}

/// 入站 Anthropic Messages API。请求体原样直通到 `provider_type: anthropic` 渠道，
/// 复用统一的鉴权、模型/Token 限额、渠道选择、failover、熔断与用量日志。
///
/// 错误响应用 Anthropic 的 `{"type":"error","error":{...}}` 形状，
/// 否则 Anthropic SDK 解析不出错误信息，只会抛一个无上下文的异常。
pub(super) async fn messages(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    match messages_inner(state, ctx, headers, body).await {
        Ok(response) => response,
        Err(err) => anthropic_error_response(err),
    }
}

async fn messages_inner(
    state: AppState,
    ctx: RequestContext,
    headers: HeaderMap,
    body: Value,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    let api_key_id = request_api_key_id(&cfg, &headers);
    let model = body_model(&body)?;
    ensure_api_key_allows_model(&auth, &model)?;
    enforce_daily_token_limit(&cfg, &auth, &api_key_id)?;
    forward_openai(
        state,
        ctx,
        headers,
        body,
        "/messages",
        "messages",
        Some(auth.permit),
        api_key_id,
        auth.key_name,
        auth.allowed_providers,
    )
    .await
}

/// 入站 `POST /v1/messages/count_tokens`。Claude Code 在发消息前会先调它估算上下文大小。
/// 纯只读估算，不产生 token 消耗，因此不写用量日志、不计入每日限额，
/// 但仍然要过鉴权和模型白名单。
pub(super) async fn count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    match count_tokens_inner(state, headers, body).await {
        Ok(response) => response,
        Err(err) => anthropic_error_response(err),
    }
}

async fn count_tokens_inner(
    state: AppState,
    headers: HeaderMap,
    body: Value,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    let model = body_model(&body)?;
    ensure_api_key_allows_model(&auth, &model)?;

    let providers = provider_attempts(&cfg, &state, &model, "messages", &auth.allowed_providers);
    let mut last_error = None;
    for (provider, request_model) in providers {
        let provider = match state.provider_with_fresh_oauth(&provider).await {
            Ok(provider) => provider,
            Err(err) => {
                last_error = Some(err);
                continue;
            }
        };
        let mut upstream_body = with_model(body.clone(), &request_model);
        apply_model_mapping(&provider, &mut upstream_body);
        let client = state.client_for_provider(&provider);
        match send_json_to_provider(
            &client,
            &provider,
            "/messages/count_tokens",
            &headers,
            upstream_body,
        )
        .await
        {
            Ok(value) => return Ok((StatusCode::OK, Json(value)).into_response()),
            Err(err) => {
                if should_stop_failover(err.status) {
                    return Err(err);
                }
                last_error = Some(err);
            }
        }
    }

    if let Some(err) = last_error {
        return Err(ProxyError::new(
            final_failover_status(err.status),
            err.message,
        ));
    }
    Err(ProxyError::new(
        StatusCode::BAD_GATEWAY,
        format!("模型 '{}' 没有可用的 Anthropic 渠道", model),
    ))
}

/// Anthropic 客户端只认 `{"type":"error","error":{"type","message"}}`。
/// 这里把内部 ProxyError 的 HTTP 状态映射到 Anthropic 的 error.type 取值。
pub(super) fn anthropic_error_response(err: ProxyError) -> Response {
    let error_type = match err.status {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::FORBIDDEN => "permission_error",
        StatusCode::NOT_FOUND => "not_found_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        StatusCode::BAD_REQUEST => "invalid_request_error",
        StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT => "overloaded_error",
        _ => "api_error",
    };
    (
        err.status,
        Json(json!({
            "type": "error",
            "error": {
                "type": error_type,
                "message": err.message,
            }
        })),
    )
        .into_response()
}

pub(super) async fn responses(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    let api_key_id = request_api_key_id(&cfg, &headers);
    let model = body_model(&body)?;
    ensure_api_key_allows_model(&auth, &model)?;
    enforce_daily_token_limit(&cfg, &auth, &api_key_id)?;
    let api_key_name = auth.key_name;
    let allowed_providers = auth.allowed_providers;
    let mut permit = Some(auth.permit);
    let affinity_key = api_key_affinity_key(&model, &api_key_id);
    let mut providers = provider_attempts(&cfg, &state, &model, "responses", &allowed_providers);
    if let Some((preferred, preferred_model)) = state.preferred_provider_for_affinity(&affinity_key)
    {
        prefer_provider(&mut providers, &preferred, &preferred_model);
    }
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut last_error = None;

    for (provider, request_model) in providers {
        let started = Instant::now();
        let provider = match state.provider_with_fresh_oauth(&provider).await {
            Ok(provider) => provider,
            Err(err) => {
                last_error = Some(AttemptFailure::new(&provider, &request_model, started, err));
                continue;
            }
        };
        let circuit_guard =
            match begin_attempt_or_record_failure(&state, &provider, &request_model, started) {
                Ok(guard) => guard,
                Err(failure) => {
                    last_error = Some(failure);
                    continue;
                }
            };
        let upstream_body = upstream_body_for_provider(&provider, &body, &request_model);
        let upstream_attempt_model = upstream_body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&request_model);
        let mut early_log_id = start_stream_attempt_log_for_key(
            &cfg,
            stream,
            "responses",
            &provider.name,
            &model,
            upstream_attempt_model,
            started,
            &api_key_id,
            &api_key_name,
            ctx.request_id(),
        );

        // force_chat：判断本次 /responses 请求是否**从一开始就走 chat 翻译**。
        //
        // 之前这里还多加了一条 `supports_responses == Some(false) → force_chat`，
        // 结果对像 rawchat 这类 "配置写着 supports_responses:false，但实际 /responses
        // 端点才是活的、/chat/completions 反而是死链路" 的上游，直接被打进了死路径。
        //
        // 现在与 codeProxyHub 的 `_provider_prefers_chat_responses` 规则对齐：
        //   1. responses_mode == "chat"：显式指定走 chat（用户强制）
        //   2. provider_type == "anthropic"：Anthropic 上游走 /messages（chat 翻译层再桥接）
        //   3. Google Gemini 的 OpenAI 端点：只支持 chat，不支持 /responses
        // 其它一律先尝试 native /responses；失败时下面的 auto-fallback 分支会自动降级 chat。
        let force_chat = provider_prefers_chat_responses(&provider);
        if force_chat {
            match forward_responses_as_chat(
                &state,
                &provider,
                &headers,
                upstream_body,
                stream,
                started,
                &ctx,
            )
            .await
            {
                Ok(result) => {
                    return Ok(commit_result(
                        state.snapshot(),
                        state,
                        "responses",
                        &provider.name,
                        &model,
                        &affinity_key,
                        &request_model,
                        result,
                        started,
                        stream,
                        early_log_id.take(),
                        permit.take(),
                        &api_key_id,
                        &api_key_name,
                        &ctx,
                        Some(circuit_guard),
                    ));
                }
                Err(err) => {
                    record_attempt_failure(circuit_guard, &err);
                    finish_failed_attempt_log_for_key(
                        &cfg,
                        early_log_id.take(),
                        "responses",
                        &provider.name,
                        &model,
                        started,
                        &err.message,
                        &api_key_id,
                        &api_key_name,
                        ctx.request_id(),
                    );
                    last_error = Some(AttemptFailure::new(&provider, &request_model, started, err));
                }
            }
            continue;
        }

        match send_to_provider(
            &state,
            &provider,
            "/responses",
            &headers,
            upstream_body.clone(),
            stream,
            started,
            &ctx,
        )
        .await
        {
            Ok(result) => {
                return Ok(commit_result(
                    state.snapshot(),
                    state,
                    "responses",
                    &provider.name,
                    &model,
                    &affinity_key,
                    &request_model,
                    result,
                    started,
                    stream,
                    early_log_id.take(),
                    permit.take(),
                    &api_key_id,
                    &api_key_name,
                    &ctx,
                    Some(circuit_guard),
                ));
            }
            Err(err)
                if provider.responses_mode == "auto"
                    && (err.status == StatusCode::NOT_FOUND
                        || err.status == StatusCode::METHOD_NOT_ALLOWED) =>
            {
                let chat_body = upstream_body;
                match forward_responses_as_chat(
                    &state, &provider, &headers, chat_body, stream, started, &ctx,
                )
                .await
                {
                    Ok(result) => {
                        return Ok(commit_result(
                            state.snapshot(),
                            state,
                            "responses",
                            &provider.name,
                            &model,
                            &affinity_key,
                            &request_model,
                            result,
                            started,
                            stream,
                            early_log_id.take(),
                            permit.take(),
                            &api_key_id,
                            &api_key_name,
                            &ctx,
                            Some(circuit_guard),
                        ));
                    }
                    Err(chat_err) => {
                        record_attempt_failure(circuit_guard, &chat_err);
                        finish_failed_attempt_log_for_key(
                            &cfg,
                            early_log_id.take(),
                            "responses",
                            &provider.name,
                            &model,
                            started,
                            &chat_err.message,
                            &api_key_id,
                            &api_key_name,
                            ctx.request_id(),
                        );
                        last_error = Some(AttemptFailure::new(
                            &provider,
                            &request_model,
                            started,
                            chat_err,
                        ));
                    }
                }
            }
            Err(err) => {
                record_attempt_failure(circuit_guard, &err);
                let failure = AttemptFailure::new(&provider, &request_model, started, err);
                // 与 forward_openai 保持一致：只有客户端鉴权错误立即中止，其它 4xx/5xx 继续尝试下个渠道
                if should_stop_failover(failure.status) {
                    finish_failed_attempt_log_for_key(
                        &cfg,
                        early_log_id.take(),
                        "responses",
                        &failure.provider,
                        &model,
                        failure.started,
                        &failure.message,
                        &api_key_id,
                        &api_key_name,
                        ctx.request_id(),
                    );
                    if !stream {
                        log_error(
                            &cfg,
                            "responses",
                            &failure.provider,
                            &failure.request_model,
                            failure.started,
                            None,
                            &failure.message,
                            &api_key_id,
                            &api_key_name,
                            ctx.request_id(),
                        );
                    }
                    return Err(ProxyError::new(failure.status, failure.message));
                }
                finish_failed_attempt_log_for_key(
                    &cfg,
                    early_log_id.take(),
                    "responses",
                    &failure.provider,
                    &model,
                    failure.started,
                    &failure.message,
                    &api_key_id,
                    &api_key_name,
                    ctx.request_id(),
                );
                last_error = Some(failure);
            }
        }
    }

    if let Some(failure) = last_error {
        if !stream {
            log_error(
                &cfg,
                "responses",
                &failure.provider,
                &failure.request_model,
                failure.started,
                None,
                &failure.message,
                &api_key_id,
                &api_key_name,
                ctx.request_id(),
            );
        }
        return Err(ProxyError::new(
            final_failover_status(failure.status),
            failure.message,
        ));
    }

    Err(ProxyError::new(
        StatusCode::BAD_GATEWAY,
        format!("模型 '{}' 没有可用渠道", model),
    ))
}

pub(super) fn provider_prefers_chat_responses(provider: &ProviderConfig) -> bool {
    provider.responses_mode == "chat"
        || provider.provider_type == "anthropic"
        || is_google_native_provider(provider)
        || is_google_openai_endpoint(&provider.base_url)
}

pub(super) fn api_key_affinity_key(model: &str, api_key_id: &str) -> String {
    if api_key_id.trim().is_empty() {
        return String::new();
    }
    format!(
        "api-key:{}:{}",
        api_key_id.trim(),
        normalize_model(model).to_ascii_lowercase()
    )
}

pub(super) fn prefer_provider(
    providers: &mut Vec<(ProviderConfig, String)>,
    preferred: &str,
    preferred_model: &str,
) {
    let Some(index) = providers.iter().position(|(provider, model)| {
        provider.name == preferred && normalize_model(model) == normalize_model(preferred_model)
    }) else {
        return;
    };
    let request_model = providers[index].1.clone();
    let item = providers.remove(index);
    let insert_at = providers
        .iter()
        .position(|(_, model)| normalize_model(model) == normalize_model(&request_model))
        .unwrap_or(providers.len());
    providers.insert(insert_at, item);
}

pub(super) fn final_failover_status(status: StatusCode) -> StatusCode {
    match status {
        StatusCode::TOO_MANY_REQUESTS
        | StatusCode::REQUEST_TIMEOUT
        | StatusCode::SERVICE_UNAVAILABLE
        | StatusCode::GATEWAY_TIMEOUT => status,
        _ => StatusCode::BAD_GATEWAY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_with_name(name: &str) -> ProviderConfig {
        serde_json::from_value(json!({
            "name": name,
            "provider_type": "openai",
            "base_url": "https://example.test/v1",
            "api_key": "sk-test",
            "models": ["gpt-test"],
            "responses_mode": "auto",
            "capabilities": {"supports_chat": true, "supports_responses": true},
        }))
        .unwrap()
    }

    #[test]
    fn api_key_affinity_key_uses_api_key_and_normalized_model() {
        assert_eq!(
            api_key_affinity_key("GPT-TEST", "key-1"),
            "api-key:key-1:gpt-test"
        );
    }

    #[test]
    fn api_key_affinity_key_ignores_empty_api_key_identity() {
        assert_eq!(api_key_affinity_key("GPT-TEST", ""), "");
    }

    #[test]
    fn prefer_provider_moves_affinity_provider_to_front() {
        let mut providers = vec![
            (provider_with_name("anyrouter"), "gpt-test".to_string()),
            (provider_with_name("muyuan"), "gpt-test".to_string()),
            (provider_with_name("rawchat"), "gpt-test".to_string()),
        ];

        prefer_provider(&mut providers, "muyuan", "gpt-test");

        assert_eq!(providers[0].0.name, "muyuan");
        assert_eq!(providers[1].0.name, "anyrouter");
    }

    #[test]
    fn prefer_provider_moves_sticky_channel_ahead_of_auth_accounts() {
        let mut auth = provider_with_name("auth:acct");
        auth.auth_account_id = Some("acct".to_string());
        let channel = provider_with_name("channel-a");
        let mut providers = vec![
            (auth, "gpt-test".to_string()),
            (channel, "gpt-test".to_string()),
        ];

        prefer_provider(&mut providers, "channel-a", "gpt-test");

        assert_eq!(providers[0].0.name, "channel-a");
        assert_eq!(providers[1].0.name, "auth:acct");
        assert!(providers[1].0.auth_account_id.is_some());
    }

    #[test]
    fn prefer_provider_keeps_sticky_auth_account_first_in_auth_group() {
        let mut first = provider_with_name("auth:acct-a");
        first.auth_account_id = Some("acct-a".to_string());
        let mut sticky = provider_with_name("auth:acct-b");
        sticky.auth_account_id = Some("acct-b".to_string());
        let channel = provider_with_name("channel-a");
        let mut providers = vec![
            (first, "gpt-test".to_string()),
            (sticky, "gpt-test".to_string()),
            (channel, "gpt-test".to_string()),
        ];

        prefer_provider(&mut providers, "auth:acct-b", "gpt-test");

        assert_eq!(providers[0].0.name, "auth:acct-b");
        assert_eq!(providers[1].0.name, "auth:acct-a");
        assert_eq!(providers[2].0.name, "channel-a");
    }

    #[test]
    fn prefer_provider_does_not_move_fallback_model_ahead_of_requested_model() {
        let mut providers = vec![
            (provider_with_name("primary-a"), "gpt-primary".to_string()),
            (provider_with_name("primary-b"), "gpt-primary".to_string()),
            (provider_with_name("fallback-a"), "gpt-fallback".to_string()),
            (provider_with_name("sticky"), "gpt-fallback".to_string()),
        ];

        prefer_provider(&mut providers, "sticky", "gpt-fallback");

        assert_eq!(providers[0].0.name, "primary-a");
        assert_eq!(providers[1].0.name, "primary-b");
        assert_eq!(providers[2].0.name, "sticky");
        assert_eq!(providers[2].1, "gpt-fallback");
    }

    #[test]
    fn upstream_body_for_provider_applies_model_mapping() {
        let mut provider = provider_with_name("mapped");
        provider
            .model_mapping
            .insert("gpt-test".to_string(), "upstream-gpt-test".to_string());
        let body = json!({"model": "original", "input": "hi"});

        let out = upstream_body_for_provider(&provider, &body, "gpt-test");

        assert_eq!(out["model"], "upstream-gpt-test");
        assert_eq!(out["input"], "hi");
    }

    #[test]
    fn upstream_body_for_provider_moves_chat_tool_id_out_of_responses_id() {
        let provider = provider_with_name("codex");
        let body = json!({
            "model": "original",
            "input": [
                {
                    "type": "function_call",
                    "id": "call_VgHo3IaCCEbCjzv9f2eUluB9",
                    "name": "shell",
                    "arguments": "{}"
                }
            ]
        });

        let out = upstream_body_for_provider(&provider, &body, "gpt-test");
        let item = &out["input"][0];

        assert!(item.get("id").is_none());
        assert_eq!(item["call_id"], "call_VgHo3IaCCEbCjzv9f2eUluB9");
    }
}

pub(super) async fn forward_openai(
    state: AppState,
    ctx: RequestContext,
    headers: HeaderMap,
    body: Value,
    path: &'static str,
    api: &'static str,
    mut permit: Option<OwnedSemaphorePermit>,
    api_key_id: String,
    api_key_name: String,
    allowed_providers: Vec<String>,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let model = body_model(&body)?;
    let affinity_key = api_key_affinity_key(&model, &api_key_id);
    let mut providers = provider_attempts(&cfg, &state, &model, api, &allowed_providers);
    if let Some((preferred, preferred_model)) = state.preferred_provider_for_affinity(&affinity_key)
    {
        prefer_provider(&mut providers, &preferred, &preferred_model);
    }
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut last_error = None;

    for (provider, request_model) in providers {
        let started = Instant::now();
        let provider = match state.provider_with_fresh_oauth(&provider).await {
            Ok(provider) => provider,
            Err(err) => {
                last_error = Some(AttemptFailure::new(&provider, &request_model, started, err));
                continue;
            }
        };
        let circuit_guard =
            match begin_attempt_or_record_failure(&state, &provider, &request_model, started) {
                Ok(guard) => guard,
                Err(failure) => {
                    last_error = Some(failure);
                    continue;
                }
            };
        let mut upstream_body = upstream_body_for_provider(&provider, &body, &request_model);
        if path == "/chat/completions" {
            apply_system_prompt_override(&provider, &mut upstream_body);
        } else if path == "/messages" {
            apply_anthropic_system_prompt_override(&provider, &mut upstream_body);
        }
        let upstream_attempt_model = upstream_body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&request_model);
        let mut early_log_id = start_stream_attempt_log_for_key(
            &cfg,
            stream,
            api,
            &provider.name,
            &model,
            upstream_attempt_model,
            started,
            &api_key_id,
            &api_key_name,
            ctx.request_id(),
        );
        // chat/completions 路径下预留一份 body 用于 responses fallback
        let fallback_body = if path == "/chat/completions" {
            Some(upstream_body.clone())
        } else {
            None
        };
        match send_to_provider(
            &state,
            &provider,
            path,
            &headers,
            upstream_body,
            stream,
            started,
            &ctx,
        )
        .await
        {
            Ok(result) => {
                return Ok(commit_result(
                    state.snapshot(),
                    state,
                    api,
                    &provider.name,
                    &model,
                    &affinity_key,
                    &request_model,
                    result,
                    started,
                    stream,
                    early_log_id.take(),
                    permit.take(),
                    &api_key_id,
                    &api_key_name,
                    &ctx,
                    Some(circuit_guard),
                ));
            }
            Err(err) => {
                // 上游不支持 /chat/completions 时（404/405），自动降级到 /responses 反向翻译
                if let (Some(fb), true) = (
                    fallback_body,
                    err.status == StatusCode::NOT_FOUND
                        || err.status == StatusCode::METHOD_NOT_ALLOWED,
                ) {
                    match send_chat_via_responses(
                        &state, &provider, &headers, fb, stream, started, &ctx,
                    )
                    .await
                    {
                        Ok(result) => {
                            return Ok(commit_result(
                                state.snapshot(),
                                state,
                                api,
                                &provider.name,
                                &model,
                                &affinity_key,
                                &request_model,
                                result,
                                started,
                                stream,
                                early_log_id.take(),
                                permit.take(),
                                &api_key_id,
                                &api_key_name,
                                &ctx,
                                Some(circuit_guard),
                            ));
                        }
                        Err(fb_err) => {
                            record_attempt_failure(circuit_guard, &fb_err);
                            finish_failed_attempt_log_for_key(
                                &cfg,
                                early_log_id.take(),
                                api,
                                &provider.name,
                                &model,
                                started,
                                &fb_err.message,
                                &api_key_id,
                                &api_key_name,
                                ctx.request_id(),
                            );
                            last_error = Some(AttemptFailure::new(
                                &provider,
                                &request_model,
                                started,
                                fb_err,
                            ));
                            continue;
                        }
                    }
                }
                record_attempt_failure(circuit_guard, &err);
                let failure = AttemptFailure::new(&provider, &request_model, started, err);
                // 只有客户端鉴权错误（401/407）立即中止：换渠道也是同样错，避免整链重试放大
                // 其它 4xx（400/403/404/…）都视为「这个上游不认可」，继续尝试下个渠道
                if should_stop_failover(failure.status) {
                    finish_failed_attempt_log_for_key(
                        &cfg,
                        early_log_id.take(),
                        api,
                        &failure.provider,
                        &model,
                        failure.started,
                        &failure.message,
                        &api_key_id,
                        &api_key_name,
                        ctx.request_id(),
                    );
                    if !stream {
                        log_error(
                            &cfg,
                            api,
                            &failure.provider,
                            &failure.request_model,
                            failure.started,
                            None,
                            &failure.message,
                            &api_key_id,
                            &api_key_name,
                            ctx.request_id(),
                        );
                    }
                    return Err(ProxyError::new(failure.status, failure.message));
                }
                finish_failed_attempt_log_for_key(
                    &cfg,
                    early_log_id.take(),
                    api,
                    &failure.provider,
                    &model,
                    failure.started,
                    &failure.message,
                    &api_key_id,
                    &api_key_name,
                    ctx.request_id(),
                );
                last_error = Some(failure);
            }
        }
    }

    if let Some(failure) = last_error {
        if !stream {
            log_error(
                &cfg,
                api,
                &failure.provider,
                &failure.request_model,
                failure.started,
                None,
                &failure.message,
                &api_key_id,
                &api_key_name,
                ctx.request_id(),
            );
        }
        return Err(ProxyError::new(
            final_failover_status(failure.status),
            failure.message,
        ));
    }

    Err(ProxyError::new(
        StatusCode::BAD_GATEWAY,
        format!("模型 '{}' 没有可用渠道", model),
    ))
}

pub(super) fn commit_result(
    cfg: Arc<AppConfig>,
    state: AppState,
    api: &str,
    provider: &str,
    model: &str,
    affinity_key: &str,
    affinity_model: &str,
    mut result: ProviderResult,
    started: Instant,
    stream: bool,
    early_log_id: Option<i64>,
    permit: Option<OwnedSemaphorePermit>,
    api_key_id: &str,
    api_key_name: &str,
    ctx: &RequestContext,
    circuit_guard: Option<ProviderCircuitGuard>,
) -> Response {
    if let Some(rx) = result.usage_rx.take() {
        let log_id = early_log_id.or_else(|| {
            log_stream_started(
                &cfg,
                api,
                provider,
                model,
                &result.upstream_model,
                started,
                api_key_id,
                api_key_name,
                ctx.request_id(),
            )
        });
        let api = api.to_string();
        let api_key_id = api_key_id.to_string();
        let api_key_name = api_key_name.to_string();
        let provider = provider.to_string();
        let model = model.to_string();
        let affinity_key = affinity_key.to_string();
        let affinity_model = affinity_model.to_string();
        let upstream_model = result.upstream_model.clone();
        let request_id = ctx.request_id().to_string();
        tokio::spawn(async move {
            let mut circuit_guard = circuit_guard;
            match rx.await {
                // 客户端提前断开：上游已被我们主动取消，这既不是上游故障也不是成功回答。
                // 单独记 aborted，熔断按成功放行（不是渠道的错），
                // 会话粘性既不加强也不清除（这次没有得到任何关于渠道健康度的信息）。
                Ok(outcome) if outcome.aborted => {
                    if let Some(guard) = circuit_guard.take() {
                        guard.mark_success();
                    }
                    finalize_stream_log(
                        &cfg,
                        log_id,
                        started,
                        UsageLogEvent {
                            api: &api,
                            api_key_id: &api_key_id,
                            api_key_name: &api_key_name,
                            provider: &provider,
                            model: &model,
                            upstream_model: &upstream_model,
                            status: STREAM_ABORTED_STATUS,
                            error: outcome.error.as_deref(),
                            usage: outcome.usage,
                            first_token_ms: outcome.first_token_ms,
                            token_source: None,
                            request_id: &request_id,
                        },
                    )
                    .await
                }
                Ok(StreamOutcome {
                    usage: _,
                    first_token_ms,
                    error: Some(error),
                    ..
                }) => {
                    if let Some(guard) = circuit_guard.take() {
                        guard.mark_failure(StatusCode::BAD_GATEWAY, &error, None);
                    }
                    state.forget_affinity_provider(&affinity_key, &provider, &affinity_model);
                    finalize_stream_log(
                        &cfg,
                        log_id,
                        started,
                        UsageLogEvent {
                            api: &api,
                            api_key_id: &api_key_id,
                            api_key_name: &api_key_name,
                            provider: &provider,
                            model: &model,
                            upstream_model: &upstream_model,
                            status: "error",
                            error: Some(&error),
                            usage: TokenUsage::default(),
                            first_token_ms,
                            token_source: None,
                            request_id: &request_id,
                        },
                    )
                    .await
                }
                Ok(StreamOutcome {
                    usage,
                    first_token_ms,
                    error: None,
                    ..
                }) => {
                    if let Some(guard) = circuit_guard.take() {
                        guard.mark_success();
                    }
                    state.remember_affinity_provider(&affinity_key, &provider, &affinity_model);
                    finalize_stream_log(
                        &cfg,
                        log_id,
                        started,
                        UsageLogEvent {
                            api: &api,
                            api_key_id: &api_key_id,
                            api_key_name: &api_key_name,
                            provider: &provider,
                            model: &model,
                            upstream_model: &upstream_model,
                            status: "ok",
                            error: None,
                            usage,
                            first_token_ms,
                            token_source: None,
                            request_id: &request_id,
                        },
                    )
                    .await
                }
                Err(_) => {
                    if let Some(guard) = circuit_guard.take() {
                        guard.mark_failure(
                            StatusCode::BAD_GATEWAY,
                            "流式结果状态通道异常关闭",
                            None,
                        );
                    }
                    state.forget_affinity_provider(&affinity_key, &provider, &affinity_model);
                    finalize_stream_log(
                        &cfg,
                        log_id,
                        started,
                        UsageLogEvent {
                            api: &api,
                            api_key_id: &api_key_id,
                            api_key_name: &api_key_name,
                            provider: &provider,
                            model: &model,
                            upstream_model: &upstream_model,
                            status: "error",
                            error: Some("流式结果状态通道异常关闭"),
                            usage: TokenUsage::default(),
                            first_token_ms: None,
                            token_source: None,
                            request_id: &request_id,
                        },
                    )
                    .await
                }
            }
        });
    } else {
        if let Some(guard) = circuit_guard {
            guard.mark_success();
        }
        state.remember_affinity_provider(affinity_key, provider, affinity_model);
        log_success(
            &cfg,
            api,
            provider,
            model,
            &result.upstream_model,
            result.usage,
            started,
            None,
            stream,
            api_key_id,
            api_key_name,
            ctx.request_id(),
        );
    }
    if stream {
        if let Some(permit) = permit {
            return hold_permit_until_body_done(result.response, permit);
        }
    }
    result.response
}

pub(super) fn hold_permit_until_body_done(
    response: Response,
    permit: OwnedSemaphorePermit,
) -> Response {
    let (parts, body) = response.into_parts();
    let guard = Some(permit);
    let stream = body.into_data_stream().map(move |item| {
        let _guard = &guard;
        item
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

fn upstream_body_for_provider(
    provider: &ProviderConfig,
    body: &Value,
    request_model: &str,
) -> Value {
    let mut upstream_body = with_model(body.clone(), request_model);
    apply_model_mapping(provider, &mut upstream_body);
    normalize_responses_function_call_ids(&mut upstream_body);
    upstream_body
}

fn normalize_responses_function_call_ids(body: &mut Value) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        let Some(obj) = item.as_object_mut() else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("function_call") {
            continue;
        }
        let Some(id) = obj
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| id.starts_with("call_"))
            .map(ToOwned::to_owned)
        else {
            continue;
        };
        obj.entry("call_id".to_string())
            .or_insert(Value::String(id));
        obj.remove("id");
    }
}

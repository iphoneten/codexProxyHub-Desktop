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
/// 复用统一的鉴权、模型/Token 限额、渠道轮询、运行统计与用量日志。
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
    Extension(ctx): Extension<RequestContext>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    match count_tokens_inner(state, ctx, headers, body).await {
        Ok(response) => response,
        Err(err) => anthropic_error_response(err),
    }
}

async fn count_tokens_inner(
    state: AppState,
    ctx: RequestContext,
    headers: HeaderMap,
    body: Value,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    let model = body_model(&body)?;
    ensure_api_key_allows_model(&auth, &model)?;

    let providers = provider_attempts(&cfg, &state, &model, "messages", &auth.allowed_providers);
    let mut retry_cycle = ProviderRetryCycle::new(
        providers, &model, "messages", auth.allowed_providers.clone(),
    ).for_token_estimate();
    let mut last_error = None;
    while let Some((provider, request_model)) = retry_cycle
        .next(&state, &ctx, &headers, last_error.take()).await?
    {
        let started = Instant::now();
        let cancel = ctx.cancel_token();
        let refreshed = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProxyError::aborted()),
            _ = state.root_cancel.cancelled() => return Err(ProxyError::aborted()),
            result = state.provider_with_fresh_oauth(&provider) => result,
        };
        let provider = match refreshed {
            Ok(provider) => provider,
            Err(err) => {
                last_error = Some(AttemptFailure::new(&provider, &request_model, started, err));
                continue;
            }
        };
        let mut upstream_body = with_model(body.clone(), &request_model);
        apply_model_mapping(&provider, &mut upstream_body);
        let client = state.client_for_provider(&provider);
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProxyError::aborted()),
            _ = state.root_cancel.cancelled() => return Err(ProxyError::aborted()),
            result = send_json_to_provider(
                &client,
                &provider,
                "/messages/count_tokens",
                &headers,
                upstream_body,
                &ctx,
            ) => result,
        };
        match result {
            Ok(value) if value.get("error").is_some_and(|error| !error.is_null()) => {
                last_error = Some(AttemptFailure::new(
                    &provider, &request_model, started,
                    ProxyError::new(StatusCode::BAD_GATEWAY, "上游 count_tokens 返回错误"),
                ));
            }
            Ok(value) => return Ok((StatusCode::OK, Json(value)).into_response()),
            Err(err) => {
                if should_stop_failover(err.status) {
                    return Err(err);
                }
                last_error = Some(AttemptFailure::new(&provider, &request_model, started, err));
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

#[derive(Clone)]
struct ResponsesAffinityUpdate {
    provider_identity: String,
    expected_owner_identity: Option<String>,
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
    let responses_affinity = responses_affinity_key(&body, &headers, &api_key_id, &model);
    let affinity_key = responses_affinity.key.clone();
    let persistent_affinity = responses_affinity.persistent;
    let responses_binding = persistent_affinity
        .then(|| state.responses_binding(&affinity_key))
        .flatten();
    // 没有显式会话键的首轮仍沿用旧的 API Key + 模型粘性；第二轮拿到 rs_* 后，
    // 用它为新的持久化 lineage 绑定引导一次，避免刚进入持久化模式就随机换账号。
    let weak_affinity_key = api_key_affinity_key(&model, &api_key_id);
    let weak_binding = responses_binding
        .is_none()
        .then(|| state.preferred_provider_for_affinity(&weak_affinity_key))
        .flatten();
    let mut providers = provider_attempts(&cfg, &state, &model, "responses", &allowed_providers);
    if let Some(binding) = &responses_binding {
        prefer_bound_provider(
            &mut providers,
            &binding.provider,
            &binding.request_model,
        );
    } else if let Some((preferred, preferred_model)) = &weak_binding {
        prefer_provider(&mut providers, preferred, preferred_model);
    }
    dedupe_provider_attempts(&mut providers);
    let has_opaque_state = responses_has_opaque_state(&body);
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let heartbeat = stream && !providers.is_empty();
    let heartbeat_ctx = ctx.clone();
    let request_cancel = ctx.cancel_token();
    let root_cancel = state.root_cancel.clone();
    let operation = async move {
        let mut last_error = None;
        let expected_owner_identity = responses_binding
            .as_ref()
            .map(|binding| binding.provider_identity.clone())
            .filter(|identity| !identity.is_empty());
        let mut opaque_owner_identity = expected_owner_identity.clone();
        let mut retry_cycle = ProviderRetryCycle::new(providers, &model, "responses", allowed_providers);
        if let Some(binding) = &responses_binding {
            retry_cycle.prefer_provider(&binding.provider, &binding.request_model);
        }
        while let Some((provider, request_model)) = retry_cycle
            .next(&state, &ctx, &headers, last_error.take()).await?
        {
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
            let provider_identity = responses_provider_identity(&provider, &upstream_body, &headers);
            let affinity_update = persistent_affinity.then(|| ResponsesAffinityUpdate {
                provider_identity: provider_identity.clone(),
                expected_owner_identity: expected_owner_identity.clone(),
            });
            if persistent_affinity {
                let rejected = state.rejected_responses_state(&affinity_key, &provider_identity);
                strip_rejected_responses_state(&mut upstream_body, &rejected);
            }
            if has_opaque_state {
                if opaque_owner_identity.is_none() {
                    opaque_owner_identity = Some(provider_identity.clone());
                } else if opaque_owner_identity.as_deref() != Some(provider_identity.as_str()) {
                    let removed = strip_all_responses_state(&mut upstream_body);
                    if !removed.is_empty() {
                        crate::runtime_log::record_repeated(
                            "WARN",
                            format!(
                                "Responses 会话切换到渠道 {} 的不同账号、项目或模型；已保留可见历史与工具结果，并移除无法跨安全域复用的加密状态",
                                provider.name
                            ),
                            format!("responses-state-domain-switch:{}", provider.name),
                        );
                        if persistent_affinity {
                            state.remember_rejected_responses_state(
                                &affinity_key,
                                &provider_identity,
                                removed,
                            );
                        }
                    }
                }
            }
            let upstream_attempt_model = upstream_body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(&request_model);
            let mut early_log_id = StreamAttemptLogGuard::new(
                &cfg,
                stream,
                "responses",
                &provider.name,
                &model,
                upstream_attempt_model,
                started,
                &api_key_id,
                &api_key_name,
                &ctx,
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
                            affinity_update.as_ref(),
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
                        let aborted = record_attempt_failure(circuit_guard, &err);
                        early_log_id.finish_error(err.status, &err.message);
                        if aborted {
                            return Err(err);
                        }
                        last_error = Some(AttemptFailure::new(&provider, &request_model, started, err));
                    }
                }
                continue;
            }

            match send_native_responses_with_recovery(
                &state,
                &provider,
                &headers,
                upstream_body.clone(),
                stream,
                started,
                &ctx,
                persistent_affinity.then_some(affinity_key.as_str()),
                &provider_identity,
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
                        affinity_update.as_ref(),
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
                    if should_fallback_responses_to_chat(&provider, err.status, &err.message) =>
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
                                affinity_update.as_ref(),
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
                            let aborted = record_attempt_failure(circuit_guard, &chat_err);
                            early_log_id.finish_error(chat_err.status, &chat_err.message);
                            if aborted {
                                return Err(chat_err);
                            }
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
                    if is_encrypted_state_rejection(&err.message) {
                        drop(circuit_guard);
                        early_log_id.finish_error(StatusCode::BAD_REQUEST, &err.message);
                        if !stream {
                            log_error(
                                &cfg,
                                "responses",
                                &provider.name,
                                &request_model,
                                started,
                                None,
                                &err.message,
                                &api_key_id,
                                &api_key_name,
                                ctx.request_id(),
                            );
                        }
                        return Err(ProxyError::new(StatusCode::BAD_REQUEST, err.message));
                    }
                    let _ = record_attempt_failure(circuit_guard, &err);
                    let failure = AttemptFailure::new(&provider, &request_model, started, err);
                    // 客户端取消立即中止；其它上游错误交给统一调度层按次数继续轮询。
                    if should_stop_failover(failure.status) {
                        early_log_id.finish_error(failure.status, &failure.message);
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
                    early_log_id.finish_error(failure.status, &failure.message);
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
    };
    tokio::select! {
        biased;
        _ = request_cancel.cancelled() => Err(ProxyError::aborted()),
        _ = root_cancel.cancelled() => Err(ProxyError::aborted()),
        result = super::keepalive::while_waiting(operation, heartbeat, heartbeat_ctx, "responses") => result,
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_native_responses_with_recovery(
    state: &AppState,
    provider: &ProviderConfig,
    headers: &HeaderMap,
    mut body: Value,
    stream: bool,
    started: Instant,
    ctx: &RequestContext,
    affinity_key: Option<&str>,
    provider_identity: &str,
) -> Result<ProviderResult, ProxyError> {
    match send_to_provider(
        state,
        provider,
        "/responses",
        headers,
        body.clone(),
        stream,
        started,
        ctx,
    )
    .await
    {
        Ok(result) => Ok(result),
        Err(err) if is_encrypted_state_rejection(&err.message) => {
            let removed = strip_all_responses_state(&mut body);
            if removed.is_empty() {
                return Err(err);
            }
            if let Some(affinity_key) = affinity_key {
                state.remember_rejected_responses_state(
                    affinity_key,
                    provider_identity,
                    removed,
                );
            }
            crate::runtime_log::record_repeated(
                "WARN",
                format!(
                    "渠道 {} 无法验证 Responses 加密会话状态；已保留可见历史与工具结果，并在同一渠道执行一次降级恢复",
                    provider.name
                ),
                format!("responses-encrypted-state-recovery:{}", provider.name),
            );
            send_to_provider(
                state,
                provider,
                "/responses",
                headers,
                body,
                stream,
                started,
                ctx,
            )
            .await
        }
        Err(err) => Err(err),
    }
}

pub(super) fn provider_prefers_chat_responses(provider: &ProviderConfig) -> bool {
    provider.responses_mode == "chat"
        || provider.provider_type == "anthropic"
        || is_google_native_provider(provider)
        || is_google_openai_endpoint(&provider.base_url)
}

/// 一些 OpenAI 兼容中转不会为不支持的 Responses API 返回 404/405，
/// 而是直接返回 400。auto 模式下这类错误也应尝试 Chat 兼容路径；
/// 如果确实是请求参数错误，Chat 翻译同样会失败，随后仍会继续故障转移。
pub(super) fn should_fallback_responses_to_chat(
    provider: &ProviderConfig,
    status: StatusCode,
    message: &str,
) -> bool {
    provider.responses_mode == "auto"
        && !is_encrypted_state_rejection(message)
        && matches!(
            status,
            StatusCode::BAD_REQUEST
                | StatusCode::NOT_FOUND
                | StatusCode::METHOD_NOT_ALLOWED
        )
}

pub(super) fn prefer_bound_provider(
    providers: &mut Vec<(ProviderConfig, String)>,
    preferred: &str,
    preferred_model: &str,
) {
    let index = providers
        .iter()
        .position(|(provider, model)| {
            provider.name == preferred && normalize_model(model) == normalize_model(preferred_model)
        })
        .or_else(|| {
            providers
                .iter()
                .position(|(provider, _)| provider.name == preferred)
        });
    if let Some(index) = index.filter(|index| *index > 0) {
        let item = providers.remove(index);
        providers.insert(0, item);
    }
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
    // 成功渠道仅在同一模型、同一优先级内优先，不能越过更高优先级或原始模型。
    let Some(index) = providers
        .iter()
        .position(|(provider, model)| {
            provider.name == preferred && normalize_model(model) == normalize_model(preferred_model)
        })
    else {
        return;
    };
    let priority = providers[index].0.priority;
    let first_in_tier = providers.iter().position(|(provider, model)| {
        provider.priority == priority && normalize_model(model) == normalize_model(preferred_model)
    }).unwrap_or(index);
    let item = providers.remove(index);
    providers.insert(first_in_tier, item);
}

pub(super) fn dedupe_provider_attempts(providers: &mut Vec<(ProviderConfig, String)>) {
    let mut seen = HashSet::new();
    providers.retain(|(provider, request_model)| {
        seen.insert((provider.name.clone(), normalize_model(request_model).to_string()))
    });
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
    use axum::http::Uri;

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

    fn successful_response() -> Value {
        json!({
            "id":"resp_test",
            "model":"gpt-test",
            "status":"completed",
            "output":[{
                "type":"message",
                "role":"assistant",
                "content":[{"type":"output_text","text":"ok"}]
            }]
        })
    }

    async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, server)
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
    fn prefer_provider_keeps_previous_fallback_after_primary_model() {
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
        assert_eq!(providers[3].0.name, "fallback-a");
    }

    #[test]
    fn prefer_provider_only_moves_within_its_priority_tier() {
        let mut first = provider_with_name("higher-priority");
        first.priority = -10;
        let mut peer = provider_with_name("same-priority");
        peer.priority = 5;
        let mut sticky = provider_with_name("previous-success");
        sticky.priority = 5;
        let mut providers = vec![
            (first, "gpt-test".into()),
            (peer, "gpt-test".into()),
            (sticky, "gpt-test".into()),
        ];

        prefer_provider(&mut providers, "previous-success", "gpt-test");

        assert_eq!(providers[0].0.name, "higher-priority");
        assert_eq!(providers[1].0.name, "previous-success");
        assert_eq!(providers[2].0.name, "same-priority");
    }

    #[test]
    fn dedupe_provider_attempts_keeps_distinct_model_attempts() {
        let mut providers = vec![
            (provider_with_name("channel-a"), "gpt-primary".to_string()),
            (provider_with_name("channel-b"), "gpt-primary".to_string()),
            (provider_with_name("channel-a"), "gpt-fallback".to_string()),
            (provider_with_name("channel-a"), "models/gpt-primary".to_string()),
        ];

        dedupe_provider_attempts(&mut providers);

        assert_eq!(providers.len(), 3);
        assert_eq!(providers[0].0.name, "channel-a");
        assert_eq!(providers[0].1, "gpt-primary");
        assert_eq!(providers[1].0.name, "channel-b");
        assert_eq!(providers[2].1, "gpt-fallback");
    }

    #[test]
    fn responses_auto_falls_back_to_chat_on_bad_request() {
        let provider = provider_with_name("compat");
        assert!(should_fallback_responses_to_chat(
            &provider,
            StatusCode::BAD_REQUEST,
            "unsupported endpoint"
        ));
        assert!(should_fallback_responses_to_chat(
            &provider,
            StatusCode::NOT_FOUND,
            "not found"
        ));
        assert!(!should_fallback_responses_to_chat(
            &provider,
            StatusCode::UNAUTHORIZED,
            "unauthorized"
        ));
        assert!(!should_fallback_responses_to_chat(
            &provider,
            StatusCode::BAD_REQUEST,
            "The encrypted content for item rs_test could not be verified"
        ));
    }

    #[test]
    fn bound_responses_provider_moves_ahead_of_priority_and_model_fallback() {
        let mut primary = provider_with_name("primary");
        primary.priority = -10;
        let mut bound = provider_with_name("auth:bound");
        bound.priority = 20;
        let mut providers = vec![
            (primary, "gpt-primary".to_string()),
            (provider_with_name("fallback"), "gpt-fallback".to_string()),
            (bound, "gpt-fallback".to_string()),
        ];

        prefer_bound_provider(&mut providers, "auth:bound", "gpt-fallback");

        assert_eq!(providers[0].0.name, "auth:bound");
        assert_eq!(providers[0].1, "gpt-fallback");
    }

    #[tokio::test]
    async fn encrypted_state_rejection_retries_once_without_opaque_items() {
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let seen = Arc::clone(&requests);
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(body): Json<Value>| {
                let seen = Arc::clone(&seen);
                async move {
                    let has_encrypted = responses_has_opaque_state(&body);
                    seen.lock().push(body);
                    if has_encrypted {
                        (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error":{
                                "code":"invalid_encrypted_content",
                                "message":"The encrypted content for item rs_test could not be verified. Reason: Encrypted content could not be decrypted or parsed."
                            }})),
                        )
                            .into_response()
                    } else {
                        Json(successful_response()).into_response()
                    }
                }
            }),
        );
        let (base, server) = serve(app).await;
        let state = crate::proxy::tests::test_state();
        let mut provider = provider_with_name("recover");
        provider.base_url = format!("{base}/v1");
        provider.responses_mode = "native".into();
        provider.max_retries = 0;
        let headers = HeaderMap::new();
        let body = json!({
            "model":"gpt-test",
            "prompt_cache_key":"session-recovery",
            "previous_response_id":"resp_old",
            "input":[
                {"type":"message","role":"user","content":"continue"},
                {"type":"reasoning","id":"rs_test","encrypted_content":"secret-cipher"},
                {"type":"function_call","call_id":"call_1","name":"read","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"ok"}
            ]
        });
        let key = responses_affinity_key(&body, &headers, "", "gpt-test");
        let identity = responses_provider_identity(&provider, &body, &headers);

        let result = send_native_responses_with_recovery(
            &state,
            &provider,
            &headers,
            body,
            false,
            Instant::now(),
            &RequestContext::detached(),
            Some(&key.key),
            &identity,
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(result.response.status(), StatusCode::OK);
        let requests = requests.lock();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].to_string().contains("secret-cipher"));
        assert!(!requests[1].to_string().contains("secret-cipher"));
        assert!(requests[1].get("previous_response_id").is_none());
        assert!(requests[1].to_string().contains("call_1"));
        assert!(requests[1].to_string().contains("continue"));
    }

    #[tokio::test]
    async fn bound_stateful_request_prefers_owner_and_sanitizes_failover() {
        let requests = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let seen = Arc::clone(&requests);
        let app = Router::new().fallback(move |uri: Uri, Json(body): Json<Value>| {
            let seen = Arc::clone(&seen);
            async move {
                let provider = uri.path().split('/').nth(1).unwrap_or_default().to_string();
                seen.lock().push((provider.clone(), body.clone()));
                if provider == "owner" {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({"error":{"message":"temporarily unavailable"}})),
                    )
                        .into_response()
                } else {
                    Json(successful_response()).into_response()
                }
            }
        });
        let (base, server) = serve(app).await;
        let state = crate::proxy::tests::test_state();
        let mut cfg = (*state.snapshot()).clone();
        let mut owner = provider_with_name("owner");
        owner.base_url = format!("{base}/owner/v1");
        owner.responses_mode = "native".into();
        owner.priority = 20;
        owner.max_retries = 1;
        let mut fallback = provider_with_name("fallback");
        fallback.base_url = format!("{base}/fallback/v1");
        fallback.responses_mode = "native".into();
        fallback.priority = -10;
        fallback.max_retries = 1;
        cfg.providers = vec![fallback.clone(), owner.clone()];
        cfg.auth_accounts.clear();
        cfg.routing.model_fallbacks.clear();
        cfg.auth.enabled = true;
        cfg.auth.api_keys = vec![serde_json::from_value(json!({
            "key":"synthetic-local", "name":"test", "max_concurrency":1
        }))
        .unwrap()];
        let log_path = std::env::temp_dir().join(format!(
            "routehub-responses-affinity-{}.sqlite3",
            Uuid::new_v4()
        ));
        cfg.usage_log.sqlite_path = log_path.to_string_lossy().into_owned();
        *state.config.write() = Arc::new(cfg);

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer synthetic-local"),
        );
        let body = json!({
            "model":"gpt-test",
            "prompt_cache_key":"session-owner",
            "previous_response_id":"resp_owner",
            "input":[
                {"type":"message","role":"user","content":"continue"},
                {"type":"reasoning","id":"rs_owner","encrypted_content":"owner-cipher"}
            ]
        });
        let key = responses_affinity_key(
            &body,
            &headers,
            &api_key_id("synthetic-local"),
            "gpt-test",
        );
        let owner_body = upstream_body_for_provider(&owner, &body, "gpt-test");
        let owner_identity = responses_provider_identity(&owner, &owner_body, &headers);
        assert!(state.responses_state.remember_binding(
            &key.key,
            "owner",
            "gpt-test",
            &owner_identity,
            None,
        ));

        let response = responses(
            State(state),
            Extension(RequestContext::detached()),
            headers,
            Json(body),
        )
        .await
        .unwrap();
        server.abort();
        let _ = fs::remove_file(log_path);

        assert_eq!(response.status(), StatusCode::OK);
        let requests = requests.lock();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0, "owner");
        assert!(requests[0].1.to_string().contains("owner-cipher"));
        assert_eq!(requests[1].0, "fallback");
        assert!(!requests[1].1.to_string().contains("owner-cipher"));
        assert!(requests[1].1.get("previous_response_id").is_none());
        assert!(requests[1].1.to_string().contains("continue"));
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
    dedupe_provider_attempts(&mut providers);
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let heartbeat = stream && !providers.is_empty();
    let heartbeat_ctx = ctx.clone();
    let request_cancel = ctx.cancel_token();
    let root_cancel = state.root_cancel.clone();
    let operation = async move {
        let mut last_error = None;
        let mut retry_cycle = ProviderRetryCycle::new(providers, &model, api, allowed_providers);
        while let Some((provider, request_model)) = retry_cycle
            .next(&state, &ctx, &headers, last_error.take()).await?
        {
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
                normalize_chat_message_content_types(&mut upstream_body);
            } else if path == "/messages" {
                apply_anthropic_system_prompt_override(&provider, &mut upstream_body);
            }
            let upstream_attempt_model = upstream_body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(&request_model);
            let mut early_log_id = StreamAttemptLogGuard::new(
                &cfg,
                stream,
                api,
                &provider.name,
                &model,
                upstream_attempt_model,
                started,
                &api_key_id,
                &api_key_name,
                &ctx,
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
                        None,
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
                                    None,
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
                                let aborted = record_attempt_failure(circuit_guard, &fb_err);
                                early_log_id.finish_error(fb_err.status, &fb_err.message);
                                if aborted {
                                    return Err(fb_err);
                                }
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
                    let _ = record_attempt_failure(circuit_guard, &err);
                    let failure = AttemptFailure::new(&provider, &request_model, started, err);
                    // 本地鉴权已在入口校验；上游 401/403 等同样按渠道次数轮询。
                    if should_stop_failover(failure.status) {
                        early_log_id.finish_error(failure.status, &failure.message);
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
                    early_log_id.finish_error(failure.status, &failure.message);
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
    };
    tokio::select! {
        biased;
        _ = request_cancel.cancelled() => Err(ProxyError::aborted()),
        _ = root_cancel.cancelled() => Err(ProxyError::aborted()),
        result = super::keepalive::while_waiting(operation, heartbeat, heartbeat_ctx, api) => result,
    }
}

#[allow(clippy::too_many_arguments)]
fn commit_result(
    cfg: Arc<AppConfig>,
    state: AppState,
    api: &str,
    provider: &str,
    model: &str,
    affinity_key: &str,
    affinity_model: &str,
    affinity_update: Option<&ResponsesAffinityUpdate>,
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
        let affinity_update = affinity_update.cloned();
        let upstream_model = result.upstream_model.clone();
        let request_id = ctx.request_id().to_string();
        tokio::spawn(async move {
            let mut circuit_guard = circuit_guard;
            match rx.await {
                // 客户端提前断开：上游已被我们主动取消，这既不是上游故障也不是成功回答。
                // 单独记 aborted，仅释放在途计数，不增加上游失败次数，
                // 会话粘性既不加强也不清除（这次没有得到任何关于渠道健康度的信息）。
                Ok(outcome) if outcome.aborted => {
                    drop(circuit_guard.take());
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
                    if affinity_update.is_none() {
                        state.forget_affinity_provider(&affinity_key, &provider, &affinity_model);
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
                    if let Some(update) = affinity_update.as_ref() {
                        state.remember_responses_binding(
                            &affinity_key,
                            &provider,
                            &affinity_model,
                            &update.provider_identity,
                            update.expected_owner_identity.as_deref(),
                        );
                    } else {
                        state.remember_affinity_provider(
                            &affinity_key,
                            &provider,
                            &affinity_model,
                        );
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
                    if affinity_update.is_none() {
                        state.forget_affinity_provider(&affinity_key, &provider, &affinity_model);
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
        if let Some(update) = affinity_update {
            state.remember_responses_binding(
                affinity_key,
                provider,
                affinity_model,
                &update.provider_identity,
                update.expected_owner_identity.as_deref(),
            );
        } else {
            state.remember_affinity_provider(affinity_key, provider, affinity_model);
        }
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

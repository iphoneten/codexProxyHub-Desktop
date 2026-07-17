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
            "/v1/responses"
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
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    let api_key_id = request_api_key_id(&cfg, &headers);
    let model = body_model(&body)?;
    ensure_api_key_allows_model(&auth, &model)?;
    forward_openai(
        state,
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
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    let api_key_id = request_api_key_id(&cfg, &headers);
    let model = body_model(&body)?;
    ensure_api_key_allows_model(&auth, &model)?;
    forward_openai(
        state,
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
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    let api_key_id = request_api_key_id(&cfg, &headers);
    let model = body_model(&body)?;
    ensure_api_key_allows_model(&auth, &model)?;
    forward_openai(
        state,
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

pub(super) async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    let api_key_id = request_api_key_id(&cfg, &headers);
    let model = body_model(&body)?;
    ensure_api_key_allows_model(&auth, &model)?;
    let api_key_name = auth.key_name;
    let allowed_providers = auth.allowed_providers;
    let mut permit = Some(auth.permit);
    let providers = provider_attempts(&cfg, &state, &model, "responses", &allowed_providers);
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut last_error = None;

    for (provider, request_model) in providers {
        let started = Instant::now();
        let circuit_guard =
            match begin_attempt_or_record_failure(&state, &provider, &request_model, started) {
                Ok(guard) => guard,
                Err(failure) => {
                    last_error = Some(failure);
                    continue;
                }
            };
        let mut upstream_body = with_model(body.clone(), &request_model);
        apply_model_mapping(&provider, &mut upstream_body);
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
            )
            .await
            {
                Ok(result) => {
                    return Ok(commit_result(
                        state.snapshot(),
                        "responses",
                        &provider.name,
                        &model,
                        result,
                        started,
                        stream,
                        early_log_id.take(),
                        permit.take(),
                        &api_key_id,
                        &api_key_name,
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
            upstream_body,
            stream,
            started,
        )
        .await
        {
            Ok(result) => {
                return Ok(commit_result(
                    state.snapshot(),
                    "responses",
                    &provider.name,
                    &model,
                    result,
                    started,
                    stream,
                    early_log_id.take(),
                    permit.take(),
                    &api_key_id,
                    &api_key_name,
                    Some(circuit_guard),
                ));
            }
            Err(err)
                if provider.responses_mode == "auto"
                    && (err.status == StatusCode::NOT_FOUND
                        || err.status == StatusCode::METHOD_NOT_ALLOWED) =>
            {
                let chat_body = with_model(body.clone(), &request_model);
                match forward_responses_as_chat(
                    &state, &provider, &headers, chat_body, stream, started,
                )
                .await
                {
                    Ok(result) => {
                        return Ok(commit_result(
                            state.snapshot(),
                            "responses",
                            &provider.name,
                            &model,
                            result,
                            started,
                            stream,
                            early_log_id.take(),
                            permit.take(),
                            &api_key_id,
                            &api_key_name,
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

pub(super) fn final_failover_status(status: StatusCode) -> StatusCode {
    match status {
        StatusCode::TOO_MANY_REQUESTS
        | StatusCode::REQUEST_TIMEOUT
        | StatusCode::SERVICE_UNAVAILABLE
        | StatusCode::GATEWAY_TIMEOUT => status,
        _ => StatusCode::BAD_GATEWAY,
    }
}

pub(super) async fn forward_openai(
    state: AppState,
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
    let providers = provider_attempts(&cfg, &state, &model, api, &allowed_providers);
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut last_error = None;

    for (provider, request_model) in providers {
        let started = Instant::now();
        let circuit_guard =
            match begin_attempt_or_record_failure(&state, &provider, &request_model, started) {
                Ok(guard) => guard,
                Err(failure) => {
                    last_error = Some(failure);
                    continue;
                }
            };
        let mut upstream_body = with_model(body.clone(), &request_model);
        apply_model_mapping(&provider, &mut upstream_body);
        if path == "/chat/completions" {
            apply_system_prompt_override(&provider, &mut upstream_body);
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
        )
        .await
        {
            Ok(result) => {
                return Ok(commit_result(
                    state.snapshot(),
                    api,
                    &provider.name,
                    &model,
                    result,
                    started,
                    stream,
                    early_log_id.take(),
                    permit.take(),
                    &api_key_id,
                    &api_key_name,
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
                    match send_chat_via_responses(&state, &provider, &headers, fb, stream, started)
                        .await
                    {
                        Ok(result) => {
                            return Ok(commit_result(
                                state.snapshot(),
                                api,
                                &provider.name,
                                &model,
                                result,
                                started,
                                stream,
                                early_log_id.take(),
                                permit.take(),
                                &api_key_id,
                                &api_key_name,
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
    api: &str,
    provider: &str,
    model: &str,
    mut result: ProviderResult,
    started: Instant,
    stream: bool,
    early_log_id: Option<i64>,
    permit: Option<OwnedSemaphorePermit>,
    api_key_id: &str,
    api_key_name: &str,
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
            )
        });
        let api = api.to_string();
        let api_key_id = api_key_id.to_string();
        let api_key_name = api_key_name.to_string();
        let provider = provider.to_string();
        let model = model.to_string();
        let upstream_model = result.upstream_model.clone();
        tokio::spawn(async move {
            let mut circuit_guard = circuit_guard;
            match rx.await {
                Ok(StreamOutcome {
                    usage: _,
                    first_token_ms,
                    error: Some(error),
                }) => {
                    if let Some(guard) = circuit_guard.take() {
                        guard.mark_failure(StatusCode::BAD_GATEWAY, &error, None);
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
                        },
                    )
                    .await
                }
                Ok(StreamOutcome {
                    usage,
                    first_token_ms,
                    error: None,
                }) => {
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
                            status: "ok",
                            error: None,
                            usage,
                            first_token_ms,
                            token_source: None,
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

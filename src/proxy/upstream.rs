use super::*;

pub(super) async fn forward_responses_as_chat(
    state: &AppState,
    provider: &ProviderConfig,
    headers: &HeaderMap,
    body: Value,
    stream: bool,
    started: Instant,
) -> Result<ProviderResult, ProxyError> {
    let chat_body = responses_to_chat_body(&body)?;
    let custom_tool_names = responses_custom_tool_names(body.get("tools"));
    if stream {
        let request_model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return send_chat_stream_as_responses(
            state,
            provider,
            headers,
            chat_body,
            request_model,
            custom_tool_names,
            started,
        )
        .await;
    }

    let mut upstream =
        if provider.provider_type == "anthropic" || is_google_native_provider(provider) {
            let result = send_to_provider(
                state,
                provider,
                "/chat/completions",
                headers,
                chat_body,
                false,
                started,
            )
            .await?;
            response_json(result.response).await?
        } else {
            let client = state.client_for_provider(provider);
            let value =
                send_json_to_provider(&client, provider, "/chat/completions", headers, chat_body)
                    .await?;
            // send_json_to_provider 只做 HTTP 层错误处理，这里作为 chat 语义调用者要自己校验结构
            // 因为该函数被复用在多种 path，不能在里面按 path 分支校验
            if let Err(msg) = validate_upstream_chat_json(&value) {
                return Err(ProxyError::new(StatusCode::BAD_GATEWAY, msg));
            }
            value
        };
    if provider.strip_thought {
        strip_thought_from_chat_json(&mut upstream);
    }
    let wrapped = chat_to_response(
        upstream,
        body.get("model")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        &custom_tool_names,
    );
    let upstream_model = response_model(&wrapped).unwrap_or_else(|| {
        body.get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    });
    let usage = extract_token_usage(&wrapped);
    Ok(ProviderResult {
        response: (StatusCode::OK, Json(wrapped)).into_response(),
        upstream_model,
        usage,
        usage_rx: None,
    })
}

pub(super) async fn response_json(response: Response) -> Result<Value, ProxyError> {
    const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
    let bytes = to_bytes(response.into_body(), MAX_RESPONSE_BYTES)
        .await
        .map_err(|err| {
            ProxyError::new(StatusCode::BAD_GATEWAY, format!("读取上游响应失败: {err}"))
        })?;
    serde_json::from_slice(&bytes).map_err(|err| {
        ProxyError::new(
            StatusCode::BAD_GATEWAY,
            format!("解析上游 JSON 失败: {err}"),
        )
    })
}

pub(super) async fn send_chat_stream_as_responses(
    state: &AppState,
    provider: &ProviderConfig,
    request_headers: &HeaderMap,
    body: Value,
    request_model: String,
    custom_tool_names: HashSet<String>,
    started: Instant,
) -> Result<ProviderResult, ProxyError> {
    if is_google_native_provider(provider) {
        let result = send_to_provider(
            state,
            provider,
            "/chat/completions",
            request_headers,
            body,
            true,
            started,
        )
        .await?;
        let stream = result
            .response
            .into_body()
            .into_data_stream()
            .map(|item| item.map_err(|err| io::Error::new(io::ErrorKind::Other, err)));
        return chat_sse_stream_to_responses(stream, request_model, custom_tool_names, started);
    }
    if provider.provider_type == "anthropic" {
        let client = state.client_for_provider(provider);
        return send_anthropic_chat_stream_as_responses(
            &client,
            provider,
            request_headers,
            body,
            request_model,
            custom_tool_names,
            started,
        )
        .await;
    }
    let client = state.client_for_provider(provider);
    let url = upstream_url(provider, "/chat/completions");
    // /responses 转 chat 走的是 OpenAI 兼容协议，按探测结果决定是否注入 include_usage
    let mut probing = state.should_inject_usage(provider);
    let mut send_body = body.clone();
    if probing {
        inject_include_usage(&mut send_body);
    }
    let mut attempt: usize = 0;
    loop {
        let req = client
            .post(url.clone())
            .headers(upstream_headers(provider, request_headers, true))
            .json(&send_body);
        match send_stream_request(req, provider.request_timeout).await {
            Ok(resp) if resp.status().is_success() => {
                // 拦截假 SSE 响应（Content-Type 非 event-stream）
                if let Err(msg) = validate_upstream_sse_content_type(resp.headers()) {
                    let text = resp.text().await.unwrap_or_default();
                    return Err(ProxyError::new(
                        StatusCode::BAD_GATEWAY,
                        format!("{} body={}", msg, clean_upstream_error(&text)),
                    ));
                }
                return chat_stream_to_responses(
                    resp,
                    request_model,
                    custom_tool_names,
                    started,
                    provider.strip_thought,
                );
            }
            Ok(resp) => {
                let status =
                    StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let retry_after = parse_retry_after(resp.headers());
                let text = resp.text().await.unwrap_or_default();
                // 只在上游明确拒绝 include_usage/stream_options 时关闭探测，避免鉴权/模型错误污染 provider 状态。
                if probing && usage_injection_rejected(status, &text) {
                    state.mark_usage_injection_unsupported(provider);
                    probing = false;
                    send_body = body.clone();
                    continue;
                }
                if attempt < provider.max_retries && retryable_status(status) {
                    tokio::time::sleep(compute_retry_delay(attempt, retry_after)).await;
                    attempt += 1;
                    continue;
                }
                return Err(upstream_status_error(status, &text, retry_after));
            }
            Err(err) if attempt < provider.max_retries => {
                tokio::time::sleep(retry_delay(attempt)).await;
                attempt += 1;
                if err.retryable() {
                    continue;
                }
                return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.message()));
            }
            Err(err) => return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.message())),
        }
    }
}

pub(super) async fn send_anthropic_chat_stream_as_responses(
    client: &Client,
    provider: &ProviderConfig,
    request_headers: &HeaderMap,
    body: Value,
    request_model: String,
    custom_tool_names: HashSet<String>,
    started: Instant,
) -> Result<ProviderResult, ProxyError> {
    let mut anthropic_body = crate::anthropic::openai_to_anthropic_request(&body)
        .map_err(|err| ProxyError::new(StatusCode::BAD_REQUEST, format!("协议翻译失败: {err}")))?;
    if let Some(obj) = anthropic_body.as_object_mut() {
        obj.insert("stream".into(), Value::Bool(true));
    }
    let url = upstream_url(provider, "/messages");
    let req = client
        .post(url)
        .headers(upstream_headers(provider, request_headers, true))
        .json(&anthropic_body);

    for attempt in 0..=provider.max_retries {
        match send_stream_request(req.try_clone().unwrap(), provider.request_timeout).await {
            Ok(resp) if resp.status().is_success() => {
                if let Err(msg) = validate_upstream_sse_content_type(resp.headers()) {
                    let text = resp.text().await.unwrap_or_default();
                    return Err(ProxyError::new(
                        StatusCode::BAD_GATEWAY,
                        format!("{} body={}", msg, clean_upstream_error(&text)),
                    ));
                }
                let (anthropic_usage_rx, chat_stream) = match prepare_anthropic_stream(
                    resp,
                    request_model.clone(),
                    provider.request_timeout,
                    provider.stream_idle_timeout,
                    provider.stream_max_duration,
                    started,
                )
                .await
                {
                    Ok(streams) => streams,
                    Err(err) if attempt < provider.max_retries && err.retryable() => {
                        tokio::time::sleep(retry_delay(attempt)).await;
                        continue;
                    }
                    Err(err) => return Err(err),
                };
                let mut result = chat_sse_stream_to_responses(
                    chat_stream,
                    request_model,
                    custom_tool_names,
                    started,
                )?;
                result.usage_rx = Some(anthropic_usage_rx);
                return Ok(result);
            }
            Ok(resp) => {
                let status =
                    StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let retry_after = parse_retry_after(resp.headers());
                let text = resp.text().await.unwrap_or_default();
                if attempt < provider.max_retries && retryable_status(status) {
                    tokio::time::sleep(compute_retry_delay(attempt, retry_after)).await;
                    continue;
                }
                return Err(upstream_status_error(status, &text, retry_after));
            }
            Err(err) if attempt < provider.max_retries => {
                tokio::time::sleep(retry_delay(attempt)).await;
                if err.retryable() {
                    continue;
                }
                return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.message()));
            }
            Err(err) => return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.message())),
        }
    }
    Err(ProxyError::new(
        StatusCode::BAD_GATEWAY,
        "Anthropic 上游请求失败",
    ))
}

pub(super) async fn send_to_provider(
    state: &AppState,
    provider: &ProviderConfig,
    path: &str,
    request_headers: &HeaderMap,
    body: Value,
    stream: bool,
    started: Instant,
) -> Result<ProviderResult, ProxyError> {
    let client = state.client_for_provider(provider);
    // Google AI Studio 原生渠道：OpenAI chat 请求翻译到 GenerateContent。
    // 如果 base_url 已经是 Google 的 /openai 兼容端点，则仍走普通 OpenAI 兼容路径。
    if is_google_native_provider(provider) && path == "/chat/completions" {
        return send_to_google_ai_provider(
            &client,
            provider,
            request_headers,
            body,
            stream,
            started,
        )
        .await;
    }
    // Anthropic 渠道：对 chat/completions 走独立协议翻译到 /messages
    if provider.provider_type == "anthropic" && path == "/chat/completions" {
        return send_to_anthropic_provider(
            &client,
            provider,
            request_headers,
            body,
            stream,
            started,
        )
        .await;
    }
    // 入站 Anthropic Messages API：请求体与响应都不翻译，原样直通。
    // 走独立分支是因为下面的泛化流式路径会用 Chat/Responses 的 SSE 探针去解析
    // Anthropic 事件（content_block_delta / message_stop），必然误判成无有效输出。
    if path == "/messages" {
        return send_anthropic_messages_passthrough(
            &client,
            provider,
            request_headers,
            body,
            stream,
            started,
        )
        .await;
    }
    if stream {
        let upstream_model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // stream_options.include_usage 只属于 Chat Completions 协议。
        // 原生 Responses 流会在 response.completed 中携带 usage，注入该字段会被严格上游以 400 拒绝。
        // 拿最新决定；send_body 里保留原始 body，用于探测失败后回退不注入版本
        let mut probing =
            supports_include_usage_injection(path) && state.should_inject_usage(provider);
        let mut send_body = body.clone();
        if probing {
            inject_include_usage(&mut send_body);
        }
        let url = upstream_url(provider, path);
        let mut attempt: usize = 0;
        loop {
            let req = client
                .post(url.clone())
                .headers(upstream_headers(provider, request_headers, true))
                .json(&send_body);
            match send_stream_request(req, provider.request_timeout).await {
                Ok(resp) if resp.status().is_success() => {
                    let status =
                        StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
                    // 上游可能 200 却回 application/json（假 SSE），直接透传会让客户端 SSE 解析器空读
                    // 检测到就抛失败，让上层 failover 换下个渠道
                    if let Err(msg) = validate_upstream_sse_content_type(resp.headers()) {
                        let text = resp.text().await.unwrap_or_default();
                        return Err(ProxyError::new(
                            StatusCode::BAD_GATEWAY,
                            format!("{} body={}", msg, clean_upstream_error(&text)),
                        ));
                    }
                    let probe_kind = if path == "/responses" {
                        SseProbeKind::Responses
                    } else {
                        SseProbeKind::Chat
                    };
                    let stream = match prepare_openai_stream(
                        resp,
                        provider.request_timeout,
                        provider.stream_idle_timeout,
                        provider.stream_max_duration,
                        probe_kind,
                    )
                    .await
                    {
                        Ok(stream) => stream,
                        Err(err) if attempt < provider.max_retries && err.retryable() => {
                            tokio::time::sleep(retry_delay(attempt)).await;
                            attempt += 1;
                            continue;
                        }
                        Err(err) => return Err(err),
                    };
                    let (usage_rx, body_stream) = stream_with_usage_probe(
                        stream,
                        probe_kind,
                        started,
                        state.raw_sse_capture_for(provider),
                    );
                    let body_stream: ProxyByteStream =
                        if provider.strip_thought && path == "/chat/completions" {
                            Box::pin(strip_thought_from_chat_sse_stream(body_stream))
                        } else {
                            Box::pin(body_stream)
                        };
                    let response = Response::builder()
                        .status(status)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .body(Body::from_stream(body_stream))
                        .map_err(|e| ProxyError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
                    return Ok(ProviderResult {
                        response,
                        upstream_model,
                        usage: TokenUsage::default(),
                        usage_rx: Some(usage_rx),
                    });
                }
                Ok(resp) => {
                    let status = StatusCode::from_u16(resp.status().as_u16())
                        .unwrap_or(StatusCode::BAD_GATEWAY);
                    let retry_after = parse_retry_after(resp.headers());
                    let text = resp.text().await.unwrap_or_default();
                    // 首次注入即遇兼容错误：回退到不注入版本重试一次
                    // 立即回退到"不注入"版本重试一次，不消耗 max_retries 名额，永久标记该 provider
                    if probing && usage_injection_rejected(status, &text) {
                        state.mark_usage_injection_unsupported(provider);
                        probing = false;
                        send_body = body.clone();
                        continue;
                    }
                    if attempt < provider.max_retries && retryable_status(status) {
                        tokio::time::sleep(compute_retry_delay(attempt, retry_after)).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(upstream_status_error(status, &text, retry_after));
                }
                Err(err) if attempt < provider.max_retries => {
                    tokio::time::sleep(retry_delay(attempt)).await;
                    attempt += 1;
                    if err.retryable() {
                        continue;
                    }
                    return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.message()));
                }
                Err(err) => return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.message())),
            }
        }
    }

    let request_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut value = send_json_to_provider(&client, provider, path, request_headers, body).await?;
    // 上游可能返回 HTTP 200 + 假 JSON（rawchat 一类跑路中转站的典型症状）
    // 结构不合规就当作该 provider 失败，抛 502 让上层 failover 到下个渠道
    let validation = match path {
        "/chat/completions" | "/completions" => validate_upstream_chat_json(&value),
        "/responses" => validate_upstream_response_json(&value),
        // 其它路径（如 /embeddings）没有统一的强校验字段，放行
        _ => Ok(()),
    };
    if let Err(msg) = validation {
        return Err(ProxyError::new(StatusCode::BAD_GATEWAY, msg));
    }
    if provider.strip_thought && path == "/chat/completions" {
        strip_thought_from_chat_json(&mut value);
    }
    let upstream_model = response_model(&value).unwrap_or(request_model);
    let usage = extract_token_usage(&value);
    Ok(ProviderResult {
        response: (StatusCode::OK, Json(value)).into_response(),
        upstream_model,
        usage,
        usage_rx: None,
    })
}

// 入站 Anthropic Messages API 原生直通：客户端已经说 Anthropic 协议，
// 请求体和响应体都不做翻译，只做渠道选择、鉴权头替换、重试与 usage 统计。
pub(super) async fn send_anthropic_messages_passthrough(
    client: &Client,
    provider: &ProviderConfig,
    request_headers: &HeaderMap,
    body: Value,
    stream: bool,
    started: Instant,
) -> Result<ProviderResult, ProxyError> {
    let request_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let url = upstream_url(provider, "/messages");
    let req = client
        .post(url)
        .headers(upstream_headers(provider, request_headers, stream))
        .json(&body);

    for attempt in 0..=provider.max_retries {
        let send_result = if stream {
            send_stream_request(req.try_clone().unwrap(), provider.request_timeout).await
        } else {
            req.try_clone()
                .unwrap()
                .timeout(Duration::from_secs(provider.request_timeout.max(1)))
                .send()
                .await
                .map_err(UpstreamSendError::Request)
        };
        match send_result {
            Ok(resp) if resp.status().is_success() => {
                let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
                if stream {
                    if let Err(msg) = validate_upstream_sse_content_type(resp.headers()) {
                        let text = resp.text().await.unwrap_or_default();
                        return Err(ProxyError::new(
                            StatusCode::BAD_GATEWAY,
                            format!("{} body={}", msg, clean_upstream_error(&text)),
                        ));
                    }
                    let upstream = match prepare_openai_stream(
                        resp,
                        provider.request_timeout,
                        provider.stream_idle_timeout,
                        provider.stream_max_duration,
                        SseProbeKind::Anthropic,
                    )
                    .await
                    {
                        Ok(stream) => stream,
                        Err(err) if attempt < provider.max_retries && err.retryable() => {
                            tokio::time::sleep(retry_delay(attempt)).await;
                            continue;
                        }
                        Err(err) => return Err(err),
                    };
                    // 原样透传字节流，只旁路解析 usage / 完成事件 / 流内错误
                    let (usage_rx, body_stream) =
                        stream_with_usage_probe(upstream, SseProbeKind::Anthropic, started, None);
                    let response = Response::builder()
                        .status(status)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .header(header::CACHE_CONTROL, "no-cache")
                        .body(Body::from_stream(body_stream))
                        .map_err(|e| ProxyError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
                    return Ok(ProviderResult {
                        response,
                        upstream_model: request_model,
                        usage: TokenUsage::default(),
                        usage_rx: Some(usage_rx),
                    });
                }
                let value = resp.json::<Value>().await.map_err(|e| {
                    ProxyError::new(
                        StatusCode::BAD_GATEWAY,
                        format!("Anthropic JSON 解析失败: {e}"),
                    )
                })?;
                // 拦截 200 + 假 JSON（缺 content 数组），交给上层 failover
                if let Err(msg) = validate_upstream_anthropic_json(&value) {
                    return Err(ProxyError::new(StatusCode::BAD_GATEWAY, msg));
                }
                let upstream_model = response_model(&value).unwrap_or(request_model);
                let usage = extract_token_usage(&value);
                return Ok(ProviderResult {
                    response: (StatusCode::OK, Json(value)).into_response(),
                    upstream_model,
                    usage,
                    usage_rx: None,
                });
            }
            Ok(resp) => {
                let status =
                    StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let retry_after = parse_retry_after(resp.headers());
                let text = resp.text().await.unwrap_or_default();
                if attempt < provider.max_retries && retryable_status(status) {
                    tokio::time::sleep(compute_retry_delay(attempt, retry_after)).await;
                    continue;
                }
                return Err(upstream_status_error(status, &text, retry_after));
            }
            Err(err) if attempt < provider.max_retries => {
                tokio::time::sleep(retry_delay(attempt)).await;
                if err.retryable() {
                    continue;
                }
                return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.message()));
            }
            Err(err) => return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.message())),
        }
    }
    Err(ProxyError::new(
        StatusCode::BAD_GATEWAY,
        "Anthropic 上游请求失败",
    ))
}

// Anthropic 渠道专用：OpenAI chat 请求翻译到 /messages，响应/流反向翻译回 OpenAI
pub(super) async fn send_to_anthropic_provider(
    client: &Client,
    provider: &ProviderConfig,
    request_headers: &HeaderMap,
    body: Value,
    stream: bool,
    started: Instant,
) -> Result<ProviderResult, ProxyError> {
    let request_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut anthropic_body = crate::anthropic::openai_to_anthropic_request(&body)
        .map_err(|err| ProxyError::new(StatusCode::BAD_REQUEST, format!("协议翻译失败: {err}")))?;
    if let Some(obj) = anthropic_body.as_object_mut() {
        obj.insert("stream".into(), Value::Bool(stream));
    }
    let url = upstream_url(provider, "/messages");
    let req = client
        .post(url)
        .headers(upstream_headers(provider, request_headers, stream))
        .json(&anthropic_body);

    for attempt in 0..=provider.max_retries {
        let send_result = if stream {
            send_stream_request(req.try_clone().unwrap(), provider.request_timeout).await
        } else {
            req.try_clone()
                .unwrap()
                .timeout(Duration::from_secs(provider.request_timeout.max(1)))
                .send()
                .await
                .map_err(UpstreamSendError::Request)
        };
        match send_result {
            Ok(resp) if resp.status().is_success() => {
                let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
                if stream {
                    // 拦截假流响应（Content-Type 不是 SSE）
                    if let Err(msg) = validate_upstream_sse_content_type(resp.headers()) {
                        let text = resp.text().await.unwrap_or_default();
                        return Err(ProxyError::new(
                            StatusCode::BAD_GATEWAY,
                            format!("{} body={}", msg, clean_upstream_error(&text)),
                        ));
                    }
                    let (usage_rx, body_stream) = match prepare_anthropic_stream(
                        resp,
                        request_model.clone(),
                        provider.request_timeout,
                        provider.stream_idle_timeout,
                        provider.stream_max_duration,
                        started,
                    )
                    .await
                    {
                        Ok(streams) => streams,
                        Err(err) if attempt < provider.max_retries && err.retryable() => {
                            tokio::time::sleep(retry_delay(attempt)).await;
                            continue;
                        }
                        Err(err) => return Err(err),
                    };
                    let body_stream: ProxyByteStream = if provider.strip_thought {
                        Box::pin(strip_thought_from_chat_sse_stream(body_stream))
                    } else {
                        Box::pin(body_stream)
                    };
                    let response = Response::builder()
                        .status(status)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .header(header::CACHE_CONTROL, "no-cache")
                        .body(Body::from_stream(body_stream))
                        .map_err(|e| ProxyError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
                    return Ok(ProviderResult {
                        response,
                        upstream_model: request_model,
                        usage: TokenUsage::default(),
                        usage_rx: Some(usage_rx),
                    });
                }
                let value = resp.json::<Value>().await.map_err(|e| {
                    ProxyError::new(
                        StatusCode::BAD_GATEWAY,
                        format!("Anthropic JSON 解析失败: {e}"),
                    )
                })?;
                // 拦截假响应（缺 content 数组等）
                if let Err(msg) = validate_upstream_anthropic_json(&value) {
                    return Err(ProxyError::new(StatusCode::BAD_GATEWAY, msg));
                }
                let mut translated =
                    crate::anthropic::anthropic_to_openai_response(value, &request_model);
                if provider.strip_thought {
                    strip_thought_from_chat_json(&mut translated);
                }
                let upstream_model = translated
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or(&request_model)
                    .to_string();
                let usage = extract_token_usage(&translated);
                return Ok(ProviderResult {
                    response: (StatusCode::OK, Json(translated)).into_response(),
                    upstream_model,
                    usage,
                    usage_rx: None,
                });
            }
            Ok(resp) => {
                let status =
                    StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let retry_after = parse_retry_after(resp.headers());
                let text = resp.text().await.unwrap_or_default();
                if attempt < provider.max_retries && retryable_status(status) {
                    tokio::time::sleep(compute_retry_delay(attempt, retry_after)).await;
                    continue;
                }
                return Err(upstream_status_error(status, &text, retry_after));
            }
            Err(err) if attempt < provider.max_retries => {
                tokio::time::sleep(retry_delay(attempt)).await;
                if err.retryable() {
                    continue;
                }
                return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.message()));
            }
            Err(err) => return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.message())),
        }
    }
    Err(ProxyError::new(
        StatusCode::BAD_GATEWAY,
        "Anthropic 上游请求失败",
    ))
}

// Google AI Studio 原生渠道专用：OpenAI chat 请求翻译到 GenerateContent，响应/流反向翻译回 OpenAI
pub(super) async fn send_to_google_ai_provider(
    client: &Client,
    provider: &ProviderConfig,
    request_headers: &HeaderMap,
    body: Value,
    stream: bool,
    started: Instant,
) -> Result<ProviderResult, ProxyError> {
    let request_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let google_body = crate::google_ai::openai_to_google_request(&body)
        .map_err(|err| ProxyError::new(StatusCode::BAD_REQUEST, format!("协议翻译失败: {err}")))?;
    let path = crate::google_ai::generate_content_path(&request_model, stream);
    let url = upstream_url(provider, &path);
    let req = client
        .post(url)
        .headers(google_ai_headers(provider, request_headers, stream))
        .json(&google_body);

    for attempt in 0..=provider.max_retries {
        let send_result = if stream {
            send_stream_request(req.try_clone().unwrap(), provider.request_timeout).await
        } else {
            req.try_clone()
                .unwrap()
                .timeout(Duration::from_secs(provider.request_timeout.max(1)))
                .send()
                .await
                .map_err(UpstreamSendError::Request)
        };
        match send_result {
            Ok(resp) if resp.status().is_success() => {
                let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
                if stream {
                    if let Err(msg) = validate_upstream_sse_content_type(resp.headers()) {
                        let text = resp.text().await.unwrap_or_default();
                        return Err(ProxyError::new(
                            StatusCode::BAD_GATEWAY,
                            format!("{} body={}", msg, clean_upstream_error(&text)),
                        ));
                    }
                    let (usage_rx, body_stream) = crate::google_ai::spawn_stream_translator(
                        resp.bytes_stream(),
                        request_model.clone(),
                        started,
                    );
                    let body_stream: ProxyByteStream = if provider.strip_thought {
                        Box::pin(strip_thought_from_chat_sse_stream(body_stream))
                    } else {
                        Box::pin(body_stream)
                    };
                    let response = Response::builder()
                        .status(status)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .header(header::CACHE_CONTROL, "no-cache")
                        .body(Body::from_stream(body_stream))
                        .map_err(|e| ProxyError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
                    return Ok(ProviderResult {
                        response,
                        upstream_model: request_model,
                        usage: TokenUsage::default(),
                        usage_rx: Some(usage_rx),
                    });
                }
                let value = resp.json::<Value>().await.map_err(|e| {
                    ProxyError::new(
                        StatusCode::BAD_GATEWAY,
                        format!("Google AI JSON 解析失败: {e}"),
                    )
                })?;
                if value.get("candidates").is_none() {
                    return Err(ProxyError::new(
                        StatusCode::BAD_GATEWAY,
                        "Google AI 响应缺少 candidates",
                    ));
                }
                let mut translated =
                    crate::google_ai::google_to_openai_response(value, &request_model);
                if provider.strip_thought {
                    strip_thought_from_chat_json(&mut translated);
                }
                let usage = extract_token_usage(&translated);
                return Ok(ProviderResult {
                    response: (StatusCode::OK, Json(translated)).into_response(),
                    upstream_model: request_model,
                    usage,
                    usage_rx: None,
                });
            }
            Ok(resp) => {
                let status =
                    StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let retry_after = parse_retry_after(resp.headers());
                let text = resp.text().await.unwrap_or_default();
                if attempt < provider.max_retries && retryable_status(status) {
                    tokio::time::sleep(compute_retry_delay(attempt, retry_after)).await;
                    continue;
                }
                return Err(upstream_status_error(status, &text, retry_after));
            }
            Err(err) if attempt < provider.max_retries => {
                tokio::time::sleep(retry_delay(attempt)).await;
                if err.retryable() {
                    continue;
                }
                return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.message()));
            }
            Err(err) => return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.message())),
        }
    }
    Err(ProxyError::new(
        StatusCode::BAD_GATEWAY,
        "Google AI 上游请求失败",
    ))
}

// 原样发送 Responses 请求体，只把上游 Responses SSE 翻译成内部统一使用的 Chat SSE。
// Codex 兼容上游可能严格校验请求体，不能先转 Chat 再重建 Responses。
pub(super) async fn send_responses_stream_as_chat(
    state: &AppState,
    provider: &ProviderConfig,
    request_headers: &HeaderMap,
    responses_body: Value,
    started: Instant,
) -> Result<ProviderResult, ProxyError> {
    let request_model = responses_body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let url = upstream_url(provider, "/responses");
    let mut attempt = 0;
    loop {
        let client = state.client_for_provider(provider);
        let result = client
            .post(url.clone())
            .headers(upstream_headers(provider, request_headers, true))
            .json(&responses_body);
        let result = send_stream_request(result, provider.request_timeout).await;
        match result {
            Ok(resp) if resp.status().is_success() => {
                let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
                if let Err(msg) = validate_upstream_sse_content_type(resp.headers()) {
                    let text = resp.text().await.unwrap_or_default();
                    return Err(ProxyError::new(
                        StatusCode::BAD_GATEWAY,
                        format!("{} body={}", msg, clean_upstream_error(&text)),
                    ));
                }
                let stream = match prepare_openai_stream(
                    resp,
                    provider.request_timeout,
                    provider.stream_idle_timeout,
                    provider.stream_max_duration,
                    SseProbeKind::Responses,
                )
                .await
                {
                    Ok(stream) => stream,
                    Err(err) if attempt < provider.max_retries && err.retryable() => {
                        tokio::time::sleep(retry_delay(attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    Err(err) => return Err(err),
                };
                let (usage_rx, body_stream) =
                    crate::responses_api::spawn_responses_stream_translator_from_stream(
                        stream,
                        request_model.clone(),
                        started,
                    );
                let response = Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .body(Body::from_stream(body_stream))
                    .map_err(|err| ProxyError::new(StatusCode::BAD_GATEWAY, err.to_string()))?;
                return Ok(ProviderResult {
                    response,
                    upstream_model: request_model,
                    usage: TokenUsage::default(),
                    usage_rx: Some(usage_rx),
                });
            }
            Ok(resp) => {
                let status =
                    StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let retry_after = parse_retry_after(resp.headers());
                let text = resp.text().await.unwrap_or_default();
                if attempt < provider.max_retries && retryable_status(status) {
                    tokio::time::sleep(compute_retry_delay(attempt, retry_after)).await;
                    attempt += 1;
                    continue;
                }
                return Err(upstream_status_error(status, &text, retry_after));
            }
            Err(err) if attempt < provider.max_retries => {
                tokio::time::sleep(retry_delay(attempt)).await;
                attempt += 1;
                if err.retryable() {
                    continue;
                }
                return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.message()));
            }
            Err(err) => return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.message())),
        }
    }
}

// 客户端 chat/completions -> 上游 /responses 反向翻译发送。
// 用于 forward_openai 的 auto fallback：上游拒绝 chat/completions 时降级到 responses。
pub(super) async fn send_chat_via_responses(
    state: &AppState,
    provider: &ProviderConfig,
    request_headers: &HeaderMap,
    body: Value,
    stream: bool,
    started: Instant,
) -> Result<ProviderResult, ProxyError> {
    let client = state.client_for_provider(provider);
    let request_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut responses_body =
        crate::responses_api::chat_to_responses_request(&body).map_err(|err| {
            ProxyError::new(
                StatusCode::BAD_REQUEST,
                format!("Chat->Responses 翻译失败: {err}"),
            )
        })?;
    if let Some(obj) = responses_body.as_object_mut() {
        obj.insert("stream".into(), Value::Bool(stream));
    }
    if stream {
        return send_responses_stream_as_chat(
            state,
            provider,
            request_headers,
            responses_body,
            started,
        )
        .await;
    }
    let url = upstream_url(provider, "/responses");
    let mut attempt: usize = 0;
    loop {
        let req = client
            .post(url.clone())
            .timeout(Duration::from_secs(provider.request_timeout.max(1)))
            .headers(upstream_headers(provider, request_headers, false))
            .json(&responses_body);
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                let value = resp.json::<Value>().await.map_err(|e| {
                    ProxyError::new(
                        StatusCode::BAD_GATEWAY,
                        format!("Responses JSON 解析失败: {e}"),
                    )
                })?;
                // 校验上游 /responses 原始响应结构；假响应就抛失败让上层 failover
                if let Err(msg) = validate_upstream_response_json(&value) {
                    return Err(ProxyError::new(StatusCode::BAD_GATEWAY, msg));
                }
                let translated =
                    crate::responses_api::responses_to_chat_response(value, &request_model);
                let upstream_model = translated
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or(&request_model)
                    .to_string();
                let usage = extract_token_usage(&translated);
                return Ok(ProviderResult {
                    response: (StatusCode::OK, Json(translated)).into_response(),
                    upstream_model,
                    usage,
                    usage_rx: None,
                });
            }
            Ok(resp) => {
                let status =
                    StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let retry_after = parse_retry_after(resp.headers());
                let text = resp.text().await.unwrap_or_default();
                if attempt < provider.max_retries && retryable_status(status) {
                    tokio::time::sleep(compute_retry_delay(attempt, retry_after)).await;
                    attempt += 1;
                    continue;
                }
                return Err(upstream_status_error(status, &text, retry_after));
            }
            Err(err) if attempt < provider.max_retries => {
                tokio::time::sleep(retry_delay(attempt)).await;
                attempt += 1;
                if err.is_timeout() || err.is_connect() || err.is_request() {
                    continue;
                }
                return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.to_string()));
            }
            Err(err) => return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.to_string())),
        }
    }
}

pub(super) async fn send_json_to_provider(
    client: &Client,
    provider: &ProviderConfig,
    path: &str,
    request_headers: &HeaderMap,
    body: Value,
) -> Result<Value, ProxyError> {
    send_json_to_provider_with_headers(
        client,
        provider,
        path,
        upstream_headers(provider, request_headers, false),
        body,
    )
    .await
}

pub(super) async fn send_json_to_provider_with_headers(
    client: &Client,
    provider: &ProviderConfig,
    path: &str,
    headers: HeaderMap,
    body: Value,
) -> Result<Value, ProxyError> {
    let url = upstream_url(provider, path);
    for attempt in 0..=provider.max_retries {
        let result = client
            .post(url.clone())
            .timeout(Duration::from_secs(provider.request_timeout.max(1)))
            .headers(headers.clone())
            .json(&body)
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                return resp.json::<Value>().await.map_err(|e| {
                    ProxyError::new(StatusCode::BAD_GATEWAY, format!("上游 JSON 解析失败: {e}"))
                });
            }
            Ok(resp) => {
                let status =
                    StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let retry_after = parse_retry_after(resp.headers());
                let text = resp.text().await.unwrap_or_default();
                if attempt < provider.max_retries && retryable_status(status) {
                    tokio::time::sleep(compute_retry_delay(attempt, retry_after)).await;
                    continue;
                }
                return Err(upstream_status_error(status, &text, retry_after));
            }
            Err(err) if attempt < provider.max_retries => {
                tokio::time::sleep(retry_delay(attempt)).await;
                if err.is_timeout() || err.is_connect() || err.is_request() {
                    continue;
                }
                return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.to_string()));
            }
            Err(err) => return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.to_string())),
        }
    }
    Err(ProxyError::new(StatusCode::BAD_GATEWAY, "上游请求失败"))
}

pub(super) fn upstream_url(provider: &ProviderConfig, path: &str) -> String {
    format!("{}{}", provider.base_url.trim_end_matches('/'), path)
}

pub(super) fn is_google_native_provider(provider: &ProviderConfig) -> bool {
    provider.provider_type == "google_ai_studio" && !is_google_openai_endpoint(&provider.base_url)
}

pub(super) fn google_ai_headers(
    provider: &ProviderConfig,
    request_headers: &HeaderMap,
    stream: bool,
) -> HeaderMap {
    let mut headers = upstream_safe_headers(request_headers);
    if let Ok(value) = HeaderValue::from_str(&provider.api_key) {
        headers.insert("x-goog-api-key", value);
    }
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static(if stream {
            "text/event-stream"
        } else {
            "application/json"
        }),
    );
    if stream {
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
    }
    for (name, value) in &provider.extra_headers {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "authorization"
                | "content-type"
                | "accept"
                | "host"
                | "content-length"
                | "x-api-key"
                | "x-goog-api-key"
        ) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

pub(super) fn upstream_headers(
    provider: &ProviderConfig,
    request_headers: &HeaderMap,
    stream: bool,
) -> HeaderMap {
    let mut headers = HeaderMap::new();

    // 渠道配置是可信配置，允许显式覆盖上游认证；客户端认证头仍在下方被拦截。
    for (name, value) in &provider.extra_headers {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "content-type" | "accept" | "host" | "content-length"
        ) {
            continue;
        }
        let value = value
            .replace("{api_key}", &provider.api_key)
            .replace("{access_token}", &provider.api_key);
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }

    if provider.provider_type == "anthropic" {
        if !headers.contains_key(header::AUTHORIZATION) && !headers.contains_key("x-api-key") {
            headers.insert(
                "x-api-key",
                HeaderValue::from_str(&provider.api_key)
                    .unwrap_or_else(|_| HeaderValue::from_static("")),
            );
        }
        if !headers.contains_key("anthropic-version") {
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        }
    } else if !headers.contains_key(header::AUTHORIZATION) {
        if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", provider.api_key)) {
            headers.insert(header::AUTHORIZATION, value);
        }
    }
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static(if stream {
            "text/event-stream"
        } else {
            "application/json"
        }),
    );
    if stream {
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
    }

    // 客户端安全头透传：
    //
    // 部分上游（rawchat / anyrouter 等 codex 兼容中转）会**校验 codex CLI 签名头**——
    // 完整的一整套 `x-stainless-*`、`x-codex-*`、`openai-client`、`x-request-id` 等
    // 缺任何一个都可能被服务端识别为"非 codex CLI"而 403 codex_access_restricted。
    //
    // 之前只透传固定 5 个头（`user-agent` 等），把 codex CLI 的签名头全吃掉了，
    // 这里改为**白名单前缀 + 少量精确名 + 强黑名单**三段式：
    //   1. 命中前缀（codex-/openai-/x-codex-/x-openai-/x-stainless-）→ 透传
    //   2. 命中精确名（openai-client / x-request-id / …）→ 透传
    //   3. 命中黑名单（authorization/cookie/host/content-length/… 敏感头）→ 拒绝，
    //      连白名单也不能压过黑名单（防止 authorization 通过 openai- 前缀溜过去）
    for (name, value) in request_headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();

        // 黑名单：绝对不能透传给上游
        if matches!(
            lower.as_str(),
            "authorization"
                | "proxy-authorization"
                | "api-key"
                | "openai-api-key"
                | "x-api-key"
                | "x-api-token"
                | "x-auth-token"
                | "x-goog-api-key"
                | "cookie"
                | "host"
                | "content-length"
                | "connection"
                | "transfer-encoding"
                | "content-type"
                | "accept"
                | "accept-encoding"
        ) {
            continue;
        }

        // 前缀白名单：codex-cli / openai SDK / stainless 系列客户端签名
        // anthropic-*：入站 Messages API 客户端（Claude Code / Anthropic SDK）会发
        // anthropic-version 与 anthropic-beta，上游按这两个头决定协议版本和特性开关，
        // 吃掉会导致 beta 特性静默失效。
        let prefix_match = lower.starts_with("codex-")
            || lower.starts_with("openai-")
            || lower.starts_with("x-codex-")
            || lower.starts_with("x-openai-")
            || lower.starts_with("x-stainless-")
            || lower.starts_with("anthropic-")
            || lower.starts_with("chatgpt-");

        // 精确名白名单：不带前缀但需要透传的少数几个
        // - user-agent / originator：codex CLI 客户端标识
        // - x-request-id：Stainless SDK 生成的请求 id
        // - session_id / conversation_id：codex CLI 会话标识
        //   （裸名字，anyrouter/rawchat 校验 codex 请求时会检查 session_id 是否存在，
        //   缺失会回 "invalid codex request"）
        let exact_match = matches!(
            lower.as_str(),
            "user-agent" | "x-request-id" | "originator" | "session_id" | "conversation_id"
        );

        if !prefix_match && !exact_match {
            continue;
        }

        // extra_headers 是渠道级显式配置；同名客户端头不覆盖它。
        if provider
            .extra_headers
            .keys()
            .any(|extra| extra.eq_ignore_ascii_case(name.as_str()))
        {
            continue;
        }
        headers.insert(name.clone(), value.clone());
    }
    headers
}

pub(super) fn upstream_safe_headers(request_headers: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in request_headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();

        if matches!(
            lower.as_str(),
            "authorization"
                | "proxy-authorization"
                | "api-key"
                | "openai-api-key"
                | "x-api-key"
                | "x-api-token"
                | "x-auth-token"
                | "x-goog-api-key"
                | "cookie"
                | "host"
                | "content-length"
                | "connection"
                | "transfer-encoding"
                | "content-type"
                | "accept"
                | "accept-encoding"
        ) {
            continue;
        }

        let prefix_match = lower.starts_with("codex-")
            || lower.starts_with("openai-")
            || lower.starts_with("x-codex-")
            || lower.starts_with("x-openai-")
            || lower.starts_with("x-stainless-")
            || lower.starts_with("chatgpt-");
        let exact_match = matches!(
            lower.as_str(),
            "user-agent" | "x-request-id" | "originator" | "session_id" | "conversation_id"
        );

        if prefix_match || exact_match {
            headers.insert(name.clone(), value.clone());
        }
    }
    headers
}

pub(crate) fn retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
        || status == StatusCode::BAD_GATEWAY
}

/// 判断一个上游错误是否应该「立即中止整个故障转移链」并返回给客户端。
///
/// 客户端访问本代理的鉴权错误在 `authorize` 阶段已经返回，不会进入 provider 循环。
/// 进入这里的 401/407 都来自某个上游渠道，通常表示该渠道的 key / 代理不可用；
/// 这种情况应该继续尝试下一个渠道，而不是中止整个 failover。
pub(crate) fn should_stop_failover(_status: StatusCode) -> bool {
    false
}

pub(crate) fn retry_delay(attempt: usize) -> Duration {
    // 指数退避：200ms * 2^attempt，封顶 2s
    let base = (200_u64)
        .saturating_mul(2_u64.saturating_pow(attempt as u32))
        .min(2_000);
    // ±50% jitter，避免多客户端同步重试放大风暴
    let jitter_factor = rand::random::<f64>() - 0.5; // -0.5 ~ +0.5
    let jittered = (base as f64 * (1.0 + jitter_factor)).max(50.0) as u64;
    Duration::from_millis(jittered)
}

// 解析上游 Retry-After 头，兼容 "秒数" 与 HTTP-date 两种表达
pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if raw.is_empty() {
        return None;
    }
    // 兜住上游给出离谱大值的情况
    const CAP: Duration = Duration::from_secs(60);
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(secs).min(CAP));
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(raw) {
        let delta = dt
            .with_timezone(&chrono::Utc)
            .signed_duration_since(chrono::Utc::now());
        if let Ok(d) = delta.to_std() {
            return Some(d.min(CAP));
        }
    }
    None
}

// 优先服从上游 Retry-After，否则走本地带 jitter 的退避
pub(crate) fn compute_retry_delay(attempt: usize, retry_after: Option<Duration>) -> Duration {
    if let Some(hint) = retry_after {
        // 加一点点抖动避免同一秒集中撞击
        let extra = (rand::random::<f64>() * 250.0) as u64;
        return hint + Duration::from_millis(extra);
    }
    retry_delay(attempt)
}

pub(super) fn usage_injection_rejected(status: StatusCode, body: &str) -> bool {
    if !status.is_client_error() {
        return false;
    }
    let lower = body.to_ascii_lowercase();
    lower.contains("include_usage") || lower.contains("stream_options")
}

pub(super) fn normalize_model(model: &str) -> &str {
    model.strip_prefix("models/").unwrap_or(model)
}

pub(crate) fn truncate(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() > 500 {
        format!(
            "{}...(truncated)",
            trimmed.chars().take(500).collect::<String>()
        )
    } else {
        trimmed.to_string()
    }
}

/// 从上游返回的 JSON 错误体中提取可读的错误消息。
/// 如 `{"error":{"message":"invalid codex request (id: ...)"}}` → `"invalid codex request (id: ...)"`。
/// 若解析失败则回退到 truncate。
pub(crate) fn clean_upstream_error(text: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        // 尝试提取 error.message
        if let Some(msg) = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return truncate(msg);
        }
        // 尝试提取顶层 message
        if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
            return truncate(msg);
        }
    }
    truncate(text)
}

// ================== 上游响应体健康性校验 ==================
//
// 部分上游（比如 rawchat 这类跑路后仍返回 HTTP 200 的中转站）会用 200 状态码
// 配一段假 JSON `{"code":0,"msg":"…链路已关闭","data":null}` 蒙混过关。仅看
// status 会误判成成功，导致客户端拿到"看着合法但没有 choices/output"的响应，
// 表现为 opencode / codex "问了没答案"。
//
// 这里几个校验器都返回 `Result<(), String>`：
//   - Ok(())            = 结构完整，可以透传给客户端
//   - Err(msg)          = 结构不对，调用方应把它当作 provider 失败，触发 failover
//
// 语义只做「有没有」而不做「够不够好」——避免过严把合法但空文本的响应也拒了。

/// 上游 chat/completions 响应至少应包含非空的 `choices` 数组（无论 delta / message）。
/// 否则视作假响应。
pub(crate) fn validate_upstream_chat_json(body: &Value) -> Result<(), String> {
    // 显式 error 字段：即便 status 是 200，也算失败
    if let Some(err) = body.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_else(|| err.as_str().unwrap_or("unknown upstream error"));
        return Err(format!("上游返回错误: {}", truncate(msg)));
    }
    match body.get("choices").and_then(Value::as_array) {
        Some(arr) if !arr.is_empty() => Ok(()),
        _ => Err(format!(
            "上游响应缺少 choices 字段（疑似假响应）: {}",
            clean_upstream_error(&body.to_string())
        )),
    }
}

/// 上游 /responses 响应至少应包含 `output`（数组或对象）或 `output_text`。
pub(crate) fn validate_upstream_response_json(body: &Value) -> Result<(), String> {
    if let Some(err) = body.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_else(|| err.as_str().unwrap_or("unknown upstream error"));
        return Err(format!("上游返回错误: {}", truncate(msg)));
    }
    let has_output = body.get("output").is_some_and(|v| !v.is_null());
    let has_text = body
        .get("output_text")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if has_output || has_text {
        Ok(())
    } else {
        Err(format!(
            "上游响应缺少 output/output_text 字段（疑似假响应）: {}",
            clean_upstream_error(&body.to_string())
        ))
    }
}

/// 上游 /messages 响应至少应包含非空的 `content` 数组。
pub(crate) fn validate_upstream_anthropic_json(body: &Value) -> Result<(), String> {
    if let Some(err) = body.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_else(|| err.as_str().unwrap_or("unknown upstream error"));
        return Err(format!("上游返回错误: {}", truncate(msg)));
    }
    match body.get("content").and_then(Value::as_array) {
        Some(arr) if !arr.is_empty() => Ok(()),
        _ => Err(format!(
            "上游响应缺少 content 字段（疑似假响应）: {}",
            clean_upstream_error(&body.to_string())
        )),
    }
}

/// 上游流式响应的 Content-Type 必须是 SSE 相关类型。
/// rawchat 这类跑路上游对流式请求也回 `application/json` 假响应，靠这个能挡住。
///
/// 语义放宽为「包含 event-stream 关键字」，兼容 `text/event-stream; charset=utf-8` 等变体。
/// 没有 Content-Type 头的按 SSE 处理（部分老上游可能不发这个头，避免误伤）。
pub(crate) fn validate_upstream_sse_content_type(
    headers: &reqwest::header::HeaderMap,
) -> Result<(), String> {
    let Some(raw) = headers.get(reqwest::header::CONTENT_TYPE) else {
        return Ok(());
    };
    let value = raw.to_str().unwrap_or("").to_ascii_lowercase();
    if value.is_empty() || value.contains("event-stream") || value.contains("text/plain") {
        Ok(())
    } else {
        Err(format!(
            "上游流式响应 Content-Type 异常（疑似假响应）: {}",
            truncate(&value)
        ))
    }
}

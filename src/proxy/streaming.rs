use super::*;

pub(super) fn chat_stream_to_responses(
    resp: reqwest::Response,
    request_model: String,
    custom_tool_names: HashSet<String>,
    started: Instant,
    strip_thought: bool,
) -> Result<ProviderResult, ProxyError> {
    let stream = resp
        .bytes_stream()
        .map(|chunk| chunk.map_err(|err| io::Error::other(err.to_string())));
    let stream: ProxyByteStream = if strip_thought {
        Box::pin(strip_thought_from_chat_sse_stream(stream))
    } else {
        Box::pin(stream)
    };
    chat_sse_stream_to_responses(stream, request_model, custom_tool_names, started)
}

pub(super) async fn prepare_anthropic_stream(
    resp: reqwest::Response,
    request_model: String,
    request_timeout: u64,
    stream_idle_timeout: u64,
    stream_max_duration: u64,
    started: Instant,
) -> Result<
    (
        oneshot::Receiver<StreamOutcome>,
        ReceiverStream<Result<Bytes, io::Error>>,
    ),
    ProxyError,
> {
    let upstream = Box::pin(
        resp.bytes_stream()
            .map(|chunk| chunk.map_err(|err| io::Error::other(err.to_string()))),
    );
    let stream = prepare_sse_stream(upstream, request_timeout, SseProbeKind::Anthropic).await?;
    let stream = apply_stream_watchdog(
        stream,
        stream_idle_timeout,
        stream_max_duration,
        "Anthropic 上游流",
    );
    Ok(crate::anthropic::spawn_stream_translator(
        stream,
        request_model,
        started,
    ))
}

pub(super) async fn prepare_openai_stream(
    resp: reqwest::Response,
    request_timeout: u64,
    stream_idle_timeout: u64,
    stream_max_duration: u64,
    kind: SseProbeKind,
) -> Result<ProxyByteStream, ProxyError> {
    let upstream = Box::pin(
        resp.bytes_stream()
            .map(|chunk| chunk.map_err(|err| io::Error::other(err.to_string()))),
    );
    let stream = prepare_sse_stream(upstream, request_timeout, kind).await?;
    Ok(apply_stream_watchdog(
        stream,
        stream_idle_timeout,
        stream_max_duration,
        "上游流",
    ))
}

pub(super) fn apply_stream_watchdog(
    stream: ProxyByteStream,
    idle_timeout_secs: u64,
    max_duration_secs: u64,
    label: &'static str,
) -> ProxyByteStream {
    if idle_timeout_secs == 0 && max_duration_secs == 0 {
        return stream;
    }

    let idle_timeout = (idle_timeout_secs > 0).then(|| Duration::from_secs(idle_timeout_secs));
    let max_duration = (max_duration_secs > 0).then(|| Duration::from_secs(max_duration_secs));
    let started = Instant::now();
    Box::pin(futures_util::stream::unfold(
        (stream, started, idle_timeout, max_duration, false),
        move |(mut stream, started, idle_timeout, max_duration, done)| async move {
            if done {
                return None;
            }
            let next = async { stream.next().await };
            let result = match (idle_timeout, max_duration) {
                (Some(idle), Some(max)) => {
                    let remaining = max.checked_sub(started.elapsed()).unwrap_or_default();
                    if remaining.is_zero() {
                        Err(format!("{label}超过最大持续时间: {max_duration_secs}s"))
                    } else {
                        let wait = idle.min(remaining);
                        match tokio::time::timeout(wait, next).await {
                            Ok(value) => Ok(value),
                            Err(_) if started.elapsed() >= max => {
                                Err(format!("{label}超过最大持续时间: {max_duration_secs}s"))
                            }
                            Err(_) => Err(format!("{label}空闲超时: {idle_timeout_secs}s")),
                        }
                    }
                }
                (Some(idle), None) => match tokio::time::timeout(idle, next).await {
                    Ok(value) => Ok(value),
                    Err(_) => Err(format!("{label}空闲超时: {idle_timeout_secs}s")),
                },
                (None, Some(max)) => {
                    let remaining = max.checked_sub(started.elapsed()).unwrap_or_default();
                    if remaining.is_zero() {
                        Err(format!("{label}超过最大持续时间: {max_duration_secs}s"))
                    } else {
                        match tokio::time::timeout(remaining, next).await {
                            Ok(value) => Ok(value),
                            Err(_) => Err(format!("{label}超过最大持续时间: {max_duration_secs}s")),
                        }
                    }
                }
                (None, None) => Ok(next.await),
            };

            match result {
                Ok(Some(item)) => {
                    Some((item, (stream, started, idle_timeout, max_duration, false)))
                }
                Ok(None) => None,
                Err(message) => Some((
                    Err(io::Error::new(io::ErrorKind::TimedOut, message)),
                    (stream, started, idle_timeout, max_duration, true),
                )),
            }
        },
    ))
}

#[derive(Clone, Copy)]
pub(super) enum SseProbeKind {
    Chat,
    Responses,
    Anthropic,
}

pub(super) enum SseProbeDecision {
    Continue,
    Ready,
    Error(ProxyError),
}

pub(super) async fn prepare_sse_stream(
    mut upstream: ProxyByteStream,
    timeout_secs: u64,
    kind: SseProbeKind,
) -> Result<ProxyByteStream, ProxyError> {
    let timeout_secs = timeout_secs.max(1);
    let result = tokio::time::timeout(Duration::from_secs(timeout_secs), async move {
        let mut prefix = Vec::new();
        let mut text_buffer = String::new();

        loop {
            match upstream.next().await {
                Some(Ok(bytes)) => {
                    text_buffer.push_str(&String::from_utf8_lossy(&bytes));
                    prefix.extend_from_slice(&bytes);
                    while let Some((event, consumed)) = next_sse_event(&text_buffer) {
                        text_buffer.drain(..consumed);
                        let Some(data) = sse_data(&event) else {
                            continue;
                        };
                        match inspect_sse_probe_event(kind, &data) {
                            SseProbeDecision::Continue => {}
                            SseProbeDecision::Ready => {
                                let prefix = Bytes::from(prefix);
                                let stream = futures_util::stream::once(async move {
                                    Ok::<Bytes, io::Error>(prefix)
                                })
                                .chain(upstream);
                                return Ok(Box::pin(stream) as ProxyByteStream);
                            }
                            SseProbeDecision::Error(err) => return Err(err),
                        }
                    }
                }
                Some(Err(err)) => {
                    return Err(ProxyError::new(
                        StatusCode::BAD_GATEWAY,
                        format!("上游流读取失败: {err}"),
                    ));
                }
                None => {
                    return Err(ProxyError::new(
                        StatusCode::BAD_GATEWAY,
                        "上游流在首个有效输出前断开",
                    ));
                }
            }
        }
    })
    .await;

    match result {
        Ok(result) => result,
        Err(_) => Err(ProxyError::new(
            StatusCode::GATEWAY_TIMEOUT,
            format!("上游流首个有效输出超时: {timeout_secs}s"),
        )),
    }
}

pub(super) fn inspect_sse_probe_event(kind: SseProbeKind, data: &str) -> SseProbeDecision {
    match kind {
        SseProbeKind::Chat => inspect_chat_sse_probe_event(data),
        SseProbeKind::Responses => inspect_responses_sse_probe_event(data),
        SseProbeKind::Anthropic => {
            if let Some(err) = anthropic_sse_error(data) {
                return SseProbeDecision::Error(err);
            }
            if anthropic_sse_has_client_output(data) {
                SseProbeDecision::Ready
            } else {
                SseProbeDecision::Continue
            }
        }
    }
}

pub(super) fn inspect_chat_sse_probe_event(data: &str) -> SseProbeDecision {
    if data.trim() == "[DONE]" {
        return SseProbeDecision::Error(ProxyError::new(
            StatusCode::BAD_GATEWAY,
            "上游流未返回有效输出",
        ));
    }
    if let Some(message) = chat_stream_error_message(data) {
        return SseProbeDecision::Error(ProxyError::new(stream_error_status(&message), message));
    }
    if chat_stream_delta(data).is_some() || !chat_stream_tool_call_deltas(data).is_empty() {
        return SseProbeDecision::Ready;
    }
    SseProbeDecision::Continue
}

pub(super) fn inspect_responses_sse_probe_event(data: &str) -> SseProbeDecision {
    if data.trim() == "[DONE]" {
        return SseProbeDecision::Error(ProxyError::new(
            StatusCode::BAD_GATEWAY,
            "Responses 上游流未返回有效输出",
        ));
    }
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return SseProbeDecision::Continue;
    };
    match value.get("type").and_then(Value::as_str) {
        Some("error") | Some("response.failed") => {
            let message = value
                .pointer("/error/message")
                .or_else(|| value.pointer("/response/error/message"))
                .and_then(Value::as_str)
                .unwrap_or("Responses upstream error");
            SseProbeDecision::Error(ProxyError::new(
                stream_error_status(message),
                truncate(message),
            ))
        }
        Some("response.output_text.delta") => {
            if value
                .get("delta")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty())
            {
                SseProbeDecision::Ready
            } else {
                SseProbeDecision::Continue
            }
        }
        Some("response.output_text.done") => {
            if value
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty())
            {
                SseProbeDecision::Ready
            } else {
                SseProbeDecision::Continue
            }
        }
        Some("response.output_item.added") => {
            let item_type = value.pointer("/item/type").and_then(Value::as_str);
            if matches!(item_type, Some("function_call") | Some("custom_tool_call")) {
                SseProbeDecision::Ready
            } else {
                SseProbeDecision::Continue
            }
        }
        Some("response.function_call_arguments.delta")
        | Some("response.custom_tool_call_input.delta") => {
            if value
                .get("delta")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty())
            {
                SseProbeDecision::Ready
            } else {
                SseProbeDecision::Continue
            }
        }
        Some("response.completed") => SseProbeDecision::Error(ProxyError::new(
            StatusCode::BAD_GATEWAY,
            "Responses 上游流完成但未返回有效输出",
        )),
        _ => SseProbeDecision::Continue,
    }
}

pub(super) fn stream_error_status(message: &str) -> StatusCode {
    let lower = message.to_ascii_lowercase();
    if lower.contains("rate") || lower.contains("limit") || lower.contains("concurrency") {
        StatusCode::TOO_MANY_REQUESTS
    } else if lower.contains("timeout") {
        StatusCode::GATEWAY_TIMEOUT
    } else if lower.contains("overload") || lower.contains("unavailable") {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::BAD_GATEWAY
    }
}

pub(super) fn anthropic_sse_error(data: &str) -> Option<ProxyError> {
    let value = serde_json::from_str::<Value>(data).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("Anthropic upstream error");
    let error_type = value
        .pointer("/error/type")
        .and_then(Value::as_str)
        .unwrap_or("upstream_error");
    let lower = format!("{error_type} {message}").to_ascii_lowercase();
    let status =
        if lower.contains("rate") || lower.contains("limit") || lower.contains("concurrency") {
            StatusCode::TOO_MANY_REQUESTS
        } else if lower.contains("overload") || lower.contains("unavailable") {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::BAD_GATEWAY
        };
    Some(ProxyError::new(status, truncate(message)))
}

pub(super) fn anthropic_sse_has_client_output(data: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return true;
    };
    match value.get("type").and_then(Value::as_str) {
        Some("content_block_start") => {
            value.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use")
        }
        Some("content_block_delta") => match value.pointer("/delta/type").and_then(Value::as_str) {
            Some("text_delta") => value
                .pointer("/delta/text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty()),
            Some("input_json_delta") => true,
            _ => false,
        },
        Some("message_stop") => true,
        _ => false,
    }
}

pub(super) fn chat_sse_stream_to_responses<S>(
    stream: S,
    request_model: String,
    custom_tool_names: HashSet<String>,
    started: Instant,
) -> Result<ProviderResult, ProxyError>
where
    S: Stream<Item = Result<Bytes, io::Error>> + Send + 'static,
{
    let model = if request_model.is_empty() {
        "unknown".to_string()
    } else {
        request_model
    };
    let result_model = model.clone();
    let stream_model = model.clone();
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(32);
    let (u_tx, u_rx) = oneshot::channel::<StreamOutcome>();
    tokio::spawn(async move {
        let mut usage = TokenUsage::default();
        let u_tx = u_tx;
        let response_id = format!("resp-{}", Uuid::new_v4().simple());
        let item_id = format!("msg-{}", Uuid::new_v4().simple());
        let created_at = chrono::Utc::now().timestamp();
        let model = stream_model;
        // 客户端断连后立即置位,后续 send/上游消费全部短路,避免浪费上游 token 与熔断 inflight 计数
        #[allow(unused_assignments)]
        let mut disconnected = false;
        macro_rules! push_evt {
            ($ev:expr, $data:expr) => {{
                if !disconnected && send_response_sse(&tx, $ev, $data).await.is_err() {
                    disconnected = true;
                }
            }};
        }
        push_evt!(
            "response.created",
            json!({
                "type": "response.created",
                "response": {
                    "id": response_id,
                    "object": "response",
                    "created_at": created_at,
                    "status": "in_progress",
                    "model": model,
                    "output": []
                }
            })
        );
        push_evt!(
            "response.in_progress",
            json!({
                "type": "response.in_progress",
                "response": {
                    "id": response_id,
                    "object": "response",
                    "created_at": created_at,
                    "status": "in_progress",
                    "model": model,
                    "output": []
                }
            })
        );
        push_evt!(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": item_id,
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": []
                }
            })
        );
        push_evt!(
            "response.content_part.added",
            json!({
                "type": "response.content_part.added",
                "item_id": item_id,
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": ""}
            })
        );

        let mut stream = Box::pin(stream);
        let mut buffer = String::new();
        let mut full_text = String::new();
        let mut tool_calls: Vec<ChatToolCallState> = Vec::new();
        let mut first_token_ms = None;
        while let Some(chunk) = stream.next().await {
            // 客户端已断开:不再消费上游、不再解析 SSE,直接结束以避免浪费 token 与占用熔断 inflight
            if disconnected {
                let _ = u_tx.send(StreamOutcome::failed(
                    usage,
                    first_token_ms,
                    "client disconnected",
                ));
                return;
            }
            match chunk {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some((event, consumed)) = next_sse_event(&buffer) {
                        buffer.drain(..consumed);
                        let Some(data) = sse_data(&event) else {
                            continue;
                        };
                        if data.trim() == "[DONE]" {
                            send_response_stream_done(
                                &tx,
                                &response_id,
                                &item_id,
                                &model,
                                created_at,
                                &full_text,
                                &tool_calls,
                            )
                            .await;
                            let _ = u_tx.send(StreamOutcome::success(usage, first_token_ms));
                            return;
                        }
                        // 顺便累积 usage（OpenAI include_usage 的 chunk 通常在 [DONE] 之前抵达）
                        accumulate_usage_from_sse_data(&data, &mut usage);
                        if let Some(message) = chat_stream_error_message(&data) {
                            push_evt!(
                                "response.failed",
                                json!({
                                    "type": "response.failed",
                                    "response": {
                                        "id": response_id,
                                        "status": "failed",
                                        "model": model,
                                        "error": {"message": message}
                                    }
                                })
                            );
                            if !disconnected {
                                let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
                            }
                            let _ =
                                u_tx.send(StreamOutcome::failed(usage, first_token_ms, message));
                            return;
                        }
                        if let Some(delta) = chat_stream_delta(&data) {
                            first_token_ms
                                .get_or_insert_with(|| started.elapsed().as_millis() as i64);
                            full_text.push_str(&delta);
                            push_evt!(
                                "response.output_text.delta",
                                json!({
                                    "type": "response.output_text.delta",
                                    "item_id": item_id,
                                    "output_index": 0,
                                    "content_index": 0,
                                    "delta": delta
                                })
                            );
                        }
                        for delta in chat_stream_tool_call_deltas(&data) {
                            first_token_ms
                                .get_or_insert_with(|| started.elapsed().as_millis() as i64);
                            while tool_calls.len() <= delta.index {
                                tool_calls.push(ChatToolCallState::default());
                            }
                            let call = &mut tool_calls[delta.index];
                            if let Some(id) = delta.id {
                                if call.call_id.is_empty() {
                                    call.call_id = id;
                                }
                            }
                            if let Some(name) = delta.name {
                                if call.name.is_empty() {
                                    call.is_custom = custom_tool_names.contains(&name);
                                    call.name = name;
                                }
                            }
                            if !call.added && !call.name.is_empty() {
                                if call.call_id.is_empty() {
                                    call.call_id = format!("call_{}", Uuid::new_v4().simple());
                                }
                                call.added = true;
                                let item = if call.is_custom {
                                    json!({
                                        "id": call.call_id,
                                        "type": "custom_tool_call",
                                        "status": "in_progress",
                                        "call_id": call.call_id,
                                        "name": call.name,
                                        "input": ""
                                    })
                                } else {
                                    json!({
                                        "id": call.call_id,
                                        "type": "function_call",
                                        "status": "in_progress",
                                        "call_id": call.call_id,
                                        "name": call.name,
                                        "arguments": ""
                                    })
                                };
                                push_evt!(
                                    "response.output_item.added",
                                    json!({
                                        "type": "response.output_item.added",
                                        "output_index": delta.index + 1,
                                        "item": item
                                    })
                                );
                            }
                            if let Some(arguments) = delta.arguments {
                                call.arguments.push_str(&arguments);
                                if call.added && !call.is_custom && !arguments.is_empty() {
                                    push_evt!(
                                        "response.function_call_arguments.delta",
                                        json!({
                                            "type": "response.function_call_arguments.delta",
                                            "output_index": delta.index + 1,
                                            "delta": arguments
                                        })
                                    );
                                }
                            }
                        }
                        if chat_stream_completed(&data) {
                            send_response_stream_done(
                                &tx,
                                &response_id,
                                &item_id,
                                &model,
                                created_at,
                                &full_text,
                                &tool_calls,
                            )
                            .await;
                            let _ = u_tx.send(StreamOutcome::success(usage, first_token_ms));
                            return;
                        }
                        if disconnected {
                            // 客户端在处理当前 chunk 期间断开,后续 SSE 事件继续解析已无意义
                            let _ = u_tx.send(StreamOutcome::failed(
                                usage,
                                first_token_ms,
                                "client disconnected",
                            ));
                            return;
                        }
                    }
                }
                Err(err) => {
                    let message = err.to_string();
                    if !disconnected {
                        let _ = send_response_sse(
                            &tx,
                            "response.failed",
                            json!({
                                "type": "response.failed",
                                "response": {
                                    "id": response_id,
                                    "status": "failed",
                                    "model": model,
                                    "error": {"message": message}
                                }
                            }),
                        )
                        .await;
                    }
                    let _ = u_tx.send(StreamOutcome::failed(usage, first_token_ms, message));
                    return;
                }
            }
        }

        send_response_stream_done(
            &tx,
            &response_id,
            &item_id,
            &model,
            created_at,
            &full_text,
            &tool_calls,
        )
        .await;
        let _ = u_tx.send(StreamOutcome::success(usage, first_token_ms));
    });

    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .map_err(|err| ProxyError::new(StatusCode::BAD_GATEWAY, err.to_string()))?;
    Ok(ProviderResult {
        response,
        upstream_model: result_model,
        usage: TokenUsage::default(),
        usage_rx: Some(u_rx),
    })
}

pub(super) async fn send_response_stream_done(
    tx: &mpsc::Sender<Result<Bytes, io::Error>>,
    response_id: &str,
    item_id: &str,
    model: &str,
    created_at: i64,
    full_text: &str,
    tool_calls: &[ChatToolCallState],
) {
    let _ = send_response_sse(
        tx,
        "response.output_text.done",
        json!({
            "type": "response.output_text.done",
            "item_id": item_id,
            "output_index": 0,
            "content_index": 0,
            "text": full_text
        }),
    )
    .await;
    let _ = send_response_sse(
        tx,
        "response.content_part.done",
        json!({
            "type": "response.content_part.done",
            "item_id": item_id,
            "output_index": 0,
            "content_index": 0,
            "part": {"type": "output_text", "text": full_text}
        }),
    )
    .await;
    let _ = send_response_sse(
        tx,
        "response.output_item.done",
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": item_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": full_text}]
            }
        }),
    )
    .await;
    let mut output = vec![json!({
        "id": item_id,
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": full_text}]
    })];
    for (idx, call) in tool_calls.iter().enumerate() {
        if !call.added
            && call.call_id.is_empty()
            && call.name.is_empty()
            && call.arguments.is_empty()
        {
            continue;
        }
        let call_id = if call.call_id.is_empty() {
            format!("call_{}", Uuid::new_v4().simple())
        } else {
            call.call_id.clone()
        };
        let output_index = idx + 1;
        let item = if call.is_custom {
            let input = custom_tool_input(&call.arguments);
            let _ = send_response_sse(
                tx,
                "response.custom_tool_call_input.done",
                json!({
                    "type": "response.custom_tool_call_input.done",
                    "output_index": output_index,
                    "input": input
                }),
            )
            .await;
            json!({
                "id": call_id,
                "type": "custom_tool_call",
                "status": "completed",
                "call_id": call_id,
                "name": call.name,
                "input": input
            })
        } else {
            let _ = send_response_sse(
                tx,
                "response.function_call_arguments.done",
                json!({
                    "type": "response.function_call_arguments.done",
                    "output_index": output_index,
                    "arguments": call.arguments
                }),
            )
            .await;
            json!({
                "id": call_id,
                "type": "function_call",
                "status": "completed",
                "call_id": call_id,
                "name": call.name,
                "arguments": call.arguments
            })
        };
        let _ = send_response_sse(
            tx,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item
            }),
        )
        .await;
        output.push(item);
    }
    let _ = send_response_sse(
        tx,
        "response.completed",
        json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "object": "response",
                "created_at": created_at,
                "status": "completed",
                "model": model,
                "output": output,
                "output_text": full_text
            }
        }),
    )
    .await;
    let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
}

#[derive(Default)]
pub(super) struct ChatToolCallState {
    call_id: String,
    name: String,
    arguments: String,
    added: bool,
    is_custom: bool,
}

pub(super) struct ChatToolCallDelta {
    pub(super) index: usize,
    pub(super) id: Option<String>,
    pub(super) name: Option<String>,
    pub(super) arguments: Option<String>,
}

pub(super) fn chat_stream_tool_call_deltas(data: &str) -> Vec<ChatToolCallDelta> {
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return Vec::new();
    };
    let Some(tool_calls) = value
        .pointer("/choices/0/delta/tool_calls")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    tool_calls
        .iter()
        .map(|call| ChatToolCallDelta {
            index: call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
            id: call
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            name: call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            arguments: call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        })
        .collect()
}

pub(super) fn chat_stream_error_message(data: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(data).ok()?;
    let error = value.get("error")?;
    if let Some(message) = error.get("message").and_then(Value::as_str) {
        return Some(truncate(message));
    }
    Some(truncate(&error.to_string()))
}

pub(super) async fn send_response_sse(
    tx: &mpsc::Sender<Result<Bytes, io::Error>>,
    event: &str,
    data: Value,
) -> Result<(), mpsc::error::SendError<Result<Bytes, io::Error>>> {
    tx.send(Ok(Bytes::from(format!("event: {event}\ndata: {data}\n\n"))))
        .await
}

pub(crate) fn next_sse_event(buffer: &str) -> Option<(String, usize)> {
    let lf = buffer.find("\n\n").map(|idx| (idx, 2));
    let crlf = buffer.find("\r\n\r\n").map(|idx| (idx, 4));
    let (idx, sep_len) = match (lf, crlf) {
        (Some(left), Some(right)) => {
            if left.0 <= right.0 {
                left
            } else {
                right
            }
        }
        (Some(value), None) | (None, Some(value)) => value,
        (None, None) => return None,
    };
    Some((buffer[..idx].to_string(), idx + sep_len))
}

pub(crate) fn sse_data(event: &str) -> Option<String> {
    let data = event
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        None
    } else {
        Some(data)
    }
}

pub(super) fn chat_stream_delta(data: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(data).ok()?;
    value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| {
            choice
                .get("delta")
                .and_then(|delta| delta.get("content"))
                .or_else(|| choice.get("text"))
        })
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

#[derive(Default)]
pub(super) struct ThoughtStripper {
    inside: bool,
    pending: String,
}

impl ThoughtStripper {
    fn push(&mut self, text: &str) -> String {
        const OPEN: &str = "<thought>";
        const CLOSE: &str = "</thought>";
        self.pending.push_str(text);
        let mut out = String::new();
        loop {
            if self.inside {
                if let Some(idx) = self.pending.find(CLOSE) {
                    self.pending.drain(..idx + CLOSE.len());
                    self.inside = false;
                    continue;
                }
                self.pending.clear();
                break;
            }
            if let Some(idx) = self.pending.find(OPEN) {
                out.push_str(&self.pending[..idx]);
                self.pending.drain(..idx + OPEN.len());
                self.inside = true;
                continue;
            }
            let keep = thought_open_prefix_suffix_len(&self.pending);
            let emit_len = self.pending.len().saturating_sub(keep);
            if emit_len > 0 {
                out.push_str(&self.pending[..emit_len]);
                self.pending.drain(..emit_len);
            }
            break;
        }
        out
    }

    fn finish(&mut self) -> String {
        if self.inside {
            self.pending.clear();
            String::new()
        } else {
            std::mem::take(&mut self.pending)
        }
    }
}

pub(super) fn thought_open_prefix_suffix_len(text: &str) -> usize {
    const OPEN: &str = "<thought>";
    (1..OPEN.len())
        .rev()
        .find(|len| text.ends_with(&OPEN[..*len]))
        .unwrap_or(0)
}

pub(super) fn strip_thought_text(text: &str) -> String {
    let mut stripper = ThoughtStripper::default();
    let mut out = stripper.push(text);
    out.push_str(&stripper.finish());
    out.trim_start().to_string()
}

pub(super) fn strip_thought_from_chat_json(value: &mut Value) {
    let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) else {
        return;
    };
    for choice in choices {
        if let Some(content) = choice.pointer_mut("/message/content") {
            strip_thought_from_content_value(content);
        }
        if let Some(content) = choice.pointer_mut("/delta/content") {
            strip_thought_from_content_value(content);
        }
        if let Some(text_value) = choice.get_mut("text") {
            if let Some(text) = text_value.as_str() {
                *text_value = Value::String(strip_thought_text(text));
            }
        }
    }
}

pub(super) fn strip_thought_from_content_value(value: &mut Value) {
    match value {
        Value::String(text) => {
            *text = strip_thought_text(text);
        }
        Value::Array(parts) => {
            for part in parts {
                if let Some(text_value) = part.get_mut("text") {
                    if let Some(text) = text_value.as_str() {
                        *text_value = Value::String(strip_thought_text(text));
                    }
                }
            }
        }
        _ => {}
    }
}

pub(super) fn strip_thought_from_chat_sse_stream<S>(
    stream: S,
) -> impl Stream<Item = Result<Bytes, io::Error>>
where
    S: Stream<Item = Result<Bytes, io::Error>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(32);
    tokio::spawn(async move {
        let mut stream = Box::pin(stream);
        let mut buffer = String::new();
        let mut stripper = ThoughtStripper::default();
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some((event, consumed)) = next_sse_event(&buffer) {
                        buffer.drain(..consumed);
                        let Some(data) = sse_data(&event) else {
                            if tx
                                .send(Ok(Bytes::from(format!("{event}\n\n"))))
                                .await
                                .is_err()
                            {
                                return;
                            }
                            continue;
                        };
                        if data.trim() == "[DONE]" {
                            let tail = stripper.finish();
                            if !tail.is_empty() {
                                let value = json!({
                                    "choices": [{
                                        "index": 0,
                                        "delta": {"content": tail},
                                        "finish_reason": null
                                    }]
                                });
                                if send_chat_data_sse(&tx, &value).await.is_err() {
                                    return;
                                }
                            }
                            if tx
                                .send(Ok(Bytes::from_static(b"data: [DONE]\n\n")))
                                .await
                                .is_err()
                            {
                                return;
                            }
                            continue;
                        }
                        let Ok(mut value) = serde_json::from_str::<Value>(&data) else {
                            if tx
                                .send(Ok(Bytes::from(format!("{event}\n\n"))))
                                .await
                                .is_err()
                            {
                                return;
                            }
                            continue;
                        };
                        strip_thought_from_chat_stream_value(&mut value, &mut stripper);
                        if chat_stream_value_is_empty_delta(&value) {
                            continue;
                        }
                        if send_chat_data_sse(&tx, &value).await.is_err() {
                            return;
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(err)).await;
                    return;
                }
            }
        }
        if !buffer.is_empty() {
            let _ = tx.send(Ok(Bytes::from(buffer))).await;
        }
    });
    ReceiverStream::new(rx)
}

pub(super) fn strip_thought_from_chat_stream_value(
    value: &mut Value,
    stripper: &mut ThoughtStripper,
) {
    let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) else {
        return;
    };
    for choice in choices {
        if let Some(delta) = choice.get_mut("delta").and_then(Value::as_object_mut) {
            if let Some(content) = delta.get_mut("content") {
                if let Some(text) = content.as_str() {
                    let cleaned = stripper.push(text);
                    if cleaned.is_empty() {
                        delta.remove("content");
                    } else {
                        *content = Value::String(cleaned);
                    }
                }
            }
        }
        if let Some(text_value) = choice.get_mut("text") {
            if let Some(text) = text_value.as_str() {
                *text_value = Value::String(stripper.push(text));
            }
        }
    }
}

pub(super) fn chat_stream_value_is_empty_delta(value: &Value) -> bool {
    value
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices.iter().all(|choice| {
                let finish_reason = choice.get("finish_reason").is_some_and(|v| !v.is_null());
                let delta_empty = choice
                    .get("delta")
                    .and_then(Value::as_object)
                    .is_none_or(Map::is_empty);
                let text_empty = choice
                    .get("text")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty);
                !finish_reason && delta_empty && text_empty
            })
        })
}

pub(super) async fn send_chat_data_sse(
    tx: &mpsc::Sender<Result<Bytes, io::Error>>,
    value: &Value,
) -> Result<(), mpsc::error::SendError<Result<Bytes, io::Error>>> {
    tx.send(Ok(Bytes::from(format!("data: {value}\n\n")))).await
}

pub(super) fn chat_to_response(
    chat: Value,
    request_model: &str,
    custom_tool_names: &HashSet<String>,
) -> Value {
    let message = chat
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let model = chat
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(request_model);
    let mut output = Vec::new();
    if !text.is_empty() {
        output.push(json!({
            "id": format!("msg-{}", Uuid::new_v4().simple()),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        }));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            if call.get("type").and_then(Value::as_str) != Some("function") {
                continue;
            }
            let call_id = call.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            if custom_tool_names.contains(name) {
                output.push(json!({
                    "id": call_id,
                    "type": "custom_tool_call",
                    "status": "completed",
                    "call_id": call_id,
                    "name": name,
                    "input": custom_tool_input(arguments)
                }));
            } else {
                output.push(json!({
                    "id": call_id,
                    "type": "function_call",
                    "status": "completed",
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments
                }));
            }
        }
    }
    if output.is_empty() {
        output.push(json!({
            "id": format!("msg-{}", Uuid::new_v4().simple()),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": ""}]
        }));
    }
    let usage = chat.get("usage").cloned().unwrap_or(Value::Null);
    let normalized_usage = json!({
        "input_tokens": usage.get("prompt_tokens").and_then(Value::as_i64).unwrap_or_default(),
        "output_tokens": usage.get("completion_tokens").and_then(Value::as_i64).unwrap_or_default(),
        "total_tokens": usage.get("total_tokens").and_then(Value::as_i64).unwrap_or_default(),
    });
    json!({
        "id": format!("resp-{}", Uuid::new_v4().simple()),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": "completed",
        "model": model,
        "output": output,
        "output_text": text,
        "usage": normalized_usage
    })
}

pub(super) fn response_model(value: &Value) -> Option<String> {
    value
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn extract_token_usage(value: &Value) -> TokenUsage {
    let Some(usage) = value.get("usage") else {
        return TokenUsage::default();
    };
    TokenUsage {
        input: usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        output: usage
            .get("output_tokens")
            .or_else(|| usage.get("completion_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or_default(),
    }
}

pub(super) fn supports_include_usage_injection(path: &str) -> bool {
    path == "/chat/completions"
}

// Chat Completions 兼容渠道默认关闭流式 usage，主动补上 stream_options.include_usage=true
pub(super) fn inject_include_usage(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let entry = obj
        .entry("stream_options".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(inner) = entry.as_object_mut() {
        inner
            .entry("include_usage".to_string())
            .or_insert(Value::Bool(true));
    }
}

// 从任意 SSE data JSON 中尽力提取 usage 并累计到 target
// 兼容 OpenAI（顶层 usage）、Anthropic（message.usage / usage）与 /responses（response.usage）
pub(super) fn accumulate_usage_from_sse_data(data: &str, target: &mut TokenUsage) {
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return;
    };
    let candidates = [
        value.get("usage"),
        value.pointer("/message/usage"),
        value.pointer("/response/usage"),
    ];
    for candidate in candidates.into_iter().flatten() {
        if let Some(v) = candidate
            .get("input_tokens")
            .or_else(|| candidate.get("prompt_tokens"))
            .and_then(Value::as_i64)
        {
            if v > 0 {
                target.input = v;
            }
        }
        if let Some(v) = candidate
            .get("output_tokens")
            .or_else(|| candidate.get("completion_tokens"))
            .and_then(Value::as_i64)
        {
            if v > 0 {
                target.output = v;
            }
        }
    }
}

// 将上游流式响应拆成：客户端可读的字节流 + 最终 usage 的 oneshot 通道
// 边转发边解析 SSE，不改写 payload；客户端断连时立即结束以避免资源浪费
pub(super) fn stream_with_usage_probe<S>(
    stream: S,
    kind: SseProbeKind,
    started: Instant,
    raw_capture: Option<RawSseCapture>,
) -> (
    oneshot::Receiver<StreamOutcome>,
    ReceiverStream<Result<Bytes, io::Error>>,
)
where
    S: Stream<Item = Result<Bytes, io::Error>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(32);
    let (u_tx, u_rx) = oneshot::channel::<StreamOutcome>();
    tokio::spawn(async move {
        let mut usage = TokenUsage::default();
        let mut buffer = String::new();
        let mut stream = Box::pin(stream);
        let mut completed = false;
        let mut stream_error = None;
        let mut client_disconnected = false;
        let mut first_token_ms = None;
        let mut raw_events = raw_capture
            .as_ref()
            .map(|capture| VecDeque::with_capacity(capture.max_events));
        let mut finish_note = "stream ended".to_string();
        'upstream: loop {
            let chunk = stream.next().await;
            let Some(chunk) = chunk else {
                finish_note = "upstream closed".to_string();
                break;
            };
            match chunk {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some((event, consumed)) = next_sse_event(&buffer) {
                        buffer.drain(..consumed);
                        if let (Some(capture), Some(events)) =
                            (raw_capture.as_ref(), raw_events.as_mut())
                        {
                            push_raw_sse_event(Some(capture), Some(events), event.clone());
                        }
                        if let Some(data) = sse_data(&event) {
                            accumulate_usage_from_sse_data(&data, &mut usage);
                            if sse_stream_completed(kind, &data) {
                                completed = true;
                            }
                            if first_token_ms.is_none()
                                && matches!(
                                    inspect_sse_probe_event(kind, &data),
                                    SseProbeDecision::Ready
                                )
                            {
                                first_token_ms = Some(started.elapsed().as_millis() as i64);
                            }
                            if stream_error.is_none() {
                                stream_error = sse_stream_error(kind, &data);
                            }
                        }
                    }
                    if tx.send(Ok(bytes)).await.is_err() {
                        // 客户端已断开，停止解析节省上游流量
                        client_disconnected = true;
                        break;
                    }
                    if completed {
                        if finish_note == "stream ended" {
                            finish_note = "completed event observed".to_string();
                        }
                        break 'upstream;
                    }
                }
                Err(err) => {
                    let message = err.to_string();
                    finish_note = format!("stream error: {message}");
                    stream_error = Some(message.clone());
                    let _ = tx.send(Err(io::Error::other(message))).await;
                    break;
                }
            }
        }
        let outcome = if let Some(error) = stream_error {
            StreamOutcome::failed(usage, first_token_ms, error)
        } else if !completed && !client_disconnected {
            StreamOutcome::failed(usage, first_token_ms, "上游流在完成事件前断开")
        } else {
            StreamOutcome::success(usage, first_token_ms)
        };
        if let (Some(capture), Some(events)) = (raw_capture, raw_events) {
            write_raw_sse_capture(&capture, kind, &finish_note, usage, first_token_ms, &events);
        }
        let _ = u_tx.send(outcome);
    });
    (u_rx, ReceiverStream::new(rx))
}

pub(super) fn push_raw_sse_event(
    capture: Option<&RawSseCapture>,
    events: Option<&mut VecDeque<String>>,
    event: String,
) {
    let (Some(capture), Some(events)) = (capture, events) else {
        return;
    };
    if events.len() >= capture.max_events {
        events.pop_front();
    }
    events.push_back(event);
}

pub(super) fn write_raw_sse_capture(
    capture: &RawSseCapture,
    kind: SseProbeKind,
    finish_note: &str,
    usage: TokenUsage,
    first_token_ms: Option<i64>,
    events: &VecDeque<String>,
) {
    let dir = PathBuf::from(&capture.path);
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let file_name = format!(
        "{}-{}-{}.sse.log",
        ts,
        sanitize_file_part(&capture.provider),
        Uuid::new_v4().simple()
    );
    let path = dir.join(file_name);
    let mut out = String::new();
    out.push_str("# RouteHub raw SSE capture\n");
    out.push_str(&format!("provider: {}\n", capture.provider));
    out.push_str(&format!("kind: {}\n", sse_probe_kind_name(kind)));
    out.push_str(&format!("finish: {finish_note}\n"));
    out.push_str(&format!("first_token_ms: {:?}\n", first_token_ms));
    out.push_str(&format!("usage_input: {}\n", usage.input));
    out.push_str(&format!("usage_output: {}\n", usage.output));
    out.push_str(&format!("events_kept: {}\n\n", events.len()));
    for (idx, event) in events.iter().enumerate() {
        out.push_str(&format!("----- event {} -----\n", idx + 1));
        out.push_str(event);
        if !event.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    let _ = fs::write(path, out);
}

pub(super) fn sanitize_file_part(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub(super) fn sse_probe_kind_name(kind: SseProbeKind) -> &'static str {
    match kind {
        SseProbeKind::Chat => "chat",
        SseProbeKind::Responses => "responses",
        SseProbeKind::Anthropic => "anthropic",
    }
}

pub(super) fn sse_stream_completed(kind: SseProbeKind, data: &str) -> bool {
    if data.trim() == "[DONE]" {
        return true;
    }
    if matches!(kind, SseProbeKind::Chat) {
        return chat_stream_completed(data);
    }
    if !matches!(kind, SseProbeKind::Responses) {
        return false;
    }
    matches!(
        serde_json::from_str::<Value>(data)
            .ok()
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .as_deref(),
        Some("response.completed")
    )
}

pub(super) fn chat_stream_completed(data: &str) -> bool {
    serde_json::from_str::<Value>(data)
        .ok()
        .and_then(|value| {
            value
                .pointer("/choices/0/finish_reason")
                .filter(|reason| !reason.is_null())
                .cloned()
        })
        .is_some()
}

pub(super) fn sse_stream_error(kind: SseProbeKind, data: &str) -> Option<String> {
    if matches!(kind, SseProbeKind::Chat) {
        return chat_stream_error_message(data);
    }
    let value = serde_json::from_str::<Value>(data).ok()?;
    let event_type = value.get("type").and_then(Value::as_str)?;
    if !matches!(
        event_type,
        "error" | "response.failed" | "response.incomplete"
    ) {
        return None;
    }
    Some(
        value
            .pointer("/error/message")
            .or_else(|| value.pointer("/response/error/message"))
            .and_then(Value::as_str)
            .map(truncate)
            .unwrap_or_else(|| format!("Responses 上游返回 {event_type}")),
    )
}

// 流式请求先写入 running 记录，流结束后原地更新，日志页可以实时看到进行中的请求。

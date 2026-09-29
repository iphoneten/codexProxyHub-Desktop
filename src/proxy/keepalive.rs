use super::*;
use std::future::Future;

mod template_store;
pub(super) use template_store::{restore_native_templates, TemplateStore};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
// 心跳仅发送给客户端，不进入请求日志、运行日志或用量统计。
const HEARTBEAT: Bytes = Bytes::from_static(b": keep-alive\n\n");

/// 授权完成后才调用。首个心跳之前保留原始 HTTP 错误状态。
pub(super) async fn while_waiting<F>(
    operation: F,
    enabled: bool,
    ctx: RequestContext,
    api: &'static str,
) -> Result<Response, ProxyError>
where
    F: Future<Output = Result<Response, ProxyError>> + Send + 'static,
{
    with_interval(operation, enabled, ctx, api, HEARTBEAT_INTERVAL).await
}

async fn with_interval<F>(
    operation: F,
    enabled: bool,
    ctx: RequestContext,
    api: &'static str,
    interval: Duration,
) -> Result<Response, ProxyError>
where
    F: Future<Output = Result<Response, ProxyError>> + Send + 'static,
{
    if !enabled {
        return operation.await;
    }
    let cancel = ctx.cancel_token();
    let mut operation = Box::pin(operation);
    let first = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(ProxyError::aborted()),
        result = &mut operation => Some(result?),
        _ = tokio::time::sleep(interval) => None,
    };
    // 快速失败和非 SSE 响应直接保留其状态、响应头和响应体。
    if let Some(response) = first.as_ref() {
        if !is_sse(response) {
            return Ok(first.unwrap());
        }
    }
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(4);
    let mut response = Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .unwrap();
    if let Some(first) = first.as_ref() {
        *response.status_mut() = first.status();
        *response.headers_mut() = first.headers().clone();
        response.headers_mut().remove(header::CONTENT_LENGTH);
    }
    tokio::spawn(async move {
        let was_waiting = first.is_none();
        let waiting = std::sync::atomic::AtomicBool::new(first.is_none());
        let work = async {
            let upstream = if let Some(first) = first {
                Ok(first)
            } else {
                if tx.send(Ok(HEARTBEAT.clone())).await.is_err() {
                    return;
                }
                operation.await
            };
            waiting.store(false, Ordering::Relaxed);
            if was_waiting {
                crate::runtime_log::record(
                    if upstream.is_ok() { "INFO" } else { "WARN" },
                    if upstream.is_ok() { "流式请求等待结束，收到上游响应" } else { "流式请求等待结束，返回错误" },
                );
            }
            match upstream {
                Ok(response) if is_sse(&response) => {
                    let mut stream = response.into_body().into_data_stream();
                    while let Some(item) = stream.next().await {
                        let item = item.map_err(|err| io::Error::new(io::ErrorKind::Other, err));
                        if tx.send(item).await.is_err() {
                            return;
                        }
                    }
                }
                result => {
                    let err = match result {
                        Err(err) => err,
                        Ok(response) => ProxyError::new(
                            if response.status().is_success() { StatusCode::BAD_GATEWAY } else { response.status() },
                            "上游未返回预期的 SSE 响应",
                        ),
                    };
                    let payload = if api == "messages" {
                        json!({"type": "error", "error": {"type": "api_error", "message": err.message}})
                    } else {
                        json!({"type": "error", "error": {"type": "proxy_error", "code": err.status.as_u16(), "message": err.message}})
                    };
                    let event = Bytes::from(format!("event: error\ndata: {payload}\n\n"));
                    let _ = tx.send(Ok(event)).await;
                }
            }
        };
        tokio::pin!(work);
        let mut tick = tokio::time::interval_at(
            tokio::time::Instant::now() + interval,
            interval,
        );
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = tx.closed() => break,
                _ = &mut work => break,
                _ = tick.tick() => {
                    // 下游拥塞时跳过心跳，不能阻塞取消和实际输出。
                    if waiting.load(Ordering::Relaxed) {
                        let _ = tx.try_send(Ok(HEARTBEAT.clone()));
                    }
                }
            }
        }
    });
    Ok(response)
}

fn is_sse(response: &Response) -> bool {
    response.status().is_success()
        && response.headers().get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("text/event-stream"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_heartbeat_model_overrides_legacy_default() {
        let mut provider: ProviderConfig = serde_json::from_value(json!({
            "name": "test", "base_url": "http://127.0.0.1", "api_key": "test",
            "models": ["first", "small"]
        })).unwrap();
        assert_eq!(heartbeat_model(&provider), Some("first"));
        provider.heartbeat_model = "small".to_string();
        assert_eq!(heartbeat_model(&provider), Some("small"));
        let restored: ProviderConfig = serde_json::from_value(serde_json::to_value(&provider).unwrap()).unwrap();
        assert_eq!(heartbeat_model(&restored), Some("small"));
    }

    #[test]
    fn native_keepalive_matches_python_request_template() {
        // 样本由 Python 项目的原始模板函数生成，输入全部为虚构数据。
        let fixture: Value = serde_json::from_str(include_str!("fixtures/python_keepalive.json")).unwrap();
        let template = NativeKeepaliveTemplate::from_request(&fixture["source_request"], &HeaderMap::new());
        let body = native_heartbeat_body(&template.body, "heartbeat-model");
        assert_eq!(body, fixture["expected_body"]);
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_output_tokens"], 1);
        assert_eq!(body["store"], false);
        assert_eq!(body["input"][0]["content"][2]["text"], "Hi");
        assert!(body.get("previous_response_id").is_none());
        assert_eq!(body["tools"], fixture["source_request"]["tools"]);
        assert!(!body.to_string().contains("synthetic-private"));
        assert!(native_heartbeat_event(r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#).unwrap().is_ok());
        assert!(native_heartbeat_event(r#"{"type":"response.failed","response":{"error":{"message":"rate limit"}}}"#).unwrap().is_err());
        assert!(native_heartbeat_event(r#"{"type":"response.created"}"#).is_none());
    }

    #[test]
    fn error_hint_does_not_repeat_upstream_secrets() {
        let err = ProxyError::new(StatusCode::BAD_REQUEST, "max_output_tokens must be >= 16; secret=fake-private-value");
        let hint = keepalive_error_hint(&err);
        assert!(hint.contains("max_output_tokens"));
        assert!(!hint.contains("fake-private-value"));
        for message in ["missing session_id", "missing originator"] {
            let err = ProxyError::new(StatusCode::BAD_REQUEST, format!("{message}; secret=fake-private-value"));
            let hint = keepalive_error_hint(&err);
            assert!(hint.contains("客户端标识或会话头"));
            assert!(!hint.contains("fake-private-value"));
        }
        let err = ProxyError::new(StatusCode::BAD_REQUEST, "invalid codex request; secret=fake-private-value");
        let hint = keepalive_error_hint(&err);
        assert!(hint.contains("invalid codex request"));
        assert!(hint.contains("无法仅凭此错误确定"));
        assert!(!hint.contains("fake-private-value"));
        let err = ProxyError::new(StatusCode::FORBIDDEN, "codex_access_restricted; secret=fake-private-value");
        assert!(keepalive_error_hint(&err).contains("codex_access_restricted"));
        assert!(!keepalive_error_hint(&err).contains("fake-private-value"));
    }

    #[test]
    fn transient_failures_do_not_pause_fresh_or_previously_successful_templates() {
        for succeeded in [false, true] {
            for status in [408, 429, 500, 502, 503, 504] {
                let mut template = NativeKeepaliveTemplate::from_request(&json!({"input": "Hi"}), &HeaderMap::new());
                template.succeeded = succeeded;
                let err = ProxyError::new(StatusCode::from_u16(status).unwrap(),
                    "model unavailable; upstream error; synthetic-private-value");
                let hint = keepalive_error_hint(&err);
                assert!(!hint.contains("拒绝心跳模型"));
                assert!(!hint.contains("synthetic-private-value"));
                template.record_outcome(&Err(err));
                assert!(!template.failed, "status={status}, succeeded={succeeded}");
                template.record_outcome(&Ok(()));
                assert!(template.succeeded);
            }
        }
    }

    #[test]
    fn structured_upstream_error_keeps_known_code_and_param_for_safe_diagnostics() {
        let err = native_keepalive_http_error(StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"Bad request","code":"invalid_responses_request"}}"#, None);
        assert!(keepalive_error_hint(&err).contains("invalid_responses_request"));
        let mut template = NativeKeepaliveTemplate::from_request(&json!({"input": "Hi"}), &HeaderMap::new());
        template.succeeded = true;
        template.record_outcome(&Err(err));
        assert!(template.failed);
        for param in ["max_output_tokens", "tools", "reasoning", "store"] {
            let err = native_keepalive_http_error(StatusCode::BAD_REQUEST,
                &json!({"error": {"message": "Bad request", "param": param}}).to_string(), None);
            assert!(keepalive_error_hint(&err).contains(param));
        }
        let err = native_keepalive_http_error(StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"Bad request","code":"synthetic-secret","param":"synthetic-private-input"}}"#, None);
        assert!(!err.message.contains("synthetic-"));
        assert!(!keepalive_error_hint(&err).contains("synthetic-"));
    }

    #[tokio::test]
    async fn capture_without_protocol_headers_preserves_previous_headers_like_python() {
        let state = super::super::tests::test_state();
        let mut provider: ProviderConfig = serde_json::from_value(json!({
            "name": "header-test", "base_url": "https://example.test/v1", "api_key": "synthetic"
        })).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::USER_AGENT, HeaderValue::from_static("synthetic-client"));
        remember_native_template(&state, &provider,
            NativeKeepaliveTemplate::from_request(&json!({"input": "first"}), &headers)).await;
        remember_native_template(&state, &provider,
            NativeKeepaliveTemplate::from_request(&json!({"input": "second", "metadata": {"version": "new"}}), &HeaderMap::new())).await;
        let template = state.keepalive_templates.lock()[&template_key(&provider)].clone();
        assert_eq!(template.headers[header::USER_AGENT], "synthetic-client");
        assert_eq!(template.body["metadata"]["version"], "new");
        assert!(template.skip_reason().is_none());
        provider.base_url = "https://other.example.test/v1".into();
        remember_native_template(&state, &provider,
            NativeKeepaliveTemplate::from_request(&json!({"input": "third"}), &HeaderMap::new())).await;
        assert!(state.keepalive_templates.lock()[&template_key(&provider)].headers.is_empty());
    }

    #[test]
    fn explicit_invalid_template_still_pauses_and_cancellation_does_not() {
        for succeeded in [false, true] {
            let mut template = NativeKeepaliveTemplate::from_request(&json!({"input": "Hi"}), &HeaderMap::new());
            template.succeeded = succeeded;
            template.record_outcome(&Err(ProxyError::aborted()));
            assert!(!template.failed);
            template.record_outcome(&Err(ProxyError::new(StatusCode::BAD_REQUEST, "invalid codex request")));
            assert!(template.failed);
        }
    }

    #[tokio::test]
    async fn native_keepalive_preserves_cached_headers_and_explicit_overrides() {
        let state = super::super::tests::test_state();
        let mut provider: ProviderConfig = serde_json::from_value(json!({
            "name": "native-test", "base_url": "https://example.test/v1", "api_key": "upstream-test",
            "extra_headers": {"Originator": "configured-client", "Session_Id": "configured-session"}
        })).unwrap();
        let mut incoming = HeaderMap::new();
        for (name, value) in [
            ("user-agent", "test-client/1.0"), ("originator", "test-client"),
            ("session_id", "client-session"), ("x-codex-version", "test-version"),
            ("authorization", "Bearer local-test-secret"), ("cookie", "local-test-cookie"),
            ("openai-api-key", "local-test-secret"),
            ("x-request-id", "client-request"),
        ] {
            incoming.insert(name, HeaderValue::from_static(value));
        }
        let template = NativeKeepaliveTemplate::from_request(&json!({"input": "private"}), &incoming);
        remember_native_template(&state, &provider, template).await;
        let cached = state.keepalive_templates.lock()[&template_key(&provider)].headers.clone();
        assert!(!cached.contains_key("authorization"));
        assert!(!cached.contains_key("cookie"));
        assert!(!cached.contains_key("openai-api-key"));
        assert!(!cached.contains_key("x-request-id"));
        let headers = native_heartbeat_headers(&provider, cached.clone());
        assert_eq!(headers["user-agent"], "test-client/1.0");
        assert_eq!(headers["x-codex-version"], "test-version");
        assert_eq!(headers["originator"], "configured-client");
        assert_eq!(headers["session_id"], "configured-session");
        assert!(!headers.contains_key("conversation_id"));
        assert_eq!(headers["authorization"], "Bearer upstream-test");
        assert!(!headers.contains_key("cookie"));
        assert_ne!(headers["x-request-id"], "client-request");
        let next = native_heartbeat_headers(&provider, cached.clone());
        assert_ne!(headers["x-request-id"], next["x-request-id"]);

        provider.extra_headers.clear();
        let mut cached = cached;
        cached.insert("conversation_id", HeaderValue::from_static("client-conversation"));
        let headers = native_heartbeat_headers(&provider, cached);
        assert_eq!(headers["originator"], "test-client");
        assert_eq!(headers["session_id"], "client-session");
        assert_eq!(headers["conversation_id"], "client-conversation");
        provider.base_url = "https://different.example.test/v1".into();
        assert!(!state.keepalive_templates.lock().contains_key(&template_key(&provider)));
        assert!(native_template_skip_reason(&state, &provider).is_some());
    }

    #[tokio::test]
    async fn native_keepalive_waits_for_a_successful_real_request_then_reuses_its_template() {
        let state = super::super::tests::test_state();
        let fixture: Value = serde_json::from_str(include_str!("fixtures/python_keepalive.json")).unwrap();
        let expected = fixture["expected_body"].clone();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let app = Router::new().route("/v1/responses", post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let seen = seen.clone();
            let expected = expected.clone();
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                if body.get("tools").is_none() || body.get("reasoning").is_none() {
                    return (StatusCode::BAD_REQUEST, Json(json!({"error": {"message": "invalid codex request"}}))).into_response();
                }
                assert_eq!(headers["user-agent"], "real-client/1.0");
                assert_eq!(headers["x-openai-client-version"], "synthetic-version");
                assert_eq!(headers["authorization"], "Bearer upstream-test");
                if body["max_output_tokens"] == 1 {
                    assert_eq!(body, expected);
                    assert!(Uuid::parse_str(headers["x-request-id"].to_str().unwrap()).is_ok());
                    assert!(!headers.contains_key("cookie"));
                }
                Response::builder().header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from(concat!(
                        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n",
                        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_test\",\"model\":\"heartbeat-model\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hi\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
                    )))
                    .unwrap()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut provider: ProviderConfig = serde_json::from_value(json!({
            "name": "native-test", "base_url": format!("http://{}/v1", listener.local_addr().unwrap()),
            "api_key": "upstream-test", "models": ["test-model"], "responses_mode": "native",
            "model_mapping": {"test-model": "heartbeat-model"}, "heartbeat_enabled": false, "request_timeout": 2
        })).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let skipped = send_idle_keepalive(&state, &provider, &RequestContext::detached()).await.unwrap();
        assert!(matches!(skipped, IdleKeepaliveOutcome::Skipped(_)));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        let mut headers = HeaderMap::new();
        headers.insert(header::USER_AGENT, HeaderValue::from_static("real-client/1.0"));
        headers.insert("x-openai-client-version", HeaderValue::from_static("synthetic-version"));
        headers.insert(header::COOKIE, HeaderValue::from_static("synthetic-secret"));
        // 原先独立拼装的短请求没有真实协议字段，会被这个模拟上游拒绝。
        let legacy = send_to_provider(&state, &provider, "/responses", &headers,
            json!({"model": "heartbeat-model", "instructions": "You are Codex. Reply briefly.",
                "input": [{"role": "user", "content": [{"type": "input_text", "text": "Hi"}]}],
                "max_output_tokens": 1, "stream": true, "store": false}),
            true, Instant::now(), &RequestContext::detached(),
        ).await;
        assert_eq!(legacy.err().unwrap().status, StatusCode::BAD_REQUEST);
        assert!(state.keepalive_templates.lock().is_empty());
        let mut request = fixture["source_request"].clone();
        request["stream"] = json!(true);
        let real = send_to_provider(&state, &provider, "/responses", &headers, request,
            true, Instant::now(), &RequestContext::detached()).await.unwrap();
        to_bytes(real.response.into_body(), 4096).await.unwrap();
        assert!(native_template_skip_reason(&state, &provider).is_none());
        let mut cfg = (*state.snapshot()).clone();
        cfg.providers = vec![provider.clone()];
        cfg.providers[0].enabled = false;
        // 尚未开启保活，或渠道暂时停用时，成功模板都应继续保留。
        retain_configured_templates(&state, &cfg);
        assert!(native_template_skip_reason(&state, &provider).is_none());
        provider.heartbeat_enabled = true;
        provider.responses_mode = "auto".into();
        assert_eq!(heartbeat_path(&state, &provider), "/responses");
        let result = send_idle_keepalive(&state, &provider, &RequestContext::detached()).await;
        server.abort();
        assert!(matches!(result.unwrap(), IdleKeepaliveOutcome::Sent));
        assert_eq!(count.load(Ordering::SeqCst), 3);
        cfg.providers[0].base_url = "https://different.example.test/v1".into();
        retain_configured_templates(&state, &cfg);
        assert!(state.keepalive_templates.lock().is_empty());
    }

    #[tokio::test]
    async fn responses_keepalive_reaches_configured_network_proxy() {
        let state = super::super::tests::test_state();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        // 模拟 HTTP 代理直接返回 SSE；目标域名不可用，绕过代理不能成功。
        let proxy = Router::new().fallback(move |uri: axum::http::Uri, Json(body): Json<Value>| {
            let seen = seen.clone();
            async move {
                assert_eq!(uri.host(), Some("keepalive.invalid"));
                assert_eq!(uri.path(), "/v1/responses");
                assert_eq!(body["input"], "Hi");
                seen.fetch_add(1, Ordering::SeqCst);
                Response::builder().header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from("data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_proxy_test\",\"output\":[]}}\n\n"))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut cfg = (*state.snapshot()).clone();
        cfg.routing.auth_proxy = format!("http://{}", listener.local_addr().unwrap());
        *state.config.write() = Arc::new(cfg);
        let provider: ProviderConfig = serde_json::from_value(json!({
            "name": "proxy-test", "base_url": "http://keepalive.invalid/v1",
            "api_key": "synthetic-test", "models": ["test-model"], "use_proxy": true,
            "responses_mode": "native", "request_timeout": 2, "max_retries": 0
        })).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::USER_AGENT, HeaderValue::from_static("synthetic-client"));
        remember_native_template(&state, &provider,
            NativeKeepaliveTemplate::from_request(&json!({"input": "synthetic"}), &headers)).await;
        let server = tokio::spawn(async move { axum::serve(listener, proxy).await.unwrap() });
        let result = send_idle_keepalive(&state, &provider, &RequestContext::detached()).await;
        server.abort();
        assert!(matches!(result.unwrap(), IdleKeepaliveOutcome::Sent));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn native_template_preserves_last_user_context_and_requires_real_headers() {
        let template = NativeKeepaliveTemplate::from_request(&json!({
            "input": [{"role": "user", "content": [
                {"type": "input_text", "text": "private-first"},
                {"type": "input_image", "image_url": "private-image"},
                {"type": "input_text", "text": "private-last"}
            ]}],
            "instructions": "private-system", "previous_response_id": "private-id"
        }), &HeaderMap::new());
        let body_text = template.body.to_string();
        assert!(!body_text.contains("private-last"));
        assert!(!body_text.contains("private-system"));
        assert!(!body_text.contains("private-id"));
        assert_eq!(template.body["input"][0]["content"].as_array().unwrap().len(), 3);
        assert_eq!(template.body["input"][0]["content"][0]["text"], "private-first");
        assert_eq!(template.body["input"][0]["content"][1]["image_url"], "private-image");
        assert_eq!(template.body["input"][0]["content"][2]["text"], "Hi");
        assert!(template.skip_reason().unwrap().contains("请求头"));
        assert_eq!(heartbeat_input(Some(&json!("private"))), json!("Hi"));
        assert_eq!(heartbeat_input(Some(&json!([{"role":"user","content":"private"}]))),
            json!([{"role":"user","content":"Hi"}]));
    }

    #[tokio::test]
    async fn rejected_native_keepalive_waits_for_new_template_without_blocking_clients() {
        let state = super::super::tests::test_state();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let app = Router::new().route("/v1/responses", post(move || {
            let seen = seen.clone();
            async move {
                if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                    (StatusCode::BAD_REQUEST, "invalid codex request").into_response()
                } else {
                    Response::builder().header(header::CONTENT_TYPE, "text/event-stream")
                        .body(Body::from("data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_test\",\"output\":[]}}\n\n"))
                        .unwrap()
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider: ProviderConfig = serde_json::from_value(json!({
            "name": "native-test", "base_url": format!("http://{}/v1", listener.local_addr().unwrap()),
            "api_key": "test", "models": ["test-model"], "responses_mode": "native",
            "heartbeat_enabled": true, "heartbeat_interval_secs": 30, "request_timeout": 2
        })).unwrap();
        let mut cfg = (*state.snapshot()).clone();
        cfg.providers = vec![provider.clone()];
        cfg.auth_accounts.clear();
        *state.config.write() = Arc::new(cfg);
        let mut headers = HeaderMap::new();
        headers.insert(header::USER_AGENT, HeaderValue::from_static("real-client/1.0"));
        remember_native_template(&state, &provider,
            NativeKeepaliveTemplate::from_request(&json!({"model":"test-model","input":"private"}), &headers)).await;
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::timeout(Duration::from_secs(2),
            recover_idle_channel(&state, &provider.name, &RequestContext::detached())).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(native_template_skip_reason(&state, &provider).unwrap().contains("失败"));
        recover_idle_channel(&state, &provider.name, &RequestContext::detached()).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider_attempts(&state.snapshot(), &state, "test-model", "responses", &[]).len(), 1);
        assert!(state.begin_provider_attempt(&provider.name).is_ok());

        remember_native_template(&state, &provider,
            NativeKeepaliveTemplate::from_request(&json!({"model":"test-model","input":"new-private"}), &headers)).await;
        let result = send_idle_keepalive(&state, &provider, &RequestContext::detached()).await;
        server.abort();
        assert!(matches!(result.unwrap(), IdleKeepaliveOutcome::Sent));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn late_keepalive_failure_does_not_reject_a_new_real_request_template() {
        let state = super::super::tests::test_state();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let server_entered = entered.clone();
        let server_release = release.clone();
        let app = Router::new().route("/v1/responses", post(move || {
            let entered = server_entered.clone();
            let release = server_release.clone();
            async move {
                entered.notify_one();
                release.notified().await;
                (StatusCode::BAD_REQUEST, "invalid codex request")
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider: ProviderConfig = serde_json::from_value(json!({
            "name":"native-test", "base_url":format!("http://{}/v1", listener.local_addr().unwrap()),
            "api_key":"test", "models":["test-model"], "responses_mode":"native", "request_timeout":2
        })).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::USER_AGENT, HeaderValue::from_static("real-client/1.0"));
        remember_native_template(&state, &provider, NativeKeepaliveTemplate::from_request(&json!({"input":"old"}), &headers)).await;
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let pending_state = state.clone();
        let pending_provider = provider.clone();
        let pending = tokio::spawn(async move {
            send_idle_keepalive(&pending_state, &pending_provider, &RequestContext::detached()).await
        });
        tokio::time::timeout(Duration::from_secs(2), entered.notified()).await.unwrap();
        let fresh = NativeKeepaliveTemplate::from_request(&json!({"input":"new"}), &headers);
        let generation = fresh.generation;
        remember_native_template(&state, &provider, fresh).await;
        release.notify_one();
        assert_eq!(pending.await.unwrap().unwrap_err().status, StatusCode::BAD_REQUEST);
        server.abort();
        assert_eq!(state.keepalive_templates.lock()[&template_key(&provider)].generation, generation);
        assert!(native_template_skip_reason(&state, &provider).is_none());
    }

    #[tokio::test]
    async fn keepalive_api_selection_matches_python() {
        let state = super::super::tests::test_state();
        let mut provider: ProviderConfig = serde_json::from_value(json!({
            "name":"test", "api_key":"test", "base_url":"https://example.test/v1"
        })).unwrap();
        assert_eq!(heartbeat_path(&state, &provider), "/chat/completions");
        provider.capabilities.insert("supports_responses".into(), true);
        assert_eq!(heartbeat_path(&state, &provider), "/responses");
        provider.capabilities.clear();
        remember_native_template(&state, &provider,
            NativeKeepaliveTemplate::from_request(&json!({"input":"test"}), &HeaderMap::new())).await;
        assert_eq!(heartbeat_path(&state, &provider), "/responses");
        provider.responses_mode = "chat".into();
        assert_eq!(heartbeat_path(&state, &provider), "/chat/completions");
        provider.responses_mode = "native".into();
        assert_eq!(heartbeat_path(&state, &provider), "/responses");
        provider.provider_type = "anthropic".into();
        assert_eq!(heartbeat_path(&state, &provider), "/messages");
    }

    #[tokio::test]
    async fn fast_error_keeps_http_status() {
        let result = with_interval(
            async { Err(ProxyError::new(StatusCode::UNAUTHORIZED, "test")) },
            true, RequestContext::detached(), "chat", Duration::from_millis(10),
        ).await;
        assert_eq!(result.unwrap_err().status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn waiting_emits_heartbeat_then_protocol_error() {
        let (tx, rx) = oneshot::channel::<()>();
        let response = with_interval(
            async move {
                let _ = rx.await;
                Err(ProxyError::new(StatusCode::BAD_GATEWAY, "test failure"))
            },
            true, RequestContext::detached(), "messages", Duration::from_millis(10),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        let first = tokio::time::timeout(Duration::from_secs(1), body.next()).await.unwrap().unwrap().unwrap();
        assert_eq!(first, HEARTBEAT);
        tx.send(()).unwrap();
        let rest = tokio::time::timeout(Duration::from_secs(1), async move {
            let mut bytes = Vec::new();
            while let Some(chunk) = body.next().await {
                bytes.extend_from_slice(&chunk.unwrap());
            }
            String::from_utf8(bytes).unwrap()
        }).await.unwrap();
        assert!(rest.contains("event: error"));
        assert!(rest.contains("api_error"));
        assert!(rest.contains("test failure"));
    }

    #[tokio::test]
    async fn dropping_body_drops_pending_upstream_future() {
        let ctx = RequestContext::detached();
        let operation_ctx = RequestContext::detached();
        let cancelled = operation_ctx.cancel_token();
        let guard = operation_ctx.drop_guard();
        let response = with_interval(
            async move {
                let _guard = guard;
                std::future::pending::<Result<Response, ProxyError>>().await
            },
            true, ctx, "responses", Duration::from_millis(10),
        ).await.unwrap();
        drop(response);
        tokio::time::timeout(Duration::from_secs(1), cancelled.cancelled()).await.unwrap();
    }

    #[tokio::test]
    async fn successful_sse_preserves_bytes_and_headers() {
        let response = with_interval(
            async {
                Ok(Response::builder()
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header("x-test", "preserved")
                    .body(Body::from("data: hello\n\ndata: [DONE]\n\n")).unwrap())
            },
            true, RequestContext::detached(), "chat", Duration::from_millis(10),
        ).await.unwrap();
        assert_eq!(response.headers()["x-test"], "preserved");
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(bytes, "data: hello\n\ndata: [DONE]\n\n");
    }
}

/// 空闲保活只记录运行状态，不经过请求日志、鉴权用量统计或渠道熔断记账。
pub(super) async fn run_idle_keepalive(state: AppState) {
    let mut next_due: HashMap<String, Instant> = HashMap::new();
    let mut jobs = tokio::task::JoinSet::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = state.root_cancel.cancelled() => break,
            _ = jobs.join_next(), if !jobs.is_empty() => {},
            _ = tick.tick() => {
                let cfg = state.snapshot();
                let enabled: HashSet<_> = cfg.providers.iter()
                    .filter(|p| p.enabled && p.heartbeat_enabled)
                    .map(|p| p.name.clone()).collect();
                retain_configured_templates(&state, &cfg);
                next_due.retain(|name, _| enabled.contains(name));
                for (name, cancel) in state.keepalive_requests.lock().iter() {
                    if !enabled.contains(name) { cancel.cancel(); }
                }
                for provider in cfg.providers.iter().filter(|p| p.enabled && p.heartbeat_enabled) {
                    let interval = Duration::from_secs(provider.heartbeat_interval_secs.clamp(1, 3600));
                    let now = Instant::now();
                    let due = next_due.entry(provider.name.clone()).or_insert_with(|| {
                        crate::runtime_log::record("INFO", format!("渠道 {}：保活已启用，间隔 {} 秒，模型 {}", provider.name, interval.as_secs(), heartbeat_model(provider).unwrap_or("未配置")));
                        now + interval
                    });
                    // 更新间隔时，较短的新间隔最迟在下一轮生效。
                    if *due > now + interval { *due = now + interval; }
                    if busy(&state, &provider.name) {
                        *due = now + interval;
                        continue;
                    }
                    if now < *due || state.keepalive_requests.lock().contains_key(&provider.name) {
                        continue;
                    }
                    *due = now + interval;
                    if heartbeat_model(provider).is_none() {
                        crate::runtime_log::record("WARN", format!("渠道 {}：跳过保活，未配置心跳模型或渠道模型", provider.name));
                        continue;
                    }
                    let cancel = state.root_cancel.child_token();
                    state.keepalive_requests.lock().insert(provider.name.clone(), cancel.clone());
                    // 注册取消信号后复查，盖住真实请求同时到达的竞争窗口。
                    if busy(&state, &provider.name) { cancel.cancel(); }
                    let state = state.clone();
                    let provider = provider.clone();
                    jobs.spawn(async move {
                        let _cleanup = KeepaliveCleanup { state: state.clone(), name: provider.name.clone() };
                        let ctx = RequestContext::child_of(&cancel, None);
                        let _guard = ctx.drop_guard();
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => {
                                crate::runtime_log::record("INFO", format!("渠道 {}：保活已取消（真实请求到达、配置关闭或服务停止）", provider.name));
                            },
                            _ = recover_idle_channel(&state, &provider.name, &ctx) => {},
                        }
                    });
                }
            }
        }
    }
    // JoinSet 的 drop 会中止所有剩余保活任务。
}

fn busy(state: &AppState, name: &str) -> bool {
    state.provider_loads.lock().get(name)
        .is_some_and(|load| load.load(Ordering::Acquire) > 0)
}

#[derive(Debug)]
enum IdleKeepaliveOutcome {
    Sent,
    Skipped(&'static str),
}

fn heartbeat_path(state: &AppState, provider: &ProviderConfig) -> &'static str {
    if provider.provider_type == "anthropic" {
        return "/messages";
    }
    if is_google_native_provider(provider) {
        return "/chat/completions";
    }
    match provider.responses_mode.as_str() {
        "native" => "/responses",
        "chat" => "/chat/completions",
        _ if is_google_openai_endpoint(&provider.base_url) => "/chat/completions",
        // auto 模式优先采用该渠道真实请求已经成功使用的接口，避免缺少能力声明时误走 Chat。
        _ if state.keepalive_templates.lock().contains_key(&template_key(provider)) => "/responses",
        _ => {
            let caps = &provider.capabilities;
            if (!caps.contains_key("supports_chat") && !caps.contains_key("supports_responses"))
                || (caps.get("supports_chat") == Some(&true) && caps.get("supports_responses") != Some(&true))
            {
                "/chat/completions"
            } else {
                "/responses"
            }
        }
    }
}

async fn send_idle_keepalive(state: &AppState, provider: &ProviderConfig, ctx: &RequestContext) -> Result<IdleKeepaliveOutcome, ProxyError> {
    let Some(model) = heartbeat_model(provider) else {
        return Err(ProxyError::new(StatusCode::BAD_REQUEST, "保活未配置模型"));
    };
    let mut provider = provider.clone();
    // 保活每轮只尝试一次，不得进入无限重试或 SSE 调试记录。
    provider.max_retries = 0;
    provider.persistent_retry = false;
    provider.debug_capture_sse = false;
    let path = heartbeat_path(state, &provider);
    if path == "/responses" {
        return send_native_heartbeat(state, &provider, model, ctx).await;
    }
    let mut body = if path == "/messages" {
        json!({"model": model, "messages": [{"role": "user", "content": "Hi"}], "max_tokens": 1, "stream": true})
    } else {
        json!({"model": model, "messages": [{"role": "user", "content": "Hi"}], "max_tokens": 1,
            "temperature": 0, "stream": true, "stream_options": {"include_usage": true}})
    };
    apply_model_mapping(&provider, &mut body);
    let result = send_to_provider(state, &provider, path, &HeaderMap::new(), body, true, Instant::now(), ctx).await?;
    let mut stream = result.response.into_body().into_data_stream();
    let cancel = ctx.cancel_token();
    let read = async {
        while let Some(chunk) = stream.next().await {
            chunk.map_err(|_| ProxyError::new(StatusCode::BAD_GATEWAY, "保活流读取失败"))?;
        }
        if let Some(usage) = result.usage_rx {
            let outcome = usage.await.map_err(|_| ProxyError::new(StatusCode::BAD_GATEWAY, "保活流缺少结束状态"))?;
            if outcome.aborted { return Err(ProxyError::aborted()); }
            if let Some(error) = outcome.error {
                return Err(ProxyError::new(StatusCode::BAD_GATEWAY, error));
            }
        }
        Ok(IdleKeepaliveOutcome::Sent)
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(ProxyError::aborted()),
        result = tokio::time::timeout(Duration::from_secs(provider.request_timeout.max(1)), read) => {
            result.unwrap_or_else(|_| Err(ProxyError::new(StatusCode::GATEWAY_TIMEOUT, "保活响应超时")))
        }
    }
}

struct KeepaliveCleanup {
    state: AppState,
    name: String,
}

impl Drop for KeepaliveCleanup {
    fn drop(&mut self) {
        self.state.keepalive_requests.lock().remove(&self.name);
    }
}

/// 与 Python 版一致，保活失败不改变渠道可用性；真实请求始终可抢占保活。
async fn recover_idle_channel(state: &AppState, name: &str, ctx: &RequestContext) {
    let cancel = ctx.cancel_token();
    loop {
        let cfg = state.snapshot();
        let Some(provider) = cfg.providers.iter()
            .find(|p| p.name == name && p.enabled && p.heartbeat_enabled).cloned() else { return; };
        if busy(state, name) { return; }
        let path = heartbeat_path(state, &provider);
        if path == "/responses" {
            if let Some(reason) = native_template_skip_reason(state, &provider) {
                crate::runtime_log::record("INFO", format!("渠道 {name}：跳过保活，{reason}；客户端仍可尝试此渠道"));
                return;
            }
        }
        let model = heartbeat_model(&provider).unwrap_or("未配置");
        let network = heartbeat_network_hint(state, &provider);
        crate::runtime_log::record("INFO", format!("渠道 {name}：发送保活，接口 {path}，模型 {model}，网络配置：{network}"));
        let started = Instant::now();
        match send_idle_keepalive(state, &provider, ctx).await {
            Ok(IdleKeepaliveOutcome::Sent) => {
                crate::runtime_log::record("INFO", format!("渠道 {name}：保活成功，接口 {path}，模型 {model}，耗时 {} ms", started.elapsed().as_millis()));
                return;
            },
            Ok(IdleKeepaliveOutcome::Skipped(reason)) => {
                crate::runtime_log::record("INFO", format!("渠道 {name}：跳过保活，{reason}；客户端仍可尝试此渠道"));
                return;
            },
            Err(err) if err.aborted => return,
            Err(err) => {
                if path == "/responses" && native_template_skip_reason(state, &provider).is_some() {
                    crate::runtime_log::record("WARN", format!("渠道 {name}：保活失败（接口 {path}，状态 {}，{}），暂停当前模板，等待新的真实 Responses 请求；客户端仍可尝试此渠道", err.status.as_u16(), keepalive_error_hint(&err)));
                    return;
                }
                let interval = Duration::from_secs(provider.heartbeat_interval_secs.clamp(1, 3600));
                let delay = err.retry_after.unwrap_or(interval).max(interval);
                crate::runtime_log::record("WARN", format!("渠道 {name}：保活失败（接口 {path}，状态 {}，{}），{} 秒后后台重试；客户端仍可尝试此渠道", err.status.as_u16(), keepalive_error_hint(&err), delay.as_secs()));
                // 分段等待，避免极端 Retry-After 溢出，并允许配置关闭时及时取消。
                let mut remaining = delay;
                while !remaining.is_zero() {
                    let step = remaining.min(Duration::from_secs(1));
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(step) => {},
                    }
                    remaining = remaining.saturating_sub(step);
                    let cfg = state.snapshot();
                    if !cfg.providers.iter().any(|p| p.name == name && p.enabled && p.heartbeat_enabled) {
                        return;
                    }
                }
            }
        }
    }
}

fn heartbeat_model(provider: &ProviderConfig) -> Option<&str> {
    let selected = provider.heartbeat_model.trim();
    if !selected.is_empty() {
        Some(selected)
    } else {
        provider.models.iter().map(|model| model.trim()).find(|model| !model.is_empty())
    }
}

fn heartbeat_network_hint(state: &AppState, provider: &ProviderConfig) -> &'static str {
    if provider.auth_account_id.is_none() && !provider.use_proxy {
        return "未启用渠道代理，使用默认网络设置";
    }
    let cfg = state.snapshot();
    let proxy = cfg.routing.auth_proxy.trim();
    if proxy.is_empty() {
        "已启用渠道代理但未填写地址，使用默认网络设置"
    } else if reqwest::Proxy::all(proxy).is_err() {
        "渠道代理地址无效，使用默认网络设置"
    } else {
        "使用渠道网络代理"
    }
}

// 只输出已知问题分类，不把可能含凭据或请求内容的上游错误原文写入日志。
fn keepalive_error_hint(err: &ProxyError) -> &'static str {
    if err.status.is_server_error() {
        return "上游服务异常或网关超时，暂不能据此判断模型或模板无效";
    }
    if err.status == StatusCode::TOO_MANY_REQUESTS {
        return "上游限流，按心跳间隔及 Retry-After 等待重试";
    }
    if err.status == StatusCode::REQUEST_TIMEOUT {
        return "上游请求超时，等待后台重试";
    }
    let message = err.message.to_ascii_lowercase();
    if message.contains("invalid_responses_request") {
        "上游拒绝 Responses 请求格式（invalid_responses_request），需检查模板兼容性"
    } else if message.contains("invalid codex request") {
        "上游拒绝保活请求（invalid codex request），无法仅凭此错误确定是请求头还是请求体不兼容"
    } else if message.contains("codex_access_restricted") {
        "上游限制客户端访问（codex_access_restricted），请核对渠道允许的客户端与接入方式"
    } else if message.contains("session_id") || message.contains("conversation_id")
        || message.contains("originator")
    {
        "上游拒绝客户端标识或会话头，请检查渠道自定义请求头与客户端兼容要求"
    } else if message.contains("max_output_tokens") {
        "上游不接受当前 max_output_tokens，请检查输出下限或字段支持"
    } else if message.contains("max_completion_tokens") || message.contains("max_tokens") {
        "上游不接受当前输出 Token 参数，请检查字段支持或输出下限"
    } else if message.contains("stream") {
        "上游不接受当前流式设置，请检查接口要求"
    } else if message.contains("model") || message.contains("模型") {
        "上游拒绝心跳模型，请检查模型名称、映射和渠道权限"
    } else if message.contains("responses") || message.contains("chat/completions") {
        "上游拒绝当前接口，请检查渠道 Responses 模式"
    } else if message.contains("instructions") || message.contains("input") {
        "上游要求不同的输入格式或 instructions 参数"
    } else if message.contains("tool_choice") || message.contains("tools") {
        "上游错误涉及 tools 或 tool_choice，请检查保留的工具定义与选择参数"
    } else if message.contains("reasoning") {
        "上游错误涉及 reasoning，请检查推理参数"
    } else if message.contains("store") {
        "上游错误涉及 store，请检查存储参数"
    } else if err.status == StatusCode::BAD_REQUEST {
        "上游拒绝保活参数，需结合上游错误说明检查接口和模型"
    } else {
        "上游调用失败"
    }
}

/// 缓存成功真实请求的协议模板，并在本地保存以供重启恢复。
#[derive(Clone)]
pub(super) struct NativeKeepaliveTemplate {
    body: Value,
    headers: HeaderMap,
    generation: Uuid,
    succeeded: bool,
    failed: bool,
}

impl NativeKeepaliveTemplate {
    fn record_outcome(&mut self, result: &Result<(), ProxyError>) {
        match result {
            Ok(()) => self.succeeded = true,
            Err(err) if !err.aborted
                && !err.status.is_server_error()
                && err.status != StatusCode::TOO_MANY_REQUESTS
                && err.status != StatusCode::REQUEST_TIMEOUT => {
                let message = err.message.to_ascii_lowercase();
                let invalid = err.status == StatusCode::BAD_REQUEST
                    && (message.contains("invalid codex request") || message.contains("invalid_responses_request"));
                if !self.succeeded || invalid { self.failed = true; }
            }
            _ => {},
        }
    }

    pub(super) fn from_request(body: &Value, headers: &HeaderMap) -> Self {
        let mut template = if body.is_object() { body.clone() } else { json!({}) };
        if let Some(obj) = template.as_object_mut() {
            obj.remove("previous_response_id");
            if obj.contains_key("instructions") {
                obj.insert("instructions".into(), json!("You are Codex. Reply briefly."));
            }
            obj.insert("input".into(), heartbeat_input(body.get("input")));
            obj.insert("stream".into(), json!(true));
            obj.insert("max_output_tokens".into(), json!(1));
        }
        Self {
            body: template,
            headers: safe_template_headers(headers),
            generation: Uuid::new_v4(),
            succeeded: false,
            failed: false,
        }
    }

    fn skip_reason(&self) -> Option<&'static str> {
        if self.failed {
            Some("当前保活模板失败，等待新的真实 Responses 请求")
        } else if self.headers.is_empty() {
            Some("尚无真实 Responses 请求头模板")
        } else {
            None
        }
    }
}

fn template_key(provider: &ProviderConfig) -> String {
    format!("{}|{}", provider.name, provider.base_url)
}

pub(super) async fn remember_native_template(state: &AppState, provider: &ProviderConfig, mut template: NativeKeepaliveTemplate) {
    let (previous, has_headers) = {
        let mut templates = state.keepalive_templates.lock();
        let key = template_key(provider);
        // 与 Python 一致：新请求没有可复用头时保留该渠道、该地址上一次的协议头。
        if template.headers.is_empty() {
            if let Some(previous) = templates.get(&key) {
                template.headers = previous.headers.clone();
            }
        }
        let has_headers = !template.headers.is_empty();
        (templates.insert(key, template), has_headers)
    };
    let saved = template_store::persist_native_template(state, provider).await;
    if previous.as_ref().is_none_or(|previous| previous.failed || previous.headers.is_empty()) {
        crate::runtime_log::record("INFO", format!(
            "渠道 {}：已捕获成功的 Responses 保活模板（{}），{}",
            provider.name, if has_headers { "包含客户端请求头" } else { "仍缺少可复用客户端请求头" },
            if saved { "已保存到本地模板文件，可在重启后复用" } else { "当前仅在内存中可用" },
        ));
    }
}

fn retain_configured_templates(state: &AppState, cfg: &AppConfig) {
    // 关闭保活或暂时停用渠道不丢弃模板；删除渠道、改地址后才清理。
    let keys: HashSet<_> = cfg.providers.iter().map(template_key).collect();
    state.keepalive_templates.lock().retain(|key, _| keys.contains(key));
}

fn native_template_skip_reason(state: &AppState, provider: &ProviderConfig) -> Option<&'static str> {
    match state.keepalive_templates.lock().get(&template_key(provider)) {
        Some(template) => template.skip_reason(),
        None => Some("尚无可用的本地 Responses 模板，等待成功的真实 Responses 请求"),
    }
}

fn safe_template_headers(headers: &HeaderMap) -> HeaderMap {
    let mut safe = HeaderMap::new();
    for (name, value) in headers {
        let name_str = name.as_str();
        // 与 Python 的协议头白名单一致，并保留本项目已有的会话头透传能力。
        // 请求 ID 每次重新生成；客户端凭据绝不能进入模板。
        if matches!(name_str, "authorization" | "proxy-authorization" | "api-key" | "openai-api-key"
            | "x-api-key" | "x-api-token" | "x-auth-token" | "x-goog-api-key"
            | "cookie" | "set-cookie" | "x-request-id")
        {
            continue;
        }
        let allowed = matches!(name_str, "user-agent" | "originator" | "session_id" | "conversation_id")
            || ["codex-", "openai-", "x-codex-", "x-openai-", "x-stainless-"]
                .iter().any(|prefix| name_str.starts_with(prefix));
        if allowed && value.as_bytes().len() <= 4096
            && value.to_str().is_ok_and(|value| !value.trim().is_empty())
        {
            safe.insert(name.clone(), value.clone());
        }
    }
    safe
}

fn heartbeat_input(input: Option<&Value>) -> Value {
    if input.is_some_and(Value::is_string) {
        return json!("Hi");
    }
    if let Some(items) = input.and_then(Value::as_array) {
        for item in items.iter().rev() {
            if item.get("role").and_then(Value::as_str) != Some("user") { continue; }
            if item.get("content").is_some_and(Value::is_string) {
                return json!([{"role": "user", "content": "Hi"}]);
            }
            if let Some(parts) = item.get("content").and_then(Value::as_array) {
                if let Some(index) = parts.iter().rposition(|part| part.get("type").and_then(Value::as_str) == Some("input_text")) {
                    let mut item = item.clone();
                    // 严格沿用 Python 的替换规则：保留其它上下文块，仅替换最后一段文本。
                    item["content"][index]["text"] = json!("Hi");
                    return json!([item]);
                }
            }
        }
    }
    json!([{"role": "user", "content": [{"type": "input_text", "text": "Hi"}]}])
}

fn native_heartbeat_body(template: &Value, model: &str) -> Value {
    let mut body = template.clone();
    body["model"] = json!(model);
    body.as_object_mut().expect("Responses template is an object")
        .entry("store").or_insert(json!(false));
    body
}

fn native_heartbeat_headers(provider: &ProviderConfig, mut headers: HeaderMap) -> HeaderMap {
    headers.entry("openai-beta").or_insert(HeaderValue::from_static("codex=v1"));
    headers.entry(header::USER_AGENT).or_insert(HeaderValue::from_static("codex-cli/0.121.0"));
    let mut headers = upstream_headers(provider, &headers, true);
    headers.entry("x-request-id").or_insert_with(|| {
        HeaderValue::from_str(&Uuid::new_v4().to_string()).expect("UUID is a valid header")
    });
    headers
}

async fn send_native_heartbeat(state: &AppState, provider: &ProviderConfig, model: &str, ctx: &RequestContext) -> Result<IdleKeepaliveOutcome, ProxyError> {
    let template = state.keepalive_templates.lock().get(&template_key(provider)).cloned();
    let Some(template) = template else {
        return Ok(IdleKeepaliveOutcome::Skipped("尚无可用的本地 Responses 模板，等待成功的真实 Responses 请求"));
    };
    if let Some(reason) = template.skip_reason() {
        return Ok(IdleKeepaliveOutcome::Skipped(reason));
    }
    let mut body = native_heartbeat_body(&template.body, model);
    apply_model_mapping(provider, &mut body);
    let headers = native_heartbeat_headers(provider, template.headers);
    let result = post_native_heartbeat(state, provider, body, headers, ctx).await;
    let changed = if let Some(current) = state.keepalive_templates.lock().get_mut(&template_key(provider)) {
        // 新真实请求可能已发布新模板，旧保活的迟到结果不能覆盖它。
        if current.generation == template.generation {
            let before = (current.succeeded, current.failed);
            current.record_outcome(&result);
            before != (current.succeeded, current.failed)
        } else {
            false
        }
    } else {
        false
    };
    if changed {
        template_store::persist_native_template(state, provider).await;
    }
    result.map(|_| IdleKeepaliveOutcome::Sent)
}

async fn post_native_heartbeat(state: &AppState, provider: &ProviderConfig, body: Value, headers: HeaderMap, ctx: &RequestContext) -> Result<(), ProxyError> {
    let cancel = ctx.cancel_token();
    let operation = async {
        let response = send_stream_request(
            state.client_for_provider(provider).post(upstream_url(provider, "/responses"))
                .headers(headers).json(&body),
            provider.request_timeout, &cancel,
        ).await.map_err(|err| err.into_proxy_error())?;
        if !response.status().is_success() {
            let status = response.status();
            let retry_after = parse_retry_after(response.headers());
            let text = response.text().await.unwrap_or_default();
            return Err(native_keepalive_http_error(status, &text, retry_after));
        }
        validate_upstream_sse_content_type(response.headers())
            .map_err(|msg| ProxyError::new(StatusCode::BAD_GATEWAY, msg))?;
        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| ProxyError::new(StatusCode::BAD_GATEWAY, "保活流读取失败"))?;
            if buffer.len() + chunk.len() > 1024 * 1024 {
                return Err(ProxyError::new(StatusCode::BAD_GATEWAY, "保活 SSE 事件过大"));
            }
            buffer.extend_from_slice(&chunk);
            loop {
                let text = String::from_utf8_lossy(&buffer);
                let Some((event, consumed)) = next_sse_event(&text) else { break; };
                buffer.drain(..consumed);
                if let Some(data) = sse_data(&event) {
                    if let Some(result) = native_heartbeat_event(&data) { return result; }
                }
            }
        }
        Err(ProxyError::new(StatusCode::BAD_GATEWAY, "保活流缺少结束事件"))
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(ProxyError::aborted()),
        result = tokio::time::timeout(Duration::from_secs(provider.request_timeout.max(1)), operation) => {
            result.unwrap_or_else(|_| Err(ProxyError::new(StatusCode::GATEWAY_TIMEOUT, "保活响应超时")))
        }
    }
}

fn native_keepalive_http_error(status: StatusCode, text: &str, retry_after: Option<Duration>) -> ProxyError {
    let mut err = upstream_status_error(status, text, retry_after);
    if let Ok(body) = serde_json::from_str::<Value>(text) {
        // 仅保留已知协议标识；不能把任意 code/param（可能包含敏感值）写入日志。
        for pointer in ["/error/code", "/error/type", "/error/param", "/code", "/type", "/param"] {
            if let Some(value) = body.pointer(pointer).and_then(Value::as_str) {
                if matches!(value,
                    "invalid_responses_request" | "codex_access_restricted" |
                    "max_output_tokens" | "max_completion_tokens" | "max_tokens" |
                    "model" | "stream" | "instructions" | "input" | "tools" |
                    "tool_choice" | "reasoning" | "store" | "session_id" |
                    "conversation_id" | "originator")
                {
                    err.message.push_str(" [");
                    err.message.push_str(value);
                    err.message.push(']');
                }
            }
        }
    }
    err
}

fn native_heartbeat_event(data: &str) -> Option<Result<(), ProxyError>> {
    let value: Value = serde_json::from_str(data).ok()?;
    match value.get("type").and_then(Value::as_str) {
        Some("response.completed") => Some(Ok(())),
        Some("response.incomplete") if value.pointer("/response/incomplete_details/reason").and_then(Value::as_str) == Some("max_output_tokens") => Some(Ok(())),
        Some("response.incomplete") => Some(Err(ProxyError::new(StatusCode::BAD_GATEWAY, "保活响应未完成"))),
        Some("error" | "response.failed") => {
            match inspect_responses_sse_probe_event(data) {
                SseProbeDecision::Error(err) => Some(Err(err)),
                _ => None,
            }
        }
        _ => None,
    }
}

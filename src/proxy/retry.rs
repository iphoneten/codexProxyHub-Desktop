//! 业务请求的唯一重试调度层：每个渠道按次数尝试，全部失败后再开始下一轮。
use super::*;

pub(super) struct ProviderRetryCycle {
    providers: Vec<(ProviderConfig, String)>,
    model: String,
    api: &'static str,
    check_daily_token_limit: bool,
    initial_allowed: Vec<String>,
    index: usize,
    used: usize,
    round: usize,
    failures: usize,
    preferred: Option<(String, String)>,
    // 只属于本次请求的重试退避，不是共享熔断/冷却状态，也不阻挡其他请求。
    retry_at: HashMap<String, tokio::time::Instant>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Uri;

    fn configured_state(base: &str, channels: &[(&str, usize)]) -> AppState {
        let state = super::super::tests::test_state();
        let mut cfg = (*state.snapshot()).clone();
        cfg.providers = channels
            .iter()
            .enumerate()
            .map(|(priority, (name, attempts))| {
                serde_json::from_value(json!({
                    "name": name, "base_url": format!("{base}/{name}/v1"),
                    "api_key": "synthetic-upstream", "models": ["test-model"],
                    "max_retries": attempts, "priority": priority, "responses_mode": "native",
                    "request_timeout": 2, "persistent_retry": true
                }))
                .unwrap()
            })
            .collect();
        cfg.auth_accounts.clear();
        cfg.routing.model_fallbacks.clear();
        cfg.routing.auth_proxy.clear();
        cfg.auth.enabled = true;
        cfg.auth.api_keys = vec![serde_json::from_value(json!({
            "key": "synthetic-local", "name": "retry-test", "max_concurrency": 1
        }))
        .unwrap()];
        cfg.usage_log.sqlite_path = std::env::temp_dir()
            .join(format!("routehub-retry-{}.sqlite3", Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        *state.config.write() = Arc::new(cfg);
        state
    }

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer synthetic-local"),
        );
        headers
    }

    async fn call(
        state: AppState,
        ctx: RequestContext,
        api: &str,
        stream: bool,
    ) -> Result<Response, ProxyError> {
        if api == "responses" {
            responses(
                State(state),
                Extension(ctx),
                headers(),
                Json(json!({
                    "model": "test-model", "input": "hello", "stream": stream
                })),
            )
            .await
        } else {
            chat_completions(State(state), Extension(ctx), headers(), Json(json!({
                "model": "test-model", "messages": [{"role":"user","content":"hello"}], "stream": stream
            }))).await
        }
    }

    fn success(api: &str) -> Value {
        if api == "responses" {
            json!({"id":"resp_test","model":"test-model","status":"completed","output":[
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}
            ]})
        } else {
            json!({"model":"test-model","choices":[{"message":{"role":"assistant","content":"answer"},"finish_reason":"stop"}]})
        }
    }

    async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, server)
    }

    #[tokio::test]
    async fn cycle_uses_five_then_three_and_rechecks_higher_priority_after_success() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let recovered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let recovery = recovered.clone();
        let app = Router::new().fallback(move |uri: Uri| {
            let seen = seen.clone();
            let recovery = recovery.clone();
            async move {
                let name = uri.path().split('/').nth(1).unwrap().to_string();
                let number = {
                    let mut calls = seen.lock();
                    calls.push(name.clone());
                    calls.len()
                };
                if name == "C" || (name == "A" && recovery.load(Ordering::SeqCst)) {
                    Json(success("responses")).into_response()
                } else {
                    let code = [400, 401, 403, 404, 429, 500, 502, 503][(number - 1) % 8];
                    (
                        StatusCode::from_u16(code).unwrap(),
                        [("retry-after", "0")],
                        Json(json!({"error":{"message":"synthetic failure"}})),
                    )
                        .into_response()
                }
            }
        });
        let (base, server) = serve(app).await;
        let state = configured_state(&base, &[("A", 5), ("B", 3), ("C", 1)]);
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            call(
                state.clone(),
                RequestContext::detached(),
                "responses",
                false,
            ),
        )
        .await;
        assert_eq!(result.unwrap().unwrap().status(), StatusCode::OK);
        assert_eq!(*calls.lock(), ["A", "A", "A", "A", "A", "B", "B", "B", "C"]);
        recovered.store(true, Ordering::SeqCst);
        let second = call(state, RequestContext::detached(), "responses", false)
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        server.abort();
        assert_eq!(calls.lock().last().unwrap(), "A");
        assert_eq!(calls.lock().len(), 10);
    }

    #[tokio::test]
    async fn cycle_priority_order_holds_across_rounds_and_stale_affinity() {
        for api in ["chat", "responses"] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let seen = calls.clone();
            let app = Router::new().fallback(move |uri: Uri| {
                let seen = seen.clone();
                async move {
                    let number = {
                        let mut calls = seen.lock();
                        calls.push(uri.path().split('/').nth(1).unwrap().to_string());
                        calls.len()
                    };
                    if number == 8 {
                        Json(success(api)).into_response()
                    } else {
                        (StatusCode::BAD_GATEWAY, [("retry-after", "0")], "failed").into_response()
                    }
                }
            });
            let (base, server) = serve(app).await;
            // 故意打乱配置顺序，并把上次成功记录指向低优先级 C。
            let state = configured_state(&base, &[("C", 1), ("A", 2), ("B", 1)]);
            let mut cfg = (*state.snapshot()).clone();
            for provider in &mut cfg.providers {
                provider.priority = match provider.name.as_str() {
                    "A" => -5,
                    "B" => 10,
                    _ => 20,
                };
            }
            *state.config.write() = Arc::new(cfg);
            state.remember_affinity_provider(
                &api_key_affinity_key("test-model", &api_key_id("synthetic-local")),
                "C", "test-model",
            );

            let result = tokio::time::timeout(Duration::from_secs(10), call(
                state, RequestContext::detached(), api, false,
            )).await;
            server.abort();

            assert_eq!(result.unwrap().unwrap().status(), StatusCode::OK);
            assert_eq!(*calls.lock(), ["A", "A", "B", "C", "A", "A", "B", "C"]);
        }
    }

    #[tokio::test]
    async fn cycle_restarts_after_all_channels_fail() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let app = Router::new().fallback(move |uri: Uri| {
            let seen = seen.clone();
            async move {
                let number = {
                    let mut calls = seen.lock();
                    calls.push(uri.path().split('/').nth(1).unwrap().to_string());
                    calls.len()
                };
                if number == 7 {
                    Json(success("chat")).into_response()
                } else {
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", "0")],
                        "busy",
                    )
                        .into_response()
                }
            }
        });
        let (base, server) = serve(app).await;
        let state = configured_state(&base, &[("A", 2), ("B", 1)]);
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            call(state, RequestContext::detached(), "chat", false),
        )
        .await;
        server.abort();
        assert_eq!(result.unwrap().unwrap().status(), StatusCode::OK);
        assert_eq!(*calls.lock(), ["A", "A", "B", "A", "A", "B", "A"]);
    }

    #[tokio::test]
    async fn cycle_single_channel_continues_beyond_one_round() {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let app = Router::new().fallback(move || {
            let seen = seen.clone();
            async move {
                if seen.fetch_add(1, Ordering::SeqCst) < 6 {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        [("retry-after", "0")],
                        "failed",
                    )
                        .into_response()
                } else {
                    Json(success("chat")).into_response()
                }
            }
        });
        let (base, server) = serve(app).await;
        let state = configured_state(&base, &[("A", 3)]);
        let mut cfg = (*state.snapshot()).clone();
        cfg.providers[0].persistent_retry = false;
        *state.config.write() = Arc::new(cfg);
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            call(state, RequestContext::detached(), "chat", false),
        )
        .await;
        server.abort();
        assert_eq!(result.unwrap().unwrap().status(), StatusCode::OK);
        assert_eq!(count.load(Ordering::SeqCst), 7);
    }

    #[tokio::test]
    async fn cycle_wait_is_cancelled_by_client_or_server() {
        for stop_server in [false, true] {
            let started = Arc::new(tokio::sync::Notify::new());
            let seen = started.clone();
            let count = Arc::new(AtomicUsize::new(0));
            let counted = count.clone();
            let app = Router::new().fallback(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                seen.notify_one();
                async {
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", "120")],
                        "busy",
                    )
                }
            });
            let (base, server) = serve(app).await;
            let state = configured_state(&base, &[("A", 5)]);
            let ctx = RequestContext::detached();
            let cancel = if stop_server {
                state.root_cancel.clone()
            } else {
                ctx.cancel_token()
            };
            let task = tokio::spawn(call(state.clone(), ctx, "chat", false));
            tokio::time::timeout(Duration::from_secs(2), started.notified())
                .await
                .unwrap();
            cancel.cancel();
            let error = tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            server.abort();
            assert!(error.aborted);
            assert_eq!(count.load(Ordering::SeqCst), 1);
            assert_eq!(provider_inflight(&state, "A"), 0);
        }
    }

    fn queue(state: &AppState) -> ProviderRetryCycle {
        let providers = provider_attempts(&state.snapshot(), state, "test-model", "responses", &[]);
        ProviderRetryCycle::new(providers, "test-model", "responses", vec![])
    }

    fn configured_anthropic_state() -> AppState {
        let state = configured_state("http://127.0.0.1:1", &[("A", 1)]);
        let mut cfg = (*state.snapshot()).clone();
        cfg.providers[0].provider_type = "anthropic".into();
        *state.config.write() = Arc::new(cfg);
        state
    }

    fn messages_queue(state: &AppState) -> ProviderRetryCycle {
        let cfg = state.snapshot();
        let allowed = cfg.auth.api_keys[0].allowed_providers.clone();
        let providers = provider_attempts(&cfg, state, "test-model", "messages", &allowed);
        ProviderRetryCycle::new(providers, "test-model", "messages", allowed)
    }

    fn failure(provider: &ProviderConfig, seconds: u64) -> AttemptFailure {
        AttemptFailure::new(
            provider,
            "test-model",
            Instant::now(),
            ProxyError::new(StatusCode::TOO_MANY_REQUESTS, "busy")
                .with_retry_after(Some(Duration::from_secs(seconds))),
        )
    }

    #[tokio::test]
    async fn cycle_retry_after_does_not_delay_a_different_channel() {
        let state = configured_state("http://127.0.0.1:1", &[("A", 1), ("B", 1)]);
        let ctx = RequestContext::detached();
        let mut cycle = queue(&state);
        let (a, _) = cycle
            .next(&state, &ctx, &headers(), None)
            .await
            .unwrap()
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            cycle.next(&state, &ctx, &headers(), Some(failure(&a, 120))),
        )
        .await;
        let (b, _) = result.unwrap().unwrap().unwrap();
        assert_eq!(a.name, "A");
        assert_eq!(b.name, "B");
        assert_eq!(b.max_retries, 0);
        assert!(!b.persistent_retry);
    }

    #[tokio::test]
    async fn cycle_rechecks_disabled_channels_and_revoked_keys() {
        for revoke_key in [false, true] {
            let state = configured_state("http://127.0.0.1:1", &[("A", 5)]);
            let ctx = RequestContext::detached();
            let mut cycle = queue(&state);
            let (a, _) = cycle
                .next(&state, &ctx, &headers(), None)
                .await
                .unwrap()
                .unwrap();
            let mut cfg = (*state.snapshot()).clone();
            if revoke_key {
                cfg.auth.api_keys[0].enabled = false;
            } else {
                cfg.providers[0].enabled = false;
            }
            *state.config.write() = Arc::new(cfg);
            let result = cycle
                .next(&state, &ctx, &headers(), Some(failure(&a, 120)))
                .await;
            if revoke_key {
                assert!(matches!(result, Err(err) if err.status == StatusCode::UNAUTHORIZED));
            } else {
                assert!(result.unwrap().is_none());
            }
        }
    }

    #[tokio::test]
    async fn cycle_skips_disallowed_and_unmatched_channels() {
        let state = configured_state(
            "http://127.0.0.1:1",
            &[("denied", 5), ("wrong-model", 5), ("allowed", 0)],
        );
        let mut cfg = (*state.snapshot()).clone();
        cfg.providers[1].models = vec!["other-model".into()];
        cfg.auth.api_keys[0].allowed_providers = vec!["wrong-model".into(), "allowed".into()];
        *state.config.write() = Arc::new(cfg);
        let mut cycle = queue(&state);
        let (p, _) = cycle
            .next(&state, &RequestContext::detached(), &headers(), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.name, "allowed");
        assert_eq!(cycle.used, 1);
    }

    #[tokio::test]
    async fn cycle_stream_error_before_output_retries_native_and_translated_responses() {
        for translated in [false, true] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let seen = calls.clone();
            let app = Router::new().fallback(move |uri: Uri| {
                let name = uri.path().split('/').nth(1).unwrap().to_string();
                seen.lock().push(name.clone());
                async move {
                    let text = if name == "A" {
                        "data: {\"error\":{\"message\":\"synthetic failure\"},\"type\":\"error\"}\n\n"
                    } else if translated {
                        "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                    } else {
                        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
                    };
                    ([("content-type", "text/event-stream")], text)
                }
            });
            let (base, server) = serve(app).await;
            let state = configured_state(&base, &[("A", 2), ("B", 1)]);
            if translated {
                let mut cfg = (*state.snapshot()).clone();
                for provider in &mut cfg.providers {
                    provider.responses_mode = "chat".into();
                }
                *state.config.write() = Arc::new(cfg);
            }
            let response = tokio::time::timeout(
                Duration::from_secs(10),
                call(state, RequestContext::detached(), "responses", true),
            )
            .await
            .unwrap()
            .unwrap();
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            server.abort();
            assert!(String::from_utf8(bytes.to_vec())
                .unwrap()
                .contains("answer"));
            assert_eq!(*calls.lock(), ["A", "A", "B"]);
        }
    }

    #[tokio::test]
    async fn cycle_does_not_replay_after_client_output() {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let app = Router::new().fallback(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            async {
                (
                    [("content-type", "text/event-stream")],
                    concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
                "data: {\"type\":\"error\",\"error\":{\"message\":\"failed after output\"}}\n\n"
            ),
                )
            }
        });
        let (base, server) = serve(app).await;
        let state = configured_state(&base, &[("A", 5), ("B", 3)]);
        let response = call(state, RequestContext::detached(), "responses", true)
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        server.abort();
        assert!(String::from_utf8(body.to_vec())
            .unwrap()
            .contains("partial"));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cycle_token_estimate_ignores_daily_limit_but_business_returns_429() {
        let state = configured_anthropic_state();
        let mut cfg = (*state.snapshot()).clone();
        cfg.auth.api_keys[0].daily_token_limit = Some(10);
        let path = cfg.usage_log_sqlite_path();
        *state.config.write() = Arc::new(cfg);
        usage_log::log_usage_sqlite_for_key(
            path.clone(),
            &chrono::Local::now().format("%Y-%m-%d 12:00:00").to_string(),
            "messages",
            &api_key_id("synthetic-local"),
            "retry-test",
            "ok",
            "A",
            "test-model",
            "test-model",
            100,
            None,
            "",
            6,
            4,
            "upstream_or_unknown",
            "synthetic-request",
        )
        .unwrap();

        let ctx = RequestContext::detached();
        let mut estimate = messages_queue(&state).for_token_estimate();
        let (provider, model) = estimate
            .next(&state, &ctx, &headers(), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(estimate.api, "messages");
        assert_eq!(provider.name, "A");
        assert_eq!(provider.provider_type, "anthropic");
        assert_eq!(model, "test-model");

        let error = messages_queue(&state)
            .next(&state, &ctx, &headers(), None)
            .await
            .unwrap_err();
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(error.message.contains("今日 Token 用量已达上限"));
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn cycle_token_estimate_preserves_key_and_model_permissions() {
        for (restriction, expected) in [
            ("missing-key", StatusCode::UNAUTHORIZED),
            ("invalid-key", StatusCode::UNAUTHORIZED),
            ("disabled-key", StatusCode::UNAUTHORIZED),
            ("disallowed-model", StatusCode::FORBIDDEN),
        ] {
            let state = configured_anthropic_state();
            let mut cycle = messages_queue(&state).for_token_estimate();
            let mut cfg = (*state.snapshot()).clone();
            let mut request_headers = headers();
            match restriction {
                "missing-key" => request_headers.clear(),
                "invalid-key" => {
                    request_headers.insert(
                        header::AUTHORIZATION,
                        HeaderValue::from_static("Bearer synthetic-invalid"),
                    );
                }
                "disabled-key" => cfg.auth.api_keys[0].enabled = false,
                _ => cfg.auth.api_keys[0].allowed_models = vec!["other-model".into()],
            }
            *state.config.write() = Arc::new(cfg);

            let error = cycle
                .next(&state, &RequestContext::detached(), &request_headers, None)
                .await
                .unwrap_err();
            assert_eq!(error.status, expected, "{restriction}");
        }
    }

    #[tokio::test]
    async fn cycle_token_estimate_preserves_channel_permissions_and_api_filter() {
        for restriction in [
            "disabled-provider",
            "disallowed-provider",
            "initially-disallowed-provider",
            "unsupported-model",
            "unsupported-api",
        ] {
            let state = configured_anthropic_state();
            let mut cfg = (*state.snapshot()).clone();
            if restriction == "initially-disallowed-provider" {
                cfg.auth.api_keys[0].allowed_providers = vec!["other-channel".into()];
                *state.config.write() = Arc::new(cfg.clone());
            }
            let mut cycle = messages_queue(&state).for_token_estimate();
            match restriction {
                "disabled-provider" => cfg.providers[0].enabled = false,
                "disallowed-provider" => {
                    cfg.auth.api_keys[0].allowed_providers = vec!["other-channel".into()];
                }
                "initially-disallowed-provider" => cfg.auth.api_keys[0].allowed_providers.clear(),
                "unsupported-model" => cfg.providers[0].models = vec!["other-model".into()],
                _ => cfg.providers[0].provider_type = "openai".into(),
            }
            *state.config.write() = Arc::new(cfg);

            assert!(
                cycle
                    .next(&state, &RequestContext::detached(), &headers(), None)
                    .await
                    .unwrap()
                    .is_none(),
                "{restriction}"
            );
        }
    }

    #[tokio::test]
    async fn cycle_count_tokens_retries_http_and_json_errors() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let app = Router::new().fallback(move |uri: Uri| {
            let name = uri.path().split('/').nth(1).unwrap().to_string();
            let number = {
                let mut calls = seen.lock();
                calls.push(name.clone());
                calls.len()
            };
            async move {
                if name == "B" {
                    Json(json!({"input_tokens": 7})).into_response()
                } else if number == 1 {
                    Json(json!({"error": {"message": "synthetic error"}})).into_response()
                } else {
                    (StatusCode::BAD_GATEWAY, [("retry-after", "0")], "failed").into_response()
                }
            }
        });
        let (base, server) = serve(app).await;
        let state = configured_state(&base, &[("A", 2), ("B", 1)]);
        let mut cfg = (*state.snapshot()).clone();
        for provider in &mut cfg.providers {
            provider.provider_type = "anthropic".into();
        }
        cfg.auth.api_keys[0].daily_token_limit = Some(1);
        // 已达每日限额仍允许只读估算，且估算失败/成功都不能新增用量记录。
        let token_id = api_key_id("synthetic-local");
        log_usage(&cfg, Instant::now(), UsageLogEvent {
            api: "messages", api_key_id: &token_id, api_key_name: "retry-test",
            provider: "A", model: "test-model", upstream_model: "test-model", status: "ok",
            error: None, usage: TokenUsage { input: 1, output: 0 }, first_token_ms: None,
            token_source: None, request_id: "synthetic-estimate-setup",
        });
        let log_path = cfg.usage_log_sqlite_path();
        *state.config.write() = Arc::new(cfg);
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            count_tokens(
                State(state),
                Extension(RequestContext::detached()),
                headers(),
                Json(json!({"model":"test-model", "messages":[{"role":"user","content":"hello"}]})),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        server.abort();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["input_tokens"],
            7
        );
        assert_eq!(*calls.lock(), ["A", "A", "B"]);
        let conn = open_usage_log_connection(log_path).unwrap();
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM usage_logs", [], |row| row.get(0)).unwrap();
        assert_eq!(rows, 1);
    }
}

impl ProviderRetryCycle {
    pub(super) fn new(
        providers: Vec<(ProviderConfig, String)>,
        model: &str,
        api: &'static str,
        initial_allowed: Vec<String>,
    ) -> Self {
        Self {
            providers,
            model: model.to_string(),
            api,
            check_daily_token_limit: true,
            initial_allowed,
            index: 0,
            used: 0,
            round: 1,
            failures: 0,
            preferred: None,
            retry_at: HashMap::new(),
        }
    }

    pub(super) fn prefer_provider(&mut self, provider: &str, request_model: &str) {
        if !provider.trim().is_empty() {
            self.preferred = Some((provider.to_string(), request_model.to_string()));
        }
    }

    /// 只读 Token 估算跳过每日额度检查，鉴权及模型、渠道限制仍照常执行。
    pub(super) fn for_token_estimate(mut self) -> Self {
        self.check_daily_token_limit = false;
        self
    }

    pub(super) async fn next(
        &mut self,
        state: &AppState,
        ctx: &RequestContext,
        headers: &HeaderMap,
        failure: Option<AttemptFailure>,
    ) -> Result<Option<(ProviderConfig, String)>, ProxyError> {
        if let Some(failure) = failure {
            if should_stop_failover(failure.status) {
                return Err(ProxyError::aborted());
            }
            self.failures = self.failures.saturating_add(1);
            let delay = compute_retry_delay(self.failures - 1, failure.retry_after);
            self.retry_at.insert(
                failure.provider.clone(),
                tokio::time::Instant::now() + delay,
            );
            {
                let group = failure_group_key(ctx.log_scope(), self.api, &failure.provider,
                    &failure.request_model, "", failure.status.as_u16(), &failure.message);
                crate::runtime_log::record_repeated(
                    "WARN",
                    format!(
                        "渠道 {}：第 {} 轮尝试失败（HTTP {}）；按每轮次数继续轮询",
                        failure.provider,
                        self.round,
                        failure.status.as_u16(),
                    ),
                    group,
                );
            }
        }

        let cancel = ctx.cancel_token();
        loop {
            if cancel.is_cancelled() || state.root_cancel.is_cancelled() {
                return Err(ProxyError::aborted());
            }
            let cfg = state.snapshot();
            let allowed = self.current_allowed(&cfg, headers)?;
            if self.index >= self.providers.len() {
                self.providers = provider_attempts(&cfg, state, &self.model, self.api, &allowed);
                self.providers.retain(|(provider, _)| {
                    api_key_allows_provider(&self.initial_allowed, &provider.name)
                });
                dedupe_provider_attempts(&mut self.providers);
                if let Some((provider, request_model)) = &self.preferred {
                    prefer_bound_provider(&mut self.providers, provider, request_model);
                }
                self.index = 0;
                self.used = 0;
                self.round = self.round.saturating_add(1);
                if self.providers.is_empty() {
                    return Ok(None);
                }
            }

            let (previous, request_model) = &self.providers[self.index];
            // 无限等待期间仍应用停用、密钥轮换、模型与权限的更新，不能永久使用旧快照。
            let provider = if let Some(account_id) = &previous.auth_account_id {
                cfg.auth_accounts
                    .iter()
                    .find(|account| &account.id == account_id)
                    .filter(|account| account.enabled)
                    .map(|account| auth_account_as_provider(account, &cfg.auth))
                    .filter(|provider| !provider_quota_exhausted(state, &provider.name))
            } else {
                cfg.providers
                    .iter()
                    .find(|provider| provider.name == previous.name)
                    .filter(|provider| provider.enabled)
                    .cloned()
            };
            let provider = provider.filter(|provider| {
                api_key_allows_provider(&allowed, &provider.name)
                    && api_key_allows_provider(&self.initial_allowed, &provider.name)
                    && provider_supports_model(provider, request_model)
                    && provider_supports_api(provider, self.api)
            });
            let Some(mut provider) = provider else {
                self.index += 1;
                self.used = 0;
                continue;
            };
            let attempts = provider.max_retries.max(1);
            if self.used >= attempts {
                self.index += 1;
                self.used = 0;
                continue;
            }
            if let Some(deadline) = self.retry_at.get(&provider.name).copied() {
                if deadline > tokio::time::Instant::now() {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(ProxyError::aborted()),
                        _ = state.root_cancel.cancelled() => return Err(ProxyError::aborted()),
                        _ = tokio::time::sleep_until(deadline) => {}
                    }
                    // 退避后重新检查配置/权限，禁止停用后的渠道被迟到重试。
                    continue;
                }
            }
            self.used += 1;
            if self.used == 1 && self.failures > 0 {
                crate::runtime_log::record_repeated(
                    "INFO",
                    format!(
                        "轮询渠道 {}：第 {} 轮，本轮最多 {} 次尝试",
                        provider.name, self.round, attempts,
                    ),
                    failure_group_key(ctx.log_scope(), self.api, &provider.name,
                        request_model, "", 0, "channel-enter"),
                );
            }
            // 底层仅执行一次业务尝试，保留必要的协议兼容协商，避免内外重试次数相乘。
            provider.max_retries = 0;
            provider.persistent_retry = false;
            return Ok(Some((provider, request_model.clone())));
        }
    }

    fn current_allowed(
        &self,
        cfg: &AppConfig,
        headers: &HeaderMap,
    ) -> Result<Vec<String>, ProxyError> {
        if !cfg.auth.enabled {
            return Ok(self.initial_allowed.clone());
        }
        let token = request_client_token(headers);
        let key = cfg
            .auth
            .api_keys
            .iter()
            .find(|key| key.enabled && key.key == token)
            .ok_or_else(|| {
                ProxyError::new(StatusCode::UNAUTHORIZED, "无效、缺失或已停用的 API Key")
            })?;
        if !key.allowed_models.is_empty()
            && !key.allowed_models.iter().any(|allowed| {
                allowed.trim() == "*"
                    || normalize_model(allowed.trim()) == normalize_model(self.model.trim())
            })
        {
            return Err(ProxyError::new(
                StatusCode::FORBIDDEN,
                "API Key 不允许访问当前模型",
            ));
        }
        if let Some(limit) = key
            .daily_token_limit
            .filter(|limit| self.check_daily_token_limit && *limit > 0)
        {
            let used = read_api_key_today_tokens(cfg.usage_log_sqlite_path(), &api_key_id(token))
                .map_err(|err| {
                ProxyError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
            })?;
            if used as u64 >= limit {
                return Err(ProxyError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "API Key 今日 Token 用量已达上限",
                ));
            }
        }
        Ok(key.allowed_providers.clone())
    }
}

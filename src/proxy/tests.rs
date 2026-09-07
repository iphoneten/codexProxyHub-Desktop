use super::*;

fn provider(provider_type: &str, responses_mode: &str, capabilities: Value) -> ProviderConfig {
    serde_json::from_value(json!({
        "name": "test",
        "provider_type": provider_type,
        "base_url": "https://example.test/v1",
        "api_key": "sk-test",
        "models": ["gpt-test"],
        "responses_mode": responses_mode,
        "capabilities": capabilities,
    }))
    .unwrap()
}

fn provider_with_name(name: &str, weight: u32, priority: i32) -> ProviderConfig {
    serde_json::from_value(json!({
        "name": name,
        "provider_type": "openai",
        "base_url": "https://example.test/v1",
        "api_key": "sk-test",
        "models": ["gpt-test"],
        "responses_mode": "auto",
        "weight": weight,
        "priority": priority,
        "capabilities": {"supports_chat": true, "supports_responses": true},
    }))
    .unwrap()
}

fn test_state() -> AppState {
    AppState {
        config: Arc::new(RwLock::new(Arc::new(
            AppConfig::load("config.example.yaml").unwrap(),
        ))),
        clients: Arc::new(Mutex::new(HashMap::new())),
        counters: Arc::new(Mutex::new(HashMap::new())),
        api_key_limiters: Arc::new(Mutex::new(HashMap::new())),
        provider_circuits: Arc::new(Mutex::new(HashMap::new())),
        provider_loads: Arc::new(Mutex::new(HashMap::new())),
        provider_statuses: Arc::new(RwLock::new(HashMap::new())),
        session_affinity: Arc::new(Mutex::new(HashMap::new())),
        usage_injection: Arc::new(Mutex::new(HashMap::new())),
        oauth_tokens: Arc::new(Mutex::new(HashMap::new())),
        oauth_refresh_locks: Arc::new(Mutex::new(HashMap::new())),
        root_cancel: CancellationToken::new(),
    }
}

fn auth_access_with_models(state: &AppState, allowed_models: &[&str]) -> AuthAccess {
    AuthAccess {
        permit: state
            .acquire_api_key_permit("allowed-models-test", 5)
            .unwrap(),
        key_name: "test".to_string(),
        daily_token_limit: None,
        allowed_models: allowed_models
            .iter()
            .map(|model| model.to_string())
            .collect(),
        allowed_providers: Vec::new(),
    }
}

#[test]
fn provider_circuit_opens_after_three_consecutive_failures() {
    let state = test_state();

    for _ in 0..PROVIDER_CIRCUIT_FAILURE_THRESHOLD {
        state
            .begin_provider_attempt("unstable")
            .unwrap()
            .mark_failure(StatusCode::BAD_GATEWAY, "upstream unavailable", None);
    }

    let err = state.begin_provider_attempt("unstable").unwrap_err();
    assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(err.message.contains("熔断冷却中"));
}

#[test]
fn provider_circuit_429_opens_immediately_with_retry_after() {
    let state = test_state();
    state
        .begin_provider_attempt("quota-limited")
        .unwrap()
        .mark_failure(
            StatusCode::TOO_MANY_REQUESTS,
            "quota exceeded",
            Some(Duration::from_secs(12)),
        );

    let err = state.begin_provider_attempt("quota-limited").unwrap_err();
    assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(err.message.contains("熔断冷却中"));
    let circuits = state.provider_circuits.lock();
    let circuit = circuits.get("quota-limited").unwrap();
    let ProviderCircuitPhase::Open { until } = circuit.phase else {
        panic!("429 应立即打开熔断器");
    };
    assert!(until.saturating_duration_since(Instant::now()) >= Duration::from_secs(11));
}

#[test]
fn provider_circuit_ignores_request_validation_failures() {
    let state = test_state();

    for _ in 0..PROVIDER_CIRCUIT_FAILURE_THRESHOLD {
        state
            .begin_provider_attempt("valid-provider")
            .unwrap()
            .mark_failure(StatusCode::BAD_REQUEST, "invalid request body", None);
    }

    assert!(state.begin_provider_attempt("valid-provider").is_ok());
    let circuits = state.provider_circuits.lock();
    let circuit = circuits.get("valid-provider").unwrap();
    assert_eq!(circuit.consecutive_failures, 0);
    assert!(matches!(circuit.phase, ProviderCircuitPhase::Closed));
}

#[test]
fn provider_circuit_half_open_allows_only_one_probe_and_success_recovers() {
    let state = test_state();
    {
        let mut circuits = state.provider_circuits.lock();
        circuits.insert(
            "recovering".to_string(),
            ProviderCircuit {
                phase: ProviderCircuitPhase::Open {
                    until: Instant::now() - Duration::from_millis(1),
                },
                consecutive_failures: PROVIDER_CIRCUIT_FAILURE_THRESHOLD,
                last_error: Some("temporary failure".to_string()),
                last_changed: Instant::now(),
            },
        );
    }

    let probe = state.begin_provider_attempt("recovering").unwrap();
    let concurrent = state.begin_provider_attempt("recovering").unwrap_err();
    assert_eq!(concurrent.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(concurrent.message.contains("正在半开探测"));

    probe.mark_success();
    assert!(state.begin_provider_attempt("recovering").is_ok());
}

#[test]
fn provider_circuit_half_open_drop_returns_to_cooldown() {
    let state = test_state();
    {
        let mut circuits = state.provider_circuits.lock();
        circuits.insert(
            "dropped-probe".to_string(),
            ProviderCircuit {
                phase: ProviderCircuitPhase::Open {
                    until: Instant::now() - Duration::from_millis(1),
                },
                consecutive_failures: PROVIDER_CIRCUIT_FAILURE_THRESHOLD,
                last_error: Some("temporary failure".to_string()),
                last_changed: Instant::now(),
            },
        );
    }

    drop(state.begin_provider_attempt("dropped-probe").unwrap());

    let err = state.begin_provider_attempt("dropped-probe").unwrap_err();
    assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(err.message.contains("熔断冷却中"));
}

#[test]
fn provider_circuit_stale_success_does_not_close_open_circuit() {
    let state = test_state();
    let stale = state.begin_provider_attempt("race").unwrap();
    state.begin_provider_attempt("race").unwrap().mark_failure(
        StatusCode::BAD_GATEWAY,
        "failed",
        None,
    );
    state.begin_provider_attempt("race").unwrap().mark_failure(
        StatusCode::BAD_GATEWAY,
        "failed",
        None,
    );
    state.begin_provider_attempt("race").unwrap().mark_failure(
        StatusCode::BAD_GATEWAY,
        "failed",
        None,
    );

    stale.mark_success();

    let err = state.begin_provider_attempt("race").unwrap_err();
    assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn provider_circuit_stale_failure_does_not_interrupt_half_open_probe() {
    let state = test_state();
    let stale = state.begin_provider_attempt("half-open-race").unwrap();
    state
        .begin_provider_attempt("half-open-race")
        .unwrap()
        .mark_failure(StatusCode::BAD_GATEWAY, "failed", None);
    state
        .begin_provider_attempt("half-open-race")
        .unwrap()
        .mark_failure(StatusCode::BAD_GATEWAY, "failed", None);
    state
        .begin_provider_attempt("half-open-race")
        .unwrap()
        .mark_failure(StatusCode::BAD_GATEWAY, "failed", None);
    {
        let mut circuits = state.provider_circuits.lock();
        let circuit = circuits.get_mut("half-open-race").unwrap();
        if let ProviderCircuitPhase::Open { until } = &mut circuit.phase {
            *until = Instant::now() - Duration::from_millis(1);
        }
    }

    let probe = state.begin_provider_attempt("half-open-race").unwrap();
    stale.mark_failure(StatusCode::BAD_GATEWAY, "late failure", None);

    let concurrent = state.begin_provider_attempt("half-open-race").unwrap_err();
    assert!(concurrent.message.contains("正在半开探测"));
    probe.mark_success();
    assert!(state.begin_provider_attempt("half-open-race").is_ok());
}

#[test]
fn cancelled_attempt_does_not_clear_previous_failures() {
    let state = test_state();
    state
        .begin_provider_attempt("cancel-neutral")
        .unwrap()
        .mark_failure(StatusCode::BAD_GATEWAY, "failed", None);

    let guard = state.begin_provider_attempt("cancel-neutral").unwrap();
    assert!(record_attempt_failure(guard, &ProxyError::aborted()));

    let circuits = state.provider_circuits.lock();
    assert_eq!(
        circuits.get("cancel-neutral").unwrap().consecutive_failures,
        1
    );
}

#[test]
fn api_key_empty_allowed_models_allows_every_model() {
    let state = test_state();
    let auth = auth_access_with_models(&state, &[]);
    assert!(ensure_api_key_allows_model(&auth, "gpt-any").is_ok());
}

#[test]
fn api_key_allowed_models_restricts_requested_model() {
    let state = test_state();
    let auth = auth_access_with_models(&state, &["gpt-5.5", "models/gemini-2.5-pro"]);
    assert!(ensure_api_key_allows_model(&auth, "gpt-5.5").is_ok());
    assert!(ensure_api_key_allows_model(&auth, "gemini-2.5-pro").is_ok());
    let err = ensure_api_key_allows_model(&auth, "gpt-4o").unwrap_err();
    assert_eq!(err.status, StatusCode::FORBIDDEN);
}

#[test]
fn api_key_empty_allowed_providers_allows_every_provider() {
    assert!(api_key_allows_provider(&[], "openai"));
    assert!(api_key_allows_provider(&["*".to_string()], "google-ai"));
}

#[test]
fn provider_attempts_only_include_allowed_providers_across_fallbacks() {
    let state = test_state();
    let mut cfg = (*state.snapshot()).clone();
    let mut primary = provider_with_name("primary", 1, 1);
    primary.models = vec!["gpt-test".to_string()];
    let mut blocked_backup = provider_with_name("blocked-backup", 1, 2);
    blocked_backup.models = vec!["gpt-test".to_string(), "gpt-fallback".to_string()];
    let mut allowed_backup = provider_with_name("allowed-backup", 1, 2);
    allowed_backup.models = vec!["gpt-fallback".to_string()];
    cfg.providers = vec![primary, blocked_backup, allowed_backup];
    cfg.routing
        .model_fallbacks
        .insert("gpt-test".to_string(), vec!["gpt-fallback".to_string()]);

    let allowed = vec!["primary".to_string(), "allowed-backup".to_string()];
    let attempts = provider_attempts(&cfg, &state, "gpt-test", "chat", &allowed);
    let names = attempts
        .iter()
        .map(|(provider, model)| (provider.name.as_str(), model.as_str()))
        .collect::<Vec<_>>();

    assert!(names.contains(&("primary", "gpt-test")));
    assert!(names.contains(&("allowed-backup", "gpt-fallback")));
    assert!(!names.iter().any(|(name, _)| *name == "blocked-backup"));
}

#[test]
fn collect_models_only_exposes_models_from_allowed_providers() {
    let state = test_state();
    let mut cfg = (*state.snapshot()).clone();
    let mut openai = provider_with_name("openai", 1, 1);
    openai.models = vec!["shared".to_string(), "openai-only".to_string()];
    let mut google = provider_with_name("google-ai", 1, 1);
    google.models = vec!["shared".to_string(), "google-only".to_string()];
    cfg.providers = vec![openai, google];

    let models = collect_models(&cfg, &["openai".to_string()]);

    assert_eq!(
        models,
        vec!["openai-only".to_string(), "shared".to_string()]
    );
}

#[test]
fn strip_thought_removes_non_stream_chat_content() {
    let mut value = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "<thought>internal notes</thought>你好"
            }
        }]
    });
    strip_thought_from_chat_json(&mut value);
    assert_eq!(value["choices"][0]["message"]["content"], "你好");
}

#[tokio::test]
async fn strip_thought_handles_stream_tags_across_chunks() {
    let first = r#"data: {"choices":[{"index":0,"delta":{"content":"<thou"}}]}"#;
    let second =
        r#"data: {"choices":[{"index":0,"delta":{"content":"ght>hidden</thought>你好"}}]}"#;
    let done = "data: [DONE]";
    let stream = futures_util::stream::iter([
        Ok::<Bytes, io::Error>(Bytes::from(format!("{first}\n\n"))),
        Ok::<Bytes, io::Error>(Bytes::from(format!("{second}\n\n{done}\n\n"))),
    ]);
    let mut stream = Box::pin(strip_thought_from_chat_sse_stream(
        stream,
        CancellationToken::new(),
    ));
    let mut out = String::new();
    while let Some(chunk) = stream.next().await {
        out.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
    }
    assert!(!out.contains("<thought>"));
    assert!(!out.contains("hidden"));
    assert!(out.contains("你好"));
    assert!(out.contains("[DONE]"));
}

#[test]
fn weighted_order_does_not_repeat_provider_in_one_attempt_chain() {
    let state = test_state();
    let providers = vec![
        provider_with_name("primary", 3, 1),
        provider_with_name("backup", 1, 1),
    ];

    let ordered = weighted_order(&state, "gpt-test", providers);
    let names = ordered
        .iter()
        .map(|provider| provider.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(names.len(), 2);
    assert_eq!(names.iter().filter(|name| **name == "primary").count(), 1);
    assert_eq!(names.iter().filter(|name| **name == "backup").count(), 1);
}

#[test]
fn weighted_order_prefers_less_loaded_provider_in_same_priority() {
    let state = test_state();
    state
        .provider_loads
        .lock()
        .insert("busy".to_string(), Arc::new(AtomicUsize::new(5)));
    state
        .provider_loads
        .lock()
        .insert("idle".to_string(), Arc::new(AtomicUsize::new(0)));

    let providers = vec![
        provider_with_name("busy", 1, 1),
        provider_with_name("idle", 1, 1),
    ];

    let ordered = weighted_order(&state, "gpt-test", providers);
    assert_eq!(
        ordered.first().map(|provider| provider.name.as_str()),
        Some("idle")
    );
}

#[test]
fn weighted_order_uses_weight_adjusted_load() {
    let state = test_state();
    state
        .provider_loads
        .lock()
        .insert("wide".to_string(), Arc::new(AtomicUsize::new(2)));
    state
        .provider_loads
        .lock()
        .insert("narrow".to_string(), Arc::new(AtomicUsize::new(1)));

    let providers = vec![
        provider_with_name("wide", 10, 1),
        provider_with_name("narrow", 1, 1),
    ];

    let ordered = weighted_order(&state, "gpt-test", providers);
    assert_eq!(
        ordered.first().map(|provider| provider.name.as_str()),
        Some("wide")
    );
}

#[test]
fn weighted_order_prefers_fully_healthy_provider_over_recent_failures() {
    let state = test_state();
    state.provider_statuses.write().insert(
        "recent-failure".to_string(),
        ProviderCircuitSnapshot {
            status: ProviderCircuitStatus::Healthy,
            failures: 1,
            inflight: 0,
            open_until: None,
            last_error: Some("temporary".to_string()),
            quota_exhausted: false,
        },
    );

    let providers = vec![
        provider_with_name("recent-failure", 1, 1),
        provider_with_name("clean", 1, 1),
    ];

    let ordered = weighted_order(&state, "gpt-test", providers);
    assert_eq!(
        ordered.first().map(|provider| provider.name.as_str()),
        Some("clean")
    );
}

#[test]
fn api_key_concurrency_limit_rejects_sixth_request_by_default() {
    let state = test_state();
    let permits = (0..5)
        .map(|_| state.acquire_api_key_permit("sk-local", 5).unwrap())
        .collect::<Vec<_>>();

    let err = state.acquire_api_key_permit("sk-local", 5).unwrap_err();
    assert_eq!(err.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(err.message.contains("并发数已达上限"));

    drop(permits);
    assert!(state.acquire_api_key_permit("sk-local", 5).is_ok());
}

#[test]
fn api_key_concurrency_is_configured_per_key() {
    let state = test_state();
    let cfg: AppConfig = serde_yaml::from_str(
        r#"
auth:
  enabled: true
  api_keys:
    - key: sk-a
      enabled: true
      max_concurrency: 2
    - key: sk-b
      enabled: true
      max_concurrency: 1
providers: []
"#,
    )
    .unwrap();
    let mut headers_a = HeaderMap::new();
    insert_header(&mut headers_a, "authorization", "Bearer sk-a");
    let mut headers_b = HeaderMap::new();
    insert_header(&mut headers_b, "authorization", "Bearer sk-b");

    let a1 = authorize_and_acquire(&state, &cfg, &headers_a).unwrap();
    let a2 = authorize_and_acquire(&state, &cfg, &headers_a).unwrap();
    let err = authorize_and_acquire(&state, &cfg, &headers_a).unwrap_err();
    assert_eq!(err.status, StatusCode::TOO_MANY_REQUESTS);

    let b1 = authorize_and_acquire(&state, &cfg, &headers_b).unwrap();
    assert_eq!(
        authorize_and_acquire(&state, &cfg, &headers_b)
            .unwrap_err()
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );

    drop(a1);
    assert!(authorize_and_acquire(&state, &cfg, &headers_a).is_ok());
    drop(a2);
    drop(b1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_key_concurrent_tasks_respect_max_concurrency() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Barrier;

    const MAX_CONCURRENCY: usize = 5;
    const TOTAL_TASKS: usize = 12;

    let state = Arc::new(test_state());
    let cfg = Arc::new(
        serde_yaml::from_str::<AppConfig>(&format!(
            r#"
auth:
  enabled: true
  api_keys:
    - key: sk-load
      enabled: true
      max_concurrency: {MAX_CONCURRENCY}
providers: []
"#
        ))
        .unwrap(),
    );

    let admitted = Arc::new(AtomicUsize::new(0));
    let rejected = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    // 让所有任务同时进入 acquire 阶段，模拟真实并发压力
    let start = Arc::new(Barrier::new(TOTAL_TASKS));

    let mut joins = Vec::with_capacity(TOTAL_TASKS);
    for _ in 0..TOTAL_TASKS {
        let state = Arc::clone(&state);
        let cfg = Arc::clone(&cfg);
        let start = Arc::clone(&start);
        let admitted = Arc::clone(&admitted);
        let rejected = Arc::clone(&rejected);
        let peak = Arc::clone(&peak);
        let active = Arc::clone(&active);

        joins.push(tokio::spawn(async move {
            start.wait().await;
            let mut headers = HeaderMap::new();
            insert_header(&mut headers, "authorization", "Bearer sk-load");
            match authorize_and_acquire(&state, &cfg, &headers) {
                Ok(access) => {
                    admitted.fetch_add(1, Ordering::SeqCst);
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    // 模拟任务在执行，保持 permit 一小段时间
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    drop(access);
                }
                Err(err) => {
                    assert_eq!(err.status, StatusCode::TOO_MANY_REQUESTS);
                    rejected.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }

    for join in joins {
        join.await.unwrap();
    }

    assert_eq!(admitted.load(Ordering::SeqCst), MAX_CONCURRENCY);
    assert_eq!(
        rejected.load(Ordering::SeqCst),
        TOTAL_TASKS - MAX_CONCURRENCY
    );
    assert_eq!(peak.load(Ordering::SeqCst), MAX_CONCURRENCY);

    // 所有任务结束后名额应完全释放，可以再次拿满 MAX_CONCURRENCY 个
    let mut headers = HeaderMap::new();
    insert_header(&mut headers, "authorization", "Bearer sk-load");
    let reacquired = (0..MAX_CONCURRENCY)
        .map(|_| authorize_and_acquire(&state, &cfg, &headers).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        authorize_and_acquire(&state, &cfg, &headers)
            .unwrap_err()
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
    drop(reacquired);
}

#[test]
fn upstream_auth_errors_do_not_stop_provider_failover() {
    assert!(!should_stop_failover(StatusCode::UNAUTHORIZED));
    assert!(!should_stop_failover(
        StatusCode::PROXY_AUTHENTICATION_REQUIRED
    ));
}

#[test]
fn chat_skips_openai_provider_that_declares_no_chat_support() {
    let p = provider(
        "openai",
        "auto",
        json!({"supports_chat": false, "supports_responses": false}),
    );

    assert!(!provider_supports_api(&p, "chat"));
}

#[test]
fn inbound_messages_api_only_selects_anthropic_providers() {
    let anthropic = provider("anthropic", "chat", json!({}));
    let openai = provider(
        "openai",
        "auto",
        json!({"supports_chat": true, "supports_responses": true}),
    );
    let google = provider("google_ai_studio", "chat", json!({"supports_chat": true}));

    // 入站 Anthropic 协议只能直通 anthropic 渠道
    assert!(provider_supports_api(&anthropic, "messages"));
    // OpenAI / Google 渠道没有 /messages 端点，必须被排除，否则 failover 打到必然 404 的上游
    assert!(!provider_supports_api(&openai, "messages"));
    assert!(!provider_supports_api(&google, "messages"));
}

#[test]
fn inbound_messages_ignores_supports_chat_capability() {
    // anthropic 渠道即使显式声明 supports_chat:false，也仍然能承接入站 /messages：
    // 该 capability 描述的是 OpenAI 协议面，与原生 Messages 直通无关
    let anthropic = provider(
        "anthropic",
        "chat",
        json!({"supports_chat": false, "supports_responses": false}),
    );

    assert!(provider_supports_api(&anthropic, "messages"));
}

#[test]
fn anthropic_stream_completes_on_message_stop() {
    // Anthropic 原生流没有 [DONE] 哨兵，以 message_stop 收尾。
    // 不识别它会让直通流被误判为「完成事件前断开」而记成失败。
    assert!(sse_stream_completed(
        SseProbeKind::Anthropic,
        &json!({"type": "message_stop"}).to_string()
    ));
    assert!(!sse_stream_completed(
        SseProbeKind::Anthropic,
        &json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "hi"}})
            .to_string()
    ));
    // Responses 的完成事件不应被 Anthropic 探针认领，反之亦然
    assert!(!sse_stream_completed(
        SseProbeKind::Anthropic,
        &json!({"type": "response.completed"}).to_string()
    ));
    assert!(!sse_stream_completed(
        SseProbeKind::Responses,
        &json!({"type": "message_stop"}).to_string()
    ));
}

#[test]
fn client_token_accepts_anthropic_x_api_key_header() {
    // Anthropic SDK / Claude Code 只发 x-api-key，不发 Authorization
    let mut headers = HeaderMap::new();
    insert_header(&mut headers, "x-api-key", "sk-anthropic-style");
    assert_eq!(request_client_token(&headers), "sk-anthropic-style");

    // Authorization 优先于 x-api-key
    let mut both = HeaderMap::new();
    insert_header(&mut both, "authorization", "Bearer sk-bearer");
    insert_header(&mut both, "x-api-key", "sk-xapikey");
    assert_eq!(request_client_token(&both), "sk-bearer");

    assert_eq!(request_client_token(&HeaderMap::new()), "");
}

#[test]
fn anthropic_version_headers_pass_through_to_upstream() {
    let mut provider = provider("anthropic", "chat", json!({}));
    provider.api_key = "sk-upstream".to_string();
    let mut request_headers = HeaderMap::new();
    insert_header(&mut request_headers, "anthropic-version", "2023-06-01");
    insert_header(
        &mut request_headers,
        "anthropic-beta",
        "prompt-caching-2024-07-31",
    );
    // 客户端鉴权头绝不能透传给上游
    insert_header(&mut request_headers, "x-api-key", "sk-client");

    let headers = upstream_headers(&provider, &request_headers, false);

    assert_eq!(
        headers.get("anthropic-beta").unwrap(),
        "prompt-caching-2024-07-31"
    );
    assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
    // 上游拿到的是渠道自己的 key，不是客户端的
    assert_eq!(headers.get("x-api-key").unwrap(), "sk-upstream");
}

#[test]
fn anthropic_system_prompt_override_uses_top_level_field() {
    let mut provider = provider("anthropic", "chat", json!({}));
    provider.system_prompt_override = Some("be terse".to_string());
    let mut body = json!({
        "model": "claude-test",
        "messages": [{"role": "user", "content": "hi"}]
    });

    apply_anthropic_system_prompt_override(&provider, &mut body);

    // Anthropic 的 system 在顶层，不是 messages 里的一条
    assert_eq!(body["system"], "be terse");
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(body["messages"][0]["role"], "user");
}

#[tokio::test]
async fn anthropic_error_response_uses_anthropic_error_shape() {
    // Anthropic SDK 只认 {"type":"error","error":{"type","message"}}；
    // 回 OpenAI 形状会让客户端解析不出错误信息
    let response = anthropic_error_response(ProxyError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate limited",
    ));
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "rate_limit_error");
    assert_eq!(value["error"]["message"], "rate limited");
}

#[tokio::test]
async fn anthropic_error_response_maps_status_to_error_type() {
    for (status, expected) in [
        (StatusCode::UNAUTHORIZED, "authentication_error"),
        (StatusCode::FORBIDDEN, "permission_error"),
        (StatusCode::BAD_REQUEST, "invalid_request_error"),
        (StatusCode::SERVICE_UNAVAILABLE, "overloaded_error"),
        (StatusCode::BAD_GATEWAY, "api_error"),
    ] {
        let response = anthropic_error_response(ProxyError::new(status, "x"));
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["type"], expected, "status={status}");
    }
}

#[test]
fn responses_auto_requires_native_responses_or_chat_fallback() {
    let no_wire_api = provider(
        "openai",
        "auto",
        json!({"supports_chat": false, "supports_responses": false}),
    );
    let chat_fallback = provider(
        "openai",
        "auto",
        json!({"supports_chat": true, "supports_responses": false}),
    );

    assert!(!provider_supports_api(&no_wire_api, "responses"));
    assert!(provider_supports_api(&chat_fallback, "responses"));
}

#[test]
fn responses_auto_tries_native_endpoint_even_when_capability_is_stale() {
    let stale_capability = provider(
        "openai",
        "auto",
        json!({"supports_chat": true, "supports_responses": false}),
    );
    let forced_chat = provider(
        "openai",
        "chat",
        json!({"supports_chat": true, "supports_responses": false}),
    );

    assert!(!provider_prefers_chat_responses(&stale_capability));
    assert!(provider_prefers_chat_responses(&forced_chat));
}

#[test]
fn include_usage_injection_is_limited_to_chat_completions() {
    assert!(supports_include_usage_injection("/chat/completions"));
    assert!(!supports_include_usage_injection("/responses"));
    assert!(!supports_include_usage_injection("/completions"));
}

#[test]
fn usage_injection_probe_only_disables_on_explicit_field_rejection() {
    assert!(usage_injection_rejected(
        StatusCode::BAD_REQUEST,
        "unknown field stream_options.include_usage"
    ));
    assert!(usage_injection_rejected(
        StatusCode::UNPROCESSABLE_ENTITY,
        "include_usage is not supported"
    ));
    assert!(!usage_injection_rejected(
        StatusCode::UNAUTHORIZED,
        "invalid api key"
    ));
    assert!(!usage_injection_rejected(
        StatusCode::NOT_FOUND,
        "model not found"
    ));
    assert!(!usage_injection_rejected(
        StatusCode::INTERNAL_SERVER_ERROR,
        "stream_options rejected"
    ));
}

#[test]
fn upstream_safe_headers_preserve_codex_markers_without_auth() {
    let mut input = HeaderMap::new();
    insert_header(&mut input, "authorization", "Bearer local");
    insert_header(&mut input, "session_id", "session-1");
    insert_header(&mut input, "conversation_id", "conversation-1");
    insert_header(&mut input, "x-stainless-runtime", "rust");

    let headers = upstream_safe_headers(&input);

    assert!(!headers.contains_key("authorization"));
    assert_eq!(
        headers
            .get("session_id")
            .and_then(|value| value.to_str().ok()),
        Some("session-1")
    );
    assert!(headers.contains_key("conversation_id"));
    assert!(headers.contains_key("x-stainless-runtime"));
}

#[test]
fn upstream_headers_extra_headers_override_client_headers() {
    let mut p = provider(
        "openai",
        "auto",
        json!({"supports_chat": true, "supports_responses": true}),
    );
    p.extra_headers
        .insert("User-Agent".to_string(), "claude-cli/2.1.142".to_string());
    p.extra_headers
        .insert("originator".to_string(), "claude_code".to_string());
    p.extra_headers
        .insert("x-app".to_string(), "cli".to_string());

    let mut input = HeaderMap::new();
    insert_header(&mut input, "User-Agent", "codex_cli_rs/0.116.0");
    insert_header(&mut input, "originator", "codex_cli_rs");
    insert_header(&mut input, "x-app", "opencode");
    insert_header(&mut input, "x-stainless-runtime", "rust");

    let headers = upstream_headers(&p, &input, false);

    assert_eq!(
        headers
            .get("user-agent")
            .and_then(|value| value.to_str().ok()),
        Some("claude-cli/2.1.142")
    );
    assert_eq!(
        headers
            .get("originator")
            .and_then(|value| value.to_str().ok()),
        Some("claude_code")
    );
    assert_eq!(
        headers.get("x-app").and_then(|value| value.to_str().ok()),
        Some("cli")
    );
    assert_eq!(
        headers
            .get("x-stainless-runtime")
            .and_then(|value| value.to_str().ok()),
        Some("rust")
    );
}

#[test]
fn anthropic_extra_authorization_uses_bearer_instead_of_x_api_key() {
    let mut p = provider(
        "anthropic",
        "chat",
        json!({"supports_chat": true, "supports_responses": false}),
    );
    p.extra_headers
        .insert("Authorization".to_string(), "Bearer {api_key}".to_string());

    let headers = upstream_headers(&p, &HeaderMap::new(), false);

    assert_eq!(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer sk-test")
    );
    assert!(!headers.contains_key("x-api-key"));
    assert_eq!(
        headers
            .get("anthropic-version")
            .and_then(|value| value.to_str().ok()),
        Some("2023-06-01")
    );
}

#[test]
fn anthropic_defaults_to_x_api_key_authentication() {
    let p = provider(
        "anthropic",
        "chat",
        json!({"supports_chat": true, "supports_responses": false}),
    );

    let headers = upstream_headers(&p, &HeaderMap::new(), false);

    assert_eq!(
        headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok()),
        Some("sk-test")
    );
    assert!(!headers.contains_key(header::AUTHORIZATION));
}

#[test]
fn explicit_chat_mode_is_not_filtered_by_stale_chat_capability() {
    let explicit_chat = provider(
        "openai",
        "chat",
        json!({"supports_chat": false, "supports_responses": false}),
    );

    assert!(provider_supports_api(&explicit_chat, "responses"));
    assert!(!provider_supports_api(&explicit_chat, "chat"));
}

#[test]
fn anthropic_provider_is_allowed_for_translated_chat_and_responses() {
    let p = provider(
        "anthropic",
        "chat",
        json!({"supports_chat": false, "supports_responses": false}),
    );

    assert!(provider_supports_api(&p, "chat"));
    assert!(provider_supports_api(&p, "responses"));
}

#[test]
fn responses_to_chat_preserves_function_calls_and_outputs() {
    let body = json!({
        "model": "gpt-test",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "list files"}]
            },
            {
                "type": "function_call",
                "call_id": "call_1",
                "name": "shell",
                "arguments": "{\"cmd\":\"ls\"}"
            },
            {
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "Cargo.toml\nsrc"
            }
        ],
        "tools": [{
            "type": "function",
            "name": "shell",
            "description": "run a shell command",
            "parameters": {"type": "object"}
        }],
        "tool_choice": {"type": "function", "name": "shell"}
    });

    let out = responses_to_chat_body(&body).unwrap();
    assert_eq!(out["messages"][0]["role"], "user");
    assert_eq!(out["messages"][1]["role"], "assistant");
    assert_eq!(out["messages"][1]["tool_calls"][0]["id"], "call_1");
    assert_eq!(out["messages"][2]["role"], "tool");
    assert_eq!(out["messages"][2]["tool_call_id"], "call_1");
    assert_eq!(out["tools"][0]["function"]["name"], "shell");
    assert_eq!(out["tool_choice"]["function"]["name"], "shell");
}

#[test]
fn chat_to_response_preserves_tool_calls() {
    let chat = json!({
        "model": "gpt-test",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "shell",
                        "arguments": "{\"cmd\":\"ls\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
    });

    let out = chat_to_response(chat, "gpt-test", &HashSet::new());
    assert_eq!(out["output"][0]["type"], "function_call");
    assert_eq!(out["output"][0]["call_id"], "call_1");
    assert_eq!(out["output"][0]["name"], "shell");
    assert_eq!(out["usage"]["input_tokens"], 3);
    assert_eq!(out["usage"]["output_tokens"], 4);
}

#[test]
fn responses_to_chat_maps_custom_tools_and_outputs() {
    let body = json!({
        "model": "claude-test",
        "input": [
            {
                "type": "custom_tool_call",
                "call_id": "call_patch",
                "name": "apply_patch",
                "input": "*** Begin Patch\n*** End Patch"
            },
            {
                "type": "custom_tool_call_output",
                "call_id": "call_patch",
                "output": "Done!"
            }
        ],
        "tools": [{
            "type": "custom",
            "name": "apply_patch",
            "description": "Apply a patch",
            "format": {"type": "text"}
        }],
        "tool_choice": {"type": "custom", "name": "apply_patch"}
    });

    let out = responses_to_chat_body(&body).unwrap();
    let arguments = out["messages"][0]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(arguments).unwrap()["input"],
        "*** Begin Patch\n*** End Patch"
    );
    assert_eq!(out["messages"][1]["role"], "tool");
    assert_eq!(out["messages"][1]["tool_call_id"], "call_patch");
    assert_eq!(out["tools"][0]["function"]["name"], "apply_patch");
    assert_eq!(
        out["tools"][0]["function"]["parameters"]["properties"]["input"]["type"],
        "string"
    );
    assert_eq!(out["tool_choice"]["function"]["name"], "apply_patch");

    let anthropic = crate::anthropic::openai_to_anthropic_request(&out).unwrap();
    assert_eq!(anthropic["tools"][0]["name"], "apply_patch");
    assert_eq!(
        anthropic["messages"][0]["content"][0]["input"]["input"],
        "*** Begin Patch\n*** End Patch"
    );
    assert_eq!(
        anthropic["messages"][1]["content"][0]["tool_use_id"],
        "call_patch"
    );
}

#[test]
fn chat_to_response_restores_custom_tool_calls() {
    let chat = json!({
        "model": "claude-test",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_patch",
                    "type": "function",
                    "function": {
                        "name": "apply_patch",
                        "arguments": "{\"input\":\"*** Begin Patch\\n*** End Patch\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    let custom_tools = HashSet::from(["apply_patch".to_string()]);

    let out = chat_to_response(chat, "claude-test", &custom_tools);
    assert_eq!(out["output"][0]["type"], "custom_tool_call");
    assert_eq!(out["output"][0]["call_id"], "call_patch");
    assert_eq!(out["output"][0]["name"], "apply_patch");
    assert_eq!(out["output"][0]["input"], "*** Begin Patch\n*** End Patch");
}

#[tokio::test]
async fn chat_stream_restores_custom_tool_calls() {
    let chunk = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_patch",
                    "type": "function",
                    "function": {
                        "name": "apply_patch",
                        "arguments": "{\"input\":\"*** Begin Patch\\n*** End Patch\"}"
                    }
                }]
            }
        }]
    });
    let bytes = Bytes::from(format!("data: {chunk}\n\ndata: [DONE]\n\n"));
    let stream = futures_util::stream::iter([Ok::<Bytes, io::Error>(bytes)]);
    let custom_tools = HashSet::from(["apply_patch".to_string()]);

    let result = chat_sse_stream_to_responses(
        stream,
        "claude-test".to_string(),
        custom_tools,
        Instant::now(),
        CancellationToken::new(),
    )
    .unwrap();
    let body = to_bytes(result.response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let sse = String::from_utf8(body.to_vec()).unwrap();

    assert!(sse.contains("\"type\":\"custom_tool_call\""));
    assert!(sse.contains("\"name\":\"apply_patch\""));
    assert!(sse.contains("*** Begin Patch\\n*** End Patch"));
}

#[tokio::test]
async fn chat_stream_error_becomes_response_failed() {
    let chunk = json!({
        "error": {
            "message": "Concurrency limit exceeded for account, please retry later",
            "type": "rate_limit_error"
        }
    });
    let bytes = Bytes::from(format!("data: {chunk}\n\n"));
    let stream = futures_util::stream::iter([Ok::<Bytes, io::Error>(bytes)]);

    let result = chat_sse_stream_to_responses(
        stream,
        "claude-test".to_string(),
        HashSet::new(),
        Instant::now(),
        CancellationToken::new(),
    )
    .unwrap();
    let body = to_bytes(result.response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let sse = String::from_utf8(body.to_vec()).unwrap();

    assert!(sse.contains("event: response.failed"));
    assert!(sse.contains("Concurrency limit exceeded"));
    assert!(!sse.contains("event: response.completed"));
    assert!(!sse.contains("[stream error:"));
}

#[tokio::test]
async fn chat_stream_to_responses_reports_error_outcome() {
    let chunk = json!({
        "error": {
            "message": "Concurrency limit exceeded for account, please retry later",
            "type": "rate_limit_error"
        }
    });
    let bytes = Bytes::from(format!("data: {chunk}\n\n"));
    let stream = futures_util::stream::iter([Ok::<Bytes, io::Error>(bytes)]);

    let result = chat_sse_stream_to_responses(
        stream,
        "claude-test".to_string(),
        HashSet::new(),
        Instant::now(),
        CancellationToken::new(),
    )
    .unwrap();
    let rx = result.usage_rx.unwrap();
    let _ = to_bytes(result.response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let outcome = rx.await.unwrap();

    assert!(outcome
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("Concurrency limit exceeded"));
}

#[tokio::test]
async fn chat_stream_to_responses_stops_when_client_disconnects() {
    // 客户端断开后,spawned 任务应立即结束并汇报「已取消」,
    // 避免持续消费上游 token 与占用熔断 inflight 计数。
    let mut payload = String::new();
    for i in 0..64 {
        let chunk = json!({"choices":[{"delta":{"content": format!("t{i}")}}]});
        payload.push_str(&format!("data: {chunk}\n\n"));
    }
    let bytes = Bytes::from(payload);
    let stream = futures_util::stream::iter([Ok::<Bytes, io::Error>(bytes)]);

    let result = chat_sse_stream_to_responses(
        stream,
        "gpt-test".to_string(),
        HashSet::new(),
        Instant::now(),
        CancellationToken::new(),
    )
    .unwrap();
    let rx = result.usage_rx.unwrap();
    // 立即丢弃 response body → 释放底层 mpsc receiver,后续 send 全部失败
    drop(result.response);

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
        .await
        .expect("usage_rx should resolve quickly after client disconnect")
        .expect("usage_rx not closed");
    // 责任在客户端,不算上游故障:记 aborted 而不是 error
    assert!(outcome.aborted);
    assert_eq!(outcome.error.as_deref(), Some(STREAM_ABORTED_MESSAGE));
}

#[tokio::test]
async fn chat_stream_to_responses_stops_when_client_disconnects_before_upstream_chunk() {
    let stream = futures_util::stream::pending::<Result<Bytes, io::Error>>();

    let result = chat_sse_stream_to_responses(
        stream,
        "gpt-test".to_string(),
        HashSet::new(),
        Instant::now(),
        CancellationToken::new(),
    )
    .unwrap();
    let rx = result.usage_rx.unwrap();
    drop(result.response);

    let outcome = tokio::time::timeout(std::time::Duration::from_millis(200), rx)
        .await
        .expect("usage_rx should resolve without waiting for another upstream chunk")
        .expect("usage_rx not closed");
    assert!(outcome.aborted);
    assert_eq!(outcome.error.as_deref(), Some(STREAM_ABORTED_MESSAGE));
}

#[tokio::test]
async fn chat_stream_to_responses_stops_when_context_is_cancelled() {
    // 客户端还挂着（response body 没被丢），但请求上下文被取消：
    // 代理关停或上层主动取消也必须能立刻叫停这个 detached 任务。
    let stream = futures_util::stream::pending::<Result<Bytes, io::Error>>();
    let cancel = CancellationToken::new();

    let result = chat_sse_stream_to_responses(
        stream,
        "gpt-test".to_string(),
        HashSet::new(),
        Instant::now(),
        cancel.clone(),
    )
    .unwrap();
    let rx = result.usage_rx.unwrap();
    // 故意不 drop response：唯一的结束信号只有取消
    cancel.cancel();

    let outcome = tokio::time::timeout(std::time::Duration::from_millis(200), rx)
        .await
        .expect("取消后应立即收到结果，而不是等上游下一个 chunk")
        .expect("usage_rx not closed");
    assert!(outcome.aborted);
    drop(result.response);
}

#[tokio::test]
async fn sse_probe_preserves_chat_prefix_after_first_delta() {
    let first = json!({"choices":[{"delta":{"role":"assistant"}}]});
    let second = json!({"choices":[{"delta":{"content":"hi"}}]});
    let bytes = Bytes::from(format!("data: {first}\n\ndata: {second}\n\n"));
    let stream: ProxyByteStream =
        Box::pin(futures_util::stream::iter([Ok::<Bytes, io::Error>(bytes)]));

    let mut prepared = prepare_sse_stream(stream, 1, SseProbeKind::Chat, &CancellationToken::new())
        .await
        .unwrap();
    let mut out = Vec::new();
    while let Some(chunk) = prepared.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    let text = String::from_utf8(out).unwrap();

    assert!(text.contains("\"role\":\"assistant\""));
    assert!(text.contains("\"content\":\"hi\""));
}

#[tokio::test]
async fn sse_probe_stops_when_context_is_cancelled() {
    let stream: ProxyByteStream =
        Box::pin(futures_util::stream::pending::<Result<Bytes, io::Error>>());
    let cancel = CancellationToken::new();
    let future = prepare_sse_stream(stream, 30, SseProbeKind::Chat, &cancel);
    tokio::pin!(future);

    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_millis(100), &mut future)
        .await
        .expect("取消后首帧探测应立即结束");
    let err = match result {
        Ok(_) => panic!("取消后的首帧探测不应成功"),
        Err(err) => err,
    };
    assert!(err.aborted);
    assert_eq!(err.status.as_u16(), 499);
}

#[tokio::test]
async fn stream_usage_probe_reports_unfinished_stream_as_error() {
    let chunk = json!({"choices":[{"delta":{"content":"hi"}}]});
    let bytes = Bytes::from(format!("data: {chunk}\n\n"));
    let stream = futures_util::stream::iter([Ok::<Bytes, io::Error>(bytes)]);

    let (rx, mut body_stream) = stream_with_usage_probe(
        stream,
        SseProbeKind::Chat,
        Instant::now(),
        None,
        CancellationToken::new(),
    );
    while let Some(chunk) = body_stream.next().await {
        chunk.unwrap();
    }
    let outcome = rx.await.unwrap();

    assert_eq!(outcome.error.as_deref(), Some("上游流在完成事件前断开"));
    assert!(outcome.first_token_ms.is_some());
}

#[tokio::test]
async fn stream_usage_probe_records_first_token_and_total_usage() {
    let first = json!({"choices":[{"delta":{"content":"hi"}}]});
    let second = json!({"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":3}});
    let bytes = Bytes::from(format!(
        "data: {first}\n\ndata: {second}\n\ndata: [DONE]\n\n"
    ));
    let stream = futures_util::stream::iter([Ok::<Bytes, io::Error>(bytes)]);

    let (rx, mut body_stream) = stream_with_usage_probe(
        stream,
        SseProbeKind::Chat,
        Instant::now(),
        None,
        CancellationToken::new(),
    );
    while let Some(chunk) = body_stream.next().await {
        chunk.unwrap();
    }
    let outcome = rx.await.unwrap();

    assert!(outcome.error.is_none());
    assert!(outcome.first_token_ms.is_some());
    assert_eq!(outcome.usage.input, 2);
    assert_eq!(outcome.usage.output, 3);
}

#[tokio::test]
async fn responses_stream_usage_probe_counts_completed_event_usage() {
    let first = json!({"type":"response.output_text.delta","delta":"hi"});
    let completed = json!({
        "type": "response.completed",
        "response": {
            "usage": {"input_tokens": 11, "output_tokens": 7}
        }
    });
    let bytes = Bytes::from(format!("data: {first}\n\ndata: {completed}\n\n"));
    let stream = futures_util::stream::iter([Ok::<Bytes, io::Error>(bytes)]);

    let (rx, mut body_stream) = stream_with_usage_probe(
        stream,
        SseProbeKind::Responses,
        Instant::now(),
        None,
        CancellationToken::new(),
    );
    while let Some(chunk) = body_stream.next().await {
        chunk.unwrap();
    }
    let outcome = rx.await.unwrap();

    assert!(outcome.error.is_none());
    assert_eq!(outcome.usage.input, 11);
    assert_eq!(outcome.usage.output, 7);
}

#[tokio::test]
async fn responses_stream_closes_after_completed_event_even_if_upstream_stays_open() {
    let completed = json!({
        "type": "response.completed",
        "response": {"status": "completed"}
    });
    let bytes = Bytes::from(format!("data: {completed}\n\n"));
    let stream = futures_util::stream::once(async move { Ok::<Bytes, io::Error>(bytes) })
        .chain(futures_util::stream::pending::<Result<Bytes, io::Error>>());

    let (rx, mut body_stream) = stream_with_usage_probe(
        stream,
        SseProbeKind::Responses,
        Instant::now(),
        None,
        CancellationToken::new(),
    );
    assert!(body_stream.next().await.is_some());
    let ended = tokio::time::timeout(Duration::from_millis(100), body_stream.next())
        .await
        .expect("下游流应在 response.completed 后立即结束");
    assert!(ended.is_none());

    let outcome = rx.await.unwrap();
    assert!(outcome.error.is_none());
}

#[tokio::test]
async fn responses_stream_does_not_synthesize_completed_from_output_item_done_tail() {
    let created = json!({
        "type": "response.created",
        "response": {
            "id": "resp_test",
            "object": "response",
            "created_at": 123,
            "status": "in_progress",
            "background": false,
            "completed_at": null,
            "error": null,
            "incomplete_details": null,
            "model": "gpt-5.5",
            "output": [],
            "output_text": null,
            "usage": {"input_tokens": 0, "output_tokens": 0}
        },
        "sequence_number": 1
    });
    let reasoning_done = json!({
        "type": "response.output_item.done",
        "item": {
            "id": "rs_test",
            "type": "reasoning",
            "content": [],
            "encrypted_content": "abc"
        },
        "sequence_number": 4
    });
    let text_delta = json!({
        "type": "response.output_text.delta",
        "item_id": "msg_test",
        "content_index": 0,
        "delta": "你"
    });
    let message_done = json!({
        "type": "response.output_item.done",
        "item": {
            "id": "msg_test",
            "type": "message",
            "status": "completed",
            "content": [{"type": "output_text", "text": "你好"}],
            "phase": "final_answer",
            "role": "assistant"
        },
        "sequence_number": 10
    });
    let bytes = Bytes::from(format!(
            "data: {created}\n\ndata: {reasoning_done}\n\ndata: {text_delta}\n\ndata: {message_done}\n\n"
        ));
    let stream = futures_util::stream::once(async move { Ok::<Bytes, io::Error>(bytes) });

    let (rx, mut body_stream) = stream_with_usage_probe(
        stream,
        SseProbeKind::Responses,
        Instant::now(),
        None,
        CancellationToken::new(),
    );
    let mut body = String::new();
    while let Some(chunk) = body_stream.next().await {
        body.push_str(&String::from_utf8(chunk.unwrap().to_vec()).unwrap());
    }

    let outcome = rx.await.unwrap();
    assert!(body.contains("response.output_item.done"));
    assert!(!body.contains("event: response.completed"));
    assert!(!body.contains("[DONE]"));
    assert!(outcome
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("上游流在完成事件前断开"));
    assert!(outcome.first_token_ms.is_some());
}

#[tokio::test]
async fn responses_stream_waits_for_native_completed_after_commentary_tool_call() {
    let commentary_text_done = json!({
        "type": "response.output_text.done",
        "item_id": "msg_commentary",
        "text": "正在检查",
        "sequence_number": 5
    });
    let commentary_done = json!({
        "type": "response.output_item.done",
        "item": {
            "id": "msg_commentary",
            "type": "message",
            "status": "completed",
            "phase": "commentary",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "正在检查"}]
        },
        "sequence_number": 6
    });
    let tool_added = json!({
        "type": "response.output_item.added",
        "item": {
            "id": "ctc_test",
            "type": "custom_tool_call",
            "status": "in_progress",
            "call_id": "call_test",
            "name": "exec"
        },
        "sequence_number": 7
    });
    let tool_done = json!({
        "type": "response.output_item.done",
        "item": {
            "id": "ctc_test",
            "type": "custom_tool_call",
            "status": "completed",
            "call_id": "call_test",
            "name": "exec",
            "input": "{}"
        },
        "sequence_number": 8
    });
    let first = Bytes::from(format!(
            "data: {commentary_text_done}\n\ndata: {commentary_done}\n\ndata: {tool_added}\n\ndata: {tool_done}\n\n"
        ));
    let completed = json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "output": [],
            "usage": {"input_tokens": 17, "output_tokens": 3, "total_tokens": 20}
        },
        "sequence_number": 9
    });
    let delayed_completed = futures_util::stream::once(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok::<Bytes, io::Error>(Bytes::from(format!("data: {completed}\n\n")))
    });
    let stream = futures_util::stream::once(async move { Ok::<Bytes, io::Error>(first) })
        .chain(delayed_completed)
        .chain(futures_util::stream::pending::<Result<Bytes, io::Error>>());

    let (rx, mut body_stream) = stream_with_usage_probe(
        stream,
        SseProbeKind::Responses,
        Instant::now(),
        None,
        CancellationToken::new(),
    );
    let mut body = String::new();
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(1), body_stream.next())
        .await
        .expect("工具调用结束后应继续等待上游原生 response.completed")
    {
        body.push_str(&String::from_utf8(chunk.unwrap().to_vec()).unwrap());
    }

    let outcome = rx.await.unwrap();
    assert_eq!(body.matches("\"type\":\"response.completed\"").count(), 1);
    assert!(body.contains("response.output_item.done"));
    assert!(!body.contains("event: response.completed"));
    assert_eq!(outcome.usage.input, 17);
    assert_eq!(outcome.usage.output, 3);
    assert!(outcome.error.is_none());
}

#[tokio::test]
async fn stream_watchdog_reports_max_duration_after_first_output() {
    let first = json!({"choices":[{"delta":{"content":"hi"}}]});
    let delayed = futures_util::stream::once(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        Ok::<Bytes, io::Error>(Bytes::from("data: [DONE]\n\n"))
    });
    let stream: ProxyByteStream = Box::pin(
        futures_util::stream::once(async move {
            Ok::<Bytes, io::Error>(Bytes::from(format!("data: {first}\n\n")))
        })
        .chain(delayed),
    );
    let stream = apply_stream_watchdog(stream, 0, 1, "test");

    let (rx, mut body_stream) = stream_with_usage_probe(
        stream,
        SseProbeKind::Chat,
        Instant::now(),
        None,
        CancellationToken::new(),
    );
    while let Some(chunk) = body_stream.next().await {
        let _ = chunk;
    }
    let outcome = rx.await.unwrap();

    assert!(outcome
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("超过最大持续时间"));
}

#[test]
fn sqlite_usage_log_persists_first_token_ms() {
    let path = std::env::temp_dir().join(format!(
        "routehub-usage-log-{}.sqlite3",
        Uuid::new_v4().simple()
    ));

    log_usage_sqlite(
        path.clone(),
        "2026-07-14 12:00:00",
        "responses",
        "local-key",
        "ok",
        "test-provider",
        "gpt-test",
        "gpt-upstream",
        197_320,
        Some(1_234),
        "",
        142_286,
        793,
        "upstream_or_unknown",
    )
    .unwrap();

    let conn = Connection::open(&path).unwrap();
    let row = conn
            .query_row(
                "SELECT first_token_ms, latency_ms, input_tokens, output_tokens, api_key_name FROM usage_logs",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .unwrap();

    assert_eq!(
        row,
        (Some(1_234), 197_320, 142_286, 793, "local-key".to_string())
    );
    let _ = fs::remove_file(path);
}

#[test]
fn sqlite_stream_log_updates_running_row_in_place() {
    let path = std::env::temp_dir().join(format!(
        "routehub-live-log-{}.sqlite3",
        Uuid::new_v4().simple()
    ));
    let id = log_usage_sqlite(
        path.clone(),
        "2026-07-15 12:00:00",
        "responses",
        "local-key",
        "running",
        "test-provider",
        "gpt-test",
        "gpt-upstream",
        0,
        None,
        "",
        0,
        0,
        "upstream_or_unknown",
    )
    .unwrap();

    let updated = update_usage_log_sqlite(
        path.clone(),
        id,
        "ok",
        "gpt-upstream",
        12_345,
        Some(456),
        "",
        321,
        45,
        "upstream_or_unknown",
    )
    .unwrap();

    let conn = Connection::open(&path).unwrap();
    let row = conn
            .query_row(
                "SELECT COUNT(*), status, latency_ms, first_token_ms, input_tokens, output_tokens FROM usage_logs",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .unwrap();

    assert_eq!(updated, 1);
    assert_eq!(row, (1, "ok".to_string(), 12_345, Some(456), 321, 45));
    drop(conn);
    let _ = fs::remove_file(path);
}

#[test]
fn stream_attempt_log_is_visible_before_upstream_finishes() {
    let path = std::env::temp_dir().join(format!(
        "routehub-early-stream-log-{}.sqlite3",
        Uuid::new_v4().simple()
    ));
    let config: AppConfig = serde_yaml::from_str(&format!(
        "usage_log:\n  backend: sqlite\n  sqlite_path: '{}'\nproviders: []\n",
        path.display()
    ))
    .unwrap();
    let started = Instant::now();

    let id = start_stream_attempt_log(
        &config,
        true,
        "responses",
        "test-provider",
        "gpt-test",
        "gpt-upstream",
        started,
        "local-key",
    )
    .expect("流式尝试开始时应立即创建日志");

    finish_failed_attempt_log(
        &config,
        Some(id),
        "responses",
        "test-provider",
        "gpt-test",
        started,
        "上游响应头超时",
        "local-key",
    );

    let conn = Connection::open(&path).unwrap();
    let row = conn
        .query_row(
            "SELECT COUNT(*), status, error, channel, request_model FROM usage_logs",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .unwrap();

    assert_eq!(
        row,
        (
            1,
            "error".to_string(),
            "上游响应头超时".to_string(),
            "test-provider".to_string(),
            "gpt-test".to_string(),
        )
    );
    drop(conn);
    let _ = fs::remove_file(path);
}

#[test]
fn usage_log_connection_enables_wal_and_busy_timeout() {
    let path = std::env::temp_dir().join(format!(
        "routehub-sqlite-config-{}.sqlite3",
        Uuid::new_v4().simple()
    ));

    let conn = open_usage_log_connection(path.clone()).unwrap();
    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    let busy_timeout: i64 = conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();

    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    assert_eq!(busy_timeout, 5_000);
    drop(conn);
    let _ = fs::remove_file(path);
}

#[test]
fn wal_allows_log_writes_while_reader_transaction_is_open() {
    let path = std::env::temp_dir().join(format!(
        "routehub-concurrent-log-{}.sqlite3",
        Uuid::new_v4().simple()
    ));
    let mut reader = open_usage_log_connection(path.clone()).unwrap();
    ensure_usage_log_schema(&reader).unwrap();
    let read_tx = reader.transaction().unwrap();
    let _: i64 = read_tx
        .query_row("SELECT COUNT(*) FROM usage_logs", [], |row| row.get(0))
        .unwrap();

    let id = log_usage_sqlite(
        path.clone(),
        "2026-07-15 12:00:00",
        "responses",
        "local-key",
        "ok",
        "test-provider",
        "gpt-test",
        "gpt-upstream",
        123,
        Some(45),
        "",
        10,
        2,
        "upstream_or_unknown",
    )
    .unwrap();

    assert!(id > 0);
    drop(read_tx);
    drop(reader);
    let _ = fs::remove_file(path);
}

#[test]
fn startup_recovers_stale_running_logs() {
    let path = std::env::temp_dir().join(format!(
        "routehub-stale-log-{}.sqlite3",
        Uuid::new_v4().simple()
    ));
    log_usage_sqlite(
        path.clone(),
        "2026-07-15 12:00:00",
        "responses",
        "local-key",
        "running",
        "test-provider",
        "gpt-test",
        "gpt-upstream",
        0,
        None,
        "",
        0,
        0,
        "upstream_or_unknown",
    )
    .unwrap();

    recover_interrupted_usage_logs(path.clone());

    let conn = open_usage_log_connection(path.clone()).unwrap();
    let row = conn
        .query_row("SELECT status, error FROM usage_logs", [], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap();
    assert_eq!(row.0, "error");
    assert!(row.1.contains("上次退出"));
    drop(conn);
    let _ = fs::remove_file(path);
}

#[tokio::test]
async fn sse_probe_rejects_chat_done_before_output() {
    let stream: ProxyByteStream = Box::pin(futures_util::stream::iter([Ok::<Bytes, io::Error>(
        Bytes::from("data: [DONE]\n\n"),
    )]));

    let err =
        match prepare_sse_stream(stream, 1, SseProbeKind::Chat, &CancellationToken::new()).await {
            Ok(_) => panic!("expected probe error"),
            Err(err) => err,
        };

    assert_eq!(err.status, StatusCode::BAD_GATEWAY);
    assert!(err.message.contains("未返回有效输出"));
}

#[tokio::test]
async fn sse_probe_maps_responses_error_before_output_to_retryable_error() {
    let chunk = json!({
        "type": "error",
        "error": {
            "message": "Concurrency limit exceeded for account",
            "type": "rate_limit_error"
        }
    });
    let stream: ProxyByteStream = Box::pin(futures_util::stream::iter([Ok::<Bytes, io::Error>(
        Bytes::from(format!("data: {chunk}\n\n")),
    )]));

    let err = match prepare_sse_stream(
        stream,
        1,
        SseProbeKind::Responses,
        &CancellationToken::new(),
    )
    .await
    {
        Ok(_) => panic!("expected probe error"),
        Err(err) => err,
    };

    assert_eq!(err.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(err.retryable());
}

#[test]
fn chat_stream_tool_call_delta_parser_handles_openai_chunks() {
    let data = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "shell", "arguments": "{\"cmd\""}
                }]
            }
        }]
    })
    .to_string();

    let deltas = chat_stream_tool_call_deltas(&data);
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].index, 0);
    assert_eq!(deltas[0].id.as_deref(), Some("call_1"));
    assert_eq!(deltas[0].name.as_deref(), Some("shell"));
    assert_eq!(deltas[0].arguments.as_deref(), Some("{\"cmd\""));
}

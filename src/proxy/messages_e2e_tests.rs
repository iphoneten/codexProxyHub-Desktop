// 入站 Anthropic Messages API 端到端测试。
//
// 起两个真实 HTTP 服务：
//   1. mock Anthropic 上游（记录收到的头/body，返回 JSON 或原生 SSE）
//   2. RouteHub 自己的 run_server
// 然后用真实 HTTP 客户端按 Anthropic 客户端的方式发请求，覆盖：
//   - x-api-key 鉴权（Anthropic SDK 不发 Authorization）
//   - 请求体原样直通 + 顶层 system 覆盖
//   - 客户端 anthropic-version / anthropic-beta 透传，且客户端 key 不泄漏给上游
//   - 原生 SSE 直通（事件序列不被翻译成 OpenAI chunk）
//   - 只选 anthropic 渠道，跳过同模型的 openai 诱饵渠道
//   - count_tokens 直通
//   - 鉴权失败时返回 Anthropic 错误形状

use super::*;
use axum::extract::Request;
use std::sync::atomic::AtomicU16;

// 上游观测到的请求，供断言
#[derive(Clone, Default)]
struct SeenRequest {
    path: String,
    x_api_key: String,
    authorization: String,
    anthropic_version: String,
    anthropic_beta: String,
    body: Value,
}

type SeenLog = Arc<Mutex<Vec<SeenRequest>>>;

async fn mock_anthropic_upstream(seen: SeenLog) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let app = Router::new()
        .route(
            "/v1/messages",
            post(move |req: Request| {
                let seen = Arc::clone(&seen);
                async move { mock_messages(seen, req).await }
            }),
        )
        .route(
            "/v1/messages/count_tokens",
            post(|| async { (StatusCode::OK, Json(json!({"input_tokens": 42}))).into_response() }),
        );

    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    port
}

async fn mock_messages(seen: SeenLog, req: Request) -> Response {
    let headers = req.headers().clone();
    let path = req.uri().path().to_string();
    let bytes = to_bytes(req.into_body(), 1024 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);

    let header_str = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    seen.lock().push(SeenRequest {
        path,
        x_api_key: header_str("x-api-key"),
        authorization: header_str("authorization"),
        anthropic_version: header_str("anthropic-version"),
        anthropic_beta: header_str("anthropic-beta"),
        body: body.clone(),
    });

    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        // Anthropic 原生事件序列，含 event: 行，无 [DONE] 哨兵
        let events = [
            json!({"type":"message_start","message":{"id":"msg_e2e","model":"claude-mock","usage":{"input_tokens":11,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}),
            json!({"type":"message_stop"}),
        ];
        let mut sse = String::new();
        for event in events {
            let name = event.get("type").and_then(Value::as_str).unwrap_or("");
            sse.push_str(&format!("event: {name}\ndata: {event}\n\n"));
        }
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(sse))
            .unwrap();
    }

    (
        StatusCode::OK,
        Json(json!({
            "id": "msg_e2e",
            "type": "message",
            "role": "assistant",
            "model": "claude-mock",
            "content": [{"type": "text", "text": "Hello world"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 11, "output_tokens": 7}
        })),
    )
        .into_response()
}

// 诱饵上游：任何路径都记录命中并回一个成功响应。
// 入站 /messages 只应直通 anthropic 渠道，这里的命中数必须始终为 0。
async fn mock_decoy_upstream(seen: SeenLog) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let app = Router::new().fallback(move |req: Request| {
        let seen = Arc::clone(&seen);
        async move {
            let path = req.uri().path().to_string();
            let bytes = to_bytes(req.into_body(), 1024 * 1024)
                .await
                .unwrap_or_default();
            seen.lock().push(SeenRequest {
                path,
                body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                ..SeenRequest::default()
            });
            // 回一个结构上合法的 OpenAI 响应：若诱饵被错误选中，
            // 请求会「成功」，测试就能凭命中记录明确定位问题
            (
                StatusCode::OK,
                Json(json!({
                    "id": "chatcmpl-decoy",
                    "object": "chat.completion",
                    "model": "claude-mock",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "decoy"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                })),
            )
                .into_response()
        }
    });

    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    port
}

static USAGE_DB_SEQ: AtomicU16 = AtomicU16::new(0);

// 起一个真实的 RouteHub 服务，返回 (base_url, shutdown)
//
// decoy_port 指向一个**同样可用**的 mock：诱饵渠道声明了同一个模型且优先级更高，
// 若入站 /messages 的渠道过滤有问题，它会先被选中并留下命中记录。
// 让它指向死端口是不够的——那样错误选中只会静默 failover 到 anthropic，
// anthropic 侧的命中数依然是 1，断言看不出区别。
async fn start_routehub(
    upstream_port: u16,
    decoy_port: u16,
) -> (String, oneshot::Sender<()>, PathBuf) {
    // 先占一个空闲端口再释放，run_server 按配置里的端口重新绑定
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let seq = USAGE_DB_SEQ.fetch_add(1, Ordering::Relaxed);
    let db_path = std::env::temp_dir().join(format!(
        "routehub-messages-e2e-{}-{seq}.sqlite3",
        Uuid::new_v4().simple()
    ));

    // 诱饵渠道：同模型、更高优先级(0 < 100)、指向一个必然连不通的端口。
    // 入站 /messages 若错误地把它纳入候选，请求就会先失败在诱饵上。
    let yaml = format!(
        r#"
server:
  host: 127.0.0.1
  port: {port}
web:
  enabled: false
auth:
  enabled: true
  api_keys:
    - name: e2e-key
      key: sk-routehub-e2e
      enabled: true
usage_log:
  backend: sqlite
  sqlite_path: {db}
providers:
  - name: mock-openai-decoy
    enabled: true
    provider_type: openai
    base_url: http://127.0.0.1:{decoy_port}/v1
    api_key: sk-decoy
    priority: 0
    models: [claude-mock]
    capabilities:
      supports_chat: true
      supports_responses: true
  - name: mock-anthropic
    enabled: true
    provider_type: anthropic
    base_url: http://127.0.0.1:{upstream_port}/v1
    api_key: sk-upstream-secret
    priority: 100
    models: [claude-mock]
    system_prompt_override: "be terse"
    responses_mode: chat
"#,
        db = db_path.display(),
    );

    let cfg: AppConfig = serde_yaml::from_str(&yaml).unwrap();
    let handle: ConfigHandle = Arc::new(RwLock::new(Arc::new(cfg)));
    let statuses: ProviderCircuitStatusHandle = Arc::new(RwLock::new(HashMap::new()));
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = run_server(handle, rx, statuses).await;
    });

    // 等待端口就绪
    let base = format!("http://127.0.0.1:{port}");
    for _ in 0..100 {
        if reqwest::Client::new()
            .get(format!("{base}/health"))
            .send()
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (base, tx, db_path)
}

// 轮询等待流式用量日志落到终态（running 会被原地更新成 ok/error）
async fn wait_for_usage_row(
    db_path: &std::path::Path,
) -> (String, String, String, String, i64, i64) {
    for _ in 0..100 {
        if let Ok(conn) = open_usage_log_connection(db_path.to_path_buf()) {
            let row = conn.query_row(
                "SELECT api, status, channel, error, input_tokens, output_tokens
                 FROM usage_logs ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            );
            if let Ok(row) = row {
                if row.1 != "running" {
                    return row;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("等待用量日志超时");
}

#[tokio::test]
async fn inbound_messages_passthrough_end_to_end() {
    let seen: SeenLog = Arc::new(Mutex::new(Vec::new()));
    let decoy_seen: SeenLog = Arc::new(Mutex::new(Vec::new()));
    let upstream_port = mock_anthropic_upstream(Arc::clone(&seen)).await;
    let decoy_port = mock_decoy_upstream(Arc::clone(&decoy_seen)).await;
    let (base, _shutdown, db_path) = start_routehub(upstream_port, decoy_port).await;
    let client = reqwest::Client::new();

    // 按 Anthropic SDK 的惯例发请求：只带 x-api-key，不带 Authorization
    let resp = client
        .post(format!("{base}/v1/messages"))
        .header("x-api-key", "sk-routehub-e2e")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "prompt-caching-2024-07-31")
        .json(&json!({
            "model": "claude-mock",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let value: Value = resp.json().await.unwrap();
    // 响应原样直通，仍是 Anthropic 形状（不是 OpenAI chat.completion）
    assert_eq!(value["type"], "message");
    assert_eq!(value["content"][0]["text"], "Hello world");
    assert_eq!(value["usage"]["input_tokens"], 11);
    assert!(value.get("choices").is_none());

    // 诱饵渠道优先级更高且可用，但不是 anthropic 类型，必须被排除
    assert!(
        decoy_seen.lock().is_empty(),
        "入站 /messages 不应打到非 anthropic 渠道，实际命中: {:?}",
        decoy_seen
            .lock()
            .iter()
            .map(|r| r.path.clone())
            .collect::<Vec<_>>()
    );

    let requests = seen.lock().clone();
    assert_eq!(requests.len(), 1, "应只打到 anthropic 渠道一次");
    let req = &requests[0];
    assert_eq!(req.path, "/v1/messages");
    // 上游拿到渠道自己的 key，客户端 key 不外泄
    assert_eq!(req.x_api_key, "sk-upstream-secret");
    assert!(!req.x_api_key.contains("routehub-e2e"));
    assert!(req.authorization.is_empty());
    // 客户端版本/beta 头透传
    assert_eq!(req.anthropic_version, "2023-06-01");
    assert_eq!(req.anthropic_beta, "prompt-caching-2024-07-31");
    // 请求体直通，system 覆盖写在顶层
    assert_eq!(req.body["messages"][0]["content"], "hi");
    assert_eq!(req.body["max_tokens"], 64);
    assert_eq!(req.body["system"], "be terse");

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn inbound_messages_streams_native_anthropic_sse() {
    let seen: SeenLog = Arc::new(Mutex::new(Vec::new()));
    let decoy_seen: SeenLog = Arc::new(Mutex::new(Vec::new()));
    let upstream_port = mock_anthropic_upstream(Arc::clone(&seen)).await;
    let decoy_port = mock_decoy_upstream(Arc::clone(&decoy_seen)).await;
    let (base, _shutdown, db_path) = start_routehub(upstream_port, decoy_port).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("x-api-key", "sk-routehub-e2e")
        .json(&json!({
            "model": "claude-mock",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert!(resp
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .contains("text/event-stream"));
    let sse = resp.text().await.unwrap();

    // 原生事件序列原样到达客户端
    assert!(sse.contains("event: message_start"));
    assert!(sse.contains("\"type\":\"content_block_delta\""));
    assert!(sse.contains("Hello"));
    assert!(sse.contains("event: message_stop"));
    // 没有被翻译成 OpenAI chunk，也不该混入 [DONE] 哨兵
    assert!(!sse.contains("chat.completion.chunk"));
    assert!(!sse.contains("[DONE]"));
    assert!(
        decoy_seen.lock().is_empty(),
        "流式也不应打到非 anthropic 渠道"
    );

    // 流式用量日志是异步落库的，等它写完
    let row = wait_for_usage_row(&db_path).await;
    let (api, status, channel, error, input_tokens, output_tokens) = row;
    assert_eq!(api, "messages");
    // 关键断言：Anthropic 原生流以 message_stop 收尾，必须被认成正常完成。
    // 漏掉该事件会记成「上游流在完成事件前断开」，日志与熔断都会误判成失败。
    assert_eq!(status, "ok", "流式请求被误记为失败: {error}");
    assert_eq!(channel, "mock-anthropic");
    assert!(error.is_empty());
    // usage 从 Anthropic 事件里正确提取（message_start 11 / message_delta 7）
    assert_eq!(input_tokens, 11);
    assert_eq!(output_tokens, 7);

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn inbound_messages_rejects_bad_key_with_anthropic_error_shape() {
    let seen: SeenLog = Arc::new(Mutex::new(Vec::new()));
    let decoy_seen: SeenLog = Arc::new(Mutex::new(Vec::new()));
    let upstream_port = mock_anthropic_upstream(Arc::clone(&seen)).await;
    let decoy_port = mock_decoy_upstream(Arc::clone(&decoy_seen)).await;
    let (base, _shutdown, db_path) = start_routehub(upstream_port, decoy_port).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("x-api-key", "sk-wrong")
        .json(&json!({
            "model": "claude-mock",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
    let value: Value = resp.json().await.unwrap();
    // Anthropic 错误形状，不是 OpenAI 的 {"error":{"type":"proxy_error"}}
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "authentication_error");
    // 鉴权失败不应触达任何上游
    assert!(seen.lock().is_empty());
    assert!(decoy_seen.lock().is_empty());

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn inbound_count_tokens_passes_through() {
    let seen: SeenLog = Arc::new(Mutex::new(Vec::new()));
    let decoy_seen: SeenLog = Arc::new(Mutex::new(Vec::new()));
    let upstream_port = mock_anthropic_upstream(Arc::clone(&seen)).await;
    let decoy_port = mock_decoy_upstream(Arc::clone(&decoy_seen)).await;
    let (base, _shutdown, db_path) = start_routehub(upstream_port, decoy_port).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages/count_tokens"))
        .header("x-api-key", "sk-routehub-e2e")
        .json(&json!({
            "model": "claude-mock",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let value: Value = resp.json().await.unwrap();
    assert_eq!(value["input_tokens"], 42);
    assert!(decoy_seen.lock().is_empty());

    let _ = std::fs::remove_file(db_path);
}

use crate::config::{AppConfig, ProviderConfig};
use anyhow::{anyhow, Result};
use axum::{
    body::{to_bytes, Body},
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use parking_lot::{Mutex, RwLock};
use reqwest::Client;
use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs, io,
    net::SocketAddr,
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

// 配置句柄：Arc<RwLock<Arc<AppConfig>>>
// 读侧 read() + clone Arc 指针，几乎无锁；写侧只在切换指针时短暂持有写锁
pub(crate) type ConfigHandle = Arc<RwLock<Arc<AppConfig>>>;
type ProxyByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>;
#[derive(Clone, Default)]
pub struct KeepaliveStatus {
    pub last_attempt: Option<String>,
    pub last_success: Option<String>,
    pub last_error: Option<String>,
}

pub type KeepaliveStatusHandle = Arc<RwLock<HashMap<String, KeepaliveStatus>>>;

#[derive(Clone)]
struct ApiKeyLimiter {
    limit: usize,
    semaphore: Arc<Semaphore>,
}

#[derive(Debug)]
struct AuthAccess {
    permit: OwnedSemaphorePermit,
    key_name: String,
}

#[derive(Clone)]
struct AppState {
    config: ConfigHandle,
    clients: Arc<Mutex<HashMap<u64, Client>>>,
    counters: Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>>,
    keepalive_headers: Arc<Mutex<HashMap<String, HeaderMap>>>,
    api_key_limiters: Arc<Mutex<HashMap<String, ApiKeyLimiter>>>,
    // 每个 provider 是否接受 stream_options.include_usage 注入的自动探测结果
    // Some(true)  = 已确认接受；Some(false) = 已确认拒绝；None = 未探测（默认注入试试）
    // 只在进程内缓存，代理重启后重新探测
    usage_injection: Arc<Mutex<HashMap<String, bool>>>,
}

#[derive(Clone)]
struct RawSseCapture {
    provider: String,
    path: String,
    max_events: usize,
}

impl AppState {
    // 拿一份当前配置快照。整个请求生命周期内用这份，避免请求中途配置换了导致的不一致
    fn snapshot(&self) -> Arc<AppConfig> {
        self.config.read().clone()
    }

    fn client_for_provider(&self, provider: &ProviderConfig) -> Client {
        let connect_timeout = provider.connect_timeout.max(1);
        let mut clients = self.clients.lock();
        clients
            .entry(connect_timeout)
            .or_insert_with(|| build_http_client(connect_timeout))
            .clone()
    }

    // 判断该 provider 是否需要注入 stream_options.include_usage
    fn should_inject_usage(&self, provider: &ProviderConfig) -> bool {
        if provider.provider_type == "anthropic" {
            return false;
        }
        self.usage_injection
            .lock()
            .get(&provider.name)
            .copied()
            .unwrap_or(true)
    }

    // 标记该 provider 不接受 include_usage 注入
    fn mark_usage_injection_unsupported(&self, provider: &ProviderConfig) {
        self.usage_injection
            .lock()
            .insert(provider.name.clone(), false);
    }

    fn remember_keepalive_headers(&self, provider: &ProviderConfig, request_headers: &HeaderMap) {
        let headers = keepalive_safe_headers(request_headers);
        if !headers.is_empty() {
            self.keepalive_headers
                .lock()
                .insert(provider.name.clone(), headers);
        }
    }

    fn cached_keepalive_headers(&self, provider: &ProviderConfig) -> Option<HeaderMap> {
        self.keepalive_headers.lock().get(&provider.name).cloned()
    }

    fn raw_sse_capture_for(&self, provider: &ProviderConfig) -> Option<RawSseCapture> {
        provider.debug_capture_sse.then(|| RawSseCapture {
            provider: provider.name.clone(),
            path: self
                .snapshot()
                .resolve_runtime_path(&provider.debug_sse_path)
                .display()
                .to_string(),
            max_events: provider.debug_sse_max_events.max(10),
        })
    }

    fn acquire_api_key_permit(
        &self,
        token: &str,
        limit: usize,
    ) -> Result<OwnedSemaphorePermit, ProxyError> {
        let limit = limit.max(1);
        let limiter = {
            let mut limiters = self.api_key_limiters.lock();
            let entry = limiters
                .entry(token.to_string())
                .or_insert_with(|| ApiKeyLimiter {
                    limit,
                    semaphore: Arc::new(Semaphore::new(limit)),
                });
            if entry.limit != limit {
                *entry = ApiKeyLimiter {
                    limit,
                    semaphore: Arc::new(Semaphore::new(limit)),
                };
            }
            entry.clone()
        };
        limiter.semaphore.try_acquire_owned().map_err(|_| {
            ProxyError::new(
                StatusCode::TOO_MANY_REQUESTS,
                format!("API Key 并发数已达上限({limit})"),
            )
        })
    }
}

fn build_http_client(connect_timeout: u64) -> Client {
    Client::builder()
        .pool_max_idle_per_host(20)
        .connect_timeout(Duration::from_secs(connect_timeout.max(1)))
        .danger_accept_invalid_certs(false)
        .build()
        .expect("failed to build reqwest client")
}

#[derive(Clone, Copy, Default)]
pub(crate) struct TokenUsage {
    pub input: i64,
    pub output: i64,
}

pub(crate) struct StreamOutcome {
    pub(crate) usage: TokenUsage,
    pub(crate) first_token_ms: Option<i64>,
    pub(crate) error: Option<String>,
}

impl StreamOutcome {
    pub(crate) fn success(usage: TokenUsage, first_token_ms: Option<i64>) -> Self {
        Self {
            usage,
            first_token_ms,
            error: None,
        }
    }

    pub(crate) fn failed(
        usage: TokenUsage,
        first_token_ms: Option<i64>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            usage,
            first_token_ms,
            error: Some(error.into()),
        }
    }
}

struct ProviderResult {
    response: Response,
    upstream_model: String,
    usage: TokenUsage,
    // 流式请求会在此提供最终 usage 和流错误；外层收到 Some 时改为异步落日志
    usage_rx: Option<oneshot::Receiver<StreamOutcome>>,
}

struct AttemptFailure {
    provider: String,
    request_model: String,
    started: Instant,
    status: StatusCode,
    message: String,
}

impl AttemptFailure {
    fn new(
        provider: &ProviderConfig,
        request_model: &str,
        started: Instant,
        err: ProxyError,
    ) -> Self {
        Self {
            provider: provider.name.clone(),
            request_model: request_model.to_string(),
            started,
            status: err.status,
            message: err.message,
        }
    }
}

#[derive(Debug)]
struct ProxyError {
    status: StatusCode,
    message: String,
}

impl ProxyError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn retryable(&self) -> bool {
        retryable_status(self.status)
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": {
                    "message": self.message,
                    "type": "proxy_error",
                    "code": self.status.as_u16()
                }
            })),
        )
            .into_response()
    }
}

#[derive(Debug)]
enum UpstreamSendError {
    Request(reqwest::Error),
    HeaderTimeout(u64),
}

impl UpstreamSendError {
    fn retryable(&self) -> bool {
        match self {
            Self::Request(err) => err.is_timeout() || err.is_connect() || err.is_request(),
            Self::HeaderTimeout(_) => true,
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Request(err) => err.to_string(),
            Self::HeaderTimeout(secs) => format!("上游响应头超时: {secs}s"),
        }
    }
}

async fn send_stream_request(
    req: reqwest::RequestBuilder,
    timeout_secs: u64,
) -> Result<reqwest::Response, UpstreamSendError> {
    match tokio::time::timeout(Duration::from_secs(timeout_secs.max(1)), req.send()).await {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(err)) => Err(UpstreamSendError::Request(err)),
        Err(_) => Err(UpstreamSendError::HeaderTimeout(timeout_secs.max(1))),
    }
}

pub async fn run_server(
    config: ConfigHandle,
    shutdown: oneshot::Receiver<()>,
    keepalive_status: KeepaliveStatusHandle,
) -> Result<()> {
    // server 监听端口只用启动时那一份配置（改端口无法热切）
    let initial = config.read().clone();
    let bind_host = if initial.server.host == "0.0.0.0" {
        "0.0.0.0"
    } else {
        initial.server.host.as_str()
    };
    let addr: SocketAddr = format!("{}:{}", bind_host, initial.server.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    recover_interrupted_usage_logs(initial.usage_log_sqlite_path());
    let state = AppState {
        config,
        clients: Arc::new(Mutex::new(HashMap::new())),
        counters: Arc::new(Mutex::new(HashMap::new())),
        keepalive_headers: Arc::new(Mutex::new(HashMap::new())),
        api_key_limiters: Arc::new(Mutex::new(HashMap::new())),
        usage_injection: Arc::new(Mutex::new(HashMap::new())),
    };
    let (keepalive_stop_tx, keepalive_stop_rx) = oneshot::channel();
    let keepalive_task = tokio::spawn(keepalive_loop(
        state.clone(),
        keepalive_status,
        keepalive_stop_rx,
    ));

    let app = Router::new()
        .route("/", get(index))
        .route("/v1", get(index))
        .route("/v1/", get(index))
        .route("/health", get(health))
        .route("/v1/health", get(health))
        .route("/v1/models", get(list_models))
        .route("/v1/models/:model", get(get_model))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/responses", post(responses))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .with_state(state);

    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = shutdown.await;
            let _ = keepalive_stop_tx.send(());
        })
        .await;
    let _ = keepalive_task.await;
    result?;
    Ok(())
}

async fn keepalive_loop(
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

async fn send_provider_keepalive(
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
    let _ = send_json_to_provider(&client, provider, path, &request_headers, body).await?;
    Ok(())
}

fn build_keepalive_request(
    provider: &ProviderConfig,
    model: &str,
    prompt: &str,
) -> Result<(&'static str, Value), ProxyError> {
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
        return Ok(("/messages", body));
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
        return Ok(("/responses", body));
    }

    let mut body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 1,
        "stream": false
    });
    apply_model_mapping(provider, &mut body);
    Ok(("/chat/completions", body))
}

fn keepalive_request_headers() -> HeaderMap {
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

fn provider_keepalive_requires_client_headers(provider: &ProviderConfig) -> bool {
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

fn has_codex_session_headers(headers: &HeaderMap) -> bool {
    headers.contains_key("session_id") || headers.contains_key("conversation_id")
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        http::header::HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        headers.insert(name, value);
    }
}

fn provider_keepalive_prefers_responses(provider: &ProviderConfig) -> bool {
    let supports_responses =
        !matches!(provider.capabilities.get("supports_responses"), Some(false));
    let supports_chat = !matches!(provider.capabilities.get("supports_chat"), Some(false));

    provider.provider_type == "codex_only"
        || provider.responses_mode == "native"
        || (provider.responses_mode == "auto" && supports_responses && !supports_chat)
}

async fn index(State(state): State<AppState>) -> impl IntoResponse {
    let cfg = state.snapshot();
    Json(json!({
        "name": "RouteHub",
        "ok": true,
        "base_url": format!("http://{}:{}/v1", display_host(&cfg.server.host), cfg.server.port),
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

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let cfg = state.snapshot();
    Json(json!({
        "ok": true,
        "providers": cfg.providers.iter().filter(|p| p.enabled).count(),
        "models": collect_models(&cfg).len(),
    }))
}

async fn list_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ProxyError> {
    let cfg = state.snapshot();
    let _auth = authorize_and_acquire(&state, &cfg, &headers)?;
    let data: Vec<Value> = collect_models(&cfg)
        .into_iter()
        .map(|id| json!({"id": id, "object": "model", "created": 0, "owned_by": "route-hub"}))
        .collect();
    Ok(Json(json!({"object": "list", "data": data})))
}

async fn get_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(model): Path<String>,
) -> Result<impl IntoResponse, ProxyError> {
    let cfg = state.snapshot();
    let _auth = authorize_and_acquire(&state, &cfg, &headers)?;
    if collect_models(&cfg).contains(&model) {
        Ok(Json(
            json!({"id": model, "object": "model", "created": 0, "owned_by": "route-hub"}),
        ))
    } else {
        Err(ProxyError::new(StatusCode::NOT_FOUND, "模型不存在"))
    }
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    forward_openai(
        state,
        headers,
        body,
        "/chat/completions",
        "chat",
        Some(auth.permit),
        auth.key_name,
    )
    .await
}

async fn completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    forward_openai(
        state,
        headers,
        body,
        "/completions",
        "completion",
        Some(auth.permit),
        auth.key_name,
    )
    .await
}

async fn embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    forward_openai(
        state,
        headers,
        body,
        "/embeddings",
        "embedding",
        Some(auth.permit),
        auth.key_name,
    )
    .await
}

async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let auth = authorize_and_acquire(&state, &cfg, &headers)?;
    let api_key_name = auth.key_name;
    let mut permit = Some(auth.permit);
    let model = body_model(&body)?;
    let providers = provider_attempts(&cfg, &state, &model, "responses");
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut last_error = None;

    for (provider, request_model) in providers {
        let started = Instant::now();
        let mut upstream_body = with_model(body.clone(), &request_model);
        apply_model_mapping(&provider, &mut upstream_body);

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
                        permit.take(),
                        &api_key_name,
                    ));
                }
                Err(err) => {
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
                    permit.take(),
                    &api_key_name,
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
                            permit.take(),
                            &api_key_name,
                        ));
                    }
                    Err(chat_err) => {
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
                let failure = AttemptFailure::new(&provider, &request_model, started, err);
                // 与 forward_openai 保持一致：只有客户端鉴权错误立即中止，其它 4xx/5xx 继续尝试下个渠道
                if should_stop_failover(failure.status) {
                    log_error(
                        &cfg,
                        "responses",
                        &failure.provider,
                        &failure.request_model,
                        failure.started,
                        None,
                        &failure.message,
                        &api_key_name,
                    );
                    return Err(ProxyError::new(failure.status, failure.message));
                }
                last_error = Some(failure);
            }
        }
    }

    if let Some(failure) = last_error {
        log_error(
            &cfg,
            "responses",
            &failure.provider,
            &failure.request_model,
            failure.started,
            None,
            &failure.message,
            &api_key_name,
        );
        return Err(ProxyError::new(StatusCode::BAD_GATEWAY, failure.message));
    }

    Err(ProxyError::new(
        StatusCode::BAD_GATEWAY,
        format!("模型 '{}' 没有可用渠道", model),
    ))
}

fn provider_prefers_chat_responses(provider: &ProviderConfig) -> bool {
    provider.responses_mode == "chat"
        || provider.provider_type == "anthropic"
        || is_google_openai_endpoint(&provider.base_url)
}

async fn forward_openai(
    state: AppState,
    headers: HeaderMap,
    body: Value,
    path: &'static str,
    api: &'static str,
    mut permit: Option<OwnedSemaphorePermit>,
    api_key_name: String,
) -> Result<Response, ProxyError> {
    let cfg = state.snapshot();
    let model = body_model(&body)?;
    let providers = provider_attempts(&cfg, &state, &model, api);
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut last_error = None;

    for (provider, request_model) in providers {
        let started = Instant::now();
        let mut upstream_body = with_model(body.clone(), &request_model);
        apply_model_mapping(&provider, &mut upstream_body);
        if path == "/chat/completions" {
            apply_system_prompt_override(&provider, &mut upstream_body);
        }
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
                    permit.take(),
                    &api_key_name,
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
                                permit.take(),
                                &api_key_name,
                            ));
                        }
                        Err(fb_err) => {
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
                let failure = AttemptFailure::new(&provider, &request_model, started, err);
                // 只有客户端鉴权错误（401/407）立即中止：换渠道也是同样错，避免整链重试放大
                // 其它 4xx（400/403/404/…）都视为「这个上游不认可」，继续尝试下个渠道
                if should_stop_failover(failure.status) {
                    log_error(
                        &cfg,
                        api,
                        &failure.provider,
                        &failure.request_model,
                        failure.started,
                        None,
                        &failure.message,
                        &api_key_name,
                    );
                    return Err(ProxyError::new(failure.status, failure.message));
                }
                last_error = Some(failure);
            }
        }
    }

    if let Some(failure) = last_error {
        log_error(
            &cfg,
            api,
            &failure.provider,
            &failure.request_model,
            failure.started,
            None,
            &failure.message,
            &api_key_name,
        );
        return Err(ProxyError::new(StatusCode::BAD_GATEWAY, failure.message));
    }

    Err(ProxyError::new(
        StatusCode::BAD_GATEWAY,
        format!("模型 '{}' 没有可用渠道", model),
    ))
}

async fn forward_responses_as_chat(
    state: &AppState,
    provider: &ProviderConfig,
    headers: &HeaderMap,
    body: Value,
    stream: bool,
    started: Instant,
) -> Result<ProviderResult, ProxyError> {
    state.remember_keepalive_headers(provider, headers);
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

    let upstream = if provider.provider_type == "anthropic" {
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

async fn response_json(response: Response) -> Result<Value, ProxyError> {
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

async fn send_chat_stream_as_responses(
    state: &AppState,
    provider: &ProviderConfig,
    request_headers: &HeaderMap,
    body: Value,
    request_model: String,
    custom_tool_names: HashSet<String>,
    started: Instant,
) -> Result<ProviderResult, ProxyError> {
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
                return chat_stream_to_responses(resp, request_model, custom_tool_names, started);
            }
            Ok(resp) => {
                let status =
                    StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let retry_after = parse_retry_after(resp.headers());
                let text = resp.text().await.unwrap_or_default();
                // 首次注入即 4xx：回退到不注入立即重试一次，永久标记该 provider
                if probing && status.is_client_error() {
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
                return Err(ProxyError::new(status, clean_upstream_error(&text)));
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

async fn send_anthropic_chat_stream_as_responses(
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
                return Err(ProxyError::new(status, clean_upstream_error(&text)));
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

fn chat_stream_to_responses(
    resp: reqwest::Response,
    request_model: String,
    custom_tool_names: HashSet<String>,
    started: Instant,
) -> Result<ProviderResult, ProxyError> {
    let stream = resp
        .bytes_stream()
        .map(|chunk| chunk.map_err(|err| io::Error::other(err.to_string())));
    chat_sse_stream_to_responses(stream, request_model, custom_tool_names, started)
}

async fn prepare_anthropic_stream(
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

async fn prepare_openai_stream(
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

fn apply_stream_watchdog(
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
enum SseProbeKind {
    Chat,
    Responses,
    Anthropic,
}

enum SseProbeDecision {
    Continue,
    Ready,
    Error(ProxyError),
}

async fn prepare_sse_stream(
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

fn inspect_sse_probe_event(kind: SseProbeKind, data: &str) -> SseProbeDecision {
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

fn inspect_chat_sse_probe_event(data: &str) -> SseProbeDecision {
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

fn inspect_responses_sse_probe_event(data: &str) -> SseProbeDecision {
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

fn stream_error_status(message: &str) -> StatusCode {
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

fn anthropic_sse_error(data: &str) -> Option<ProxyError> {
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

fn anthropic_sse_has_client_output(data: &str) -> bool {
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

fn chat_sse_stream_to_responses<S>(
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
        let _ = send_response_sse(
            &tx,
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
            }),
        )
        .await;
        let _ = send_response_sse(
            &tx,
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
            }),
        )
        .await;
        let _ = send_response_sse(
            &tx,
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
            }),
        )
        .await;
        let _ = send_response_sse(
            &tx,
            "response.content_part.added",
            json!({
                "type": "response.content_part.added",
                "item_id": item_id,
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": ""}
            }),
        )
        .await;

        let mut stream = Box::pin(stream);
        let mut buffer = String::new();
        let mut full_text = String::new();
        let mut tool_calls: Vec<ChatToolCallState> = Vec::new();
        let mut first_token_ms = None;
        while let Some(chunk) = stream.next().await {
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
                            let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
                            let _ =
                                u_tx.send(StreamOutcome::failed(usage, first_token_ms, message));
                            return;
                        }
                        if let Some(delta) = chat_stream_delta(&data) {
                            first_token_ms
                                .get_or_insert_with(|| started.elapsed().as_millis() as i64);
                            full_text.push_str(&delta);
                            let _ = send_response_sse(
                                &tx,
                                "response.output_text.delta",
                                json!({
                                    "type": "response.output_text.delta",
                                    "item_id": item_id,
                                    "output_index": 0,
                                    "content_index": 0,
                                    "delta": delta
                                }),
                            )
                            .await;
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
                                let _ = send_response_sse(
                                    &tx,
                                    "response.output_item.added",
                                    json!({
                                        "type": "response.output_item.added",
                                        "output_index": delta.index + 1,
                                        "item": item
                                    }),
                                )
                                .await;
                            }
                            if let Some(arguments) = delta.arguments {
                                call.arguments.push_str(&arguments);
                                if call.added && !call.is_custom && !arguments.is_empty() {
                                    let _ = send_response_sse(
                                        &tx,
                                        "response.function_call_arguments.delta",
                                        json!({
                                            "type": "response.function_call_arguments.delta",
                                            "output_index": delta.index + 1,
                                            "delta": arguments
                                        }),
                                    )
                                    .await;
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
                    }
                }
                Err(err) => {
                    let message = err.to_string();
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

async fn send_response_stream_done(
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
struct ChatToolCallState {
    call_id: String,
    name: String,
    arguments: String,
    added: bool,
    is_custom: bool,
}

struct ChatToolCallDelta {
    index: usize,
    id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
}

fn chat_stream_tool_call_deltas(data: &str) -> Vec<ChatToolCallDelta> {
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

fn chat_stream_error_message(data: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(data).ok()?;
    let error = value.get("error")?;
    if let Some(message) = error.get("message").and_then(Value::as_str) {
        return Some(truncate(message));
    }
    Some(truncate(&error.to_string()))
}

async fn send_response_sse(
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

fn chat_stream_delta(data: &str) -> Option<String> {
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

async fn send_to_provider(
    state: &AppState,
    provider: &ProviderConfig,
    path: &str,
    request_headers: &HeaderMap,
    body: Value,
    stream: bool,
    started: Instant,
) -> Result<ProviderResult, ProxyError> {
    state.remember_keepalive_headers(provider, request_headers);
    let client = state.client_for_provider(provider);
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
                    // 首次注入即遇 4xx：极大概率是上游拒绝 include_usage 字段
                    // 立即回退到"不注入"版本重试一次，不消耗 max_retries 名额，永久标记该 provider
                    if probing && status.is_client_error() {
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
                    return Err(ProxyError::new(status, clean_upstream_error(&text)));
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
    let value = send_json_to_provider(&client, provider, path, request_headers, body).await?;
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
    let upstream_model = response_model(&value).unwrap_or(request_model);
    let usage = extract_token_usage(&value);
    Ok(ProviderResult {
        response: (StatusCode::OK, Json(value)).into_response(),
        upstream_model,
        usage,
        usage_rx: None,
    })
}

// Anthropic 渠道专用：OpenAI chat 请求翻译到 /messages，响应/流反向翻译回 OpenAI
async fn send_to_anthropic_provider(
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
                let translated =
                    crate::anthropic::anthropic_to_openai_response(value, &request_model);
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
                return Err(ProxyError::new(status, clean_upstream_error(&text)));
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

// 原样发送 Responses 请求体，只把上游 Responses SSE 翻译成内部统一使用的 Chat SSE。
// Codex 兼容上游可能严格校验请求体，不能先转 Chat 再重建 Responses。
async fn send_responses_stream_as_chat(
    state: &AppState,
    provider: &ProviderConfig,
    request_headers: &HeaderMap,
    responses_body: Value,
    started: Instant,
) -> Result<ProviderResult, ProxyError> {
    state.remember_keepalive_headers(provider, request_headers);
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
                return Err(ProxyError::new(status, clean_upstream_error(&text)));
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
async fn send_chat_via_responses(
    state: &AppState,
    provider: &ProviderConfig,
    request_headers: &HeaderMap,
    body: Value,
    stream: bool,
    started: Instant,
) -> Result<ProviderResult, ProxyError> {
    state.remember_keepalive_headers(provider, request_headers);
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
                return Err(ProxyError::new(status, clean_upstream_error(&text)));
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

async fn send_json_to_provider(
    client: &Client,
    provider: &ProviderConfig,
    path: &str,
    request_headers: &HeaderMap,
    body: Value,
) -> Result<Value, ProxyError> {
    let url = upstream_url(provider, path);
    for attempt in 0..=provider.max_retries {
        let result = client
            .post(url.clone())
            .timeout(Duration::from_secs(provider.request_timeout.max(1)))
            .headers(upstream_headers(provider, request_headers, false))
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
                return Err(ProxyError::new(status, clean_upstream_error(&text)));
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

fn authorize_and_acquire(
    state: &AppState,
    config: &AppConfig,
    headers: &HeaderMap,
) -> Result<AuthAccess, ProxyError> {
    if !config.auth.enabled {
        return Ok(AuthAccess {
            permit: state.acquire_api_key_permit("__auth_disabled__", 1_000_000)?,
            key_name: String::new(),
        });
    }
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or_default();

    if let Some(key) = config
        .auth
        .api_keys
        .iter()
        .find(|k| k.enabled && k.key == token)
    {
        let limit = key
            .max_concurrency
            .or(config.auth.max_concurrency_per_key)
            .unwrap_or(5);
        Ok(AuthAccess {
            permit: state.acquire_api_key_permit(token, limit)?,
            key_name: key.name.clone(),
        })
    } else {
        Err(ProxyError::new(
            StatusCode::UNAUTHORIZED,
            "无效或缺失 API Key",
        ))
    }
}

fn provider_attempts(
    cfg: &AppConfig,
    state: &AppState,
    model: &str,
    api: &str,
) -> Vec<(ProviderConfig, String)> {
    let mut attempts = vec![model.to_string()];
    if let Some(fallbacks) = cfg.routing.model_fallbacks.get(model) {
        for item in fallbacks {
            if !attempts.contains(item) {
                attempts.push(item.clone());
            }
        }
    }

    let mut out = Vec::new();
    for request_model in attempts {
        let mut providers: Vec<ProviderConfig> = cfg
            .providers
            .iter()
            .filter(|p| {
                p.enabled
                    && provider_supports_model(p, &request_model)
                    && provider_supports_api(p, api)
            })
            .cloned()
            .collect();
        providers.sort_by_key(|p| (p.priority, p.name.clone()));
        let mut by_priority: HashMap<i32, Vec<ProviderConfig>> = HashMap::new();
        for provider in providers {
            by_priority
                .entry(provider.priority)
                .or_default()
                .push(provider);
        }
        let mut priorities: Vec<_> = by_priority.keys().copied().collect();
        priorities.sort();
        for priority in priorities {
            let weighted = weighted_order(
                state,
                &request_model,
                by_priority.remove(&priority).unwrap_or_default(),
            );
            out.extend(weighted.into_iter().map(|p| (p, request_model.clone())));
        }
    }
    out
}

fn weighted_order(
    state: &AppState,
    model: &str,
    providers: Vec<ProviderConfig>,
) -> Vec<ProviderConfig> {
    let mut expanded = Vec::new();
    for provider in providers {
        for _ in 0..provider.weight.max(1) {
            expanded.push(provider.clone());
        }
    }
    if expanded.is_empty() {
        return expanded;
    }
    let counter = {
        let mut counters = state.counters.lock();
        counters
            .entry(model.to_string())
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone()
    };
    let start = counter.fetch_add(1, Ordering::Relaxed) % expanded.len();
    expanded.rotate_left(start);
    let mut seen = HashSet::new();
    expanded
        .into_iter()
        .filter(|provider| seen.insert(provider.name.clone()))
        .collect()
}

fn provider_supports_model(provider: &ProviderConfig, model: &str) -> bool {
    let normalized = normalize_model(model);
    provider
        .models
        .iter()
        .any(|m| normalize_model(m) == normalized)
        || provider
            .model_mapping
            .keys()
            .any(|m| normalize_model(m) == normalized)
}

/// Google Gemini 提供的 OpenAI 兼容端点。识别规则与 codeProxyHub 完全一致：
/// URL 同时包含 `generativelanguage.googleapis.com` 与 `/openai`。
///
/// 这个端点只支持 Chat Completions，不支持 Responses API，因此 responses 请求
/// 打到这类 provider 时必须强制走 chat 翻译（force_chat=true）。
pub(crate) fn is_google_openai_endpoint(base_url: &str) -> bool {
    let lowered = base_url.to_ascii_lowercase();
    lowered.contains("generativelanguage.googleapis.com") && lowered.contains("/openai")
}

fn provider_supports_api(provider: &ProviderConfig, api: &str) -> bool {
    let supports_chat = !matches!(provider.capabilities.get("supports_chat"), Some(false));
    let supports_responses =
        !matches!(provider.capabilities.get("supports_responses"), Some(false));

    match api {
        // Anthropic providers are translated through /messages, so their native
        // supports_chat capability does not apply to the proxy-facing API.
        "chat" => provider.provider_type == "anthropic" || supports_chat,
        "responses" => {
            if provider.provider_type == "anthropic" {
                return true;
            }
            match provider.responses_mode.as_str() {
                // 显式指定 chat 代表用户要求将 Responses 请求转换到
                // /chat/completions，不应再被过期的 supports_chat:false 提前过滤。
                "chat" => true,
                "native" => supports_responses,
                _ => supports_responses || supports_chat,
            }
        }
        _ => true,
    }
}

fn collect_models(config: &AppConfig) -> Vec<String> {
    let mut models = Vec::new();
    for provider in config.providers.iter().filter(|p| p.enabled) {
        for model in &provider.models {
            if !models.contains(model) {
                models.push(model.clone());
            }
        }
        for model in provider.model_mapping.keys() {
            if !models.contains(model) {
                models.push(model.clone());
            }
        }
    }
    models.sort();
    models
}

fn body_model(body: &Value) -> Result<String, ProxyError> {
    body.get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| ProxyError::new(StatusCode::BAD_REQUEST, "请求体缺少 model"))
}

fn with_model(mut body: Value, model: &str) -> Value {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_string(), Value::String(model.to_string()));
    }
    body
}

fn apply_model_mapping(provider: &ProviderConfig, body: &mut Value) {
    let Some(model) = body.get("model").and_then(Value::as_str) else {
        return;
    };
    let normalized = normalize_model(model);
    let mapped = provider
        .model_mapping
        .get(model)
        .cloned()
        .or_else(|| {
            provider
                .model_mapping
                .iter()
                .find(|(k, _)| normalize_model(k) == normalized)
                .map(|(_, v)| v.clone())
        })
        .or_else(|| {
            provider
                .models
                .iter()
                .find(|m| normalize_model(m) == normalized)
                .cloned()
        });
    if let (Some(obj), Some(mapped)) = (body.as_object_mut(), mapped) {
        obj.insert("model".to_string(), Value::String(mapped));
    }
}

fn apply_system_prompt_override(provider: &ProviderConfig, body: &mut Value) {
    let Some(prompt) = provider
        .system_prompt_override
        .as_ref()
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let Some(messages) = obj.get("messages").and_then(Value::as_array) else {
        return;
    };
    let mut next = vec![json!({"role": "system", "content": prompt})];
    next.extend(
        messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) != Some("system"))
            .cloned(),
    );
    obj.insert("messages".to_string(), Value::Array(next));
}

fn responses_to_chat_body(body: &Value) -> Result<Value, ProxyError> {
    let mut obj = Map::new();
    obj.insert(
        "model".to_string(),
        body.get("model")
            .cloned()
            .unwrap_or(Value::String(String::new())),
    );
    obj.insert(
        "messages".to_string(),
        Value::Array(responses_input_to_messages(body.get("input"))),
    );
    for key in [
        "temperature",
        "top_p",
        "max_tokens",
        "max_output_tokens",
        "stream",
    ] {
        if let Some(value) = body.get(key) {
            let target = if key == "max_output_tokens" {
                "max_tokens"
            } else {
                key
            };
            obj.insert(target.to_string(), value.clone());
        }
    }
    if let Some(tools) = responses_tools_to_chat_tools(body.get("tools")) {
        obj.insert("tools".to_string(), tools);
    }
    if let Some(tool_choice) = responses_tool_choice_to_chat(body.get("tool_choice")) {
        obj.insert("tool_choice".to_string(), tool_choice);
    }
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
        if let Some(Value::Array(messages)) = obj.get_mut("messages") {
            messages.insert(0, json!({"role": "system", "content": instructions}));
        }
    }
    Ok(Value::Object(obj))
}

fn responses_input_to_messages(input: Option<&Value>) -> Vec<Value> {
    match input {
        Some(Value::String(text)) => vec![json!({"role": "user", "content": text})],
        Some(Value::Array(items)) => {
            let mut messages = Vec::new();
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("message") | None => {
                        let role = match item.get("role").and_then(Value::as_str).unwrap_or("user")
                        {
                            "developer" | "system" => "system",
                            "assistant" => "assistant",
                            _ => "user",
                        };
                        let content = responses_message_content_to_chat(role, item.get("content"));
                        messages.push(json!({"role": role, "content": content}));
                    }
                    Some("function_call") | Some("custom_tool_call") => {
                        let call_id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                        let arguments = if item.get("type").and_then(Value::as_str)
                            == Some("custom_tool_call")
                        {
                            custom_tool_arguments(
                                item.get("input").and_then(Value::as_str).unwrap_or(""),
                            )
                        } else {
                            item.get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("{}")
                                .to_string()
                        };
                        messages.push(json!({
                            "role": "assistant",
                            "content": Value::Null,
                            "tool_calls": [{
                                "id": call_id,
                                "type": "function",
                                "function": {"name": name, "arguments": arguments}
                            }]
                        }));
                    }
                    Some("function_call_output") | Some("custom_tool_call_output") => {
                        let call_id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let output = item
                            .get("output")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned)
                            .unwrap_or_else(|| {
                                item.get("output").map(Value::to_string).unwrap_or_default()
                            });
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": output
                        }));
                    }
                    Some("reasoning") => {}
                    _ => {
                        let text = item
                            .get("content")
                            .and_then(|c| responses_text_content(c, "user"))
                            .unwrap_or_default();
                        if !text.is_empty() {
                            messages.push(json!({"role": "user", "content": text}));
                        }
                    }
                }
            }
            if messages.is_empty() {
                messages.push(json!({"role": "user", "content": ""}));
            }
            messages
        }
        _ => vec![json!({"role": "user", "content": ""})],
    }
}

fn responses_message_content_to_chat(role: &str, content: Option<&Value>) -> Value {
    let Some(content) = content else {
        return Value::String(String::new());
    };
    if role == "assistant" || role == "system" {
        return Value::String(responses_text_content(content, role).unwrap_or_default());
    }
    match content {
        Value::String(text) => Value::String(text.clone()),
        Value::Array(parts) => {
            let mut out = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text") | Some("text") => {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                        out.push(json!({"type": "text", "text": text}));
                    }
                    Some("input_image") => {
                        if let Some(url) = part
                            .get("image_url")
                            .or_else(|| part.get("file_id"))
                            .and_then(Value::as_str)
                        {
                            out.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        }
                    }
                    _ => {}
                }
            }
            if out.is_empty() {
                Value::String(String::new())
            } else {
                Value::Array(out)
            }
        }
        other => Value::String(other.to_string()),
    }
}

fn responses_text_content(content: &Value, role: &str) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                let ty = part.get("type").and_then(Value::as_str);
                let wanted = if role == "assistant" {
                    matches!(ty, Some("output_text") | Some("text"))
                } else {
                    matches!(ty, Some("input_text") | Some("output_text") | Some("text"))
                };
                if wanted {
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        text.push_str(t);
                    }
                }
            }
            Some(text)
        }
        other => Some(other.to_string()),
    }
}

fn responses_tools_to_chat_tools(tools: Option<&Value>) -> Option<Value> {
    let tools = tools?.as_array()?;
    let out = tools
        .iter()
        .filter_map(|tool| match tool.get("type").and_then(Value::as_str) {
            Some("function") => {
                if tool.get("function").is_some() {
                    return Some(tool.clone());
                }
                let name = tool.get("name").and_then(Value::as_str)?;
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let parameters = tool
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": parameters
                    }
                }))
            }
            Some("custom") => {
                let name = tool.get("name").and_then(Value::as_str)?;
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "input": {
                                    "type": "string",
                                    "description": "Raw input for this custom tool"
                                }
                            },
                            "required": ["input"],
                            "additionalProperties": false
                        }
                    }
                }))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if out.is_empty() {
        None
    } else {
        Some(Value::Array(out))
    }
}

fn responses_custom_tool_names(tools: Option<&Value>) -> HashSet<String> {
    tools
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("custom"))
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect()
}

fn custom_tool_arguments(input: &str) -> String {
    serde_json::to_string(&json!({"input": input}))
        .unwrap_or_else(|_| "{\"input\":\"\"}".to_string())
}

fn custom_tool_input(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("input")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| arguments.to_string())
}

fn responses_tool_choice_to_chat(tool_choice: Option<&Value>) -> Option<Value> {
    match tool_choice? {
        Value::String(s) => Some(Value::String(s.clone())),
        Value::Object(_) => {
            if matches!(
                tool_choice?.get("type").and_then(Value::as_str),
                Some("function") | Some("custom")
            ) {
                let name = tool_choice?.get("name").and_then(Value::as_str)?;
                Some(json!({"type": "function", "function": {"name": name}}))
            } else {
                Some(tool_choice?.clone())
            }
        }
        _ => None,
    }
}

fn chat_to_response(
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

fn response_model(value: &Value) -> Option<String> {
    value
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .map(ToOwned::to_owned)
}

fn extract_token_usage(value: &Value) -> TokenUsage {
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

fn supports_include_usage_injection(path: &str) -> bool {
    path == "/chat/completions"
}

// Chat Completions 兼容渠道默认关闭流式 usage，主动补上 stream_options.include_usage=true
fn inject_include_usage(body: &mut Value) {
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
fn accumulate_usage_from_sse_data(data: &str, target: &mut TokenUsage) {
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
fn stream_with_usage_probe<S>(
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

fn push_raw_sse_event(
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

fn write_raw_sse_capture(
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

fn sanitize_file_part(value: &str) -> String {
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

fn sse_probe_kind_name(kind: SseProbeKind) -> &'static str {
    match kind {
        SseProbeKind::Chat => "chat",
        SseProbeKind::Responses => "responses",
        SseProbeKind::Anthropic => "anthropic",
    }
}

fn sse_stream_completed(kind: SseProbeKind, data: &str) -> bool {
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

fn chat_stream_completed(data: &str) -> bool {
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

fn sse_stream_error(kind: SseProbeKind, data: &str) -> Option<String> {
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
fn commit_result(
    cfg: Arc<AppConfig>,
    api: &str,
    provider: &str,
    model: &str,
    mut result: ProviderResult,
    started: Instant,
    stream: bool,
    permit: Option<OwnedSemaphorePermit>,
    api_key_name: &str,
) -> Response {
    if let Some(rx) = result.usage_rx.take() {
        let log_id = log_stream_started(
            &cfg,
            api,
            provider,
            model,
            &result.upstream_model,
            started,
            api_key_name,
        );
        let api = api.to_string();
        let api_key_name = api_key_name.to_string();
        let provider = provider.to_string();
        let model = model.to_string();
        let upstream_model = result.upstream_model.clone();
        tokio::spawn(async move {
            match rx.await {
                Ok(StreamOutcome {
                    usage: _,
                    first_token_ms,
                    error: Some(error),
                }) => {
                    finalize_stream_log(
                        &cfg,
                        log_id,
                        started,
                        UsageLogEvent {
                            api: &api,
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
                    finalize_stream_log(
                        &cfg,
                        log_id,
                        started,
                        UsageLogEvent {
                            api: &api,
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
                    finalize_stream_log(
                        &cfg,
                        log_id,
                        started,
                        UsageLogEvent {
                            api: &api,
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

fn hold_permit_until_body_done(response: Response, permit: OwnedSemaphorePermit) -> Response {
    let (parts, body) = response.into_parts();
    let guard = Some(permit);
    let stream = body.into_data_stream().map(move |item| {
        let _guard = &guard;
        item
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

fn upstream_url(provider: &ProviderConfig, path: &str) -> String {
    format!("{}{}", provider.base_url.trim_end_matches('/'), path)
}

fn upstream_headers(
    provider: &ProviderConfig,
    request_headers: &HeaderMap,
    stream: bool,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if provider.provider_type == "anthropic" {
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(&provider.api_key)
                .unwrap_or_else(|_| HeaderValue::from_static("")),
        );
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    } else if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", provider.api_key)) {
        headers.insert(header::AUTHORIZATION, value);
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
            "authorization" | "content-type" | "accept" | "host" | "content-length" | "x-api-key"
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

    // 客户端头透传（与 codeProxyHub 的 _keepalive_safe_headers 对齐）：
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
        let prefix_match = lower.starts_with("codex-")
            || lower.starts_with("openai-")
            || lower.starts_with("x-codex-")
            || lower.starts_with("x-openai-")
            || lower.starts_with("x-stainless-")
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

        // extra_headers 已经在前面写入过，这里若客户端也发了同名头则以客户端为准
        // （codex CLI 的头才是校验目标，配置里的静态值只是兜底）
        headers.insert(name.clone(), value.clone());
    }
    headers
}

fn keepalive_safe_headers(request_headers: &HeaderMap) -> HeaderMap {
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

fn normalize_model(model: &str) -> &str {
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

fn display_host(host: &str) -> &str {
    if host == "0.0.0.0" {
        "127.0.0.1"
    } else {
        host
    }
}

struct UsageLogEvent<'a> {
    api: &'a str,
    api_key_name: &'a str,
    provider: &'a str,
    model: &'a str,
    upstream_model: &'a str,
    status: &'a str,
    error: Option<&'a str>,
    usage: TokenUsage,
    first_token_ms: Option<i64>,
    token_source: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
fn log_success(
    config: &AppConfig,
    api: &str,
    provider: &str,
    model: &str,
    upstream_model: &str,
    usage: TokenUsage,
    started: Instant,
    first_token_ms: Option<i64>,
    stream: bool,
    api_key_name: &str,
) {
    log_usage(
        config,
        started,
        UsageLogEvent {
            api,
            api_key_name,
            provider,
            model,
            upstream_model,
            status: if stream { "stream_started" } else { "ok" },
            error: None,
            usage,
            first_token_ms,
            token_source: None,
        },
    );
}

fn log_error(
    config: &AppConfig,
    api: &str,
    provider: &str,
    model: &str,
    started: Instant,
    first_token_ms: Option<i64>,
    error: &str,
    api_key_name: &str,
) {
    log_usage(
        config,
        started,
        UsageLogEvent {
            api,
            api_key_name,
            provider,
            model,
            upstream_model: "",
            status: "error",
            error: Some(error),
            usage: TokenUsage::default(),
            first_token_ms,
            token_source: None,
        },
    );
}

fn log_stream_started(
    config: &AppConfig,
    api: &str,
    provider: &str,
    model: &str,
    upstream_model: &str,
    started: Instant,
    api_key_name: &str,
) -> Option<i64> {
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    log_usage_sqlite(
        config.usage_log_sqlite_path(),
        &ts,
        api,
        api_key_name,
        "running",
        provider,
        model,
        upstream_model,
        started.elapsed().as_millis() as i64,
        None,
        "",
        0,
        0,
        "upstream_or_unknown",
    )
    .ok()
}

fn update_usage_log(
    config: &AppConfig,
    id: i64,
    started: Instant,
    event: &UsageLogEvent<'_>,
) -> rusqlite::Result<usize> {
    update_usage_log_sqlite(
        config.usage_log_sqlite_path(),
        id,
        event.status,
        event.upstream_model,
        started.elapsed().as_millis() as i64,
        event.first_token_ms,
        event.error.unwrap_or(""),
        event.usage.input,
        event.usage.output,
        event.token_source.unwrap_or("upstream_or_unknown"),
    )
}

async fn finalize_stream_log(
    config: &AppConfig,
    log_id: Option<i64>,
    started: Instant,
    event: UsageLogEvent<'_>,
) {
    if let Some(id) = log_id {
        for attempt in 0..3 {
            match update_usage_log(config, id, started, &event) {
                Ok(updated) if updated > 0 => return,
                Ok(_) => break,
                Err(err) if sqlite_lock_error(&err) && attempt < 2 => {
                    tokio::time::sleep(Duration::from_millis(100 * (attempt + 1) as u64)).await;
                }
                Err(_) => break,
            }
        }
    }
    log_usage(config, started, event);
}

fn sqlite_lock_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

fn log_usage(config: &AppConfig, started: Instant, event: UsageLogEvent<'_>) {
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let latency_ms = started.elapsed().as_millis() as i64;
    let error = event.error.unwrap_or("");
    let token_source = event.token_source.unwrap_or("upstream_or_unknown");

    let _ = log_usage_sqlite(
        config.usage_log_sqlite_path(),
        &ts,
        event.api,
        event.api_key_name,
        event.status,
        event.provider,
        event.model,
        event.upstream_model,
        latency_ms,
        event.first_token_ms,
        error,
        event.usage.input,
        event.usage.output,
        token_source,
    );
}

#[allow(clippy::too_many_arguments)]
fn log_usage_sqlite(
    path: PathBuf,
    ts: &str,
    api: &str,
    api_key_name: &str,
    status: &str,
    provider: &str,
    model: &str,
    upstream_model: &str,
    latency_ms: i64,
    first_token_ms: Option<i64>,
    error: &str,
    input_tokens: i64,
    output_tokens: i64,
    token_source: &str,
) -> rusqlite::Result<i64> {
    let conn = open_usage_log_connection(path)?;
    ensure_usage_log_schema(&conn)?;
    conn.execute(
        r#"
        INSERT INTO usage_logs
            (
                ts, api, status, channel, request_model, upstream_model,
                latency_ms, first_token_ms, input_tokens, output_tokens, error, token_source,
                api_key_name
            )
        VALUES
            (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        "#,
        params![
            ts,
            api,
            status,
            provider,
            model,
            upstream_model,
            latency_ms,
            first_token_ms,
            input_tokens,
            output_tokens,
            error,
            token_source,
            api_key_name
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

#[allow(clippy::too_many_arguments)]
fn update_usage_log_sqlite(
    path: PathBuf,
    id: i64,
    status: &str,
    upstream_model: &str,
    latency_ms: i64,
    first_token_ms: Option<i64>,
    error: &str,
    input_tokens: i64,
    output_tokens: i64,
    token_source: &str,
) -> rusqlite::Result<usize> {
    let conn = open_usage_log_connection(path)?;
    ensure_usage_log_schema(&conn)?;
    conn.execute(
        r#"
        UPDATE usage_logs
        SET status = ?1, upstream_model = ?2, latency_ms = ?3, first_token_ms = ?4,
            input_tokens = ?5, output_tokens = ?6, error = ?7, token_source = ?8
        WHERE id = ?9
        "#,
        params![
            status,
            upstream_model,
            latency_ms,
            first_token_ms,
            input_tokens,
            output_tokens,
            error,
            token_source,
            id
        ],
    )
}

pub(crate) fn open_usage_log_connection(path: PathBuf) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

fn recover_interrupted_usage_logs(path: PathBuf) {
    let Ok(conn) = open_usage_log_connection(path) else {
        return;
    };
    if ensure_usage_log_schema(&conn).is_err() {
        return;
    }
    let _ = conn.execute(
        r#"
        UPDATE usage_logs
        SET status = 'error', error = '代理上次退出前请求未完成'
        WHERE status = 'running'
        "#,
        [],
    );
}

pub fn ensure_usage_log_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS usage_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts TEXT NOT NULL,
            api TEXT NOT NULL,
            status TEXT NOT NULL,
            channel TEXT NOT NULL,
            request_model TEXT NOT NULL,
            upstream_model TEXT NOT NULL DEFAULT '',
            latency_ms INTEGER NOT NULL,
            first_token_ms INTEGER,
            input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0,
            error TEXT NOT NULL DEFAULT '',
            token_source TEXT NOT NULL DEFAULT 'upstream_or_unknown',
            api_key_name TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_usage_logs_ts ON usage_logs(ts);
        CREATE INDEX IF NOT EXISTS idx_usage_logs_status ON usage_logs(status);
        CREATE INDEX IF NOT EXISTS idx_usage_logs_channel ON usage_logs(channel);
        "#,
    )?;
    ensure_column(conn, "upstream_model", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "first_token_ms", "INTEGER")?;
    ensure_column(conn, "input_tokens", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "output_tokens", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "api_key_name", "TEXT NOT NULL DEFAULT ''")?;
    Ok(())
}

fn ensure_column(conn: &Connection, name: &str, definition: &str) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(usage_logs)")?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for column in columns {
        if column? == name {
            return Ok(());
        }
    }
    conn.execute(
        &format!("ALTER TABLE usage_logs ADD COLUMN {name} {definition}"),
        [],
    )?;
    Ok(())
}

pub fn validate_config(config: &AppConfig) -> Result<()> {
    if config.providers.iter().filter(|p| p.enabled).count() == 0 {
        return Err(anyhow!("没有启用的 provider"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
            keepalive_headers: Arc::new(Mutex::new(HashMap::new())),
            api_key_limiters: Arc::new(Mutex::new(HashMap::new())),
            usage_injection: Arc::new(Mutex::new(HashMap::new())),
        }
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
    fn keepalive_applies_model_mapping_and_uses_responses_when_chat_is_unsupported() {
        let mut p = provider(
            "openai",
            "auto",
            json!({"supports_chat": false, "supports_responses": true}),
        );
        p.model_mapping
            .insert("gpt-5.5".to_string(), "gpt-5.6-sol".to_string());

        let (path, body) = build_keepalive_request(&p, "gpt-5.5", "Hi").unwrap();

        assert_eq!(path, "/responses");
        assert_eq!(
            body.get("model").and_then(Value::as_str),
            Some("gpt-5.6-sol")
        );
        assert_eq!(
            body.pointer("/input/0/content/0/text")
                .and_then(Value::as_str),
            Some("Hi")
        );
    }

    #[test]
    fn keepalive_auto_uses_chat_when_provider_supports_both_apis() {
        let p = provider(
            "openai",
            "auto",
            json!({"supports_chat": true, "supports_responses": true}),
        );

        let (path, _) = build_keepalive_request(&p, "gpt-test", "Hi").unwrap();

        assert_eq!(path, "/chat/completions");
    }

    #[test]
    fn keepalive_headers_include_codex_session_markers() {
        let headers = keepalive_request_headers();

        assert!(headers.contains_key("x-request-id"));
        assert!(headers.contains_key("session_id"));
        assert!(headers.contains_key("conversation_id"));
        assert!(headers.contains_key("openai-client"));
    }

    #[test]
    fn keepalive_safe_headers_preserve_codex_markers_without_auth() {
        let mut input = HeaderMap::new();
        insert_header(&mut input, "authorization", "Bearer local");
        insert_header(&mut input, "session_id", "session-1");
        insert_header(&mut input, "conversation_id", "conversation-1");
        insert_header(&mut input, "x-stainless-runtime", "rust");

        let headers = keepalive_safe_headers(&input);

        assert!(!headers.contains_key("authorization"));
        assert_eq!(
            headers
                .get("session_id")
                .and_then(|value| value.to_str().ok()),
            Some("session-1")
        );
        assert!(headers.contains_key("conversation_id"));
        assert!(headers.contains_key("x-stainless-runtime"));
        assert!(has_codex_session_headers(&headers));
    }

    #[test]
    fn codex_responses_keepalive_waits_for_real_client_headers() {
        let mut p = provider(
            "openai",
            "auto",
            json!({"supports_chat": false, "supports_responses": true}),
        );
        p.models.push("gpt-5-codex".to_string());
        p.extra_headers
            .insert("originator".to_string(), "codex_cli_rs".to_string());

        assert!(provider_keepalive_requires_client_headers(&p));
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
    async fn sse_probe_preserves_chat_prefix_after_first_delta() {
        let first = json!({"choices":[{"delta":{"role":"assistant"}}]});
        let second = json!({"choices":[{"delta":{"content":"hi"}}]});
        let bytes = Bytes::from(format!("data: {first}\n\ndata: {second}\n\n"));
        let stream: ProxyByteStream =
            Box::pin(futures_util::stream::iter([Ok::<Bytes, io::Error>(bytes)]));

        let mut prepared = prepare_sse_stream(stream, 1, SseProbeKind::Chat)
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
    async fn stream_usage_probe_reports_unfinished_stream_as_error() {
        let chunk = json!({"choices":[{"delta":{"content":"hi"}}]});
        let bytes = Bytes::from(format!("data: {chunk}\n\n"));
        let stream = futures_util::stream::iter([Ok::<Bytes, io::Error>(bytes)]);

        let (rx, mut body_stream) =
            stream_with_usage_probe(stream, SseProbeKind::Chat, Instant::now(), None);
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

        let (rx, mut body_stream) =
            stream_with_usage_probe(stream, SseProbeKind::Chat, Instant::now(), None);
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

        let (rx, mut body_stream) =
            stream_with_usage_probe(stream, SseProbeKind::Responses, Instant::now(), None);
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

        let (rx, mut body_stream) =
            stream_with_usage_probe(stream, SseProbeKind::Responses, Instant::now(), None);
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

        let (rx, mut body_stream) =
            stream_with_usage_probe(stream, SseProbeKind::Responses, Instant::now(), None);
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

        let (rx, mut body_stream) =
            stream_with_usage_probe(stream, SseProbeKind::Responses, Instant::now(), None);
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

        let (rx, mut body_stream) =
            stream_with_usage_probe(stream, SseProbeKind::Chat, Instant::now(), None);
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
        let stream: ProxyByteStream =
            Box::pin(futures_util::stream::iter([Ok::<Bytes, io::Error>(
                Bytes::from("data: [DONE]\n\n"),
            )]));

        let err = match prepare_sse_stream(stream, 1, SseProbeKind::Chat).await {
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
        let stream: ProxyByteStream =
            Box::pin(futures_util::stream::iter([Ok::<Bytes, io::Error>(
                Bytes::from(format!("data: {chunk}\n\n")),
            )]));

        let err = match prepare_sse_stream(stream, 1, SseProbeKind::Responses).await {
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
}

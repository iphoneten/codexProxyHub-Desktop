use crate::config::{AppConfig, ProviderConfig};
use crate::request_ctx::RequestContext;
use anyhow::{anyhow, Result};
use axum::{
    body::{to_bytes, Body},
    extract::{DefaultBodyLimit, Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
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
use tokio_util::sync::CancellationToken;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;
const MAX_ANTHROPIC_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

mod auth;
mod handlers;
mod oauth;
mod request_layer;
mod routing;
mod streaming;
mod upstream;
mod usage_log;

use auth::*;
use handlers::*;
use oauth::*;
use request_layer::*;
use routing::*;
use streaming::*;
use upstream::*;
pub(crate) use usage_log::USAGE_LOG_FILTER_WHERE;
use usage_log::*;

#[cfg(test)]
mod anthropic_passthrough_tests;
#[cfg(test)]
mod messages_e2e_tests;
#[cfg(test)]
mod tests;

pub(crate) fn next_sse_event(buffer: &str) -> Option<(String, usize)> {
    streaming::next_sse_event(buffer)
}

pub(crate) fn sse_data(event: &str) -> Option<String> {
    streaming::sse_data(event)
}

pub(crate) fn is_google_openai_endpoint(base_url: &str) -> bool {
    routing::is_google_openai_endpoint(base_url)
}

pub(crate) fn open_usage_log_connection(path: PathBuf) -> rusqlite::Result<Connection> {
    usage_log::open_usage_log_connection(path)
}

pub(crate) fn ensure_usage_log_schema(conn: &Connection) -> rusqlite::Result<()> {
    usage_log::ensure_usage_log_schema(conn)
}

pub(crate) fn recover_stale_running_usage_logs(path: PathBuf, max_age: Duration) {
    usage_log::recover_stale_running_usage_logs(path, max_age)
}

pub(crate) fn delete_usage_logs(
    path: PathBuf,
    range_start: Option<&str>,
    api_key_id: Option<&str>,
    api_key_name: Option<&str>,
    status_kind: Option<&str>,
) -> rusqlite::Result<usize> {
    usage_log::delete_usage_logs(path, range_start, api_key_id, api_key_name, status_kind)
}

pub(crate) fn api_key_id(api_key: &str) -> String {
    if api_key.trim().is_empty() {
        return String::new();
    }
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("routehub:api-key:{}", api_key.trim()).as_bytes(),
    )
    .to_string()
}

fn request_api_key_id(config: &AppConfig, headers: &HeaderMap) -> String {
    if !config.auth.enabled {
        return String::new();
    }
    api_key_id(request_client_token(headers))
}

/// 提取客户端 API Key。兼容两种客户端惯例：
/// - OpenAI / codex 系：`Authorization: Bearer <key>`
/// - Anthropic SDK / Claude Code：`x-api-key: <key>`
///
/// Anthropic 官方 SDK 只发 `x-api-key`，不发 Authorization，
/// 入站 `/v1/messages` 必须认这个头，否则开启鉴权后必然 401。
pub(super) fn request_client_token(headers: &HeaderMap) -> &str {
    if let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        return token;
    }
    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or_default()
}

#[cfg(test)]
pub(super) fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        header::HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        headers.insert(name, value);
    }
}

// 配置句柄：Arc<RwLock<Arc<AppConfig>>>
// 读侧 read() + clone Arc 指针，几乎无锁；写侧只在切换指针时短暂持有写锁
pub(crate) type ConfigHandle = Arc<RwLock<Arc<AppConfig>>>;
type ProxyByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>;
const PROVIDER_CIRCUIT_FAILURE_THRESHOLD: usize = 3;
const PROVIDER_CIRCUIT_DEFAULT_COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProviderCircuitStatus {
    #[default]
    Healthy,
    Open,
    HalfOpen,
}

#[derive(Clone, Debug, Default)]
pub struct ProviderCircuitSnapshot {
    pub status: ProviderCircuitStatus,
    pub failures: usize,
    pub inflight: usize,
    pub open_until: Option<Instant>,
    pub last_error: Option<String>,
}

pub type ProviderCircuitStatusHandle = Arc<RwLock<HashMap<String, ProviderCircuitSnapshot>>>;

#[derive(Clone, Debug)]
enum ProviderCircuitPhase {
    Closed,
    Open { until: Instant },
    HalfOpen { probe_in_flight: bool },
}

#[derive(Clone, Debug)]
struct ProviderCircuit {
    phase: ProviderCircuitPhase,
    consecutive_failures: usize,
    last_error: Option<String>,
    last_changed: Instant,
}

impl Default for ProviderCircuit {
    fn default() -> Self {
        Self {
            phase: ProviderCircuitPhase::Closed,
            consecutive_failures: 0,
            last_error: None,
            last_changed: Instant::now(),
        }
    }
}

#[derive(Debug)]
struct ProviderCircuitGuard {
    provider: String,
    circuits: Arc<Mutex<HashMap<String, ProviderCircuit>>>,
    statuses: ProviderCircuitStatusHandle,
    inflight: Arc<AtomicUsize>,
    half_open_probe: bool,
    completed: bool,
}

impl ProviderCircuitGuard {
    fn mark_success(mut self) {
        let mut circuits = self.circuits.lock();
        let circuit = circuits.entry(self.provider.clone()).or_default();
        if !self.half_open_probe
            && matches!(
                circuit.phase,
                ProviderCircuitPhase::Open { .. } | ProviderCircuitPhase::HalfOpen { .. }
            )
        {
            self.completed = true;
            return;
        }
        circuits.insert(
            self.provider.clone(),
            ProviderCircuit {
                phase: ProviderCircuitPhase::Closed,
                consecutive_failures: 0,
                last_error: None,
                last_changed: Instant::now(),
            },
        );
        drop(circuits);
        let mut statuses = self.statuses.write();
        let status = statuses.entry(self.provider.clone()).or_default();
        status.status = ProviderCircuitStatus::Healthy;
        status.failures = 0;
        status.open_until = None;
        status.last_error = None;
        self.completed = true;
    }

    fn mark_failure(mut self, status: StatusCode, message: &str, retry_after: Option<Duration>) {
        let quota_exhausted = quota_exhausted_error(message);
        if !circuit_breaker_failure(status, message) {
            self.mark_success();
            return;
        }

        let mut circuits = self.circuits.lock();
        let circuit = circuits.entry(self.provider.clone()).or_default();
        circuit.last_error = Some(message.to_string());
        circuit.last_changed = Instant::now();

        if !self.half_open_probe {
            match circuit.phase.clone() {
                ProviderCircuitPhase::Open { until } => {
                    circuit.consecutive_failures += 1;
                    let until = if status == StatusCode::TOO_MANY_REQUESTS || quota_exhausted {
                        until.max(
                            Instant::now()
                                + retry_after.unwrap_or(PROVIDER_CIRCUIT_DEFAULT_COOLDOWN),
                        )
                    } else {
                        until
                    };
                    circuit.phase = ProviderCircuitPhase::Open { until };
                    let failures = circuit.consecutive_failures;
                    drop(circuits);
                    self.update_failure_status(
                        ProviderCircuitStatus::Open,
                        failures,
                        Some(until),
                        message,
                    );
                    self.completed = true;
                    return;
                }
                ProviderCircuitPhase::HalfOpen { .. } => {
                    circuit.consecutive_failures += 1;
                    let failures = circuit.consecutive_failures;
                    drop(circuits);
                    self.update_failure_status(
                        ProviderCircuitStatus::HalfOpen,
                        failures,
                        None,
                        message,
                    );
                    self.completed = true;
                    return;
                }
                ProviderCircuitPhase::Closed => {}
            }
        }

        if status == StatusCode::TOO_MANY_REQUESTS || quota_exhausted {
            circuit.consecutive_failures += 1;
            let until = Instant::now() + retry_after.unwrap_or(PROVIDER_CIRCUIT_DEFAULT_COOLDOWN);
            circuit.phase = ProviderCircuitPhase::Open { until };
            let failures = circuit.consecutive_failures;
            drop(circuits);
            self.update_failure_status(ProviderCircuitStatus::Open, failures, Some(until), message);
            self.completed = true;
            return;
        }

        circuit.consecutive_failures += 1;
        let opened = matches!(circuit.phase, ProviderCircuitPhase::HalfOpen { .. })
            || circuit.consecutive_failures >= PROVIDER_CIRCUIT_FAILURE_THRESHOLD;
        let until = opened.then(|| Instant::now() + PROVIDER_CIRCUIT_DEFAULT_COOLDOWN);
        if let Some(until) = until {
            circuit.phase = ProviderCircuitPhase::Open { until };
        } else {
            circuit.phase = ProviderCircuitPhase::Closed;
        }
        let failures = circuit.consecutive_failures;
        drop(circuits);
        self.update_failure_status(
            if opened {
                ProviderCircuitStatus::Open
            } else {
                ProviderCircuitStatus::Healthy
            },
            failures,
            until,
            message,
        );
        self.completed = true;
    }

    fn update_failure_status(
        &self,
        circuit_status: ProviderCircuitStatus,
        failures: usize,
        open_until: Option<Instant>,
        message: &str,
    ) {
        let mut statuses = self.statuses.write();
        let status = statuses.entry(self.provider.clone()).or_default();
        status.status = circuit_status;
        status.failures = failures;
        status.open_until = open_until;
        status.last_error = Some(message.to_string());
    }
}

impl Drop for ProviderCircuitGuard {
    fn drop(&mut self) {
        if !self.completed && self.half_open_probe {
            let until = Instant::now() + PROVIDER_CIRCUIT_DEFAULT_COOLDOWN;
            let failures = {
                let mut circuits = self.circuits.lock();
                let circuit = circuits.entry(self.provider.clone()).or_default();
                circuit.phase = ProviderCircuitPhase::Open { until };
                circuit.last_error = Some("半开探测请求中断".to_string());
                circuit.last_changed = Instant::now();
                circuit.consecutive_failures = circuit
                    .consecutive_failures
                    .max(PROVIDER_CIRCUIT_FAILURE_THRESHOLD);
                circuit.consecutive_failures
            };
            self.update_failure_status(
                ProviderCircuitStatus::Open,
                failures,
                Some(until),
                "半开探测请求中断",
            );
        }

        let remaining = self.inflight.fetch_sub(1, Ordering::AcqRel) - 1;
        let mut statuses = self.statuses.write();
        statuses.entry(self.provider.clone()).or_default().inflight = remaining;
    }
}

fn circuit_breaker_failure(status: StatusCode, message: &str) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::UNAUTHORIZED
        || status == StatusCode::FORBIDDEN
        || status == StatusCode::PROXY_AUTHENTICATION_REQUIRED
        || status.is_server_error()
        || quota_exhausted_error(message)
}

fn quota_exhausted_error(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("insufficient_quota")
        || normalized.contains("quota exceeded")
        || normalized.contains("quota_exceeded")
        || normalized.contains("limit_reached")
        || message.contains("额度耗尽")
        || message.contains("额度用尽")
}

#[derive(Clone)]
struct ApiKeyLimiter {
    limit: usize,
    semaphore: Arc<Semaphore>,
}

#[derive(Debug)]
struct AuthAccess {
    permit: OwnedSemaphorePermit,
    key_name: String,
    daily_token_limit: Option<u64>,
    allowed_models: Vec<String>,
    allowed_providers: Vec<String>,
}

#[derive(Clone)]
struct SessionAffinity {
    provider: String,
    request_model: String,
}

#[derive(Clone)]
struct AppState {
    config: ConfigHandle,
    clients: Arc<Mutex<HashMap<(u64, String), Client>>>,
    counters: Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>>,
    api_key_limiters: Arc<Mutex<HashMap<String, ApiKeyLimiter>>>,
    provider_circuits: Arc<Mutex<HashMap<String, ProviderCircuit>>>,
    provider_loads: Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>>,
    provider_statuses: ProviderCircuitStatusHandle,
    session_affinity: Arc<Mutex<HashMap<String, SessionAffinity>>>,
    // 每个 provider 是否接受 stream_options.include_usage 注入的自动探测结果
    // Some(true)  = 已确认接受；Some(false) = 已确认拒绝；None = 未探测（默认注入试试）
    // 只在进程内缓存，代理重启后重新探测
    usage_injection: Arc<Mutex<HashMap<String, bool>>>,
    oauth_tokens: Arc<Mutex<HashMap<String, OAuthRuntimeToken>>>,
    oauth_refresh_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    // server 级取消信号。每个请求的 ctx 都是它的子 token，
    // 代理停止时一次取消就能叫停所有在飞的流式任务。
    root_cancel: CancellationToken,
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
        // Auth 账号始终使用统一代理；普通渠道按各自开关决定。
        let proxy_url = if provider.auth_account_id.is_some() || provider.use_proxy {
            self.snapshot().routing.auth_proxy.trim().to_string()
        } else {
            String::new()
        };
        let cache_key = (connect_timeout, proxy_url.clone());
        let mut clients = self.clients.lock();
        clients
            .entry(cache_key)
            .or_insert_with(|| {
                build_http_client(
                    connect_timeout,
                    if proxy_url.is_empty() {
                        None
                    } else {
                        Some(proxy_url.as_str())
                    },
                )
            })
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

    fn preferred_provider_for_affinity(&self, affinity_key: &str) -> Option<(String, String)> {
        if affinity_key.trim().is_empty() {
            return None;
        }
        self.session_affinity
            .lock()
            .get(affinity_key)
            .map(|affinity| (affinity.provider.clone(), affinity.request_model.clone()))
    }

    fn remember_affinity_provider(&self, affinity_key: &str, provider: &str, request_model: &str) {
        if affinity_key.trim().is_empty()
            || provider.trim().is_empty()
            || request_model.trim().is_empty()
        {
            return;
        }
        self.session_affinity.lock().insert(
            affinity_key.to_string(),
            SessionAffinity {
                provider: provider.to_string(),
                request_model: request_model.to_string(),
            },
        );
    }

    fn forget_affinity_provider(&self, affinity_key: &str, provider: &str, request_model: &str) {
        if affinity_key.trim().is_empty() {
            return;
        }
        let mut affinities = self.session_affinity.lock();
        let should_remove = affinities.get(affinity_key).is_some_and(|affinity| {
            affinity.provider == provider
                && normalize_model(&affinity.request_model) == normalize_model(request_model)
        });
        if should_remove {
            affinities.remove(affinity_key);
        }
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

    fn begin_provider_attempt(&self, provider: &str) -> Result<ProviderCircuitGuard, ProxyError> {
        let now = Instant::now();
        let mut circuits = self.provider_circuits.lock();
        let circuit = circuits.entry(provider.to_string()).or_default();
        let mut half_open_probe = false;

        match &mut circuit.phase {
            ProviderCircuitPhase::Closed => {}
            ProviderCircuitPhase::Open { until } if *until > now => {
                let remaining = until.saturating_duration_since(now).as_secs().max(1);
                return Err(ProxyError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("渠道 '{provider}' 熔断冷却中，约 {remaining} 秒后重试"),
                ));
            }
            ProviderCircuitPhase::Open { .. } => {
                half_open_probe = true;
                circuit.phase = ProviderCircuitPhase::HalfOpen {
                    probe_in_flight: true,
                };
                circuit.last_changed = now;
                let mut statuses = self.provider_statuses.write();
                let status = statuses.entry(provider.to_string()).or_default();
                status.status = ProviderCircuitStatus::HalfOpen;
                status.open_until = None;
            }
            ProviderCircuitPhase::HalfOpen { probe_in_flight } => {
                if *probe_in_flight {
                    return Err(ProxyError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        format!("渠道 '{provider}' 正在半开探测"),
                    ));
                }
                half_open_probe = true;
                *probe_in_flight = true;
                circuit.last_changed = now;
            }
        }
        drop(circuits);

        let inflight = {
            let mut loads = self.provider_loads.lock();
            loads
                .entry(provider.to_string())
                .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
                .clone()
        };
        let current_inflight = inflight.fetch_add(1, Ordering::AcqRel) + 1;
        self.provider_statuses
            .write()
            .entry(provider.to_string())
            .or_default()
            .inflight = current_inflight;

        Ok(ProviderCircuitGuard {
            provider: provider.to_string(),
            circuits: Arc::clone(&self.provider_circuits),
            statuses: Arc::clone(&self.provider_statuses),
            inflight,
            half_open_probe,
            completed: false,
        })
    }
}

fn build_http_client(connect_timeout: u64, proxy_url: Option<&str>) -> Client {
    let mut builder = Client::builder()
        .pool_max_idle_per_host(20)
        .connect_timeout(Duration::from_secs(connect_timeout.max(1)))
        .danger_accept_invalid_certs(false);
    if let Some(proxy_url) = proxy_url.map(str::trim).filter(|value| !value.is_empty()) {
        match reqwest::Proxy::all(proxy_url) {
            Ok(proxy) => {
                // 显式 Auth 代理时禁用系统代理，避免叠加
                builder = builder.no_proxy().proxy(proxy);
            }
            Err(err) => {
                eprintln!("invalid routing.auth_proxy `{proxy_url}`: {err}");
            }
        }
    }
    builder.build().expect("failed to build reqwest client")
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
    /// 客户端提前断开（或代理关停）导致的中断。
    /// 与 `error` 的区别在于责任方不是上游：不该记成成功，也不该算渠道故障。
    pub(crate) aborted: bool,
}

impl StreamOutcome {
    pub(crate) fn success(usage: TokenUsage, first_token_ms: Option<i64>) -> Self {
        Self {
            usage,
            first_token_ms,
            error: None,
            aborted: false,
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
            aborted: false,
        }
    }

    /// 客户端断开/代理关停导致的中断，日志记为「已取消」。
    pub(crate) fn aborted(usage: TokenUsage, first_token_ms: Option<i64>) -> Self {
        Self {
            usage,
            first_token_ms,
            error: Some(STREAM_ABORTED_MESSAGE.to_string()),
            aborted: true,
        }
    }
}

pub(crate) const STREAM_ABORTED_MESSAGE: &str = "客户端提前断开，已取消上游生成";
pub(crate) const STREAM_ABORTED_STATUS: &str = "aborted";

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

fn begin_attempt_or_record_failure(
    state: &AppState,
    provider: &ProviderConfig,
    request_model: &str,
    started: Instant,
) -> Result<ProviderCircuitGuard, AttemptFailure> {
    state
        .begin_provider_attempt(&provider.name)
        .map_err(|err| AttemptFailure::new(provider, request_model, started, err))
}

fn record_attempt_failure(guard: ProviderCircuitGuard, err: &ProxyError) -> bool {
    if err.aborted {
        drop(guard);
        true
    } else {
        guard.mark_failure(err.status, &err.message, err.retry_after);
        false
    }
}

#[derive(Debug)]
struct ProxyError {
    status: StatusCode,
    message: String,
    retry_after: Option<Duration>,
    aborted: bool,
}

impl ProxyError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            retry_after: None,
            aborted: false,
        }
    }

    fn aborted() -> Self {
        Self {
            status: StatusCode::from_u16(499).expect("499 is a valid HTTP status"),
            message: STREAM_ABORTED_MESSAGE.to_string(),
            retry_after: None,
            aborted: true,
        }
    }

    fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
        self.retry_after = retry_after;
        self
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

fn upstream_status_error(
    status: StatusCode,
    text: &str,
    retry_after: Option<Duration>,
) -> ProxyError {
    ProxyError::new(status, clean_upstream_error(text)).with_retry_after(retry_after)
}

#[derive(Debug)]
enum UpstreamSendError {
    Request(reqwest::Error),
    HeaderTimeout(u64),
    Cancelled,
}

impl UpstreamSendError {
    fn retryable(&self) -> bool {
        match self {
            Self::Request(err) => err.is_timeout() || err.is_connect() || err.is_request(),
            Self::HeaderTimeout(_) => true,
            Self::Cancelled => false,
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Request(err) => err.to_string(),
            Self::HeaderTimeout(secs) => format!("上游响应头超时: {secs}s"),
            Self::Cancelled => STREAM_ABORTED_MESSAGE.to_string(),
        }
    }

    fn into_proxy_error(self) -> ProxyError {
        match self {
            Self::Cancelled => ProxyError::aborted(),
            other => ProxyError::new(StatusCode::BAD_GATEWAY, other.message()),
        }
    }
}

async fn send_stream_request(
    req: reqwest::RequestBuilder,
    timeout_secs: u64,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, UpstreamSendError> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(UpstreamSendError::Cancelled),
        result = tokio::time::timeout(Duration::from_secs(timeout_secs.max(1)), req.send()) => {
            match result {
                Ok(Ok(resp)) => Ok(resp),
                Ok(Err(err)) => Err(UpstreamSendError::Request(err)),
                Err(_) => Err(UpstreamSendError::HeaderTimeout(timeout_secs.max(1))),
            }
        }
    }
}

async fn send_request(
    req: reqwest::RequestBuilder,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, UpstreamSendError> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(UpstreamSendError::Cancelled),
        result = req.send() => result.map_err(UpstreamSendError::Request),
    }
}

pub async fn run_server(
    config: ConfigHandle,
    shutdown: oneshot::Receiver<()>,
    circuit_status: ProviderCircuitStatusHandle,
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
    let root_cancel = CancellationToken::new();
    let state = AppState {
        config: Arc::clone(&config),
        clients: Arc::new(Mutex::new(HashMap::new())),
        counters: Arc::new(Mutex::new(HashMap::new())),
        api_key_limiters: Arc::new(Mutex::new(HashMap::new())),
        provider_circuits: Arc::new(Mutex::new(HashMap::new())),
        provider_loads: Arc::new(Mutex::new(HashMap::new())),
        provider_statuses: circuit_status,
        session_affinity: Arc::new(Mutex::new(HashMap::new())),
        usage_injection: Arc::new(Mutex::new(HashMap::new())),
        oauth_tokens: Arc::new(Mutex::new(HashMap::new())),
        oauth_refresh_locks: Arc::new(Mutex::new(HashMap::new())),
        root_cancel: root_cancel.clone(),
    };
    let mut app = Router::new()
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
        .route(
            "/v1/messages",
            post(messages).layer(DefaultBodyLimit::max(MAX_ANTHROPIC_REQUEST_BODY_BYTES)),
        )
        .route(
            "/v1/messages/count_tokens",
            post(count_tokens).layer(DefaultBodyLimit::max(MAX_ANTHROPIC_REQUEST_BODY_BYTES)),
        )
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        // 注意顺序：这一层必须在 with_state 之前加，才能覆盖上面所有代理路由；
        // 之后 nest 进来的 /user、/admin 是本地管理页面，不需要请求上下文。
        .layer(axum::middleware::from_fn({
            let root_cancel = state.root_cancel.clone();
            move |req, next| attach_request_context(root_cancel.clone(), req, next)
        }))
        .with_state(state);
    if initial.web.enabled {
        app = app
            .nest("/user", crate::web::router(Arc::clone(&config)))
            .nest("/admin", crate::web::admin_router(config));
    }

    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.await;
            // 先取消在飞请求再让 graceful shutdown 收尾：
            // 流式响应体不会自己结束，不取消的话 axum 会一直等着它们，
            // 桌面端「停止」按钮看起来就像卡住了。
            root_cancel.cancel();
        })
        .await;
    result?;
    Ok(())
}

/// 启动前校验：普通渠道和 Auth 账号在路由时是等价的可用上游，
/// 因此任一侧有启用项即可启动。
pub fn validate_config(config: &AppConfig) -> Result<()> {
    let enabled_providers = config.providers.iter().filter(|p| p.enabled).count();
    let enabled_auth_accounts = config
        .auth_accounts
        .iter()
        .filter(|account| account.enabled)
        .count();
    if enabled_providers == 0 && enabled_auth_accounts == 0 {
        return Err(anyhow!("没有启用的渠道或 Auth 账号"));
    }
    Ok(())
}

#[cfg(test)]
mod validate_config_tests {
    use super::*;
    use crate::config::AuthAccountConfig;

    fn base_config() -> AppConfig {
        let mut cfg = AppConfig::load("config.example.yaml").unwrap();
        cfg.providers.clear();
        cfg.auth_accounts.clear();
        cfg
    }

    fn enabled_account(id: &str) -> AuthAccountConfig {
        serde_json::from_value(json!({"id": id})).unwrap()
    }

    #[test]
    fn validate_config_rejects_config_without_provider_or_auth_account() {
        let cfg = base_config();
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn validate_config_accepts_auth_account_only_config() {
        let mut cfg = base_config();
        cfg.auth_accounts = vec![enabled_account("acct-1")];
        assert!(validate_config(&cfg).is_ok());
    }

    #[test]
    fn validate_config_ignores_disabled_auth_accounts() {
        let mut cfg = base_config();
        let mut account = enabled_account("acct-1");
        account.enabled = false;
        cfg.auth_accounts = vec![account];
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn validate_config_accepts_provider_only_config() {
        let mut cfg = base_config();
        let provider: ProviderConfig = serde_json::from_value(json!({
            "name": "channel-a",
            "base_url": "https://example.test/v1",
            "api_key": "sk-test"
        }))
        .unwrap();
        cfg.providers = vec![provider];
        assert!(validate_config(&cfg).is_ok());
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;

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

    #[test]
    fn session_affinity_remembers_successful_provider() {
        let state = test_state();
        state.remember_affinity_provider("api-key:key-1:gpt-test", "muyuan", "gpt-test");

        assert_eq!(
            state.preferred_provider_for_affinity("api-key:key-1:gpt-test"),
            Some(("muyuan".to_string(), "gpt-test".to_string()))
        );
    }

    #[test]
    fn provider_client_uses_shared_proxy_only_when_enabled_or_auth_account() {
        let state = test_state();
        let mut cfg = (*state.snapshot()).clone();
        cfg.routing.auth_proxy = "http://127.0.0.1:7890".to_string();
        *state.config.write() = Arc::new(cfg);

        let mut provider: ProviderConfig = serde_json::from_value(json!({
            "name": "test",
            "provider_type": "openai",
            "base_url": "https://example.test/v1",
            "api_key": "sk-test"
        }))
        .unwrap();

        state.client_for_provider(&provider);
        assert!(state.clients.lock().contains_key(&(10, String::new())));

        provider.use_proxy = true;
        state.client_for_provider(&provider);
        assert!(state
            .clients
            .lock()
            .contains_key(&(10, "http://127.0.0.1:7890".to_string())));

        provider.use_proxy = false;
        provider.auth_account_id = Some("auth-1".to_string());
        state.client_for_provider(&provider);
        assert!(state
            .clients
            .lock()
            .contains_key(&(10, "http://127.0.0.1:7890".to_string())));
    }

    #[test]
    fn session_affinity_ignores_empty_session_key() {
        let state = test_state();
        state.remember_affinity_provider("", "muyuan", "gpt-test");

        assert_eq!(state.preferred_provider_for_affinity(""), None);
    }

    #[test]
    fn session_affinity_only_forgets_matching_provider_and_model() {
        let state = test_state();
        let key = "api-key:key-1:gpt-test";
        state.remember_affinity_provider(key, "channel-b", "gpt-fallback");

        state.forget_affinity_provider(key, "channel-a", "gpt-fallback");
        assert_eq!(
            state.preferred_provider_for_affinity(key),
            Some(("channel-b".to_string(), "gpt-fallback".to_string()))
        );

        state.forget_affinity_provider(key, "channel-b", "gpt-primary");
        assert_eq!(
            state.preferred_provider_for_affinity(key),
            Some(("channel-b".to_string(), "gpt-fallback".to_string()))
        );

        state.forget_affinity_provider(key, "channel-b", "gpt-fallback");
        assert_eq!(state.preferred_provider_for_affinity(key), None);
    }

    #[test]
    fn provider_circuit_quota_error_text_opens_immediately() {
        let state = test_state();
        state
            .begin_provider_attempt("quota-text-limited")
            .unwrap()
            .mark_failure(
                StatusCode::BAD_REQUEST,
                "insufficient_quota: account quota exceeded",
                None,
            );

        let err = state
            .begin_provider_attempt("quota-text-limited")
            .unwrap_err();
        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(err.message.contains("熔断冷却中"));
    }
}

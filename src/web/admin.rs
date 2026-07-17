use super::{session_ttl, AdminSession, WebError, WebState};
use crate::{config::AppConfig, proxy};
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::HashMap, path::PathBuf, time::Instant};
use uuid::Uuid;

const SESSION_COOKIE: &str = "routehub_admin_session";

#[derive(Deserialize)]
pub(super) struct LoginRequest {
    admin_key: String,
}

#[derive(Clone, Serialize)]
struct AdminProfile {
    name: &'static str,
}

#[derive(Deserialize)]
pub(super) struct DashboardQuery {
    #[serde(default)]
    page: usize,
    #[serde(default = "default_page_size")]
    page_size: usize,
}

#[derive(Default, Serialize)]
struct AdminSummary {
    requests: i64,
    success: i64,
    errors: i64,
    running: i64,
    input_tokens: i64,
    output_tokens: i64,
    avg_latency_ms: i64,
}

#[derive(Default, Clone)]
struct UsageBreakdown {
    requests: i64,
    success: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
    last_seen: String,
    today_tokens: i64,
}

#[derive(Serialize)]
struct ProviderView {
    name: String,
    enabled: bool,
    provider_type: String,
    priority: i32,
    weight: u32,
    model_count: usize,
    requests: i64,
    success: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
    last_seen: String,
    today_tokens: i64,
}

#[derive(Serialize)]
struct ApiKeyView {
    name: String,
    enabled: bool,
    created_at: String,
    max_concurrency: usize,
    daily_token_limit: Option<u64>,
    allowed_models: Vec<String>,
    requests: i64,
    success: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
    last_seen: String,
    today_tokens: i64,
}

#[derive(Serialize)]
struct AdminLogRow {
    id: i64,
    ts: String,
    status: String,
    api_key_name: String,
    api: String,
    channel: String,
    request_model: String,
    upstream_model: String,
    latency_ms: i64,
    first_token_ms: Option<i64>,
    input_tokens: i64,
    output_tokens: i64,
    error: String,
}

struct AdminDashboardData {
    summary: AdminSummary,
    providers: Vec<ProviderView>,
    api_keys: Vec<ApiKeyView>,
    logs: Vec<AdminLogRow>,
    total: usize,
}

pub(super) async fn login(
    State(state): State<WebState>,
    Json(payload): Json<LoginRequest>,
) -> Result<Response, WebError> {
    let cfg = state.config.read().clone();
    let configured_key = configured_admin_key(&cfg)?;
    let submitted_key = payload.admin_key.trim();
    if submitted_key.is_empty() || submitted_key != configured_key {
        return Err(WebError::unauthorized("管理员密钥无效"));
    }

    let token = Uuid::new_v4().to_string();
    let ttl = session_ttl(&cfg);
    state.admin_sessions.lock().insert(
        token.clone(),
        AdminSession {
            credential_id: admin_credential_id(configured_key),
            expires_at: Instant::now() + ttl,
        },
    );

    let mut response = Json(json!({"ok": true, "admin": admin_profile()})).into_response();
    let cookie = format!(
        "{SESSION_COOKIE}={token}; Path=/admin; HttpOnly; SameSite=Strict; Max-Age={}",
        ttl.as_secs()
    );
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|_| WebError::internal("创建管理员会话失败"))?,
    );
    Ok(response)
}

pub(super) async fn logout(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    if let Some(token) = session_token(&headers) {
        state.admin_sessions.lock().remove(token);
    }
    let mut response = Json(json!({"ok": true})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "routehub_admin_session=; Path=/admin; HttpOnly; SameSite=Strict; Max-Age=0",
        ),
    );
    Ok(response)
}

pub(super) async fn session(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, WebError> {
    authorized_admin(&state, &headers)?;
    Ok(Json(json!({"ok": true, "admin": admin_profile()})))
}

pub(super) async fn dashboard(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(query): Query<DashboardQuery>,
) -> Result<Json<serde_json::Value>, WebError> {
    authorized_admin(&state, &headers)?;
    let cfg = state.config.read().clone();
    let path = cfg.usage_log_sqlite_path();
    let page = query.page;
    let page_size = query.page_size.clamp(10, 100);
    let data =
        tokio::task::spawn_blocking(move || read_admin_dashboard(&cfg, path, page, page_size))
            .await
            .map_err(|err| WebError::internal(format!("读取管理数据任务失败: {err}")))?
            .map_err(WebError::internal)?;

    Ok(Json(json!({
        "ok": true,
        "admin": admin_profile(),
        "summary": data.summary,
        "providers": data.providers,
        "api_keys": data.api_keys,
        "logs": data.logs,
        "total": data.total,
        "page": page,
        "page_size": page_size,
    })))
}

fn authorized_admin(state: &WebState, headers: &HeaderMap) -> Result<(), WebError> {
    let token =
        session_token(headers).ok_or_else(|| WebError::unauthorized("请先登录管理控制台"))?;
    let credential_id = {
        let mut sessions = state.admin_sessions.lock();
        sessions.retain(|_, session| session.expires_at > Instant::now());
        sessions
            .get(token)
            .map(|session| session.credential_id.clone())
            .ok_or_else(|| WebError::unauthorized("管理员会话已过期"))?
    };
    let cfg = state.config.read().clone();
    let configured_key = configured_admin_key(&cfg)?;
    if admin_credential_id(configured_key) != credential_id {
        state.admin_sessions.lock().remove(token);
        return Err(WebError::unauthorized("管理员密钥已变更，请重新登录"));
    }
    Ok(())
}

fn configured_admin_key(config: &AppConfig) -> Result<&str, WebError> {
    config
        .auth
        .admin_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| WebError::unauthorized("尚未配置 auth.admin_key"))
}

fn admin_credential_id(key: &str) -> String {
    proxy::api_key_id(&format!("routehub-admin:{key}"))
}

fn admin_profile() -> AdminProfile {
    AdminProfile { name: "管理员" }
}

fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|cookie| cookie.strip_prefix(&format!("{SESSION_COOKIE}=")))
        .filter(|value| !value.is_empty())
}

fn read_admin_dashboard(
    config: &AppConfig,
    path: PathBuf,
    page: usize,
    page_size: usize,
) -> Result<AdminDashboardData, String> {
    let mut summary = AdminSummary::default();
    let mut provider_usage = HashMap::new();
    let mut api_key_usage = HashMap::new();
    let mut logs = Vec::new();
    let mut total = 0;

    if path.exists() {
        let conn = proxy::open_usage_log_connection(path)
            .map_err(|err| format!("打开日志数据库失败: {err}"))?;
        proxy::ensure_usage_log_schema(&conn)
            .map_err(|err| format!("初始化日志数据库失败: {err}"))?;
        summary = read_summary(&conn)?;
        provider_usage = read_usage_breakdown(&conn, "channel", "channel != ''")?;
        api_key_usage = read_usage_breakdown(&conn, "api_key_id", "api_key_id != ''")?;
        total = summary.requests.max(0) as usize;
        logs = read_logs(&conn, page, page_size)?;
    }

    let mut providers = config
        .providers
        .iter()
        .map(|provider| {
            let usage = provider_usage.remove(&provider.name).unwrap_or_default();
            ProviderView {
                name: provider.name.clone(),
                enabled: provider.enabled,
                provider_type: provider.provider_type.clone(),
                priority: provider.priority,
                weight: provider.weight,
                model_count: provider.models.len(),
                requests: usage.requests,
                success: usage.success,
                errors: usage.errors,
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                last_seen: usage.last_seen,
                today_tokens: usage.today_tokens,
            }
        })
        .collect::<Vec<_>>();
    providers.sort_by(|a, b| {
        b.enabled
            .cmp(&a.enabled)
            .then(a.priority.cmp(&b.priority))
            .then(b.weight.cmp(&a.weight))
            .then(a.name.cmp(&b.name))
    });

    let mut api_keys = config
        .auth
        .api_keys
        .iter()
        .map(|key| {
            let id = proxy::api_key_id(&key.key);
            let usage = api_key_usage.remove(&id).unwrap_or_default();
            ApiKeyView {
                name: key.name.clone(),
                enabled: key.enabled,
                created_at: key.created_at.clone(),
                max_concurrency: key
                    .max_concurrency
                    .or(config.auth.max_concurrency_per_key)
                    .unwrap_or(5),
                daily_token_limit: key.daily_token_limit,
                allowed_models: key.allowed_models.clone(),
                requests: usage.requests,
                success: usage.success,
                errors: usage.errors,
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                last_seen: usage.last_seen,
                today_tokens: usage.today_tokens,
            }
        })
        .collect::<Vec<_>>();
    api_keys.sort_by(|a, b| b.enabled.cmp(&a.enabled).then(a.name.cmp(&b.name)));

    Ok(AdminDashboardData {
        summary,
        providers,
        api_keys,
        logs,
        total,
    })
}

fn read_summary(conn: &rusqlite::Connection) -> Result<AdminSummary, String> {
    conn.query_row(
        r#"
        SELECT
            COUNT(*),
            COALESCE(SUM(CASE WHEN status IN ('ok', 'stream_started') THEN 1 ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN status NOT IN ('ok', 'stream_started', 'running', 'raw', '-') THEN 1 ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN status = 'running' THEN 1 ELSE 0 END), 0),
            COALESCE(SUM(input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COALESCE(AVG(NULLIF(latency_ms, 0)), 0)
        FROM usage_logs
        "#,
        [],
        |row| {
            Ok(AdminSummary {
                requests: row.get(0)?,
                success: row.get(1)?,
                errors: row.get(2)?,
                running: row.get(3)?,
                input_tokens: row.get(4)?,
                output_tokens: row.get(5)?,
                avg_latency_ms: row.get::<_, f64>(6)?.round() as i64,
            })
        },
    )
    .map_err(|err| format!("读取管理汇总失败: {err}"))
}

fn read_usage_breakdown(
    conn: &rusqlite::Connection,
    column: &str,
    condition: &str,
) -> Result<HashMap<String, UsageBreakdown>, String> {
    let sql = format!(
        r#"
        SELECT
            {column},
            COUNT(*),
            COALESCE(SUM(CASE WHEN status IN ('ok', 'stream_started') THEN 1 ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN status NOT IN ('ok', 'stream_started', 'running', 'raw', '-') THEN 1 ELSE 0 END), 0),
            COALESCE(SUM(input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COALESCE(MAX(ts), ''),
            COALESCE(SUM(CASE WHEN ts >= ?1 THEN input_tokens + output_tokens ELSE 0 END), 0)
        FROM usage_logs
        WHERE {condition}
        GROUP BY {column}
        "#
    );
    let mut statement = conn
        .prepare(&sql)
        .map_err(|err| format!("准备管理统计失败: {err}"))?;
    let rows = statement
        .query_map([today_start()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                UsageBreakdown {
                    requests: row.get(1)?,
                    success: row.get(2)?,
                    errors: row.get(3)?,
                    input_tokens: row.get(4)?,
                    output_tokens: row.get(5)?,
                    last_seen: row.get(6)?,
                    today_tokens: row.get(7)?,
                },
            ))
        })
        .map_err(|err| format!("查询管理统计失败: {err}"))?;
    rows.collect::<Result<HashMap<_, _>, _>>()
        .map_err(|err| format!("解析管理统计失败: {err}"))
}

fn today_start() -> String {
    chrono::Local::now().format("%Y-%m-%d 00:00:00").to_string()
}

fn read_logs(
    conn: &rusqlite::Connection,
    page: usize,
    page_size: usize,
) -> Result<Vec<AdminLogRow>, String> {
    let mut statement = conn
        .prepare(
            r#"
            SELECT
                id, ts, status, api_key_name, api, channel, request_model, upstream_model,
                latency_ms, first_token_ms, input_tokens, output_tokens, error
            FROM usage_logs
            ORDER BY id DESC
            LIMIT ?1 OFFSET ?2
            "#,
        )
        .map_err(|err| format!("准备管理日志查询失败: {err}"))?;
    let rows = statement
        .query_map(
            params![page_size as i64, page.saturating_mul(page_size) as i64],
            |row| {
                Ok(AdminLogRow {
                    id: row.get(0)?,
                    ts: row.get(1)?,
                    status: row.get(2)?,
                    api_key_name: row.get(3)?,
                    api: row.get(4)?,
                    channel: row.get(5)?,
                    request_model: row.get(6)?,
                    upstream_model: row.get(7)?,
                    latency_ms: row.get(8)?,
                    first_token_ms: row.get(9)?,
                    input_tokens: row.get(10)?,
                    output_tokens: row.get(11)?,
                    error: row.get(12)?,
                })
            },
        )
        .map_err(|err| format!("查询管理日志失败: {err}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("解析管理日志失败: {err}"))
}

fn default_page_size() -> usize {
    20
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn admin_dashboard_does_not_expose_raw_credentials() {
        let config: AppConfig = serde_yaml::from_str(
            r#"
auth:
  enabled: true
  admin_key: admin-secret
  api_keys:
    - key: sk-user-secret
      name: tester
providers:
  - name: provider-a
    base_url: https://example.test/v1
    api_key: upstream-secret
    models: [gpt-test]
"#,
        )
        .unwrap();
        let path = std::env::temp_dir().join(format!(
            "routehub-admin-web-{}.sqlite3",
            Uuid::new_v4().simple()
        ));
        let conn = Connection::open(&path).unwrap();
        proxy::ensure_usage_log_schema(&conn).unwrap();
        conn.execute(
            r#"
            INSERT INTO usage_logs (
                ts, api, status, channel, request_model, upstream_model,
                latency_ms, input_tokens, output_tokens, api_key_id, api_key_name
            )
            VALUES ('2026-07-17 12:00:00', 'responses', 'ok', 'provider-a',
                    'gpt-test', 'gpt-test', 100, 10, 5, ?1, 'tester')
            "#,
            params![proxy::api_key_id("sk-user-secret")],
        )
        .unwrap();
        drop(conn);

        let data = read_admin_dashboard(&config, path.clone(), 0, 20).unwrap();
        assert_eq!(data.summary.requests, 1);
        assert_eq!(data.providers[0].requests, 1);
        assert_eq!(data.api_keys[0].requests, 1);
        let encoded = serde_json::to_string(&json!({
            "providers": data.providers,
            "api_keys": data.api_keys,
        }))
        .unwrap();
        assert!(!encoded.contains("admin-secret"));
        assert!(!encoded.contains("sk-user-secret"));
        assert!(!encoded.contains("upstream-secret"));

        let _ = std::fs::remove_file(path);
    }
}

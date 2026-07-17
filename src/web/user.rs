use super::{auth, WebError, WebState};
use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Deserialize)]
pub(super) struct DashboardQuery {
    #[serde(default)]
    page: usize,
    #[serde(default = "default_page_size")]
    page_size: usize,
}

#[derive(Default, Serialize)]
struct UserSummary {
    requests: i64,
    success: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
}

#[derive(Serialize)]
struct UserLogRow {
    id: i64,
    ts: String,
    status: String,
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

pub(super) async fn dashboard(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(query): Query<DashboardQuery>,
) -> Result<Json<serde_json::Value>, WebError> {
    let user = auth::authorized_user(&state, &headers)?;
    let path = state.config.read().usage_log_sqlite_path();
    let page = query.page;
    let page_size = query.page_size.clamp(10, 100);
    let api_key_id = user.api_key_id.clone();
    let data =
        tokio::task::spawn_blocking(move || read_dashboard(path, &api_key_id, page, page_size))
            .await
            .map_err(|err| WebError::internal(format!("读取日志任务失败: {err}")))?
            .map_err(WebError::internal)?;

    Ok(Json(json!({
        "ok": true,
        "user": user,
        "summary": data.0,
        "logs": data.1,
        "total": data.2,
        "page": page,
        "page_size": page_size,
    })))
}

fn read_dashboard(
    path: std::path::PathBuf,
    api_key_id: &str,
    page: usize,
    page_size: usize,
) -> Result<(UserSummary, Vec<UserLogRow>, usize), String> {
    if !path.exists() {
        return Ok((UserSummary::default(), Vec::new(), 0));
    }
    let conn = crate::proxy::open_usage_log_connection(path)
        .map_err(|err| format!("打开日志数据库失败: {err}"))?;
    crate::proxy::ensure_usage_log_schema(&conn)
        .map_err(|err| format!("初始化日志数据库失败: {err}"))?;
    let summary = conn
        .query_row(
            r#"
            SELECT
                COUNT(*),
                COALESCE(SUM(CASE WHEN status = 'ok' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'error' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0)
            FROM usage_logs
            WHERE api_key_id = ?1
            "#,
            params![api_key_id],
            |row| {
                Ok(UserSummary {
                    requests: row.get(0)?,
                    success: row.get(1)?,
                    errors: row.get(2)?,
                    input_tokens: row.get(3)?,
                    output_tokens: row.get(4)?,
                })
            },
        )
        .map_err(|err| format!("读取日志汇总失败: {err}"))?;
    let total = summary.requests.max(0) as usize;
    let mut statement = conn
        .prepare(
            r#"
            SELECT
                id, ts, status, api, channel, request_model, upstream_model,
                latency_ms, first_token_ms, input_tokens, output_tokens, error
            FROM usage_logs
            WHERE api_key_id = ?1
            ORDER BY id DESC
            LIMIT ?2 OFFSET ?3
            "#,
        )
        .map_err(|err| format!("准备日志查询失败: {err}"))?;
    let rows = statement
        .query_map(
            params![
                api_key_id,
                page_size as i64,
                page.saturating_mul(page_size) as i64
            ],
            |row| {
                Ok(UserLogRow {
                    id: row.get(0)?,
                    ts: row.get(1)?,
                    status: row.get(2)?,
                    api: row.get(3)?,
                    channel: row.get(4)?,
                    request_model: row.get(5)?,
                    upstream_model: row.get(6)?,
                    latency_ms: row.get(7)?,
                    first_token_ms: row.get(8)?,
                    input_tokens: row.get(9)?,
                    output_tokens: row.get(10)?,
                    error: row.get(11)?,
                })
            },
        )
        .map_err(|err| format!("查询日志失败: {err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("解析日志失败: {err}"))?;
    Ok((summary, rows, total))
}

fn default_page_size() -> usize {
    20
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use uuid::Uuid;

    #[test]
    fn dashboard_only_returns_logs_for_the_authenticated_key() {
        let path = std::env::temp_dir().join(format!(
            "routehub-user-web-{}.sqlite3",
            Uuid::new_v4().simple()
        ));
        let conn = Connection::open(&path).unwrap();
        crate::proxy::ensure_usage_log_schema(&conn).unwrap();
        for (key_id, channel) in [("key-a", "provider-a"), ("key-b", "provider-b")] {
            conn.execute(
                r#"
                INSERT INTO usage_logs (
                    ts, api, status, channel, request_model, upstream_model,
                    latency_ms, input_tokens, output_tokens, api_key_id, api_key_name
                )
                VALUES (?1, 'responses', 'ok', ?2, 'gpt-test', 'gpt-test', 100, 10, 5, ?3, 'user')
                "#,
                params!["2026-07-17 12:00:00", channel, key_id],
            )
            .unwrap();
        }
        drop(conn);

        let (summary, rows, total) = read_dashboard(path.clone(), "key-a", 0, 20).unwrap();
        assert_eq!(summary.requests, 1);
        assert_eq!(total, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].channel, "provider-a");

        let _ = std::fs::remove_file(path);
    }
}

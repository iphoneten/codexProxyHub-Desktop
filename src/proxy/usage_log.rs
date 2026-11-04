use super::*;
use sha2::{Digest, Sha256};

/// 合并键只存哈希，不把上游错误文本或内部请求标识带入运行日志。
pub(super) fn failure_group_key(
    scope: &str,
    api: &str,
    provider: &str,
    model: &str,
    upstream_model: &str,
    code: u16,
    error: &str,
) -> String {
    let mut digest = Sha256::new();
    let code = code.to_string();
    for value in [scope, api, provider, model, upstream_model, code.as_str(), error] {
        digest.update((value.len() as u64).to_le_bytes());
        digest.update(value.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

pub(super) struct UsageLogEvent<'a> {
    pub(super) api: &'a str,
    pub(super) api_key_id: &'a str,
    pub(super) api_key_name: &'a str,
    pub(super) provider: &'a str,
    pub(super) model: &'a str,
    pub(super) upstream_model: &'a str,
    pub(super) status: &'a str,
    pub(super) error: Option<&'a str>,
    pub(super) usage: TokenUsage,
    pub(super) first_token_ms: Option<i64>,
    pub(super) token_source: Option<&'a str>,
    /// 与响应头 `x-request-id` 同值，用来把一行日志和客户端侧的一次请求对上。
    pub(super) request_id: &'a str,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn log_success(
    config: &AppConfig,
    api: &str,
    provider: &str,
    model: &str,
    upstream_model: &str,
    usage: TokenUsage,
    started: Instant,
    first_token_ms: Option<i64>,
    stream: bool,
    api_key_id: &str,
    api_key_name: &str,
    request_id: &str,
) {
    log_usage(
        config,
        started,
        UsageLogEvent {
            api,
            api_key_id,
            api_key_name,
            provider,
            model,
            upstream_model,
            status: if stream { "stream_started" } else { "ok" },
            error: None,
            usage,
            first_token_ms,
            token_source: None,
            request_id,
        },
    );
}

pub(super) fn log_error(
    config: &AppConfig,
    api: &str,
    provider: &str,
    model: &str,
    started: Instant,
    first_token_ms: Option<i64>,
    error: &str,
    api_key_id: &str,
    api_key_name: &str,
    request_id: &str,
) {
    let status = if error == STREAM_ABORTED_MESSAGE {
        STREAM_ABORTED_STATUS
    } else {
        "error"
    };
    log_usage(
        config,
        started,
        UsageLogEvent {
            api,
            api_key_id,
            api_key_name,
            provider,
            model,
            upstream_model: "",
            status,
            error: Some(error),
            usage: TokenUsage::default(),
            first_token_ms,
            token_source: None,
            request_id,
        },
    );
}

pub(super) fn log_stream_started(
    config: &AppConfig,
    api: &str,
    provider: &str,
    model: &str,
    upstream_model: &str,
    started: Instant,
    api_key_id: &str,
    api_key_name: &str,
    request_id: &str,
) -> Option<i64> {
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let log_provider = log_provider_name(config, provider);
    log_usage_sqlite_for_key(
        config.usage_log_sqlite_path(),
        &ts,
        api,
        api_key_id,
        api_key_name,
        "running",
        &log_provider,
        model,
        upstream_model,
        started.elapsed().as_millis() as i64,
        None,
        "",
        0,
        0,
        "upstream_or_unknown",
        request_id,
    )
    .ok()
}

#[cfg(test)]
pub(super) fn start_stream_attempt_log(
    config: &AppConfig,
    stream: bool,
    api: &str,
    provider: &str,
    model: &str,
    upstream_model: &str,
    started: Instant,
    api_key_name: &str,
) -> Option<i64> {
    start_stream_attempt_log_for_key(
        config,
        stream,
        api,
        provider,
        model,
        upstream_model,
        started,
        "",
        api_key_name,
        "",
    )
}

pub(super) fn start_stream_attempt_log_for_key(
    config: &AppConfig,
    stream: bool,
    api: &str,
    provider: &str,
    model: &str,
    upstream_model: &str,
    started: Instant,
    api_key_id: &str,
    api_key_name: &str,
    request_id: &str,
) -> Option<i64> {
    stream.then(|| {
        log_stream_started(
            config,
            api,
            provider,
            model,
            upstream_model,
            started,
            api_key_id,
            api_key_name,
            request_id,
        )
    })?
}

/// 首个上游输出前，handler future 可能被取消分支或客户端断开直接丢弃。
/// 正常交付/失败时 take() 把日志交给已有收尾流程；未交付的尝试在 Drop 中记为取消。
pub(super) struct StreamAttemptLogGuard {
    config: Arc<AppConfig>,
    id: Option<i64>,
    api: &'static str,
    provider: String,
    model: String,
    started: Instant,
    api_key_id: String,
    api_key_name: String,
    request_id: String,
    log_scope: String,
    upstream_model: String,
}

impl StreamAttemptLogGuard {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        config: &Arc<AppConfig>,
        stream: bool,
        api: &'static str,
        provider: &str,
        model: &str,
        upstream_model: &str,
        started: Instant,
        api_key_id: &str,
        api_key_name: &str,
        ctx: &RequestContext,
    ) -> Self {
        Self {
            config: Arc::clone(config),
            id: start_stream_attempt_log_for_key(
                config, stream, api, provider, model, upstream_model, started,
                api_key_id, api_key_name, ctx.request_id(),
            ),
            api,
            provider: provider.to_string(),
            model: model.to_string(),
            started,
            api_key_id: api_key_id.to_string(),
            api_key_name: api_key_name.to_string(),
            request_id: ctx.request_id().to_string(),
            log_scope: ctx.log_scope().to_string(),
            upstream_model: upstream_model.to_string(),
        }
    }

    pub(super) fn take(&mut self) -> Option<i64> {
        self.id.take()
    }

    pub(super) fn finish_error(&mut self, status: StatusCode, error: &str) {
        let group = if status.as_u16() != 499 && error != STREAM_ABORTED_MESSAGE {
            failure_group_key(&self.log_scope, self.api, &self.provider, &self.model,
                &self.upstream_model, status.as_u16(), error)
        } else {
            String::new()
        };
        finish_failed_attempt_log_in_group(
            &self.config, self.id.take(), self.api, &self.provider, &self.model,
            self.started, error, &self.api_key_id, &self.api_key_name, &self.request_id, &group,
        );
    }
}

impl Drop for StreamAttemptLogGuard {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else { return; };
        let config = Arc::clone(&self.config);
        let api = self.api;
        let provider = std::mem::take(&mut self.provider);
        let model = std::mem::take(&mut self.model);
        let started = self.started;
        let api_key_id = std::mem::take(&mut self.api_key_id);
        let api_key_name = std::mem::take(&mut self.api_key_name);
        let request_id = std::mem::take(&mut self.request_id);
        let finish = move || finish_failed_attempt_log_for_key(
            &config, Some(id), api, &provider, &model, started, STREAM_ABORTED_MESSAGE,
            &api_key_id, &api_key_name, &request_id,
        );
        // SQLite 的锁等待不能拖延网络取消或占用异步工作线程。
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(finish);
        } else {
            finish();
        }
    }
}

#[cfg(test)]
pub(super) fn finish_failed_attempt_log(
    config: &AppConfig,
    log_id: Option<i64>,
    api: &str,
    provider: &str,
    model: &str,
    started: Instant,
    error: &str,
    api_key_name: &str,
) {
    finish_failed_attempt_log_for_key(
        config,
        log_id,
        api,
        provider,
        model,
        started,
        error,
        "",
        api_key_name,
        "",
    );
}

pub(super) fn finish_failed_attempt_log_for_key(
    config: &AppConfig,
    log_id: Option<i64>,
    api: &str,
    provider: &str,
    model: &str,
    started: Instant,
    error: &str,
    api_key_id: &str,
    api_key_name: &str,
    request_id: &str,
) {
    finish_failed_attempt_log_in_group(config, log_id, api, provider, model, started,
        error, api_key_id, api_key_name, request_id, "");
}

#[allow(clippy::too_many_arguments)]
fn finish_failed_attempt_log_in_group(
    config: &AppConfig,
    log_id: Option<i64>,
    api: &str,
    provider: &str,
    model: &str,
    started: Instant,
    error: &str,
    api_key_id: &str,
    api_key_name: &str,
    request_id: &str,
    group: &str,
) {
    let Some(id) = log_id else {
        return;
    };
    let status = if error == STREAM_ABORTED_MESSAGE {
        STREAM_ABORTED_STATUS
    } else {
        "error"
    };
    if update_usage_log_sqlite_in_group(
        config.usage_log_sqlite_path(),
        id,
        status, "", started.elapsed().as_millis() as i64, None,
        error, 0, 0, "upstream_or_unknown", group,
    )
    .is_err()
    {
        log_error(
            config,
            api,
            provider,
            model,
            started,
            None,
            error,
            api_key_id,
            api_key_name,
            request_id,
        );
    }
}

pub(super) fn update_usage_log(
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

pub(super) async fn finalize_stream_log(
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

pub(super) fn sqlite_lock_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

pub(super) fn log_usage(config: &AppConfig, started: Instant, event: UsageLogEvent<'_>) {
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let latency_ms = started.elapsed().as_millis() as i64;
    let error = event.error.unwrap_or("");
    let token_source = event.token_source.unwrap_or("upstream_or_unknown");
    let log_provider = log_provider_name(config, event.provider);

    let _ = log_usage_sqlite_for_key(
        config.usage_log_sqlite_path(),
        &ts,
        event.api,
        event.api_key_id,
        event.api_key_name,
        event.status,
        &log_provider,
        event.model,
        event.upstream_model,
        latency_ms,
        event.first_token_ms,
        error,
        event.usage.input,
        event.usage.output,
        token_source,
        event.request_id,
    );
}

fn log_provider_name(config: &AppConfig, provider: &str) -> String {
    let Some(account_id) = provider.strip_prefix("auth:") else {
        return provider.to_string();
    };
    let Some(account) = config
        .auth_accounts
        .iter()
        .find(|account| account.id == account_id)
    else {
        return provider.to_string();
    };
    let name = account.name.trim();
    if name.is_empty() {
        provider.to_string()
    } else {
        format!("auth:{name}")
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(super) fn log_usage_sqlite(
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
    log_usage_sqlite_for_key(
        path,
        ts,
        api,
        "",
        api_key_name,
        status,
        provider,
        model,
        upstream_model,
        latency_ms,
        first_token_ms,
        error,
        input_tokens,
        output_tokens,
        token_source,
        "",
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn log_usage_sqlite_for_key(
    path: PathBuf,
    ts: &str,
    api: &str,
    api_key_id: &str,
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
    request_id: &str,
) -> rusqlite::Result<i64> {
    let conn = open_usage_log_connection(path)?;
    ensure_usage_log_schema(&conn)?;
    conn.execute(
        r#"
        INSERT INTO usage_logs
            (
                ts, api, status, channel, request_model, upstream_model,
                latency_ms, first_token_ms, input_tokens, output_tokens, error, token_source,
                api_key_id, api_key_name, request_id
            )
        VALUES
            (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
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
            api_key_id,
            api_key_name,
            request_id
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn update_usage_log_sqlite(
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
    update_usage_log_sqlite_in_group(path, id, status, upstream_model, latency_ms,
        first_token_ms, error, input_tokens, output_tokens, token_source, "")
}

#[allow(clippy::too_many_arguments)]
fn update_usage_log_sqlite_in_group(
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
    group: &str,
) -> rusqlite::Result<usize> {
    let mut conn = open_usage_log_connection(path)?;
    ensure_usage_log_schema(&conn)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let last_ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    let updated = tx.execute(
        r#"
        UPDATE usage_logs
        SET status = ?1, upstream_model = ?2, latency_ms = ?3, first_token_ms = ?4,
            input_tokens = ?5, output_tokens = ?6, error = ?7, token_source = ?8,
            last_ts = ?10, failure_group = ?11,
            latency_samples = CASE WHEN ?3 > 0 THEN 1 ELSE 0 END
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
            id,
            last_ts,
            group,
        ],
    )?;
    // 只合并本请求新生成、无业务输出/用量的重复失败；成功、取消与旧记录不参与。
    if updated > 0 && status == "error" && !group.is_empty()
        && first_token_ms.is_none() && input_tokens == 0 && output_tokens == 0
    {
        use rusqlite::OptionalExtension;
        let current: (String, i64, i64, i64) = tx.query_row(
            "SELECT ts, repeat_count, latency_ms, latency_samples FROM usage_logs WHERE id = ?1",
            [id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let previous: Option<(i64, i64, i64, i64)> = tx.query_row(
            "SELECT id, repeat_count, latency_ms, latency_samples FROM usage_logs
             WHERE failure_group = ?1 AND failure_group != '' AND status = 'error' AND id != ?2
               AND first_token_ms IS NULL AND input_tokens = 0 AND output_tokens = 0
               AND substr(ts, 1, 10) = substr(?3, 1, 10)
             ORDER BY id LIMIT 1",
            params![group, id, current.0],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).optional()?;
        if let Some((target, count, elapsed, samples)) = previous {
            tx.execute(
                "UPDATE usage_logs SET repeat_count = ?1, latency_ms = ?2, latency_samples = ?3,
                     last_ts = ?4 WHERE id = ?5",
                params![count.max(1).saturating_add(current.1.max(1)),
                    elapsed.saturating_add(current.2), samples.max(0).saturating_add(current.3.max(0)),
                    last_ts, target],
            )?;
            // 当前行刚由本次尝试创建，只有聚合写入成功后才折叠；事务失败保留两行。
            tx.execute("DELETE FROM usage_logs WHERE id = ?1", [id])?;
        }
    }
    tx.commit()?;
    Ok(updated)
}

pub(super) fn read_api_key_today_tokens(path: PathBuf, api_key_id: &str) -> rusqlite::Result<i64> {
    if api_key_id.trim().is_empty() {
        return Ok(0);
    }
    let conn = open_usage_log_connection(path)?;
    ensure_usage_log_schema(&conn)?;
    let day_start = chrono::Local::now().format("%Y-%m-%d 00:00:00").to_string();
    conn.query_row(
        r#"
        SELECT COALESCE(SUM(input_tokens + output_tokens), 0)
        FROM usage_logs
        WHERE api_key_id = ?1 AND ts >= ?2
        "#,
        params![api_key_id, day_start],
        |row| row.get(0),
    )
}

/// 请求日志的公共筛选条件，读取（计数 / 分页 / 汇总）和清空共用同一份，
/// 保证「删的就是筛选出来的」。绑定参数顺序固定为：
/// `?1` 起始时间、`?2` api_key_id、`?3` api_key_name、`?4` 状态标记，
/// 四个都允许传 `NULL` 表示该维度不过滤。
///
/// 状态标记用的是界面「状态」列的口径而不是数据库原始值：`success` 覆盖
/// `ok` / `stream_started`，`running` 只匹配运行中，`aborted` 是客户端提前
/// 断开导致的取消，`failed` 是其余一切（`error` 以及任何非预期值），
/// 这样不会有行从所有筛选里漏掉。
pub(crate) const USAGE_LOG_FILTER_WHERE: &str = r#"
    (?1 IS NULL OR ts >= ?1)
      AND (
          ?2 IS NULL
          OR api_key_id = ?2
          OR (api_key_id = '' AND ?3 IS NOT NULL AND api_key_name = ?3)
      )
      AND (
          ?4 IS NULL
          OR (?4 = 'success' AND status IN ('ok', 'stream_started'))
          OR (?4 = 'running' AND status = 'running')
          OR (?4 = 'aborted' AND status = 'aborted')
          OR (?4 = 'failed' AND status NOT IN ('ok', 'stream_started', 'running', 'aborted'))
      )
"#;

/// 按筛选条件删除请求日志，返回实际删除的行数。参数含义与
/// `USAGE_LOG_FILTER_WHERE` 一致，全部传 `None` 即清空整表。
///
/// `status = 'running'` 的行一律保留：流式请求的日志是先插入再按 id 回填的
/// （见 `update_usage_log_sqlite`），删掉正在进行的行会让这次请求完成后
/// 永久丢失记录。因此状态筛选选中「运行中」时这里会删不到任何行。
pub(super) fn delete_usage_logs(
    path: PathBuf,
    range_start: Option<&str>,
    api_key_id: Option<&str>,
    api_key_name: Option<&str>,
    status_kind: Option<&str>,
) -> rusqlite::Result<usize> {
    let conn = open_usage_log_connection(path)?;
    ensure_usage_log_schema(&conn)?;
    let removed = conn.execute(
        &format!("DELETE FROM usage_logs WHERE status != 'running' AND {USAGE_LOG_FILTER_WHERE}"),
        params![range_start, api_key_id, api_key_name, status_kind],
    )?;
    // 只在全量清空后回收磁盘空间：SQLite 删行只会把页标记为可复用，文件不会变小。
    // WAL 模式下 VACUUM 需要独占访问，代理正在写日志时会失败，忽略即可。
    if removed > 0 && range_start.is_none() && api_key_id.is_none() && status_kind.is_none() {
        let _ = conn.execute_batch("VACUUM");
    }
    Ok(removed)
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

pub(super) fn recover_interrupted_usage_logs(path: PathBuf) {
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

pub(super) fn recover_stale_running_usage_logs(path: PathBuf, max_age: Duration) {
    let Ok(conn) = open_usage_log_connection(path) else {
        return;
    };
    if ensure_usage_log_schema(&conn).is_err() {
        return;
    }
    let cutoff = chrono::Local::now()
        .checked_sub_signed(
            chrono::Duration::from_std(max_age).unwrap_or_else(|_| chrono::Duration::minutes(30)),
        )
        .unwrap_or_else(chrono::Local::now)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let _ = conn.execute(
        r#"
        UPDATE usage_logs
        SET status = 'error', error = '请求超过最大运行时长仍未完成，已自动结束运行中状态'
        WHERE status = 'running' AND ts <= ?1
        "#,
        params![cutoff],
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
            api_key_id TEXT NOT NULL DEFAULT '',
            api_key_name TEXT NOT NULL DEFAULT '',
            request_id TEXT NOT NULL DEFAULT ''
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
    ensure_column(conn, "api_key_id", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "api_key_name", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "request_id", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "repeat_count", "INTEGER NOT NULL DEFAULT 1")?;
    ensure_column(conn, "last_ts", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "failure_group", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "latency_samples", "INTEGER NOT NULL DEFAULT 1")?;
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_usage_logs_failure_group ON usage_logs(failure_group) WHERE failure_group != '';")?;
    Ok(())
}

pub(super) fn ensure_column(
    conn: &Connection,
    name: &str,
    definition: &str,
) -> rusqlite::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    fn merge_config() -> Arc<AppConfig> {
        let path = std::env::temp_dir().join(format!("routehub-merged-failures-{}.sqlite3", Uuid::new_v4()));
        Arc::new(serde_json::from_value(json!({
            "usage_log": {"backend":"sqlite", "sqlite_path":path.to_string_lossy()},
            "providers": []
        })).unwrap())
    }

    fn failed_attempt(cfg: &Arc<AppConfig>, ctx: &RequestContext, provider: &str, code: u16, error: &str) {
        let mut guard = StreamAttemptLogGuard::new(cfg, true, "responses", provider, "client-model",
            "upstream-model", Instant::now(), "synthetic-key", "key", ctx);
        guard.finish_error(StatusCode::from_u16(code).unwrap(), error);
    }

    #[test]
    fn repeated_failures_merge_counts_and_leave_success_independent() {
        let cfg = merge_config();
        let ctx = RequestContext::detached();
        for _ in 0..5 { failed_attempt(&cfg, &ctx, "A", 502, "same error"); }
        failed_attempt(&cfg, &ctx, "B", 502, "same error");
        let mut success = StreamAttemptLogGuard::new(&cfg, true, "responses", "A", "client-model",
            "upstream-model", Instant::now(), "synthetic-key", "key", &ctx);
        let success_id = success.take().unwrap();
        update_usage_log_sqlite(cfg.usage_log_sqlite_path(), success_id, "ok", "upstream-model",
            100, Some(10), "", 7, 3, "upstream").unwrap();
        let conn = open_usage_log_connection(cfg.usage_log_sqlite_path()).unwrap();
        let totals: (i64, i64, i64) = conn.query_row(
            "SELECT COUNT(*), SUM(repeat_count), SUM(input_tokens + output_tokens) FROM usage_logs", [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!(totals, (3, 7, 10));
        let count: i64 = conn.query_row("SELECT repeat_count FROM usage_logs WHERE channel='A' AND status='error'", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 5);
        let last: String = conn.query_row("SELECT last_ts FROM usage_logs WHERE channel='A' AND status='error'", [], |row| row.get(0)).unwrap();
        assert!(!last.is_empty());
    }

    #[test]
    fn merging_uses_actual_request_scope_and_error_identity() {
        let cfg = merge_config();
        let root = CancellationToken::new();
        let first = RequestContext::child_of(&root, Some("same-client-request-id".into()));
        let second = RequestContext::child_of(&root, Some("same-client-request-id".into()));
        assert_eq!(first.request_id(), second.request_id());
        assert_ne!(first.log_scope(), second.log_scope());
        failed_attempt(&cfg, &first, "A", 502, "same error");
        failed_attempt(&cfg, &first.clone(), "A", 502, "same error");
        failed_attempt(&cfg, &second, "A", 502, "same error");
        failed_attempt(&cfg, &first, "A", 503, "same error");
        failed_attempt(&cfg, &first, "A", 502, "changed error");
        failed_attempt(&cfg, &first, "auth:account-a", 502, "same error");
        failed_attempt(&cfg, &first, "auth:account-b", 502, "same error");
        let mut model_change = StreamAttemptLogGuard::new(&cfg, true, "responses", "A", "client-model",
            "another-upstream-model", Instant::now(), "synthetic-key", "key", &first);
        model_change.finish_error(StatusCode::BAD_GATEWAY, "same error");
        let conn = open_usage_log_connection(cfg.usage_log_sqlite_path()).unwrap();
        let totals: (i64, i64) = conn.query_row("SELECT COUNT(*), SUM(repeat_count) FROM usage_logs", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(totals, (7, 8));
    }

    #[test]
    fn old_records_and_different_days_are_not_folded() {
        let cfg = merge_config();
        log_usage_sqlite_for_key(cfg.usage_log_sqlite_path(), "2026-10-26 12:00:00", "responses",
            "synthetic-key", "key", "error", "A", "client-model", "", 10, None, "same error",
            0, 0, "upstream", "same-client-request-id").unwrap();
        let ctx = RequestContext::detached();
        failed_attempt(&cfg, &ctx, "A", 502, "same error");
        let conn = open_usage_log_connection(cfg.usage_log_sqlite_path()).unwrap();
        conn.execute("UPDATE usage_logs SET ts='2026-10-25 12:00:00' WHERE failure_group != ''", []).unwrap();
        drop(conn);
        failed_attempt(&cfg, &ctx, "A", 502, "same error");
        let conn = open_usage_log_connection(cfg.usage_log_sqlite_path()).unwrap();
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM usage_logs", [], |row| row.get(0)).unwrap();
        assert_eq!(rows, 3);
        let old: (i64, String) = conn.query_row("SELECT repeat_count, failure_group FROM usage_logs WHERE id=1", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(old, (1, String::new()));
    }

    #[test]
    fn merged_elapsed_time_preserves_positive_latency_samples() {
        let cfg = merge_config();
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(log_stream_started(&cfg, "responses", "A", "client-model", "upstream-model",
                Instant::now(), "synthetic-key", "key", "test-request").unwrap());
        }
        for (id, duration) in ids.into_iter().zip([0, 100, 300]) {
            update_usage_log_sqlite_in_group(cfg.usage_log_sqlite_path(), id, "error", "",
                duration, None, "same error", 0, 0, "upstream", "test-scoped-group").unwrap();
        }
        let conn = open_usage_log_connection(cfg.usage_log_sqlite_path()).unwrap();
        let row: (i64, i64, i64) = conn.query_row("SELECT repeat_count, latency_ms, latency_samples FROM usage_logs", [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap();
        assert_eq!(row, (3, 400, 2));
    }

    #[tokio::test]
    async fn cancelled_attempt_is_kept_separate_from_merged_failures() {
        let cfg = merge_config();
        let ctx = RequestContext::detached();
        for _ in 0..3 { failed_attempt(&cfg, &ctx, "A", 502, "same error"); }
        drop(StreamAttemptLogGuard::new(&cfg, true, "responses", "A", "client-model", "upstream-model",
            Instant::now(), "synthetic-key", "key", &ctx));
        let conn = open_usage_log_connection(cfg.usage_log_sqlite_path()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let cancelled: i64 = conn.query_row("SELECT COUNT(*) FROM usage_logs WHERE status='aborted'", [], |row| row.get(0)).unwrap();
                if cancelled == 1 { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        let counts: (i64, i64) = conn.query_row("SELECT COUNT(*), SUM(repeat_count) FROM usage_logs", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(counts, (2, 4));
    }

    #[test]
    fn concurrent_same_request_errors_do_not_lose_counts() {
        let cfg = merge_config();
        let ctx = RequestContext::detached();
        // 先建立表，避免并发迁移本身干扰合并检查。
        ensure_usage_log_schema(&open_usage_log_connection(cfg.usage_log_sqlite_path()).unwrap()).unwrap();
        let workers = (0..8).map(|_| {
            let cfg = cfg.clone(); let ctx = ctx.clone();
            std::thread::spawn(move || failed_attempt(&cfg, &ctx, "A", 502, "same error"))
        }).collect::<Vec<_>>();
        for worker in workers { worker.join().unwrap(); }
        let conn = open_usage_log_connection(cfg.usage_log_sqlite_path()).unwrap();
        let counts: (i64, i64) = conn.query_row("SELECT COUNT(*), SUM(repeat_count) FROM usage_logs", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(counts, (1, 8));
    }

    #[test]
    fn auth_provider_logs_account_name_instead_of_id() {
        let config: AppConfig = serde_yaml::from_str(
            r#"
auth_accounts:
  - id: account-id
    name: codex-main
"#,
        )
        .unwrap();

        assert_eq!(
            log_provider_name(&config, "auth:account-id"),
            "auth:codex-main"
        );
        assert_eq!(log_provider_name(&config, "provider-a"), "provider-a");
        assert_eq!(
            log_provider_name(&config, "auth:missing-account"),
            "auth:missing-account"
        );
    }
}

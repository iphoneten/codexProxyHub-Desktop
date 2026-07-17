use super::*;

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
            upstream_model: "",
            status: "error",
            error: Some(error),
            usage: TokenUsage::default(),
            first_token_ms,
            token_source: None,
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
) -> Option<i64> {
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    log_usage_sqlite_for_key(
        config.usage_log_sqlite_path(),
        &ts,
        api,
        api_key_id,
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
        )
    })?
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
) {
    let Some(id) = log_id else {
        return;
    };
    if update_usage_log(
        config,
        id,
        started,
        &UsageLogEvent {
            api,
            api_key_id,
            api_key_name,
            provider,
            model,
            upstream_model: "",
            status: "error",
            error: Some(error),
            usage: TokenUsage::default(),
            first_token_ms: None,
            token_source: None,
        },
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

    let _ = log_usage_sqlite_for_key(
        config.usage_log_sqlite_path(),
        &ts,
        event.api,
        event.api_key_id,
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
) -> rusqlite::Result<i64> {
    let conn = open_usage_log_connection(path)?;
    ensure_usage_log_schema(&conn)?;
    conn.execute(
        r#"
        INSERT INTO usage_logs
            (
                ts, api, status, channel, request_model, upstream_model,
                latency_ms, first_token_ms, input_tokens, output_tokens, error, token_source,
                api_key_id, api_key_name
            )
        VALUES
            (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
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
            api_key_name
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
    ensure_column(conn, "api_key_id", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "api_key_name", "TEXT NOT NULL DEFAULT ''")?;
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

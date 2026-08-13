use super::assets::AnimatedGif;
use super::common::{
    accent, border, confirm_delete_button, format_compact_tokens, good, loading_icon, metric_tile,
    muteds, section, soft_button, switch, table_header,
};
use super::{AppMessage, MessageKind};
use crate::config::AppConfig;
use crate::proxy;
use eframe::egui;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LogRange {
    Today,
    SevenDays,
    ThirtyDays,
    All,
}

impl Default for LogRange {
    fn default() -> Self {
        Self::All
    }
}

impl LogRange {
    const OPTIONS: [(Self, &'static str); 4] = [
        (Self::Today, "今日"),
        (Self::SevenDays, "7日"),
        (Self::ThirtyDays, "30日"),
        (Self::All, "全部"),
    ];

    fn start_timestamp(self) -> Option<String> {
        let today = chrono::Local::now().date_naive();
        self.start_timestamp_for(today)
    }

    fn start_timestamp_for(self, today: chrono::NaiveDate) -> Option<String> {
        let days_back = match self {
            Self::Today => 0,
            Self::SevenDays => 6,
            Self::ThirtyDays => 29,
            Self::All => return None,
        };
        let start = today
            .checked_sub_signed(chrono::Duration::days(days_back))
            .unwrap_or(today)
            .and_hms_opt(0, 0, 0)?;
        Some(start.format("%Y-%m-%d %H:%M:%S").to_string())
    }
}

/// 日志状态筛选。用的是表格「状态」列展示的口径（见 `status_text`），
/// 不是数据库里的原始值：成功对应 `ok` / `stream_started`，运行中对应 `running`，
/// 失败是其余一切（`error` 以及任何非预期值），确保没有行会从所有筛选里漏掉。
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum LogStatusFilter {
    #[default]
    All,
    Success,
    Failed,
    Running,
}

impl LogStatusFilter {
    const OPTIONS: [(Self, &'static str); 4] = [
        (Self::All, "全部状态"),
        (Self::Success, "成功"),
        (Self::Failed, "失败"),
        (Self::Running, "运行中"),
    ];

    fn label(self) -> &'static str {
        Self::OPTIONS
            .iter()
            .find(|(status, _)| *status == self)
            .map(|(_, label)| *label)
            .unwrap_or("全部状态")
    }

    /// 传给 SQL 的筛选标记，`None` 表示不限状态。取值必须与
    /// `STATUS_FILTER_CLAUSE` 里的分支一致。
    fn sql_kind(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Success => Some("success"),
            Self::Failed => Some("failed"),
            Self::Running => Some("running"),
        }
    }
}

pub struct LogViewState {
    pub(crate) rows: Vec<LogRow>,
    pub(crate) loaded_path: String,
    loaded_range: LogRange,
    loaded_api_key_id: String,
    loaded_status: LogStatusFilter,
    range: LogRange,
    api_key_id: String,
    status: LogStatusFilter,
    pub(crate) page: usize,
    pub(crate) total: usize,
    pub(crate) last_refresh: Option<Instant>,
    pub(crate) totals: LogTotals,
    pub(crate) live: bool,
}

impl Default for LogViewState {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            loaded_path: String::new(),
            loaded_range: LogRange::default(),
            loaded_api_key_id: String::new(),
            loaded_status: LogStatusFilter::default(),
            range: LogRange::default(),
            api_key_id: String::new(),
            status: LogStatusFilter::default(),
            page: 0,
            total: 0,
            last_refresh: None,
            totals: LogTotals::default(),
            live: true,
        }
    }
}

/// 日志页当前生效的筛选条件。读取和清空共用同一份，避免两边条件走偏。
#[derive(Clone, Copy)]
struct LogFilter<'a> {
    range: LogRange,
    status: LogStatusFilter,
    api_key_id: &'a str,
}

impl LogViewState {
    fn filter(&self) -> LogFilter<'_> {
        LogFilter {
            range: self.range,
            status: self.status,
            api_key_id: &self.api_key_id,
        }
    }
}

#[derive(Default, Clone, Copy)]
pub struct LogTotals {
    pub(crate) input_tokens: i64,
    pub(crate) output_tokens: i64,
}

pub fn logs_section(
    ui: &mut egui::Ui,
    config: &AppConfig,
    log_view: &mut LogViewState,
    message: &mut AppMessage,
    loading_gif: Option<&AnimatedGif>,
) {
    const LOG_PAGE_SIZE: usize = 20;
    let path = config.usage_log_sqlite_path();
    let source_key = format!("sqlite:{}", path.display());
    let filter_changed = log_view.loaded_range != log_view.range
        || log_view.loaded_api_key_id != log_view.api_key_id
        || log_view.loaded_status != log_view.status;
    if log_view.loaded_path != source_key || filter_changed {
        log_view.page = 0;
        refresh_logs(config, log_view, Some(message));
    } else if log_view.live
        && log_view
            .last_refresh
            .map(|t| t.elapsed() >= std::time::Duration::from_secs(1))
            .unwrap_or(true)
    {
        refresh_logs(config, log_view, None);
    }

    section(ui, "请求日志", |ui| {
        ui.horizontal(|ui| {
            if soft_button(ui, "刷新").clicked() {
                refresh_logs(config, log_view, Some(message));
            }
            switch(ui, &mut log_view.live).on_hover_text("自动刷新请求日志");
            ui.label(
                egui::RichText::new(if log_view.live { "实时" } else { "已暂停" })
                    .color(if log_view.live { good() } else { muteds() }),
            );
            ui.separator();
            let mut changed = false;
            for (range, label) in LogRange::OPTIONS {
                changed |= ui
                    .selectable_value(&mut log_view.range, range, label)
                    .changed();
            }
            egui::ComboBox::from_id_source("log_status_filter")
                .selected_text(log_view.status.label())
                .show_ui(ui, |ui| {
                    for (status, label) in LogStatusFilter::OPTIONS {
                        changed |= ui
                            .selectable_value(&mut log_view.status, status, label)
                            .changed();
                    }
                });
            egui::ComboBox::from_id_source("log_api_key_filter")
                .selected_text(selected_api_key_label(config, log_view))
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(&mut log_view.api_key_id, String::new(), "全部 API Key")
                        .changed();
                    for option in api_key_filter_options(config) {
                        changed |= ui
                            .selectable_value(&mut log_view.api_key_id, option.id, option.label)
                            .changed();
                    }
                });
            if changed {
                log_view.page = 0;
                refresh_logs(config, log_view, Some(message));
            }
            if soft_button(ui, "清空选项").clicked() {
                log_view.range = LogRange::All;
                log_view.api_key_id.clear();
                log_view.status = LogStatusFilter::All;
                log_view.page = 0;
                refresh_logs(config, log_view, None);
                *message = AppMessage::new("已清空筛选选项", MessageKind::Success);
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if confirm_delete_button(ui, "usage_logs_clear", "清空日志") {
                clear_logs(config, log_view, message);
            }
            ui.label(
                egui::RichText::new(format!("清空范围: {}", clear_scope_label(config, log_view)))
                    .color(muteds()),
            )
            .on_hover_text(
                "按当前筛选条件删除请求日志，运行中的请求会保留。\n\
                 清空包含今日的日志会同时重置各 API Key 的当日 Token 用量统计。",
            );
        });
        let totals = log_view.totals;
        ui.horizontal_wrapped(|ui| {
            metric_tile(
                ui,
                "输入",
                &format_compact_tokens(totals.input_tokens),
                "累计 prompt tokens",
                accent(),
            );
            metric_tile(
                ui,
                "输出",
                &format_compact_tokens(totals.output_tokens),
                "累计 completion tokens",
                egui::Color32::from_rgb(110, 106, 220),
            );
            metric_tile(
                ui,
                "总 Token",
                &format_compact_tokens(totals.input_tokens + totals.output_tokens),
                "",
                good(),
            );
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let total_pages = log_view.total.div_ceil(LOG_PAGE_SIZE).max(1);
            if ui
                .add_enabled(log_view.page > 0, egui::Button::new("上一页"))
                .clicked()
            {
                log_view.page = log_view.page.saturating_sub(1);
                refresh_logs(config, log_view, Some(message));
            }
            ui.label(format!(
                "第 {} / {} 页，共 {} 条",
                log_view.page + 1,
                total_pages,
                log_view.total
            ));
            if ui
                .add_enabled(log_view.page + 1 < total_pages, egui::Button::new("下一页"))
                .clicked()
            {
                log_view.page += 1;
                refresh_logs(config, log_view, Some(message));
            }
        });
        ui.add_space(8.0);
        if log_view.rows.is_empty() {
            ui.label(egui::RichText::new("暂无日志").color(muteds()));
            return;
        }

        egui::Frame::none()
            .fill(egui::Color32::from_rgb(248, 250, 252))
            .stroke(egui::Stroke::new(1.0, border()))
            .rounding(6.0)
            .inner_margin(egui::Margin::symmetric(10.0, 8.0))
            .show(ui, |ui| {
                egui::Grid::new("usage_logs")
                    .striped(true)
                    .min_col_width(80.0)
                    .show(ui, |ui| {
                        table_header(ui, "时间");
                        table_header(ui, "状态");
                        table_header(ui, "API Key");
                        table_header(ui, "接口");
                        table_header(ui, "渠道");
                        table_header(ui, "模型");
                        table_header(ui, "Token");
                        table_header(ui, "用时/首字(秒)");
                        table_header(ui, "错误");
                        ui.end_row();

                        for row in &log_view.rows {
                            ui.monospace(&row.ts);
                            log_status_cell(ui, &row.status, loading_gif);
                            ui.label(row.api_key_label());
                            ui.label(&row.api);
                            ui.label(&row.channel);
                            model_cell(ui, row);
                            ui.label(format!("{}/{}", row.input_tokens, row.output_tokens));
                            ui.label(format!("{}/{}", row.display_latency(), row.first_token));
                            let err_display = if row.error.chars().count() > 30 {
                                let truncated: String = row.error.chars().take(28).collect();
                                format!("{truncated}...")
                            } else {
                                row.error.clone()
                            };
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(&err_display).color(muteds()))
                                    .on_hover_text(&row.error);
                                if !row.error.is_empty() {
                                    if ui
                                        .small_button("📋")
                                        .on_hover_text("复制完整错误信息")
                                        .clicked()
                                    {
                                        ui.output_mut(|output| {
                                            output.copied_text = row.error.clone();
                                        });
                                    }
                                }
                            });
                            ui.end_row();
                        }
                    });
            });
    });
}

struct ApiKeyFilterOption {
    id: String,
    label: String,
}

fn api_key_filter_options(config: &AppConfig) -> Vec<ApiKeyFilterOption> {
    config
        .auth
        .api_keys
        .iter()
        .map(|key| ApiKeyFilterOption {
            id: proxy::api_key_id(&key.key),
            label: if key.name.trim().is_empty() {
                "未命名 Key".to_string()
            } else {
                key.name.clone()
            },
        })
        .collect()
}

fn selected_api_key_label(config: &AppConfig, log_view: &LogViewState) -> String {
    if log_view.api_key_id.trim().is_empty() {
        return "全部 API Key".to_string();
    }
    api_key_filter_options(config)
        .into_iter()
        .find(|option| option.id == log_view.api_key_id)
        .map(|option| option.label)
        .unwrap_or_else(|| "已选 API Key".to_string())
}

fn api_key_name_for_id(config: &AppConfig, api_key_id: &str) -> Option<String> {
    let target = api_key_id.trim();
    if target.is_empty() {
        return None;
    }
    config.auth.api_keys.iter().find_map(|key| {
        (proxy::api_key_id(&key.key) == target).then(|| {
            if key.name.trim().is_empty() {
                "未命名 Key".to_string()
            } else {
                key.name.clone()
            }
        })
    })
}

fn normalized_api_key_filter(api_key_id: &str) -> Option<&str> {
    let trimmed = api_key_id.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn normalized_api_key_name_filter<'a>(
    api_key_filter: Option<&str>,
    api_key_name: Option<&'a str>,
) -> Option<&'a str> {
    api_key_filter?;
    api_key_name
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != "未命名 Key")
}

/// 「清空日志」按钮旁展示的生效范围，和实际 DELETE 使用的筛选条件一一对应。
fn clear_scope_label(config: &AppConfig, log_view: &LogViewState) -> String {
    let range_label = LogRange::OPTIONS
        .iter()
        .find(|(range, _)| *range == log_view.range)
        .map(|(_, label)| *label)
        .unwrap_or("全部");
    let mut label = range_label.to_string();
    if log_view.status != LogStatusFilter::All {
        label.push_str(" · ");
        label.push_str(log_view.status.label());
    }
    if !log_view.api_key_id.trim().is_empty() {
        label.push_str(" · ");
        label.push_str(&selected_api_key_label(config, log_view));
    }
    label
}

/// 按日志页当前的筛选条件删除记录，删完后立刻重新加载当前页。
fn clear_logs(config: &AppConfig, log_view: &mut LogViewState, message: &mut AppMessage) {
    let path = config.usage_log_sqlite_path();
    if !path.exists() {
        *message = AppMessage::new("暂无日志文件，无需清空", MessageKind::Info);
        return;
    }
    let scope = clear_scope_label(config, log_view);
    let filter = log_view.filter();
    let api_key_name = api_key_name_for_id(config, filter.api_key_id);
    let api_key_filter = normalized_api_key_filter(filter.api_key_id);
    let api_key_name_filter =
        normalized_api_key_name_filter(api_key_filter, api_key_name.as_deref());
    let result = proxy::delete_usage_logs(
        path,
        filter.range.start_timestamp().as_deref(),
        api_key_filter,
        api_key_name_filter,
        filter.status.sql_kind(),
    );
    match result {
        Ok(0) => {
            let hint = if log_view.status == LogStatusFilter::Running {
                "运行中的请求不会被清空"
            } else {
                "没有匹配的日志可清空"
            };
            *message = AppMessage::new(format!("{hint}（{scope}）"), MessageKind::Info);
        }
        Ok(removed) => {
            log_view.page = 0;
            refresh_logs(config, log_view, None);
            *message = AppMessage::new(
                format!("已清空 {removed} 条日志（{scope}）"),
                MessageKind::Success,
            );
        }
        Err(err) => {
            *message = AppMessage::new(format!("清空日志失败: {err}"), MessageKind::Error);
        }
    }
}

pub fn refresh_logs(
    config: &AppConfig,
    log_view: &mut LogViewState,
    message: Option<&mut AppMessage>,
) {
    const LOG_PAGE_SIZE: usize = 20;
    let path = config.usage_log_sqlite_path();
    log_view.loaded_path = format!("sqlite:{}", path.display());
    log_view.loaded_range = log_view.range;
    log_view.loaded_api_key_id = log_view.api_key_id.clone();
    log_view.loaded_status = log_view.status;
    log_view.last_refresh = Some(Instant::now());
    let (final_rows, final_total, note) =
        match read_log_page(config, log_view.filter(), log_view.page, LOG_PAGE_SIZE) {
            Ok((rows, total)) => {
                let total_pages = total.div_ceil(LOG_PAGE_SIZE).max(1);
                if log_view.page >= total_pages {
                    log_view.page = total_pages - 1;
                    match read_log_page(config, log_view.filter(), log_view.page, LOG_PAGE_SIZE) {
                        Ok((rows2, total2)) => {
                            let count = rows2.len();
                            (rows2, total2, Ok(format!("已加载 {} 条日志", count)))
                        }
                        Err(err) => (Vec::new(), 0, Err(err)),
                    }
                } else {
                    let count = rows.len();
                    (rows, total, Ok(format!("已加载 {} 条日志", count)))
                }
            }
            Err(err) => (Vec::new(), 0, Err(err)),
        };
    log_view.total = final_total;
    log_view.rows = final_rows;
    if let Ok(totals) = read_filtered_log_totals(config, log_view.filter()) {
        log_view.totals = totals;
    }
    if let Some(msg) = message {
        match note {
            Ok(text) => *msg = AppMessage::new(text, MessageKind::Success),
            Err(text) => *msg = AppMessage::new(text, MessageKind::Error),
        }
    }
}

fn read_filtered_log_totals(
    config: &AppConfig,
    filter: LogFilter<'_>,
) -> Result<LogTotals, String> {
    let api_key_name = api_key_name_for_id(config, filter.api_key_id);
    read_sqlite_log_totals(
        config.usage_log_sqlite_path(),
        filter,
        api_key_name.as_deref(),
    )
}

fn read_sqlite_log_totals(
    path: PathBuf,
    filter: LogFilter<'_>,
    api_key_name: Option<&str>,
) -> Result<LogTotals, String> {
    if !path.exists() {
        return Ok(LogTotals::default());
    }
    let conn = proxy::open_usage_log_connection(path)
        .map_err(|err| format!("打开 SQLite 日志失败: {err}"))?;
    proxy::ensure_usage_log_schema(&conn)
        .map_err(|err| format!("初始化 SQLite 日志表失败: {err}"))?;
    let range_start = filter.range.start_timestamp();
    let api_key_filter = normalized_api_key_filter(filter.api_key_id);
    let api_key_name_filter = normalized_api_key_name_filter(api_key_filter, api_key_name);
    let (input_tokens, output_tokens) = conn
        .query_row(
            &format!(
                "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0)
                 FROM usage_logs
                 WHERE {}",
                proxy::USAGE_LOG_FILTER_WHERE
            ),
            rusqlite::params![
                range_start.as_deref(),
                api_key_filter,
                api_key_name_filter,
                filter.status.sql_kind()
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(|err| format!("汇总 SQLite Token 失败: {err}"))?;
    Ok(LogTotals {
        input_tokens,
        output_tokens,
    })
}

fn read_log_page(
    config: &AppConfig,
    filter: LogFilter<'_>,
    page: usize,
    page_size: usize,
) -> Result<(Vec<LogRow>, usize), String> {
    let api_key_name = api_key_name_for_id(config, filter.api_key_id);
    read_sqlite_log_page(
        config.usage_log_sqlite_path(),
        filter,
        api_key_name.as_deref(),
        page,
        page_size,
    )
}

fn read_sqlite_log_page(
    path: PathBuf,
    filter: LogFilter<'_>,
    api_key_name: Option<&str>,
    page: usize,
    page_size: usize,
) -> Result<(Vec<LogRow>, usize), String> {
    if !path.exists() {
        return Ok((Vec::new(), 0));
    }
    proxy::recover_stale_running_usage_logs(path.clone(), std::time::Duration::from_secs(30 * 60));
    let conn = proxy::open_usage_log_connection(path)
        .map_err(|err| format!("打开 SQLite 日志失败: {err}"))?;
    proxy::ensure_usage_log_schema(&conn)
        .map_err(|err| format!("初始化 SQLite 日志表失败: {err}"))?;
    let range_start = filter.range.start_timestamp();
    let api_key_filter = normalized_api_key_filter(filter.api_key_id);
    let api_key_name_filter = normalized_api_key_name_filter(api_key_filter, api_key_name);
    let status_kind = filter.status.sql_kind();
    let total = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM usage_logs WHERE {}",
                proxy::USAGE_LOG_FILTER_WHERE
            ),
            rusqlite::params![
                range_start.as_deref(),
                api_key_filter,
                api_key_name_filter,
                status_kind
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|err| format!("读取 SQLite 日志数量失败: {err}"))? as usize;
    let mut stmt = conn
        .prepare(&format!(
            r#"
            SELECT
                ts, status, api_key_name, api, channel, request_model, upstream_model,
                latency_ms, first_token_ms, input_tokens, output_tokens, error
            FROM usage_logs
            WHERE {}
            ORDER BY id DESC
            LIMIT ?5
            OFFSET ?6
            "#,
            proxy::USAGE_LOG_FILTER_WHERE
        ))
        .map_err(|err| format!("读取 SQLite 日志失败: {err}"))?;
    let rows = stmt
        .query_map(
            rusqlite::params![
                range_start.as_deref(),
                api_key_filter,
                api_key_name_filter,
                status_kind,
                page_size as i64,
                page.saturating_mul(page_size) as i64
            ],
            |row| {
                let latency_ms: i64 = row.get(7)?;
                let first_token_ms: Option<i64> = row.get(8)?;
                Ok(LogRow {
                    ts: row.get(0)?,
                    status: row.get(1)?,
                    api_key_name: row.get(2)?,
                    api: row.get(3)?,
                    channel: row.get(4)?,
                    model: row.get(5)?,
                    upstream_model: row.get(6)?,
                    first_token: first_token_ms
                        .map(|value| format!("{:.2}", value as f64 / 1000.0))
                        .unwrap_or_else(|| "-".to_string()),
                    latency: format!("{:.2}", latency_ms as f64 / 1000.0),
                    input_tokens: row.get(9)?,
                    output_tokens: row.get(10)?,
                    error: row.get(11)?,
                })
            },
        )
        .map_err(|err| format!("读取 SQLite 日志失败: {err}"))?;
    let out = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("解析 SQLite 日志失败: {err}"))?;
    Ok((out, total))
}

pub struct LogRow {
    pub(crate) ts: String,
    pub(crate) status: String,
    pub(crate) api_key_name: String,
    pub(crate) api: String,
    pub(crate) channel: String,
    pub(crate) model: String,
    pub(crate) upstream_model: String,
    pub(crate) first_token: String,
    pub(crate) latency: String,
    pub(crate) input_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) error: String,
}

impl LogRow {
    fn api_key_label(&self) -> &str {
        if self.api_key_name.trim().is_empty() {
            "-"
        } else {
            self.api_key_name.as_str()
        }
    }

    fn display_latency(&self) -> String {
        if self.status != "running" {
            return self.latency.clone();
        }
        chrono::NaiveDateTime::parse_from_str(&self.ts, "%Y-%m-%d %H:%M:%S")
            .ok()
            .map(|started| {
                let elapsed = chrono::Local::now().naive_local() - started;
                format!("{:.2}", elapsed.num_milliseconds().max(0) as f64 / 1000.0)
            })
            .unwrap_or_else(|| self.latency.clone())
    }
}

fn model_cell(ui: &mut egui::Ui, row: &LogRow) {
    ui.vertical(|ui| {
        ui.label(&row.model);
        let upstream = row.upstream_model.trim();
        if !upstream.is_empty() && upstream != row.model.trim() {
            ui.horizontal(|ui| {
                ui.add_space(6.0);
                forward_arrow(ui);
                ui.label(egui::RichText::new(upstream).size(11.0).color(muteds()));
            });
        }
    });
}

fn forward_arrow(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    let stroke = egui::Stroke::new(1.2, muteds());
    let bend = egui::pos2(rect.left() + 2.0, rect.center().y);
    let tip = egui::pos2(rect.right() - 2.0, rect.center().y);
    ui.painter()
        .line_segment([egui::pos2(bend.x, rect.top() + 1.0), bend], stroke);
    ui.painter().line_segment([bend, tip], stroke);
    ui.painter()
        .line_segment([egui::pos2(tip.x - 3.0, tip.y - 3.0), tip], stroke);
    ui.painter()
        .line_segment([tip, egui::pos2(tip.x - 3.0, tip.y + 3.0)], stroke);
}

fn status_text(status: &str) -> egui::RichText {
    let (label, color) = if status == "running" {
        ("运行中", accent())
    } else if status == "ok" || status == "stream_started" {
        ("成功", good())
    } else if status == "-" || status == "raw" {
        (status, muteds())
    } else {
        ("失败", egui::Color32::from_rgb(176, 54, 64))
    };
    egui::RichText::new(label).strong().color(color)
}

fn log_status_cell(ui: &mut egui::Ui, status: &str, loading_gif: Option<&AnimatedGif>) {
    if status == "running" {
        ui.horizontal(|ui| {
            loading_icon(ui, loading_gif, egui::vec2(14.0, 14.0));
            ui.label(status_text(status));
        });
    } else {
        ui.label(status_text(status));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_test_log(
        conn: &rusqlite::Connection,
        ts: &str,
        api_key_id: &str,
        api_key_name: &str,
        input_tokens: i64,
    ) {
        conn.execute(
            r#"
            INSERT INTO usage_logs
                (ts, api, status, channel, request_model, upstream_model,
                 latency_ms, first_token_ms, input_tokens, output_tokens, error,
                 token_source, api_key_id, api_key_name)
            VALUES
                (?1, 'responses', 'ok', 'test-channel', 'gpt-test', 'gpt-test',
                 100, 50, ?4, 1, '', 'upstream', ?2, ?3)
            "#,
            rusqlite::params![ts, api_key_id, api_key_name, input_tokens],
        )
        .unwrap();
    }

    fn insert_running_test_log(conn: &rusqlite::Connection, ts: &str, api_key_id: &str) {
        conn.execute(
            r#"
            INSERT INTO usage_logs
                (ts, api, status, channel, request_model, upstream_model,
                 latency_ms, first_token_ms, input_tokens, output_tokens, error,
                 token_source, api_key_id, api_key_name)
            VALUES
                (?1, 'responses', 'running', 'test-channel', 'gpt-test', 'gpt-test',
                 0, NULL, 0, 0, '', 'pending', ?2, 'A')
            "#,
            rusqlite::params![ts, api_key_id],
        )
        .unwrap();
    }

    /// 写入指定原始状态的行，用于验证状态筛选的分组口径。
    fn insert_test_log_with_status(conn: &rusqlite::Connection, ts: &str, status: &str) {
        conn.execute(
            r#"
            INSERT INTO usage_logs
                (ts, api, status, channel, request_model, upstream_model,
                 latency_ms, first_token_ms, input_tokens, output_tokens, error,
                 token_source, api_key_id, api_key_name)
            VALUES
                (?1, 'responses', ?2, 'test-channel', 'gpt-test', 'gpt-test',
                 100, 50, 5, 1, '', 'upstream', 'key-a', 'A')
            "#,
            rusqlite::params![ts, status],
        )
        .unwrap();
    }

    fn test_filter(range: LogRange, status: LogStatusFilter, api_key_id: &str) -> LogFilter<'_> {
        LogFilter {
            range,
            status,
            api_key_id,
        }
    }

    #[test]
    fn log_range_starts_at_local_day_boundary() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 17).unwrap();

        assert_eq!(
            LogRange::Today.start_timestamp_for(today).as_deref(),
            Some("2026-07-17 00:00:00")
        );
        assert_eq!(
            LogRange::SevenDays.start_timestamp_for(today).as_deref(),
            Some("2026-07-11 00:00:00")
        );
        assert_eq!(
            LogRange::ThirtyDays.start_timestamp_for(today).as_deref(),
            Some("2026-06-18 00:00:00")
        );
        assert_eq!(LogRange::All.start_timestamp_for(today), None);
    }

    #[test]
    fn sqlite_logs_filter_by_range_and_api_key() {
        let path = std::env::temp_dir().join(format!(
            "routehub-log-filter-{}.sqlite3",
            uuid::Uuid::new_v4().simple()
        ));
        let conn = proxy::open_usage_log_connection(path.clone()).unwrap();
        proxy::ensure_usage_log_schema(&conn).unwrap();
        let today = chrono::Local::now().date_naive();
        let today_ts = today
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let old_ts = today
            .checked_sub_signed(chrono::Duration::days(10))
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        insert_test_log(&conn, &today_ts, "key-a", "A", 10);
        insert_test_log(&conn, &today_ts, "key-b", "B", 20);
        insert_test_log(&conn, &today_ts, "", "A", 40);
        insert_test_log(&conn, &old_ts, "key-a", "A", 30);
        drop(conn);

        let (rows, total) = read_sqlite_log_page(
            path.clone(),
            test_filter(LogRange::Today, LogStatusFilter::All, "key-a"),
            Some("A"),
            0,
            20,
        )
        .unwrap();
        assert_eq!(total, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].api_key_name, "A");
        let today_totals = read_sqlite_log_totals(
            path.clone(),
            test_filter(LogRange::Today, LogStatusFilter::All, "key-a"),
            Some("A"),
        )
        .unwrap();
        assert_eq!(today_totals.input_tokens, 50);
        assert_eq!(today_totals.output_tokens, 2);

        let (_, total) = read_sqlite_log_page(
            path.clone(),
            test_filter(LogRange::All, LogStatusFilter::All, "key-a"),
            Some("A"),
            0,
            20,
        )
        .unwrap();
        assert_eq!(total, 3);
        let all_totals = read_sqlite_log_totals(
            path.clone(),
            test_filter(LogRange::All, LogStatusFilter::All, ""),
            None,
        )
        .unwrap();
        assert_eq!(all_totals.input_tokens, 100);
        assert_eq!(all_totals.output_tokens, 4);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sqlite_logs_filter_by_status_group() {
        let path = std::env::temp_dir().join(format!(
            "routehub-log-status-{}.sqlite3",
            uuid::Uuid::new_v4().simple()
        ));
        let conn = proxy::open_usage_log_connection(path.clone()).unwrap();
        proxy::ensure_usage_log_schema(&conn).unwrap();
        // 运行中的行用当前时间，避免被 recover_stale_running_usage_logs 改成 error。
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        insert_test_log_with_status(&conn, &ts, "ok");
        insert_test_log_with_status(&conn, &ts, "stream_started");
        insert_test_log_with_status(&conn, &ts, "error");
        insert_test_log_with_status(&conn, &ts, "some_unexpected_status");
        insert_running_test_log(&conn, &ts, "key-a");
        drop(conn);

        let count = |status: LogStatusFilter| {
            read_sqlite_log_page(
                path.clone(),
                test_filter(LogRange::All, status, ""),
                None,
                0,
                20,
            )
            .unwrap()
            .1
        };
        assert_eq!(count(LogStatusFilter::All), 5);
        // 成功只含 ok / stream_started。
        assert_eq!(count(LogStatusFilter::Success), 2);
        // 失败兜住 error 以及任何非预期状态，保证没有行从所有筛选里漏掉。
        assert_eq!(count(LogStatusFilter::Failed), 2);
        assert_eq!(count(LogStatusFilter::Running), 1);

        // 汇总口径必须跟着状态筛选走。
        let success_totals = read_sqlite_log_totals(
            path.clone(),
            test_filter(LogRange::All, LogStatusFilter::Success, ""),
            None,
        )
        .unwrap();
        assert_eq!(success_totals.input_tokens, 10);
        assert_eq!(success_totals.output_tokens, 2);

        // 只清失败的：成功和运行中都留着。
        let removed = proxy::delete_usage_logs(
            path.clone(),
            None,
            None,
            None,
            LogStatusFilter::Failed.sql_kind(),
        )
        .unwrap();
        assert_eq!(removed, 2);
        assert_eq!(count(LogStatusFilter::All), 3);
        assert_eq!(count(LogStatusFilter::Failed), 0);

        // 选中「运行中」时清空删不到任何行，运行中的记录必须保住。
        let removed_running = proxy::delete_usage_logs(
            path.clone(),
            None,
            None,
            None,
            LogStatusFilter::Running.sql_kind(),
        )
        .unwrap();
        assert_eq!(removed_running, 0);
        assert_eq!(count(LogStatusFilter::Running), 1);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn clearing_logs_respects_filter_and_keeps_running_rows() {
        let path = std::env::temp_dir().join(format!(
            "routehub-log-clear-{}.sqlite3",
            uuid::Uuid::new_v4().simple()
        ));
        let conn = proxy::open_usage_log_connection(path.clone()).unwrap();
        proxy::ensure_usage_log_schema(&conn).unwrap();
        let today = chrono::Local::now().date_naive();
        let today_ts = today
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let old_ts = today
            .checked_sub_signed(chrono::Duration::days(10))
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        // 运行中的行用当前时间，避免被 recover_stale_running_usage_logs 判定为超时。
        let running_ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        insert_test_log(&conn, &today_ts, "key-a", "A", 10);
        insert_test_log(&conn, &today_ts, "key-b", "B", 20);
        insert_test_log(&conn, &old_ts, "key-a", "A", 30);
        insert_running_test_log(&conn, &running_ts, "key-a");
        drop(conn);

        // 只清 key-a 的今日日志：key-b 今日、key-a 历史和运行中的行都要保留。
        let removed = proxy::delete_usage_logs(
            path.clone(),
            LogRange::Today.start_timestamp().as_deref(),
            Some("key-a"),
            Some("A"),
            None,
        )
        .unwrap();
        assert_eq!(removed, 1);
        let (_, total) = read_sqlite_log_page(
            path.clone(),
            test_filter(LogRange::All, LogStatusFilter::All, ""),
            None,
            0,
            20,
        )
        .unwrap();
        assert_eq!(total, 3);

        // 全量清空：只剩运行中的行。
        let removed_all = proxy::delete_usage_logs(path.clone(), None, None, None, None).unwrap();
        assert_eq!(removed_all, 2);
        let (rows, total) = read_sqlite_log_page(
            path.clone(),
            test_filter(LogRange::All, LogStatusFilter::All, ""),
            None,
            0,
            20,
        )
        .unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].status, "running");
        let totals = read_sqlite_log_totals(
            path.clone(),
            test_filter(LogRange::All, LogStatusFilter::All, ""),
            None,
        )
        .unwrap();
        assert_eq!(totals.input_tokens, 0);
        assert_eq!(totals.output_tokens, 0);

        // 已经清空后再点一次不应该报错，也不会再删到任何行。
        let removed_again = proxy::delete_usage_logs(path.clone(), None, None, None, None).unwrap();
        assert_eq!(removed_again, 0);

        let _ = std::fs::remove_file(path);
    }
}

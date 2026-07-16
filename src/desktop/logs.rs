use crate::config::AppConfig;
use crate::proxy;
use eframe::egui;
use std::path::PathBuf;
use std::time::Instant;
use super::assets::AnimatedGif;
use super::common::{
    accent, border, format_compact_tokens, good, loading_icon, metric_tile, muteds, section,
    soft_button, switch, table_header,
};
use super::{AppMessage, MessageKind};

pub struct LogViewState {
    pub(crate) rows: Vec<LogRow>,
    pub(crate) loaded_path: String,
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
            page: 0,
            total: 0,
            last_refresh: None,
            totals: LogTotals::default(),
            live: true,
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
    if log_view.loaded_path != source_key {
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
            if soft_button(ui, "清空").clicked() {
                match clear_logs(config) {
                    Ok(()) => {
                        log_view.page = 0;
                        refresh_logs(config, log_view, None);
                        *message = AppMessage::new("已清空日志", MessageKind::Success);
                    }
                    Err(err) => *message = AppMessage::new(err, MessageKind::Error),
                }
            }
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
                            ui.label(egui::RichText::new(&row.error).color(muteds()));
                            ui.end_row();
                        }
                    });
            });
    });
}

pub fn refresh_logs(config: &AppConfig, log_view: &mut LogViewState, message: Option<&mut AppMessage>) {
    const LOG_PAGE_SIZE: usize = 20;
    let path = config.usage_log_sqlite_path();
    log_view.loaded_path = format!("sqlite:{}", path.display());
    log_view.last_refresh = Some(Instant::now());
    let (final_rows, final_total, note) = match read_log_page(config, log_view.page, LOG_PAGE_SIZE)
    {
        Ok((rows, total)) => {
            let total_pages = total.div_ceil(LOG_PAGE_SIZE).max(1);
            if log_view.page >= total_pages {
                log_view.page = total_pages - 1;
                match read_log_page(config, log_view.page, LOG_PAGE_SIZE) {
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
    if let Ok(totals) = read_log_totals(config) {
        log_view.totals = totals;
    }
    if let Some(msg) = message {
        match note {
            Ok(text) => *msg = AppMessage::new(text, MessageKind::Success),
            Err(text) => *msg = AppMessage::new(text, MessageKind::Error),
        }
    }
}

fn read_log_totals(config: &AppConfig) -> Result<LogTotals, String> {
    read_sqlite_log_totals(config.usage_log_sqlite_path())
}

fn read_sqlite_log_totals(path: PathBuf) -> Result<LogTotals, String> {
    if !path.exists() {
        return Ok(LogTotals::default());
    }
    let conn = proxy::open_usage_log_connection(path)
        .map_err(|err| format!("打开 SQLite 日志失败: {err}"))?;
    proxy::ensure_usage_log_schema(&conn)
        .map_err(|err| format!("初始化 SQLite 日志表失败: {err}"))?;
    let (input_tokens, output_tokens) = conn
        .query_row(
            "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0) FROM usage_logs",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(|err| format!("汇总 SQLite Token 失败: {err}"))?;
    Ok(LogTotals {
        input_tokens,
        output_tokens,
    })
}

fn clear_logs(config: &AppConfig) -> Result<(), String> {
    clear_sqlite_logs(config.usage_log_sqlite_path())
}

fn clear_sqlite_logs(path_buf: PathBuf) -> Result<(), String> {
    if !path_buf.exists() {
        return Ok(());
    }
    let conn = proxy::open_usage_log_connection(path_buf)
        .map_err(|err| format!("打开 SQLite 日志失败: {err}"))?;
    proxy::ensure_usage_log_schema(&conn)
        .map_err(|err| format!("初始化 SQLite 日志表失败: {err}"))?;
    conn.execute("DELETE FROM usage_logs", [])
        .map_err(|err| format!("清空 SQLite 日志失败: {err}"))?;
    Ok(())
}

fn read_log_page(
    config: &AppConfig,
    page: usize,
    page_size: usize,
) -> Result<(Vec<LogRow>, usize), String> {
    read_sqlite_log_page(config.usage_log_sqlite_path(), page, page_size)
}

fn read_sqlite_log_page(
    path: PathBuf,
    page: usize,
    page_size: usize,
) -> Result<(Vec<LogRow>, usize), String> {
    if !path.exists() {
        return Ok((Vec::new(), 0));
    }
    let conn = proxy::open_usage_log_connection(path)
        .map_err(|err| format!("打开 SQLite 日志失败: {err}"))?;
    proxy::ensure_usage_log_schema(&conn)
        .map_err(|err| format!("初始化 SQLite 日志表失败: {err}"))?;
    let total = conn
        .query_row("SELECT COUNT(*) FROM usage_logs", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|err| format!("读取 SQLite 日志数量失败: {err}"))? as usize;
    let mut stmt = conn
        .prepare(
            r#"
            SELECT
                ts, status, api_key_name, api, channel, request_model, upstream_model,
                latency_ms, first_token_ms, input_tokens, output_tokens, error
            FROM usage_logs
            ORDER BY id DESC
            LIMIT ?1
            OFFSET ?2
            "#,
        )
        .map_err(|err| format!("读取 SQLite 日志失败: {err}"))?;
    let rows = stmt
        .query_map(
            [page_size as i64, page.saturating_mul(page_size) as i64],
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

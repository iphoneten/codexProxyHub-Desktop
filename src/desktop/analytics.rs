use super::*;
use super::common::{section, muteds, stat_color, accent, good, heading_color, format_compact_tokens};
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct OverviewAnalyticsState {
    data: OverviewAnalyticsData,
    loaded_path: String,
    last_refresh: Option<Instant>,
    error: Option<String>,
}

#[derive(Default)]
struct OverviewAnalyticsData {
    total_requests: i64,
    avg_latency_ms: i64,
    totals: LogTotals,
    status_counts: Vec<(String, i64)>,
    channel_counts: Vec<(String, i64)>,
    api_key_counts: Vec<(String, i64)>,
    api_key_tokens: Vec<(String, i64)>,
    channel_usage: HashMap<String, ChannelUsage>,
}

#[derive(Default, Clone, Copy)]
pub(super) struct ChannelUsage {
    pub(super) requests: i64,
    pub(super) input_tokens: i64,
    pub(super) output_tokens: i64,
}

impl OverviewAnalyticsState {
    pub(super) fn channel_usage(&self, channel: &str) -> ChannelUsage {
        self.data
            .channel_usage
            .get(channel)
            .copied()
            .unwrap_or_default()
    }
}

pub(super) fn overview_analytics_section(
    ui: &mut egui::Ui,
    config: &AppConfig,
    analytics: &mut OverviewAnalyticsState,
) {
    refresh_overview_analytics(config, analytics);
    section(ui, "统计分析", |ui| {
        if let Some(error) = analytics.error.as_deref() {
            ui.label(egui::RichText::new(error).color(egui::Color32::from_rgb(176, 54, 64)));
            return;
        }
        if analytics.data.total_requests <= 0 {
            ui.label(egui::RichText::new("暂无日志数据").color(muteds()));
            return;
        }

        ui.horizontal_wrapped(|ui| {
            analytics_stat(
                ui,
                "请求数",
                &analytics.data.total_requests.to_string(),
                "累计日志记录",
            );
            analytics_stat(
                ui,
                "平均用时",
                &format!(
                    "{:.2}s",
                    analytics.data.avg_latency_ms.max(0) as f64 / 1000.0
                ),
                "按已记录耗时统计",
            );
            analytics_stat(
                ui,
                "Token",
                &format_compact_tokens(
                    analytics.data.totals.input_tokens + analytics.data.totals.output_tokens,
                ),
                "输入 + 输出",
            );
        });
        ui.add_space(10.0);

        ui.columns(5, |columns| {
            analytics_bar_group(
                &mut columns[0],
                "状态分布",
                &analytics.data.status_counts,
                accent(),
                |status| status_label(status).to_string(),
                5,
            );
            analytics_bar_group(
                &mut columns[1],
                "渠道请求",
                &analytics.data.channel_counts,
                good(),
                |value| value.to_string(),
                5,
            );
            let token_items = vec![
                ("输入".to_string(), analytics.data.totals.input_tokens),
                ("输出".to_string(), analytics.data.totals.output_tokens),
            ];
            analytics_bar_group(
                &mut columns[2],
                "Token 分布",
                &token_items,
                egui::Color32::from_rgb(110, 106, 220),
                |value| value.to_string(),
                5,
            );
            analytics_bar_group(
                &mut columns[3],
                "API Key 请求",
                &analytics.data.api_key_counts,
                accent(),
                |value| value.to_string(),
                6,
            );
            analytics_bar_group(
                &mut columns[4],
                "API Key Token",
                &analytics.data.api_key_tokens,
                egui::Color32::from_rgb(110, 106, 220),
                |value| value.to_string(),
                6,
            );
        });
    });
}

fn analytics_stat(ui: &mut egui::Ui, label: &str, value: &str, detail: &str) {
    ui.vertical(|ui| {
        ui.set_min_width(150.0);
        ui.label(egui::RichText::new(label).size(12.0).color(muteds()));
        ui.label(
            egui::RichText::new(value)
                .size(22.0)
                .strong()
                .color(stat_color()),
        );
        ui.label(egui::RichText::new(detail).size(12.0).color(muteds()));
    });
}

fn analytics_bar_group(
    ui: &mut egui::Ui,
    title: &str,
    items: &[(String, i64)],
    color: egui::Color32,
    label: impl Fn(&str) -> String,
    limit: usize,
) {
    ui.label(
        egui::RichText::new(title)
            .size(13.0)
            .strong()
            .color(heading_color()),
    );
    ui.add_space(4.0);
    let display_items = top_items_with_other(items, limit);
    let max = display_items
        .iter()
        .map(|(_, value)| *value)
        .max()
        .unwrap_or(1)
        .max(1);
    for (name, value) in display_items {
        let fraction = (value as f32 / max as f32).clamp(0.0, 1.0);
        ui.add(
            egui::ProgressBar::new(fraction)
                .fill(color)
                .desired_width(ui.available_width())
                .text(format!("{} {}", label(&name), format_compact_tokens(value))),
        );
    }
}

fn top_items_with_other(items: &[(String, i64)], limit: usize) -> Vec<(String, i64)> {
    let mut out = items.iter().take(limit).cloned().collect::<Vec<_>>();
    let other = items
        .iter()
        .skip(limit)
        .map(|(_, value)| *value)
        .sum::<i64>();
    if other > 0 {
        out.push(("其它".to_string(), other));
    }
    out
}

fn refresh_overview_analytics(config: &AppConfig, analytics: &mut OverviewAnalyticsState) {
    let path = config.usage_log_sqlite_path();
    let source = format!("sqlite:{}", path.display());
    let stale = analytics
        .last_refresh
        .map(|last| last.elapsed() >= Duration::from_secs(2))
        .unwrap_or(true);
    if analytics.loaded_path == source && !stale {
        return;
    }
    analytics.loaded_path = source;
    analytics.last_refresh = Some(Instant::now());
    match read_overview_analytics(path) {
        Ok(data) => {
            analytics.data = data;
            analytics.error = None;
        }
        Err(err) => analytics.error = Some(err),
    }
}

fn read_overview_analytics(path: PathBuf) -> Result<OverviewAnalyticsData, String> {
    if !path.exists() {
        return Ok(OverviewAnalyticsData::default());
    }
    let conn = proxy::open_usage_log_connection(path)
        .map_err(|err| format!("打开 SQLite 日志失败: {err}"))?;
    proxy::ensure_usage_log_schema(&conn)
        .map_err(|err| format!("初始化 SQLite 日志表失败: {err}"))?;

    let (total_requests, avg_latency, input_tokens, output_tokens) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(AVG(NULLIF(latency_ms, 0)), 0),
                    COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0)
             FROM usage_logs",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .map_err(|err| format!("汇总 SQLite 日志失败: {err}"))?;

    Ok(OverviewAnalyticsData {
        total_requests,
        avg_latency_ms: avg_latency.round() as i64,
        totals: LogTotals {
            input_tokens,
            output_tokens,
        },
        status_counts: read_group_counts(
            &conn,
            "SELECT status, COUNT(*) FROM usage_logs GROUP BY status ORDER BY COUNT(*) DESC",
        )?,
        channel_counts: read_group_counts(
            &conn,
            "SELECT channel, COUNT(*) FROM usage_logs
             WHERE channel != ''
             GROUP BY channel
             ORDER BY COUNT(*) DESC
             LIMIT 8",
        )?,
        api_key_counts: read_group_counts(
            &conn,
            "SELECT COALESCE(NULLIF(api_key_name, ''), '未命名'), COUNT(*)
             FROM usage_logs
             GROUP BY COALESCE(NULLIF(api_key_name, ''), '未命名')
             ORDER BY COUNT(*) DESC",
        )?,
        api_key_tokens: read_group_counts(
            &conn,
            "SELECT COALESCE(NULLIF(api_key_name, ''), '未命名'),
                    COALESCE(SUM(input_tokens + output_tokens), 0)
             FROM usage_logs
             GROUP BY COALESCE(NULLIF(api_key_name, ''), '未命名')
             ORDER BY COALESCE(SUM(input_tokens + output_tokens), 0) DESC",
        )?,
        channel_usage: read_channel_usage(&conn)?,
    })
}

fn read_group_counts(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<(String, i64)>, String> {
    let mut stmt = conn.prepare(sql).map_err(|err| err.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|err| err.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())
}

fn read_channel_usage(
    conn: &rusqlite::Connection,
) -> Result<HashMap<String, ChannelUsage>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT channel, COUNT(*),
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0)
             FROM usage_logs
             WHERE channel != ''
             GROUP BY channel",
        )
        .map_err(|err| err.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                ChannelUsage {
                    requests: row.get::<_, i64>(1)?,
                    input_tokens: row.get::<_, i64>(2)?,
                    output_tokens: row.get::<_, i64>(3)?,
                },
            ))
        })
        .map_err(|err| err.to_string())?;
    rows.collect::<Result<HashMap<_, _>, _>>()
        .map_err(|err| err.to_string())
}

fn status_label(status: &str) -> &'static str {
    if status == "running" {
        "运行中"
    } else if status == "ok" || status == "stream_started" {
        "成功"
    } else if status == "-" || status == "raw" {
        "原始"
    } else {
        "失败"
    }
}

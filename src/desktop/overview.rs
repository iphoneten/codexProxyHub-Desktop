use crate::config::AppConfig;
use crate::proxy;
use eframe::egui;
use std::time::Instant;
use super::analytics::OverviewAnalyticsState;
use super::common::{
    accent, badge, format_compact_tokens, good, muteds, section, switch, table_header, text_color,
};
use super::AppView;

pub fn provider_summary_section(
    ui: &mut egui::Ui,
    config: &mut AppConfig,
    selected_provider: &mut Option<usize>,
    view: &mut AppView,
    analytics: &OverviewAnalyticsState,
    circuit_status: Option<&proxy::ProviderCircuitStatusHandle>,
) {
    section(ui, "渠道概览", |ui| {
        egui::Grid::new("provider_summary")
            .striped(true)
            .min_col_width(84.0)
            .show(ui, |ui| {
                table_header(ui, "状态");
                table_header(ui, "名称");
                table_header(ui, "类型");
                table_header(ui, "运行");
                table_header(ui, "优先级");
                table_header(ui, "模型");
                table_header(ui, "请求");
                table_header(ui, "Token");
                ui.end_row();
                let mut ordered: Vec<usize> = (0..config.providers.len()).collect();
                ordered.sort_by(|left, right| {
                    let a = &config.providers[*left];
                    let b = &config.providers[*right];
                    b.enabled
                        .cmp(&a.enabled)
                        .then_with(|| a.priority.cmp(&b.priority))
                        .then_with(|| b.weight.cmp(&a.weight))
                        .then_with(|| a.name.cmp(&b.name))
                });
                for idx in ordered {
                    let provider = &mut config.providers[idx];
                    switch(ui, &mut provider.enabled);
                    let link_text = egui::RichText::new(&provider.name).strong().color(accent());
                    if ui.link(link_text).clicked() {
                        *selected_provider = Some(idx);
                        *view = AppView::Providers;
                    }
                    ui.label(&provider.provider_type);
                    provider_runtime_badge(ui, circuit_status, &provider.name);
                    ui.horizontal(|ui| {
                        badge(
                            ui,
                            &format!("P{}", provider.priority),
                            egui::Color32::from_rgb(239, 246, 255),
                            accent(),
                        );
                        badge(
                            ui,
                            &format!("W{}", provider.weight),
                            egui::Color32::from_rgb(240, 253, 244),
                            good(),
                        );
                    });
                    badge(
                        ui,
                        &provider.models.len().to_string(),
                        egui::Color32::from_rgb(248, 250, 252),
                        text_color(),
                    );
                    let usage = analytics.channel_usage(&provider.name);
                    ui.label(format_compact_tokens(usage.requests));
                    ui.label(format_compact_tokens(
                        usage.input_tokens + usage.output_tokens,
                    ));
                    ui.end_row();
                }
            });
    });
}

fn provider_runtime_badge(
    ui: &mut egui::Ui,
    circuit_status: Option<&proxy::ProviderCircuitStatusHandle>,
    provider_name: &str,
) {
    let Some(status_handle) = circuit_status else {
        badge(
            ui,
            "未启动",
            egui::Color32::from_rgb(248, 250, 252),
            muteds(),
        );
        return;
    };
    let status = status_handle.read().get(provider_name).cloned();
    let Some(status) = status else {
        badge(
            ui,
            "待请求",
            egui::Color32::from_rgb(248, 250, 252),
            muteds(),
        );
        return;
    };

    match status.status {
        proxy::ProviderCircuitStatus::Healthy => {
            if status.failures == 0 {
                badge(
                    ui,
                    &format!("健康 · {}", status.inflight),
                    egui::Color32::from_rgb(240, 253, 244),
                    good(),
                );
            } else {
                badge(
                    ui,
                    &format!("失败 {}/3 · {}", status.failures, status.inflight),
                    egui::Color32::from_rgb(255, 247, 237),
                    egui::Color32::from_rgb(190, 120, 24),
                );
            }
        }
        proxy::ProviderCircuitStatus::Open => {
            let text = match status.open_until {
                Some(until) if until > Instant::now() => {
                    let remaining = until
                        .saturating_duration_since(Instant::now())
                        .as_secs()
                        .max(1);
                    format!("熔断 {remaining}s")
                }
                _ => "待探测".to_string(),
            };
            badge(
                ui,
                &text,
                egui::Color32::from_rgb(254, 242, 242),
                egui::Color32::from_rgb(176, 54, 64),
            );
        }
        proxy::ProviderCircuitStatus::HalfOpen => {
            badge(
                ui,
                &format!("半开 · {}", status.inflight),
                egui::Color32::from_rgb(255, 247, 237),
                egui::Color32::from_rgb(190, 120, 24),
            );
        }
    }
}

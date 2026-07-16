use super::assets::cached_png_texture;
use super::common::{
    accent, badge, border, copy_icon_button, form_group, form_label, good, heading_color, muteds,
    section, surface, switch, text_color,
};
use super::{AppMessage, MessageKind, TextDraft};
use crate::config::{AppConfig, ProviderConfig};
use crate::proxy;
use eframe::egui;
use std::time::Instant;

pub fn provider_section(
    ui: &mut egui::Ui,
    config: &mut AppConfig,
    selected: &mut Option<usize>,
    message: &mut AppMessage,
    mapping_drafts: &mut Vec<TextDraft>,
    header_drafts: &mut Vec<TextDraft>,
    keepalive_status: Option<&proxy::KeepaliveStatusHandle>,
    circuit_status: Option<&proxy::ProviderCircuitStatusHandle>,
) {
    resize_drafts(mapping_drafts, config.providers.len());
    resize_drafts(header_drafts, config.providers.len());

    if config.providers.is_empty() {
        section(ui, "渠道", |ui| {
            ui.label(egui::RichText::new("暂无渠道").color(muteds()));
            if primary_button(ui, "新增渠道").clicked() {
                config.providers.push(default_provider());
                *selected = Some(0);
            }
        });
        return;
    }

    let mut idx = selected.unwrap_or(0).min(config.providers.len() - 1);
    let list_width = 280.0;
    let gap = 14.0;
    let detail_width = (ui.available_width() - list_width - gap).max(360.0);
    ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(list_width, ui.available_height()),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                provider_list_panel(ui, config, &mut idx);
            },
        );
        resize_drafts(mapping_drafts, config.providers.len());
        resize_drafts(header_drafts, config.providers.len());
        idx = idx.min(config.providers.len().saturating_sub(1));
        ui.add_space(gap);
        ui.allocate_ui_with_layout(
            egui::vec2(detail_width, ui.available_height()),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                if idx >= config.providers.len() {
                    ui.label("渠道索引无效");
                    return;
                }
                *selected = Some(idx);
                let provider = &mut config.providers[idx];
                egui::ScrollArea::vertical()
                    .id_source("provider_detail_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        provider_detail_panel(
                            ui,
                            provider,
                            message,
                            &mut mapping_drafts[idx],
                            &mut header_drafts[idx],
                            keepalive_status,
                            circuit_status,
                        );
                    });
            },
        );
    });
}

fn provider_list_panel(ui: &mut egui::Ui, config: &mut AppConfig, selected: &mut usize) {
    egui::Frame::none()
        .fill(surface())
        .stroke(egui::Stroke::new(1.0, border()))
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(12.0, 12.0))
        .show(ui, |ui| {
            ui.set_width(260.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("渠道列表")
                        .size(18.0)
                        .strong()
                        .color(heading_color()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if soft_button(ui, "新增").clicked() {
                        config.providers.push(default_provider());
                        *selected = config.providers.len() - 1;
                    }
                });
            });
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (idx, provider) in config.providers.iter_mut().enumerate() {
                        let active = idx == *selected;
                        let fill = if active {
                            egui::Color32::from_rgb(239, 246, 255)
                        } else {
                            surface()
                        };
                        let stroke = if active {
                            egui::Stroke::new(1.0, egui::Color32::from_rgb(191, 219, 254))
                        } else {
                            egui::Stroke::new(1.0, border())
                        };
                        let response = egui::Frame::none()
                            .fill(fill)
                            .stroke(stroke)
                            .rounding(6.0)
                            .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    switch(ui, &mut provider.enabled);
                                    ui.vertical(|ui| {
                                        ui.label(
                                            egui::RichText::new(&provider.name)
                                                .size(14.0)
                                                .strong()
                                                .color(if active {
                                                    accent()
                                                } else {
                                                    text_color()
                                                }),
                                        );
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{} · {} models",
                                                provider.provider_type,
                                                provider.models.len()
                                            ))
                                            .size(12.0)
                                            .color(muteds()),
                                        );
                                    });
                                });
                            })
                            .response;
                        if ui
                            .interact(response.rect, response.id, egui::Sense::click())
                            .clicked()
                        {
                            *selected = idx;
                        }
                        ui.add_space(6.0);
                    }
                });
        });
}

fn provider_detail_panel(
    ui: &mut egui::Ui,
    provider: &mut ProviderConfig,
    message: &mut AppMessage,
    mapping_draft: &mut TextDraft,
    header_draft: &mut TextDraft,
    keepalive_status: Option<&proxy::KeepaliveStatusHandle>,
    circuit_status: Option<&proxy::ProviderCircuitStatusHandle>,
) {
    section(ui, "渠道详情", |ui| {
        ui.horizontal_wrapped(|ui| {
            switch(ui, &mut provider.enabled);
            ui.label("启用");
            badge(
                ui,
                &provider.provider_type,
                egui::Color32::from_rgb(239, 246, 255),
                accent(),
            );
            badge(
                ui,
                &format!("{} models", provider.models.len()),
                egui::Color32::from_rgb(248, 250, 252),
                text_color(),
            );
        });
        ui.add_space(10.0);

        form_group(ui, "基础信息", |ui| {
            egui::Grid::new("provider_basic_form")
                .num_columns(2)
                .spacing(egui::vec2(14.0, 10.0))
                .show(ui, |ui| {
                    form_label(ui, "名称");
                    ui.text_edit_singleline(&mut provider.name);
                    ui.end_row();

                    form_label(ui, "类型");
                    egui::ComboBox::from_id_source("provider_type")
                        .selected_text(&provider.provider_type)
                        .show_ui(ui, |ui| {
                            for item in [
                                "openai",
                                "google_ai_studio",
                                "anthropic",
                                "codex_only",
                                "custom",
                            ] {
                                ui.selectable_value(
                                    &mut provider.provider_type,
                                    item.to_string(),
                                    item,
                                );
                            }
                        });
                    ui.end_row();

                    form_label(ui, "Base URL");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut provider.base_url);
                        copy_icon_button(ui, &provider.base_url)
                            .on_hover_text("复制 Provider Base URL");
                    });
                    ui.end_row();

                    form_label(ui, "API Key");
                    ui.horizontal(|ui| {
                        let visibility_id = egui::Id::new((
                            "provider_api_key_visible",
                            provider.name.as_str(),
                            provider.base_url.as_str(),
                        ));
                        let mut visible = ui
                            .ctx()
                            .data_mut(|data| data.get_temp::<bool>(visibility_id).unwrap_or(false));

                        ui.add(
                            egui::TextEdit::singleline(&mut provider.api_key)
                                .password(!visible)
                                .desired_width(260.0),
                        );

                        let (icon_name, icon_bytes, tooltip) = if visible {
                            (
                                "eye-disable",
                                include_bytes!("../../assets/eye_disable.png").as_slice(),
                                "隐藏 API Key",
                            )
                        } else {
                            (
                                "eye-able",
                                include_bytes!("../../assets/eye_able.png").as_slice(),
                                "显示 API Key",
                            )
                        };

                        let clicked = if let Some(texture) =
                            cached_png_texture(ui.ctx(), icon_name, icon_bytes)
                        {
                            let image = egui::Image::new(&texture)
                                .fit_to_exact_size(egui::vec2(18.0, 18.0));
                            ui.add(egui::Button::image(image).min_size(egui::vec2(30.0, 30.0)))
                                .on_hover_text(tooltip)
                                .clicked()
                        } else {
                            ui.small_button(if visible { "隐藏" } else { "显示" })
                                .on_hover_text(tooltip)
                                .clicked()
                        };

                        if clicked {
                            visible = !visible;
                            ui.ctx()
                                .data_mut(|data| data.insert_temp(visibility_id, visible));
                        }
                    });
                    ui.end_row();
                });
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                form_label(ui, "运行状态");
                if let Some(status_handle) = keepalive_status {
                    let status = status_handle.read().get(&provider.name).cloned();
                    match status {
                        Some(status) => {
                            if let Some(success) = status.last_success {
                                ui.colored_label(good(), format!("最近成功 {}", success));
                            } else {
                                ui.label(egui::RichText::new("等待首次心跳").color(muteds()));
                            }
                            if let Some(error) = status.last_error {
                                ui.colored_label(
                                    egui::Color32::from_rgb(176, 54, 64),
                                    format!("最近失败: {}", error),
                                );
                            }
                        }
                        None => {
                            ui.label(egui::RichText::new("等待首次心跳").color(muteds()));
                        }
                    }
                } else {
                    ui.label(egui::RichText::new("代理未启动").color(muteds()));
                }
            });
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                form_label(ui, "熔断状态");
                provider_circuit_detail(ui, circuit_status, &provider.name);
            });
        });

        ui.add_space(12.0);
        form_group(ui, "路由参数", |ui| {
            egui::Grid::new("provider_route_form")
                .num_columns(4)
                .spacing(egui::vec2(14.0, 10.0))
                .show(ui, |ui| {
                    form_label(ui, "Priority");
                    ui.add(egui::DragValue::new(&mut provider.priority).speed(1));
                    form_label(ui, "Weight");
                    ui.add(egui::DragValue::new(&mut provider.weight).range(1..=100));
                    ui.end_row();

                    form_label(ui, "Connect");
                    ui.add(egui::DragValue::new(&mut provider.connect_timeout).range(1..=120));
                    form_label(ui, "Request");
                    ui.add(egui::DragValue::new(&mut provider.request_timeout).range(1..=600));
                    ui.end_row();

                    form_label(ui, "Retries");
                    ui.add(egui::DragValue::new(&mut provider.max_retries).range(0..=50));
                    form_label(ui, "Stream Idle");
                    ui.add(egui::DragValue::new(&mut provider.stream_idle_timeout).range(0..=3600));
                    ui.end_row();

                    form_label(ui, "Stream Max");
                    ui.add(
                        egui::DragValue::new(&mut provider.stream_max_duration).range(0..=86400),
                    );
                    ui.label(egui::RichText::new("0 表示关闭").color(muteds()));
                    ui.label("");
                    ui.end_row();

                    form_label(ui, "Responses");
                    egui::ComboBox::from_id_source("responses_mode")
                        .selected_text(&provider.responses_mode)
                        .show_ui(ui, |ui| {
                            for item in ["auto", "native", "chat"] {
                                ui.selectable_value(
                                    &mut provider.responses_mode,
                                    item.to_string(),
                                    item,
                                );
                            }
                        });
                    form_label(ui, "Strip Thought");
                    ui.horizontal(|ui| {
                        switch(ui, &mut provider.strip_thought);
                        ui.label("过滤");
                    });
                    ui.end_row();

                    form_label(ui, "抓取 SSE");
                    ui.horizontal(|ui| {
                        switch(ui, &mut provider.debug_capture_sse);
                        ui.label("启用");
                    });
                    form_label(ui, "保留事件");
                    ui.add(
                        egui::DragValue::new(&mut provider.debug_sse_max_events).range(10..=500),
                    );
                    ui.end_row();

                    form_label(ui, "抓取目录");
                    ui.add(
                        egui::TextEdit::singleline(&mut provider.debug_sse_path)
                            .desired_width(220.0),
                    );
                    ui.label(egui::RichText::new("排查后建议关闭").color(muteds()));
                    ui.label("");
                    ui.end_row();
                });
        });

        ui.add_space(12.0);
        form_group(ui, "连接保活", |ui| {
            egui::Grid::new("provider_keepalive_form")
                .num_columns(4)
                .spacing(egui::vec2(14.0, 10.0))
                .show(ui, |ui| {
                    form_label(ui, "心跳");
                    ui.horizontal(|ui| {
                        switch(ui, &mut provider.persist_keepalive);
                        ui.label("启用");
                    });
                    form_label(ui, "间隔(秒)");
                    ui.add(
                        egui::DragValue::new(&mut provider.persist_keepalive_interval)
                            .range(5..=86400),
                    );
                    ui.end_row();

                    form_label(ui, "模型");
                    let model = provider
                        .persist_keepalive_model
                        .get_or_insert_with(String::new);
                    ui.add(egui::TextEdit::singleline(model).desired_width(220.0));
                    form_label(ui, "提示词");
                    ui.add(
                        egui::TextEdit::singleline(&mut provider.persist_keepalive_prompt)
                            .desired_width(220.0),
                    );
                    ui.end_row();
                });
        });

        ui.add_space(12.0);
        form_group(ui, "模型与转发", |ui| {
            ui.label(
                egui::RichText::new("系统提示词覆盖")
                    .size(12.0)
                    .color(muteds()),
            );
            ui.text_edit_multiline(
                provider
                    .system_prompt_override
                    .get_or_insert_with(String::new),
            );

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("模型列表，每行一个")
                        .size(12.0)
                        .color(muteds()),
                );
                if soft_button(ui, "同步上游模型").clicked() {
                    match sync_upstream_models(provider) {
                        Ok(models) => {
                            let count = models.len();
                            provider.models = models;
                            *message = AppMessage::new(
                                format!("已同步渠道 [{}] 的 {} 个模型", provider.name, count),
                                MessageKind::Success,
                            );
                        }
                        Err(err) => {
                            *message = AppMessage::new(
                                format!("同步渠道 [{}] 模型失败: {err}", provider.name),
                                MessageKind::Error,
                            );
                        }
                    }
                }
            });
            let mut models_text = provider.models.join("\n");
            if ui
                .add(egui::TextEdit::multiline(&mut models_text).desired_rows(9))
                .changed()
            {
                provider.models = models_text
                    .lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
                    .collect();
            }

            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("模型映射，本地=上游，每行一个")
                    .size(12.0)
                    .color(muteds()),
            );
            sync_draft_from_source(mapping_draft, serialize_model_mapping(provider));
            if ui
                .add(egui::TextEdit::multiline(&mut mapping_draft.text).desired_rows(5))
                .changed()
            {
                provider.model_mapping.clear();
                for line in mapping_draft.text.lines() {
                    if let Some((k, v)) = line.split_once('=') {
                        let k = k.trim();
                        let v = v.trim();
                        if !k.is_empty() && !v.is_empty() {
                            provider.model_mapping.insert(k.to_string(), v.to_string());
                        }
                    }
                }
                mapping_draft.source = serialize_model_mapping(provider);
            }

            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("额外 Headers，Name=Value，每行一个")
                    .size(12.0)
                    .color(muteds()),
            );
            sync_draft_from_source(header_draft, serialize_extra_headers(provider));
            if ui
                .add(egui::TextEdit::multiline(&mut header_draft.text).desired_rows(4))
                .changed()
            {
                provider.extra_headers.clear();
                for line in header_draft.text.lines() {
                    if let Some((k, v)) = line.split_once('=') {
                        let k = k.trim();
                        let v = v.trim();
                        if !k.is_empty() && !v.is_empty() {
                            provider.extra_headers.insert(k.to_string(), v.to_string());
                        }
                    }
                }
                header_draft.source = serialize_extra_headers(provider);
            }
        });
    });
}

fn soft_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).color(text_color()))
            .fill(egui::Color32::from_rgb(244, 247, 251))
            .rounding(6.0)
            .min_size(egui::vec2(78.0, 32.0)),
    )
}

fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            egui::RichText::new(text)
                .strong()
                .color(egui::Color32::WHITE),
        )
        .fill(accent())
        .rounding(6.0)
        .min_size(egui::vec2(86.0, 32.0)),
    )
}

fn provider_circuit_detail(
    ui: &mut egui::Ui,
    circuit_status: Option<&proxy::ProviderCircuitStatusHandle>,
    provider_name: &str,
) {
    let Some(status_handle) = circuit_status else {
        ui.label(egui::RichText::new("代理未启动").color(muteds()));
        return;
    };
    let status = status_handle.read().get(provider_name).cloned();
    let Some(status) = status else {
        ui.label(egui::RichText::new("等待首次请求").color(muteds()));
        return;
    };

    match status.status {
        proxy::ProviderCircuitStatus::Healthy => {
            if status.failures == 0 {
                ui.colored_label(good(), format!("渠道健康，当前并发 {}", status.inflight));
            } else {
                ui.colored_label(
                    egui::Color32::from_rgb(190, 120, 24),
                    format!(
                        "渠道可用，连续失败 {}/3，当前并发 {}",
                        status.failures, status.inflight
                    ),
                );
            }
        }
        proxy::ProviderCircuitStatus::Open => match status.open_until {
            Some(until) if until > Instant::now() => {
                let remaining = until
                    .saturating_duration_since(Instant::now())
                    .as_secs()
                    .max(1);
                ui.colored_label(
                    egui::Color32::from_rgb(176, 54, 64),
                    format!("渠道已熔断，约 {remaining} 秒后探测"),
                );
            }
            _ => {
                ui.colored_label(
                    egui::Color32::from_rgb(190, 120, 24),
                    "冷却已结束，等待半开探测",
                );
            }
        },
        proxy::ProviderCircuitStatus::HalfOpen => {
            ui.colored_label(
                egui::Color32::from_rgb(190, 120, 24),
                format!("渠道半开探测中，当前并发 {}", status.inflight),
            );
        }
    }

    if let Some(error) = status.last_error {
        ui.colored_label(
            egui::Color32::from_rgb(176, 54, 64),
            format!("最近请求失败: {}", error),
        );
    }
}

fn resize_drafts(drafts: &mut Vec<TextDraft>, len: usize) {
    drafts.resize_with(len, TextDraft::default);
}

fn sync_draft_from_source(draft: &mut TextDraft, source: String) {
    if draft.text.is_empty() && draft.source.is_empty() {
        draft.text = source.clone();
        draft.source = source;
        return;
    }
    if draft.source != source && draft.text == draft.source {
        draft.text = source.clone();
        draft.source = source;
    }
}

fn serialize_model_mapping(provider: &ProviderConfig) -> String {
    let mut entries: Vec<_> = provider.model_mapping.iter().collect();
    entries.sort_by_key(|(key, _)| *key);
    entries
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn serialize_extra_headers(provider: &ProviderConfig) -> String {
    let mut entries: Vec<_> = provider.extra_headers.iter().collect();
    entries.sort_by_key(|(key, _)| *key);
    entries
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn sync_upstream_models(provider: &ProviderConfig) -> Result<Vec<String>, String> {
    let url = format!("{}/models", provider.base_url.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(
            provider.connect_timeout.max(1),
        ))
        .timeout(std::time::Duration::from_secs(
            provider.request_timeout.max(1),
        ))
        .build()
        .map_err(|err| err.to_string())?;

    let mut request = client.get(&url);
    if provider.provider_type == "anthropic" {
        request = request
            .header("x-api-key", &provider.api_key)
            .header("anthropic-version", "2023-06-01");
    } else if provider.provider_type == "google_ai_studio"
        && !crate::proxy::is_google_openai_endpoint(&provider.base_url)
    {
        request = request.header("x-goog-api-key", &provider.api_key);
    } else {
        request = request.bearer_auth(&provider.api_key);
    }
    for (name, value) in &provider.extra_headers {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "authorization"
                | "content-type"
                | "accept"
                | "host"
                | "content-length"
                | "x-api-key"
                | "x-goog-api-key"
        ) {
            continue;
        }
        request = request.header(name, value);
    }

    let response = request
        .send()
        .map_err(|err| format_model_sync_request_error(&url, &err))?;
    let status = response.status();
    let payload: serde_json::Value = response.json().map_err(|err| err.to_string())?;
    if !status.is_success() {
        return Err(format!("上游返回 {status}: {payload}"));
    }

    let mut models = Vec::new();
    if let Some(data) = payload.get("data").and_then(|value| value.as_array()) {
        for item in data {
            if let Some(id) = item
                .get("id")
                .or_else(|| item.get("name"))
                .and_then(|value| value.as_str())
            {
                push_unique(&mut models, id);
            }
        }
    } else if let Some(data) = payload.get("models").and_then(|value| value.as_array()) {
        for item in data {
            if let Some(id) = item.as_str().or_else(|| {
                item.get("id")
                    .or_else(|| item.get("name"))
                    .and_then(|value| value.as_str())
            }) {
                push_unique(&mut models, id.trim_start_matches("models/"));
            }
        }
    }

    if models.is_empty() {
        Err("响应中没有可识别的模型列表".to_string())
    } else {
        models.sort();
        Ok(models)
    }
}

fn format_model_sync_request_error(url: &str, err: &reqwest::Error) -> String {
    let mut parts = vec![format!("请求失败: {url}")];
    if err.is_timeout() {
        parts.push("连接或响应超时".to_string());
    } else if err.is_connect() {
        parts.push("连接上游失败".to_string());
    } else if err.is_request() {
        parts.push("请求构造失败".to_string());
    }
    parts.push(err.to_string());
    if let Some(source) = std::error::Error::source(err) {
        parts.push(format!("source: {source}"));
    }
    if url.contains("api.openai.com") {
        parts.push("如果当前网络无法直连 OpenAI，请为桌面应用进程配置 HTTP_PROXY/HTTPS_PROXY，或使用可直连的中转 base_url。".to_string());
    }
    parts.join("；")
}

fn push_unique(models: &mut Vec<String>, id: &str) {
    let id = id.trim();
    if !id.is_empty() && !models.iter().any(|item| item == id) {
        models.push(id.to_string());
    }
}

pub fn default_provider() -> ProviderConfig {
    ProviderConfig {
        name: "new-provider".to_string(),
        enabled: false,
        provider_type: "openai".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        website: None,
        api_key: String::new(),
        models: vec!["gpt-5.5".to_string()],
        model_mapping: Default::default(),
        extra_headers: Default::default(),
        capabilities: Default::default(),
        health_check_mode: "models".to_string(),
        model_sync_filter: "all".to_string(),
        responses_mode: "auto".to_string(),
        client_mode: "normal".to_string(),
        connect_timeout: 10,
        request_timeout: 60,
        stream_idle_timeout: 0,
        stream_max_duration: 0,
        debug_capture_sse: false,
        debug_sse_path: "logs/raw_sse".to_string(),
        debug_sse_max_events: 80,
        max_retries: 3,
        weight: 1,
        priority: 1,
        description: None,
        system_prompt_override: None,
        strip_thought: false,
        persistent_session: false,
        persist_interval: 3.0,
        persist_max_wait: 0,
        persist_keepalive: false,
        persist_keepalive_interval: 30,
        persist_keepalive_model: None,
        persist_keepalive_prompt: "Hi".to_string(),
        extra: Default::default(),
    }
}

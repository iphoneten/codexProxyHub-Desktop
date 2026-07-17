use super::common::{
    base_url, copy_icon_button, display_host, field_icon, muteds, section, switch,
};
use super::RoutingDraft;
use crate::config::AppConfig;
use eframe::egui;
use std::collections::HashMap;

pub fn server_section(ui: &mut egui::Ui, config: &mut AppConfig) {
    section(ui, "服务", |ui| {
        ui.horizontal(|ui| {
            field_icon(ui, "network");
            ui.label("Host");
            ui.add(egui::TextEdit::singleline(&mut config.server.host).desired_width(180.0));
            ui.add_space(8.0);
            field_icon(ui, "plug");
            ui.label("Port");
            let mut port = config.server.port as i64;
            if ui
                .add(egui::DragValue::new(&mut port).range(1..=65535))
                .changed()
            {
                config.server.port = port as u16;
            }
        });
        let url = base_url(config);
        ui.horizontal(|ui| {
            ui.label("Base URL");
            let mut readonly_url = url.clone();
            ui.add_enabled(
                false,
                egui::TextEdit::singleline(&mut readonly_url).desired_width(320.0),
            );
            copy_icon_button(ui, &url).on_hover_text("复制 Base URL");
        });
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            switch(ui, &mut config.web.enabled);
            ui.label("用户 Web");
            ui.colored_label(muteds(), "与代理共用端口，挂载在 /user");
        });
        ui.horizontal(|ui| {
            ui.label("会话时长");
            ui.add(
                egui::DragValue::new(&mut config.web.session_ttl_hours)
                    .range(1..=24 * 30)
                    .suffix(" 小时"),
            );
            if config.web.enabled {
                let web_url = format!(
                    "http://{}:{}/user",
                    display_host(&config.server.host),
                    config.server.port
                );
                ui.add_space(10.0);
                ui.label("用户地址");
                let mut readonly_url = web_url.clone();
                ui.add_enabled(
                    false,
                    egui::TextEdit::singleline(&mut readonly_url).desired_width(260.0),
                );
                copy_icon_button(ui, &web_url).on_hover_text("复制用户 Web 地址");
            }
        });
    });
}

pub fn routing_section(
    ui: &mut egui::Ui,
    config: &mut AppConfig,
    drafts: &mut Vec<RoutingDraft>,
    drafts_source: &mut String,
) {
    section(ui, "模型映射", |ui| {
        ui.set_max_width(640.0);
        let source = routing_source_signature(&config.routing.model_fallbacks);
        if drafts.is_empty() && !config.routing.model_fallbacks.is_empty()
            || *drafts_source != source
        {
            *drafts = config
                .routing
                .model_fallbacks
                .iter()
                .map(|(k, v)| RoutingDraft {
                    key: k.clone(),
                    value: v.join(", "),
                })
                .collect();
            drafts.sort_by(|a, b| a.key.cmp(&b.key));
            *drafts_source = source;
        }

        if drafts.is_empty() {
            ui.label("未配置映射");
        } else {
            ui.horizontal(|ui| {
                ui.add_sized(
                    egui::vec2(180.0, 20.0),
                    egui::Label::new(egui::RichText::new("源模型").small().color(muteds())),
                );
                ui.add_space(20.0);
                ui.add_sized(
                    egui::vec2(260.0, 20.0),
                    egui::Label::new(
                        egui::RichText::new("映射后模型 (逗号分隔)")
                            .small()
                            .color(muteds()),
                    ),
                );
            });
        }

        let mut to_remove: Option<usize> = None;
        for (idx, draft) in drafts.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut draft.key).desired_width(180.0));
                ui.label("→");
                ui.add(egui::TextEdit::singleline(&mut draft.value).desired_width(260.0));
                if ui.button("删除").clicked() {
                    to_remove = Some(idx);
                }
            });
        }
        if let Some(idx) = to_remove {
            drafts.remove(idx);
        }
        if ui.button("新增映射").clicked() {
            drafts.push(RoutingDraft::default());
        }

        let mut next = HashMap::new();
        for draft in drafts.iter() {
            let key = draft.key.trim().to_string();
            if key.is_empty() {
                continue;
            }
            let values: Vec<String> = draft
                .value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect();
            next.insert(key, values);
        }
        if config.routing.model_fallbacks != next {
            config.routing.model_fallbacks = next;
            *drafts_source = routing_source_signature(&config.routing.model_fallbacks);
        }
    });
}

fn routing_source_signature(map: &HashMap<String, Vec<String>>) -> String {
    let mut items: Vec<(&String, &Vec<String>)> = map.iter().collect();
    items.sort_by(|a, b| a.0.cmp(b.0));
    let mut buf = String::new();
    for (k, v) in items {
        buf.push_str(k);
        buf.push('=');
        buf.push_str(&v.join(","));
        buf.push('\n');
    }
    buf
}

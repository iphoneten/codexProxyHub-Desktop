use super::assets::cached_png_texture;
use super::common::{
    base_url, confirm_delete_button, copy_icon_button, display_host, field_icon, muteds, section,
    switch,
};
use super::RoutingDraft;
use crate::config::AppConfig;
use eframe::egui;
use std::collections::HashMap;

pub fn auth_models_section(ui: &mut egui::Ui, config: &mut AppConfig) {
    section(ui, "Auth 模型", |ui| {
        auth_models_editor(
            ui,
            "OpenAI",
            &mut config.auth.openai_models,
            "gpt-5.4,gpt-5.5,gpt-5.6-luna,gpt-5.6-sol,gpt-5.6-terra",
        );
        ui.add_space(10.0);
        auth_models_editor(ui, "Grok", &mut config.auth.grok_models, "grok-4.5");
    });
}

fn auth_models_editor(ui: &mut egui::Ui, label: &str, models: &mut Vec<String>, hint: &str) {
    ui.horizontal(|ui| {
        ui.label(label);
        let mut text = models.join(",");
        if ui
            .add(
                egui::TextEdit::singleline(&mut text)
                    .desired_width(440.0)
                    .hint_text(hint),
            )
            .changed()
        {
            *models = text
                .split(',')
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_string)
                .collect();
        }
    });
}

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
            ui.label("Web 控制台");
            ui.colored_label(muteds(), "与代理共用端口，挂载在 /user 和 /admin");
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
        if config.web.enabled {
            let admin_url = format!(
                "http://{}:{}/admin",
                display_host(&config.server.host),
                config.server.port
            );
            ui.horizontal(|ui| {
                ui.label("管理地址");
                let mut readonly_admin_url = admin_url.clone();
                ui.add_enabled(
                    false,
                    egui::TextEdit::singleline(&mut readonly_admin_url).desired_width(260.0),
                );
                copy_icon_button(ui, &admin_url).on_hover_text("复制管理 Web 地址");
            });
        }
        ui.horizontal(|ui| {
            ui.label("Admin Key");
            let mut admin_key = config.auth.admin_key.clone().unwrap_or_default();
            let visibility_id = egui::Id::new("admin_key_visible");
            let mut visible = ui
                .ctx()
                .data_mut(|data| data.get_temp::<bool>(visibility_id).unwrap_or(false));
            let response = ui.add(
                egui::TextEdit::singleline(&mut admin_key)
                    .password(!visible)
                    .desired_width(260.0),
            );
            if response.changed() {
                config.auth.admin_key = if admin_key.trim().is_empty() {
                    None
                } else {
                    Some(admin_key.clone())
                };
            }

            let (icon_name, icon_bytes, tooltip) = if visible {
                (
                    "admin-eye-disable",
                    include_bytes!("../../assets/eye_disable.png").as_slice(),
                    "隐藏 Admin Key",
                )
            } else {
                (
                    "admin-eye-able",
                    include_bytes!("../../assets/eye_able.png").as_slice(),
                    "显示 Admin Key",
                )
            };
            let clicked = if let Some(texture) = cached_png_texture(ui.ctx(), icon_name, icon_bytes)
            {
                let image = egui::Image::new(&texture).fit_to_exact_size(egui::vec2(18.0, 18.0));
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
            if !admin_key.is_empty() {
                copy_icon_button(ui, &admin_key).on_hover_text("复制 Admin Key");
            }
            ui.colored_label(muteds(), "用于 /admin 登录，留空则禁用");
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
        ui.horizontal(|ui| {
            ui.label("请求优先级");
            ui.radio_value(
                &mut config.routing.auth_preference,
                "provider_first".to_string(),
                "渠道优先",
            );
            ui.radio_value(
                &mut config.routing.auth_preference,
                "auth_first".to_string(),
                "账号优先",
            );
        });
        ui.colored_label(
            muteds(),
            "账号优先：先走 Auth 账号（使用 Auth 代理）；渠道优先：先普通渠道。各组内部仍按 priority/weight 排序。",
        );
        ui.add_space(10.0);

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
                if confirm_delete_button(ui, ("routing_delete", idx, draft.key.as_str()), "删除")
                {
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

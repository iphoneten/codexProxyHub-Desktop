use super::common::{section, switch};
use crate::config::{ApiKeyConfig, AppConfig};
use eframe::egui;
use std::collections::{HashMap, HashSet};

fn mask_api_key(key: &str) -> String {
    let len = key.chars().count();
    if len <= 8 {
        return "*".repeat(len.max(4));
    }
    let suffix: String = key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("sk-...{}", suffix)
}

/// 生成随机 API Key，格式与既有配置保持一致：`sk-proxy-` + 32 位小写字母数字。
pub fn generate_api_key() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    let suffix: String = (0..32)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect();
    format!("sk-proxy-{}", suffix)
}

pub fn auth_section(ui: &mut egui::Ui, config: &mut AppConfig, new_key_name: &mut String) {
    let selectable_models = selectable_allowed_models(config);
    let selectable_providers = selectable_allowed_providers(config);
    section(ui, "鉴权", |ui| {
        ui.horizontal(|ui| {
            switch(ui, &mut config.auth.enabled);
            ui.label("启用 API 秘钥鉴权");
        });
        ui.horizontal(|ui| {
            ui.label("新秘钥名称");
            ui.add(egui::TextEdit::singleline(new_key_name).desired_width(220.0));
            if ui.button("添加").clicked() && !new_key_name.trim().is_empty() {
                let name = new_key_name.trim().to_string();
                let final_name = if name.is_empty() {
                    format!("key-{}", config.auth.api_keys.len() + 1)
                } else {
                    name
                };
                config.auth.api_keys.push(ApiKeyConfig {
                    key: generate_api_key(),
                    name: final_name,
                    enabled: true,
                    created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                    max_concurrency: Some(5),
                    allowed_models: Vec::new(),
                    allowed_providers: Vec::new(),
                });
                new_key_name.clear();
            }
        });

        egui::Grid::new("keys")
            .striped(true)
            .spacing(egui::vec2(14.0, 8.0))
            .show(ui, |ui| {
                ui.label("启用");
                ui.label("名称");
                ui.label("并发");
                ui.label("允许模型");
                ui.label("允许渠道");
                ui.label("API 秘钥");
                ui.label("创建时间");
                ui.label("操作");
                ui.end_row();
                let mut remove = None;
                for (idx, key) in config.auth.api_keys.iter_mut().enumerate() {
                    switch(ui, &mut key.enabled);
                    ui.add_sized([120.0, 24.0], egui::TextEdit::singleline(&mut key.name));
                    let max_concurrency = key.max_concurrency.get_or_insert(5);
                    ui.add(
                        egui::DragValue::new(max_concurrency)
                            .range(1..=500)
                            .speed(1),
                    );
                    allowed_models_selector(ui, idx, key, &selectable_models);
                    allowed_providers_selector(ui, idx, key, &selectable_providers);
                    ui.horizontal(|ui| {
                        ui.monospace(mask_api_key(&key.key));
                        if ui
                            .small_button("📋")
                            .on_hover_text("复制 API 秘钥")
                            .clicked()
                        {
                            ui.output_mut(|output| {
                                output.copied_text = key.key.clone();
                            });
                        }
                    });
                    let date_str = if key.created_at.len() >= 10 {
                        &key.created_at[0..10]
                    } else if key.created_at.trim().is_empty() {
                        "-"
                    } else {
                        key.created_at.as_str()
                    };
                    ui.label(date_str).on_hover_text(&key.created_at);
                    ui.horizontal(|ui| {
                        let button = egui::Button::new(
                            egui::RichText::new("删除").color(egui::Color32::from_rgb(239, 68, 68)),
                        );
                        if ui.add(button).clicked() {
                            remove = Some(idx);
                        }
                    });
                    ui.end_row();
                }
                if let Some(idx) = remove {
                    config.auth.api_keys.remove(idx);
                }
            });
    });
}

fn allowed_models_selector(
    ui: &mut egui::Ui,
    index: usize,
    key: &mut ApiKeyConfig,
    selectable_models: &[String],
) {
    let all_allowed = api_key_allows_all_models(&key.allowed_models);
    let selected_text = if all_allowed {
        "全部模型".to_string()
    } else {
        format!("已选 {} 个", key.allowed_models.len())
    };
    let popup_id = ui.make_persistent_id(("allowed_models_popup", index));
    let response = ui.add_sized(
        [120.0, 24.0],
        egui::Button::new(format!("{selected_text}  v")),
    );
    if response.clicked() {
        ui.memory_mut(|memory| memory.toggle_popup(popup_id));
    }
    egui::popup::popup_below_widget(
        ui,
        popup_id,
        &response,
        egui::popup::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            ui.set_min_width(220.0);
            let mut select_all = api_key_allows_all_models(&key.allowed_models);
            if ui.checkbox(&mut select_all, "全部模型").changed() {
                if select_all {
                    key.allowed_models.clear();
                } else {
                    key.allowed_models = selectable_models.to_vec();
                }
            }
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(280.0)
                .show(ui, |ui| {
                    for model in selectable_models {
                        let all_selected = api_key_allows_all_models(&key.allowed_models);
                        let mut selected = all_selected
                            || key
                                .allowed_models
                                .iter()
                                .any(|allowed| model_names_match(allowed, model));
                        if ui.checkbox(&mut selected, model).changed() {
                            if all_selected {
                                key.allowed_models = selectable_models
                                    .iter()
                                    .filter(|candidate| !model_names_match(candidate, model))
                                    .cloned()
                                    .collect();
                            } else if selected {
                                if !key
                                    .allowed_models
                                    .iter()
                                    .any(|allowed| model_names_match(allowed, model))
                                {
                                    key.allowed_models.push(model.clone());
                                }
                            } else {
                                key.allowed_models
                                    .retain(|allowed| !model_names_match(allowed, model));
                            }
                        }
                    }
                });
        },
    );
}

fn api_key_allows_all_models(allowed_models: &[String]) -> bool {
    allowed_models.is_empty() || allowed_models.iter().any(|model| model.trim() == "*")
}

fn allowed_providers_selector(
    ui: &mut egui::Ui,
    index: usize,
    key: &mut ApiKeyConfig,
    selectable_providers: &[(String, bool)],
) {
    let enabled_providers = selectable_providers
        .iter()
        .filter(|(_, enabled)| *enabled)
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    let selected_enabled_count = key
        .allowed_providers
        .iter()
        .filter(|allowed| {
            enabled_providers
                .iter()
                .any(|provider| provider.trim() == allowed.trim())
        })
        .count();
    let all_allowed = api_key_allows_all_providers(&key.allowed_providers);
    let selected_text = if all_allowed {
        "全部渠道".to_string()
    } else {
        format!("已选 {} 个", selected_enabled_count)
    };
    let popup_id = ui.make_persistent_id(("allowed_providers_popup", index));
    let response = ui.add_sized(
        [110.0, 24.0],
        egui::Button::new(format!("{selected_text}  v")),
    );
    if response.clicked() {
        ui.memory_mut(|memory| memory.toggle_popup(popup_id));
    }
    egui::popup::popup_below_widget(
        ui,
        popup_id,
        &response,
        egui::popup::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            ui.set_min_width(200.0);
            let mut select_all = api_key_allows_all_providers(&key.allowed_providers);
            if ui.checkbox(&mut select_all, "全部渠道").changed() {
                if select_all {
                    key.allowed_providers.clear();
                } else {
                    key.allowed_providers = enabled_providers.clone();
                }
            }
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(280.0)
                .show(ui, |ui| {
                    for (provider, enabled) in selectable_providers {
                        let all_selected = api_key_allows_all_providers(&key.allowed_providers);
                        let mut selected = all_selected
                            || key
                                .allowed_providers
                                .iter()
                                .any(|allowed| allowed.trim() == provider.trim());
                        ui.add_enabled_ui(*enabled, |ui| {
                            let response = ui.checkbox(&mut selected, provider.as_str());
                            let changed = response.changed();
                            if !*enabled {
                                response.on_hover_text("渠道未启用，不能勾选");
                            }
                            if changed {
                                if all_selected {
                                    key.allowed_providers = enabled_providers
                                        .iter()
                                        .filter(|candidate| candidate.trim() != provider.trim())
                                        .cloned()
                                        .collect();
                                } else if selected {
                                    if !key
                                        .allowed_providers
                                        .iter()
                                        .any(|allowed| allowed.trim() == provider.trim())
                                    {
                                        key.allowed_providers.push(provider.clone());
                                    }
                                } else {
                                    key.allowed_providers
                                        .retain(|allowed| allowed.trim() != provider.trim());
                                }
                            }
                        });
                        if !*enabled && selected && !all_selected {
                            key.allowed_providers
                                .retain(|allowed| allowed.trim() != provider.trim());
                        }
                    }
                });
        },
    );
}

fn api_key_allows_all_providers(allowed_providers: &[String]) -> bool {
    allowed_providers.is_empty()
        || allowed_providers
            .iter()
            .any(|provider| provider.trim() == "*")
}

fn model_names_match(left: &str, right: &str) -> bool {
    left.trim().trim_start_matches("models/") == right.trim().trim_start_matches("models/")
}

pub fn selectable_allowed_models(config: &AppConfig) -> Vec<String> {
    let mut models = available_model_names(config)
        .into_iter()
        .collect::<HashSet<_>>();
    for key in &config.auth.api_keys {
        for model in &key.allowed_models {
            let model = model.trim();
            if !model.is_empty() && model != "*" {
                models.insert(model.to_string());
            }
        }
    }
    let mut models = models.into_iter().collect::<Vec<_>>();
    models.sort();
    models
}

pub fn selectable_allowed_providers(config: &AppConfig) -> Vec<(String, bool)> {
    let mut providers = config
        .providers
        .iter()
        .filter_map(|provider| {
            let name = provider.name.trim();
            if name.is_empty() {
                None
            } else {
                Some((name.to_string(), provider.enabled))
            }
        })
        .collect::<HashMap<_, _>>();
    for key in &config.auth.api_keys {
        for provider in &key.allowed_providers {
            let provider = provider.trim();
            if !provider.is_empty() && provider != "*" {
                providers.entry(provider.to_string()).or_insert(false);
            }
        }
    }
    let mut providers = providers.into_iter().collect::<Vec<_>>();
    providers.sort_by(|(left, _), (right, _)| left.cmp(right));
    providers
}

pub fn available_model_names(config: &AppConfig) -> Vec<String> {
    let mut models = HashSet::new();
    for provider in config.providers.iter().filter(|provider| provider.enabled) {
        for model in &provider.models {
            models.insert(model.trim().to_string());
        }
        for model in provider.model_mapping.keys() {
            models.insert(model.trim().to_string());
        }
    }
    models.remove("");
    let mut models = models.into_iter().collect::<Vec<_>>();
    models.sort();
    models
}

use super::common::{
    accent, badge, border, good, heading_color, muteds, primary_button, soft_button, surface,
    switch, text_color,
};
use super::{AppMessage, MessageKind};
use crate::auth_quota::{self, AuthQuotaSnapshot};
use crate::config::{AppConfig, AuthAccountConfig};
use eframe::egui;
use parking_lot::Mutex;
use std::{collections::HashMap, path::PathBuf, sync::Arc, thread, time::Instant};
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthAvailability {
    Unknown,
    Checking,
    Available,
    QuotaExhausted,
    Unavailable,
    Disabled,
}

#[derive(Clone, Default)]
struct AccountQuotaView {
    availability: AuthAvailability,
    plan_type: Option<String>,
    primary_used: Option<f64>,
    secondary_used: Option<f64>,
    code_review_used: Option<f64>,
    primary_reset: Option<String>,
    secondary_reset: Option<String>,
    code_review_reset: Option<String>,
    primary_window: Option<String>,
    secondary_window: Option<String>,
    code_review_window: Option<String>,
    checked_at: Option<String>,
    error: Option<String>,
    refreshing: bool,
}

impl Default for AuthAvailability {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Default)]
pub struct AuthAccountsState {
    views: HashMap<String, AccountQuotaView>,
    pending: Arc<Mutex<Vec<QuotaJobResult>>>,
}

impl AuthAccountsState {
    pub fn has_refreshing(&self) -> bool {
        self.views.values().any(|view| view.refreshing) || !self.pending.lock().is_empty()
    }
}

struct QuotaJobResult {
    account_id: String,
    result: Result<(AuthQuotaSnapshot, TokenUpdate), String>,
}

struct TokenUpdate {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<i64>,
}

pub fn auth_accounts_section(
    ui: &mut egui::Ui,
    config: &mut AppConfig,
    message: &mut AppMessage,
    state: &mut AuthAccountsState,
) {
    apply_pending(config, state, message);

    egui::Frame::none()
        .fill(surface())
        .stroke(egui::Stroke::new(1.0, border()))
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(14.0, 12.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Auth 账号")
                        .size(18.0)
                        .strong()
                        .color(heading_color()),
                );
                ui.label(
                    egui::RichText::new(format!("共 {} 个", config.auth_accounts.len()))
                        .color(muteds()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if primary_button(ui, "新增账号").clicked() {
                        config.auth_accounts.push(default_auth_account());
                    }
                    if soft_button(ui, "全部刷新额度").clicked() {
                        refresh_all_accounts(config, state, message);
                    }
                    if soft_button(ui, "导入账号").clicked() {
                        import_auth_accounts(config, message);
                    }
                    if soft_button(ui, "导出账号").clicked() {
                        export_auth_accounts(config, message);
                    }
                });
            });
        });
    ui.add_space(12.0);

    if config.auth_accounts.is_empty() {
        ui.label(
            egui::RichText::new("暂无 Auth 账号，可通过 CPA/sub2api JSON 导入或手动新增。")
                .color(muteds()),
        );
        return;
    }

    let mut remove = None;
    let available_width = ui.available_width();
    let columns = if available_width >= 1180.0 {
        3
    } else if available_width >= 780.0 {
        2
    } else {
        1
    };
    let gap = 12.0;
    let card_width = ((available_width - gap * (columns as f32 - 1.0)) / columns as f32).max(320.0);

    let mut idx = 0;
    while idx < config.auth_accounts.len() {
        ui.horizontal_top(|ui| {
            for column in 0..columns {
                let account_idx = idx + column;
                if account_idx >= config.auth_accounts.len() {
                    break;
                }
                let account = &mut config.auth_accounts[account_idx];
                let view = state.views.entry(account.id.clone()).or_default().clone();
                ui.allocate_ui_with_layout(
                    egui::vec2(card_width, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(card_width);
                        if account_card(ui, account, &view, state, message) {
                            remove = Some(account_idx);
                        }
                    },
                );
                if column + 1 < columns {
                    ui.add_space(gap);
                }
            }
        });
        idx += columns;
        if idx < config.auth_accounts.len() {
            ui.add_space(gap);
        }
    }

    if let Some(idx) = remove {
        let removed = config.auth_accounts.remove(idx);
        state.views.remove(&removed.id);
        *message = AppMessage::new(
            format!("已删除 Auth 账号: {}", removed.name),
            MessageKind::Info,
        );
    }
}

fn account_card(
    ui: &mut egui::Ui,
    account: &mut AuthAccountConfig,
    view: &AccountQuotaView,
    state: &mut AuthAccountsState,
    message: &mut AppMessage,
) -> bool {
    let mut delete = false;
    let availability = effective_availability(account, view);
    let (status_label, status_fill, status_color) = availability_style(availability);

    egui::Frame::none()
        .fill(surface())
        .stroke(egui::Stroke::new(1.0, border()))
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(14.0, 12.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                switch(ui, &mut account.enabled);
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(&account.name)
                            .size(15.0)
                            .strong()
                            .color(heading_color()),
                    );
                    let email = account
                        .email
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or("-");
                    ui.label(egui::RichText::new(email).size(12.0).color(muteds()));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    badge(ui, status_label, status_fill, status_color);
                    if let Some(plan) = view.plan_type.as_deref() {
                        badge(ui, plan, egui::Color32::from_rgb(239, 246, 255), accent());
                    }
                });
            });

            ui.add_space(10.0);
            usage_row(
                ui,
                "主额度",
                view.primary_used,
                view.primary_reset.as_deref(),
                view.primary_window.as_deref(),
            );
            usage_row(
                ui,
                "次额度",
                view.secondary_used,
                view.secondary_reset.as_deref(),
                view.secondary_window.as_deref(),
            );
            usage_row(
                ui,
                "代码审查",
                view.code_review_used,
                view.code_review_reset.as_deref(),
                view.code_review_window.as_deref(),
            );

            if let Some(error) = view.error.as_deref() {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(error)
                        .size(12.0)
                        .color(egui::Color32::from_rgb(220, 38, 38)),
                );
            } else if let Some(checked_at) = view.checked_at.as_deref() {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!("最近刷新 {checked_at}"))
                        .size(11.0)
                        .color(muteds()),
                );
            }

            ui.add_space(10.0);
            ui.collapsing("账号详情", |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("账号 ID").size(12.0).color(muteds()));
                    ui.add(
                        egui::TextEdit::singleline(
                            account.account_id.get_or_insert_with(String::new),
                        )
                        .desired_width(220.0)
                        .hint_text("ChatGPT Account ID"),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Access").size(12.0).color(muteds()));
                    ui.add(
                        egui::TextEdit::singleline(&mut account.access_token)
                            .password(true)
                            .desired_width(240.0)
                            .hint_text("access_token"),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Refresh").size(12.0).color(muteds()));
                    ui.add(
                        egui::TextEdit::singleline(
                            account.refresh_token.get_or_insert_with(String::new),
                        )
                        .password(true)
                        .desired_width(240.0)
                        .hint_text("refresh_token"),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("模型").size(12.0).color(muteds()));
                    models_editor(ui, account);
                });
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("优先级").size(12.0).color(muteds()));
                    ui.add(egui::DragValue::new(&mut account.priority).speed(1));
                    ui.label(egui::RichText::new("权重").size(12.0).color(muteds()));
                    ui.add(egui::DragValue::new(&mut account.weight).range(1..=100));
                });
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let refresh_label = if view.refreshing {
                    "刷新中..."
                } else {
                    "刷新额度"
                };
                if soft_button(ui, refresh_label).clicked() && !view.refreshing {
                    queue_refresh(account, state);
                    *message = AppMessage::new(
                        format!("正在刷新额度: {}", account.name),
                        MessageKind::Info,
                    );
                }
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("删除").color(egui::Color32::from_rgb(239, 68, 68)),
                        )
                        .fill(egui::Color32::from_rgb(254, 242, 242)),
                    )
                    .clicked()
                {
                    delete = true;
                }
            });
        });

    delete
}

fn usage_row(
    ui: &mut egui::Ui,
    label: &str,
    used: Option<f64>,
    reset: Option<&str>,
    window: Option<&str>,
) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(label)
                .size(12.0)
                .color(muteds())
                .strong(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let value = used
                .map(|value| format!("{value:.1}%"))
                .unwrap_or_else(|| "-".to_string());
            ui.label(egui::RichText::new(value).size(12.0).color(text_color()));
        });
    });
    let fraction = used.unwrap_or(0.0).clamp(0.0, 100.0) as f32 / 100.0;
    let color = if fraction >= 0.95 {
        egui::Color32::from_rgb(239, 68, 68)
    } else if fraction >= 0.8 {
        egui::Color32::from_rgb(245, 158, 11)
    } else {
        good()
    };
    let desired = egui::vec2(ui.available_width(), 8.0);
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 4.0, egui::Color32::from_rgb(241, 245, 249));
    if fraction > 0.0 {
        let mut filled = rect;
        filled.set_width(rect.width() * fraction);
        ui.painter().rect_filled(filled, 4.0, color);
    }
    let window = window
        .map(|value| format!(" · {value}"))
        .unwrap_or_default();
    ui.label(
        egui::RichText::new(format!("重置 {}{}", reset.unwrap_or("未检查"), window))
            .size(11.0)
            .color(muteds()),
    );
    ui.add_space(4.0);
}

fn format_window_seconds(seconds: i64) -> String {
    if seconds >= 86_400 {
        format!("{} 天窗口", seconds / 86_400)
    } else if seconds >= 3_600 {
        format!("{} 小时窗口", seconds / 3_600)
    } else if seconds >= 60 {
        format!("{} 分钟窗口", seconds / 60)
    } else {
        format!("{seconds} 秒窗口")
    }
}

fn effective_availability(
    account: &AuthAccountConfig,
    view: &AccountQuotaView,
) -> AuthAvailability {
    if !account.enabled {
        return AuthAvailability::Disabled;
    }
    if view.refreshing {
        return AuthAvailability::Checking;
    }
    view.availability
}

fn availability_style(
    availability: AuthAvailability,
) -> (&'static str, egui::Color32, egui::Color32) {
    match availability {
        AuthAvailability::Available => (
            "可用",
            egui::Color32::from_rgb(220, 252, 231),
            egui::Color32::from_rgb(22, 163, 74),
        ),
        AuthAvailability::QuotaExhausted => (
            "额度耗尽",
            egui::Color32::from_rgb(254, 243, 199),
            egui::Color32::from_rgb(217, 119, 6),
        ),
        AuthAvailability::Unavailable => (
            "不可用",
            egui::Color32::from_rgb(254, 226, 226),
            egui::Color32::from_rgb(220, 38, 38),
        ),
        AuthAvailability::Checking => ("检查中", egui::Color32::from_rgb(239, 246, 255), accent()),
        AuthAvailability::Disabled => ("已停用", egui::Color32::from_rgb(241, 245, 249), muteds()),
        AuthAvailability::Unknown => ("未检查", egui::Color32::from_rgb(241, 245, 249), muteds()),
    }
}

fn apply_pending(config: &mut AppConfig, state: &mut AuthAccountsState, message: &mut AppMessage) {
    let jobs = {
        let mut pending = state.pending.lock();
        std::mem::take(&mut *pending)
    };
    for job in jobs {
        let view = state.views.entry(job.account_id.clone()).or_default();
        view.refreshing = false;
        match job.result {
            Ok((snapshot, tokens)) => {
                if let Some(account) = config
                    .auth_accounts
                    .iter_mut()
                    .find(|account| account.id == job.account_id)
                {
                    account.access_token = tokens.access_token;
                    if tokens.refresh_token.is_some() {
                        account.refresh_token = tokens.refresh_token;
                    }
                    account.expires_at = tokens.expires_at;
                }
                view.plan_type = snapshot.plan_type;
                view.primary_used = snapshot.primary.as_ref().map(|item| item.used_percent);
                view.secondary_used = snapshot.secondary.as_ref().map(|item| item.used_percent);
                view.code_review_used = snapshot.code_review.as_ref().map(|item| item.used_percent);
                view.primary_reset = snapshot
                    .primary
                    .as_ref()
                    .map(|item| auth_quota::format_reset_at(item.reset_at));
                view.secondary_reset = snapshot
                    .secondary
                    .as_ref()
                    .map(|item| auth_quota::format_reset_at(item.reset_at));
                view.code_review_reset = snapshot
                    .code_review
                    .as_ref()
                    .map(|item| auth_quota::format_reset_at(item.reset_at));
                view.primary_window = snapshot
                    .primary
                    .as_ref()
                    .and_then(|item| item.limit_window_seconds)
                    .map(format_window_seconds);
                view.secondary_window = snapshot
                    .secondary
                    .as_ref()
                    .and_then(|item| item.limit_window_seconds)
                    .map(format_window_seconds);
                view.code_review_window = snapshot
                    .code_review
                    .as_ref()
                    .and_then(|item| item.limit_window_seconds)
                    .map(format_window_seconds);
                view.checked_at = Some(snapshot.checked_at);
                view.error = None;
                view.availability = if snapshot.limit_reached
                    || snapshot.allowed == Some(false)
                    || snapshot
                        .primary
                        .as_ref()
                        .is_some_and(|item| item.used_percent >= 100.0)
                {
                    AuthAvailability::QuotaExhausted
                } else {
                    AuthAvailability::Available
                };
            }
            Err(err) => {
                view.error = Some(err);
                view.availability = AuthAvailability::Unavailable;
                view.checked_at =
                    Some(chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string());
            }
        }
    }
    if !state.pending.lock().is_empty() {
        // keep polling
        let _ = Instant::now();
    }
    let _ = message;
}

fn refresh_all_accounts(
    config: &AppConfig,
    state: &mut AuthAccountsState,
    message: &mut AppMessage,
) {
    if config.auth_accounts.is_empty() {
        *message = AppMessage::new("没有可刷新的 Auth 账号", MessageKind::Error);
        return;
    }
    let mut count = 0;
    for account in &config.auth_accounts {
        if account.enabled {
            queue_refresh(account, state);
            count += 1;
        }
    }
    if count == 0 {
        *message = AppMessage::new("没有已启用的 Auth 账号可刷新", MessageKind::Error);
    } else {
        *message = AppMessage::new(
            format!("正在刷新 {count} 个 Auth 账号额度"),
            MessageKind::Info,
        );
    }
}

fn queue_refresh(account: &AuthAccountConfig, state: &mut AuthAccountsState) {
    let view = state.views.entry(account.id.clone()).or_default();
    if view.refreshing {
        return;
    }
    view.refreshing = true;
    view.availability = AuthAvailability::Checking;
    view.error = None;

    let account = account.clone();
    let pending = Arc::clone(&state.pending);
    thread::spawn(move || {
        let result = auth_quota::refresh_auth_account_quota(&account).map(|result| {
            (
                result.snapshot,
                TokenUpdate {
                    access_token: result.access_token,
                    refresh_token: result.refresh_token,
                    expires_at: result.expires_at,
                },
            )
        });
        pending.lock().push(QuotaJobResult {
            account_id: account.id,
            result,
        });
    });
}

fn import_auth_accounts(config: &mut AppConfig, message: &mut AppMessage) {
    let Some(source_path) = rfd::FileDialog::new()
        .add_filter("Auth 账号 JSON", &["json"])
        .pick_file()
    else {
        return;
    };
    match crate::config_import::import_auth_accounts_file(config, &source_path) {
        Ok(count) => {
            *message = AppMessage::new(
                format!(
                    "已导入或更新 {count} 个 Auth 账号: {}",
                    source_path.display()
                ),
                MessageKind::Success,
            );
        }
        Err(err) => {
            *message = AppMessage::new(format!("导入 Auth 账号失败: {err}"), MessageKind::Error);
        }
    }
}

fn export_auth_accounts(config: &AppConfig, message: &mut AppMessage) {
    if config.auth_accounts.is_empty() {
        *message = AppMessage::new("没有可导出的 Auth 账号", MessageKind::Error);
        return;
    }
    let Some(target_path) = rfd::FileDialog::new()
        .add_filter("Auth 账号 JSON", &["json"])
        .set_file_name("auth_accounts.json")
        .save_file()
    else {
        return;
    };
    let target_path = ensure_json_extension(target_path);
    match crate::config_import::export_auth_accounts_file(config, &target_path) {
        Ok(()) => {
            *message = AppMessage::new(
                format!("Auth 账号已导出: {}", target_path.display()),
                MessageKind::Success,
            );
        }
        Err(err) => {
            *message = AppMessage::new(format!("导出 Auth 账号失败: {err}"), MessageKind::Error);
        }
    }
}

fn ensure_json_extension(path: PathBuf) -> PathBuf {
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("json"))
    {
        path
    } else {
        path.with_extension("json")
    }
}

fn models_editor(ui: &mut egui::Ui, account: &mut AuthAccountConfig) {
    let mut text = account.models.join(",");
    if ui
        .add_sized(
            [220.0, 24.0],
            egui::TextEdit::singleline(&mut text).hint_text("gpt-5.4,gpt-5.4-codex"),
        )
        .changed()
    {
        account.models = text
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect();
    }
}

fn default_auth_account() -> AuthAccountConfig {
    AuthAccountConfig {
        id: Uuid::new_v4().simple().to_string(),
        name: "OpenAI Auth".to_string(),
        enabled: false,
        email: None,
        access_token: String::new(),
        refresh_token: None,
        account_id: None,
        client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_string(),
        token_url: "https://auth.openai.com/oauth/token".to_string(),
        expires_at: None,
        models: vec!["gpt-5.4".to_string()],
        model_mapping: Default::default(),
        weight: 1,
        priority: 1,
        description: None,
    }
}

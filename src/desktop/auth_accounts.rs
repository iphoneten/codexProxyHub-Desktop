use super::common::{
    accent, badge, border, confirm_delete_button, good, heading_color, muteds, primary_button,
    section, soft_button, surface, switch, text_color,
};
use super::{AppMessage, MessageKind};
use crate::auth_quota::{self, AuthQuotaSnapshot};
use crate::config::{AppConfig, AuthAccountConfig};
use eframe::egui;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Instant,
};
use uuid::Uuid;
mod grok;
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
    grok: grok::GrokCheckState,
    oauth_pending: Arc<Mutex<Vec<crate::oauth_login::OAuthLoginOutcome>>>,
    oauth_cancelled: Option<Arc<AtomicBool>>,
    oauth_waiting: bool,
    proxy_check_pending: Arc<Mutex<Option<Result<String, String>>>>,
    proxy_check_running: bool,
    proxy_exit_ip: Option<String>,
    proxy_check_error: Option<String>,
    proxy_checked_via: Option<String>,
    active_account_type: String,
}
impl AuthAccountsState {
    pub fn has_refreshing(&self) -> bool {
        self.views.values().any(|view| view.refreshing)
            || !self.pending.lock().is_empty()
            || self.grok.has_pending()
            || self.oauth_waiting
            || !self.oauth_pending.lock().is_empty()
            || self.proxy_check_running
            || self.proxy_check_pending.lock().is_some()
    }
}

pub fn proxy_settings_section(
    ui: &mut egui::Ui,
    config: &mut AppConfig,
    state: &mut AuthAccountsState,
    message: &mut AppMessage,
) {
    apply_proxy_check_pending(state, message);
    section(ui, "网络代理", |ui| {
        ui.horizontal(|ui| {
            ui.label("代理地址");
            ui.add(
                egui::TextEdit::singleline(&mut config.routing.auth_proxy)
                    .desired_width(320.0)
                    .hint_text("例如 http://127.0.0.1:7890"),
            );
            let check_label = if state.proxy_check_running {
                "检查中..."
            } else {
                "检查代理"
            };
            if soft_button(ui, check_label).clicked() && !state.proxy_check_running {
                start_proxy_check(config, state, message);
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(
                muteds(),
                "Auth 账号始终使用；普通渠道可在渠道详情中单独开启。",
            );
            if let Some(ip) = state.proxy_exit_ip.as_ref() {
                let via = state
                    .proxy_checked_via
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .unwrap_or("直连");
                ui.label(
                    egui::RichText::new(format!("出口 IP: {ip}（{via}）"))
                        .size(12.0)
                        .color(good()),
                );
            } else if let Some(err) = state.proxy_check_error.as_ref() {
                ui.label(
                    egui::RichText::new(format!("检查失败: {err}"))
                        .size(12.0)
                        .color(egui::Color32::from_rgb(220, 38, 38)),
                );
            } else {
                ui.label(
                    egui::RichText::new("未检查出口 IP")
                        .size(12.0)
                        .color(muteds()),
                );
            }
        });
    });
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
pub fn tick_background_grok_check(config: &mut AppConfig, state: &mut AuthAccountsState) {
    grok::tick_background(config, state);
}
pub fn auth_accounts_section(
    ui: &mut egui::Ui,
    config: &mut AppConfig,
    message: &mut AppMessage,
    state: &mut AuthAccountsState,
) {
    apply_pending(config, state, message);
    grok::apply_pending(config, state, Some(message), false);
    apply_oauth_pending(config, state, message);
    apply_proxy_check_pending(state, message);
    if state.active_account_type.trim().is_empty() {
        state.active_account_type = "openai".to_string();
    }
    let active_type = state.active_account_type.clone();
    let active_count = account_count(config, &active_type);

    egui::Frame::none()
        .fill(surface())
        .stroke(egui::Stroke::new(1.0, border()))
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(14.0, 12.0))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Auth 账号")
                            .size(18.0)
                            .strong()
                            .color(heading_color()),
                    );
                    ui.label(egui::RichText::new(format!("共 {active_count} 个")).color(muteds()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if active_type == "openai"
                            && state.oauth_waiting
                            && soft_button(ui, "取消授权").clicked()
                        {
                            cancel_oauth_login(state, message);
                        }
                        if active_type == "openai"
                            && primary_button(ui, "新增账号").clicked()
                            && !state.oauth_waiting
                        {
                            start_oauth_login(config, state, message);
                        }
                        if active_type == "grok" && primary_button(ui, "新增 Grok 授权").clicked()
                        {
                            config.auth_accounts.insert(0, default_grok_account());
                        }
                        if active_type == "openai" && soft_button(ui, "全部刷新额度").clicked()
                        {
                            refresh_all_accounts(config, state, message, &active_type);
                        }
                        if active_type == "grok" && soft_button(ui, "全部检查授权").clicked()
                        {
                            grok::check_all(config, state, message);
                        }
                        if soft_button(ui, "导入账号").clicked() {
                            import_auth_accounts(config, message, &active_type);
                        }
                        if soft_button(ui, "导出账号").clicked() {
                            export_auth_accounts(config, message, &active_type);
                        }
                        if active_type == "openai" && soft_button(ui, "手动新增").clicked() {
                            config.auth_accounts.insert(0, default_auth_account());
                        }
                    });
                });
                ui.horizontal(|ui| {
                    account_type_tab(ui, &mut state.active_account_type, "openai", "OpenAI");
                    account_type_tab(ui, &mut state.active_account_type, "grok", "Grok");
                });
            });
        });
    ui.add_space(12.0);

    let account_indexes = config
        .auth_accounts
        .iter()
        .enumerate()
        .filter_map(|(idx, account)| account_type_matches(account, &active_type).then_some(idx))
        .collect::<Vec<_>>();

    if account_indexes.is_empty() {
        ui.label(egui::RichText::new(empty_account_text(&active_type)).color(muteds()));
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
    let proxy_url = auth_proxy_url(config);

    let mut idx = 0;
    while idx < account_indexes.len() {
        ui.horizontal_top(|ui| {
            for column in 0..columns {
                let Some(account_idx) = account_indexes.get(idx + column).copied() else {
                    break;
                };
                let account = &mut config.auth_accounts[account_idx];
                let view = state.views.entry(account.id.clone()).or_default().clone();
                let card_id = account.id.clone();
                ui.allocate_ui_with_layout(
                    egui::vec2(card_width, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(card_width);
                        ui.push_id(("auth_account_card", account_idx, card_id.as_str()), |ui| {
                            if account_card(ui, account, &view, state, message, proxy_url.clone()) {
                                remove = Some(account_idx);
                            }
                        });
                    },
                );
                if column + 1 < columns {
                    ui.add_space(gap);
                }
            }
        });
        idx += columns;
        if idx < account_indexes.len() {
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

fn account_type_tab(ui: &mut egui::Ui, active: &mut String, value: &str, label: &str) {
    let selected = active == value;
    let fill = if selected {
        egui::Color32::from_rgb(239, 246, 255)
    } else {
        egui::Color32::from_rgb(244, 247, 251)
    };
    let color = if selected { accent() } else { text_color() };
    if ui
        .add(
            egui::Button::new(egui::RichText::new(label).color(color))
                .fill(fill)
                .rounding(6.0)
                .min_size(egui::vec2(86.0, 30.0)),
        )
        .clicked()
    {
        *active = value.to_string();
    }
}

fn account_type_matches(account: &AuthAccountConfig, account_type: &str) -> bool {
    let current = account.account_type.trim();
    let expected = account_type.trim();
    current.eq_ignore_ascii_case(expected)
        || (expected.eq_ignore_ascii_case("grok") && current.eq_ignore_ascii_case("gork"))
}

fn account_count(config: &AppConfig, account_type: &str) -> usize {
    config
        .auth_accounts
        .iter()
        .filter(|account| account_type_matches(account, account_type))
        .count()
}

fn empty_account_text(account_type: &str) -> &'static str {
    if account_type == "grok" {
        "暂无 Grok 授权，可点击「新增 Grok 授权」填写 API Key，或导入 Grok 账号 JSON。"
    } else {
        "暂无 OpenAI Auth 账号，可点击「新增账号」浏览器授权，或导入 CPA/sub2api JSON。"
    }
}

fn account_card(
    ui: &mut egui::Ui,
    account: &mut AuthAccountConfig,
    view: &AccountQuotaView,
    state: &mut AuthAccountsState,
    message: &mut AppMessage,
    proxy_url: Option<String>,
) -> bool {
    let mut delete = false;
    let is_grok = account_type_matches(account, "grok");
    let is_openai = account_type_matches(account, "openai");
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
            if is_openai {
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
            } else {
                ui.label(
                    egui::RichText::new("OpenAI-compatible API Key 授权")
                        .size(12.0)
                        .color(muteds()),
                );
            }

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
                if is_grok {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Base URL").size(12.0).color(muteds()));
                        ui.add(
                            egui::TextEdit::singleline(&mut account.base_url)
                                .desired_width(260.0)
                                .hint_text("https://api.x.ai/v1"),
                        );
                    });
                }
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("账号 ID").size(12.0).color(muteds()));
                    let hint = if is_grok {
                        "Grok Account ID"
                    } else {
                        "ChatGPT Account ID"
                    };
                    ui.add(
                        egui::TextEdit::singleline(
                            account.account_id.get_or_insert_with(String::new),
                        )
                        .desired_width(220.0)
                        .hint_text(hint),
                    );
                });
                ui.horizontal(|ui| {
                    let access_label = if is_grok { "API Key" } else { "Access" };
                    ui.label(egui::RichText::new(access_label).size(12.0).color(muteds()));
                    ui.add(
                        egui::TextEdit::singleline(&mut account.access_token)
                            .password(true)
                            .desired_width(240.0)
                            .hint_text(if is_grok {
                                "xAI API Key"
                            } else {
                                "access_token"
                            }),
                    );
                });
                if is_openai {
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
                }
                if is_grok {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("模型映射").size(12.0).color(muteds()));
                        model_mapping_editor(ui, account);
                    });
                }
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
                if is_openai && soft_button(ui, refresh_label).clicked() && !view.refreshing {
                    queue_refresh(account, state, proxy_url.clone());
                    *message = AppMessage::new(
                        format!("正在刷新额度: {}", account.name),
                        MessageKind::Info,
                    );
                }
                let check_label = if view.refreshing {
                    "检查中..."
                } else {
                    "检查授权"
                };
                if is_grok && soft_button(ui, check_label).clicked() && !view.refreshing {
                    grok::queue_single(account, state, proxy_url.clone());
                    *message = AppMessage::new(
                        format!("正在检查 Grok 授权: {}", account.name),
                        MessageKind::Info,
                    );
                }
                if confirm_delete_button(ui, ("auth_account_delete", account.id.as_str()), "删除")
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
    account_type: &str,
) {
    if account_count(config, account_type) == 0 {
        *message = AppMessage::new("没有可刷新的 Auth 账号", MessageKind::Error);
        return;
    }
    let mut count = 0;
    for account in &config.auth_accounts {
        if account.enabled && account_type_matches(account, account_type) {
            queue_refresh(account, state, auth_proxy_url(config));
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

fn queue_refresh(
    account: &AuthAccountConfig,
    state: &mut AuthAccountsState,
    proxy_url: Option<String>,
) {
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
        let result =
            auth_quota::refresh_auth_account_quota(&account, proxy_url.as_deref()).map(|result| {
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

fn apply_oauth_pending(
    config: &mut AppConfig,
    state: &mut AuthAccountsState,
    message: &mut AppMessage,
) {
    let outcomes = {
        let mut pending = state.oauth_pending.lock();
        std::mem::take(&mut *pending)
    };
    if outcomes.is_empty() {
        return;
    }
    state.oauth_waiting = false;
    state.oauth_cancelled = None;
    for outcome in outcomes {
        match outcome {
            crate::oauth_login::OAuthLoginOutcome::Success(tokens) => {
                let account = crate::oauth_login::tokens_to_account(tokens);
                let name = crate::oauth_login::merge_oauth_account(config, account);
                *message = AppMessage::new(
                    format!("已通过 OAuth 添加/更新账号: {name}"),
                    MessageKind::Success,
                );
            }
            crate::oauth_login::OAuthLoginOutcome::Failed(err) => {
                *message = AppMessage::new(format!("OAuth 授权失败: {err}"), MessageKind::Error);
            }
        }
    }
}

fn apply_proxy_check_pending(state: &mut AuthAccountsState, message: &mut AppMessage) {
    let result = {
        let mut pending = state.proxy_check_pending.lock();
        pending.take()
    };
    let Some(result) = result else {
        return;
    };
    state.proxy_check_running = false;
    match result {
        Ok(ip) => {
            state.proxy_exit_ip = Some(ip.clone());
            state.proxy_check_error = None;
            let via = state
                .proxy_checked_via
                .clone()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "直连".to_string());
            *message = AppMessage::new(
                format!("代理检查成功，出口 IP: {ip}（{via}）"),
                MessageKind::Success,
            );
        }
        Err(err) => {
            state.proxy_exit_ip = None;
            state.proxy_check_error = Some(err.clone());
            *message = AppMessage::new(format!("代理检查失败: {err}"), MessageKind::Error);
        }
    }
}

fn start_proxy_check(config: &AppConfig, state: &mut AuthAccountsState, message: &mut AppMessage) {
    if state.proxy_check_running {
        *message = AppMessage::new("代理检查进行中", MessageKind::Info);
        return;
    }
    let proxy_url = config.routing.auth_proxy.trim().to_string();
    let via = if proxy_url.is_empty() {
        "直连".to_string()
    } else {
        proxy_url.clone()
    };
    state.proxy_check_running = true;
    state.proxy_check_error = None;
    state.proxy_exit_ip = None;
    state.proxy_checked_via = Some(via);
    let pending = Arc::clone(&state.proxy_check_pending);
    let proxy_arg = (!proxy_url.is_empty()).then_some(proxy_url);
    thread::spawn(move || {
        let result = crate::oauth_login::check_proxy_exit_ip(proxy_arg.as_deref());
        *pending.lock() = Some(result);
    });
    *message = AppMessage::new("正在检查 Auth 代理出口 IP...", MessageKind::Info);
}

fn start_oauth_login(config: &AppConfig, state: &mut AuthAccountsState, message: &mut AppMessage) {
    if state.oauth_waiting {
        *message = AppMessage::new("已有 OAuth 授权进行中", MessageKind::Info);
        return;
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let proxy_url = (!config.routing.auth_proxy.trim().is_empty())
        .then(|| config.routing.auth_proxy.trim().to_string());
    match crate::oauth_login::start_chatgpt_oauth_login(
        Arc::clone(&state.oauth_pending),
        Arc::clone(&cancelled),
        proxy_url,
    ) {
        Ok(()) => {
            state.oauth_cancelled = Some(cancelled);
            state.oauth_waiting = true;
            *message = AppMessage::new(
                "已打开浏览器，请完成 ChatGPT 授权；回调地址为 localhost:1455",
                MessageKind::Info,
            );
        }
        Err(err) => {
            state.oauth_waiting = false;
            *message = AppMessage::new(err, MessageKind::Error);
        }
    }
}

fn auth_proxy_url(config: &AppConfig) -> Option<String> {
    let proxy = config.routing.auth_proxy.trim();
    (!proxy.is_empty()).then(|| proxy.to_string())
}

fn cancel_oauth_login(state: &mut AuthAccountsState, message: &mut AppMessage) {
    if let Some(cancelled) = state.oauth_cancelled.take() {
        cancelled.store(true, Ordering::Relaxed);
    }
    state.oauth_waiting = false;
    state.oauth_pending.lock().clear();
    *message = AppMessage::new("已取消 OAuth 授权", MessageKind::Info);
}

fn import_auth_accounts(config: &mut AppConfig, message: &mut AppMessage, account_type: &str) {
    let Some(paths) = rfd::FileDialog::new()
        .add_filter("Auth 账号", &["json", "zip"])
        .pick_files()
    else {
        return;
    };
    if paths.is_empty() {
        return;
    }
    match crate::config_import::import_auth_accounts_paths_for_type(
        config,
        &paths,
        Some(account_type),
    ) {
        Ok((total, errors)) if errors.is_empty() => {
            *message = AppMessage::new(
                format!(
                    "已导入或更新 {total} 个 Auth 账号（{} 个文件）",
                    paths.len()
                ),
                MessageKind::Success,
            );
        }
        Ok((total, errors)) => {
            *message = AppMessage::new(
                format!(
                    "已导入或更新 {total} 个 Auth 账号，部分失败: {}",
                    errors.join("；")
                ),
                MessageKind::Error,
            );
        }
        Err(err) => *message = AppMessage::new(err.to_string(), MessageKind::Error),
    }
}
fn export_auth_accounts(config: &AppConfig, message: &mut AppMessage, account_type: &str) {
    if account_count(config, account_type) == 0 {
        *message = AppMessage::new("没有可导出的 Auth 账号", MessageKind::Error);
        return;
    }
    let Some(target_path) = rfd::FileDialog::new()
        .add_filter("Auth 账号 JSON", &["json"])
        .set_file_name(format!("auth_accounts_{account_type}.json"))
        .save_file()
    else {
        return;
    };
    let target_path = ensure_json_extension(target_path);
    match crate::config_import::export_auth_accounts_file_for_type(
        config,
        &target_path,
        Some(account_type),
    ) {
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
fn model_mapping_editor(ui: &mut egui::Ui, account: &mut AuthAccountConfig) {
    let draft_id = egui::Id::new(("auth_account_model_mapping_draft", account.id.as_str()));
    let source = serialize_model_mapping(account);
    let mut text = ui
        .ctx()
        .data(|d| d.get_temp::<String>(draft_id))
        .unwrap_or(source);
    let resp = ui.add_sized(
        [300.0, 84.0],
        egui::TextEdit::multiline(&mut text)
            .desired_rows(4)
            .hint_text("本地模型=上游模型"),
    );
    if resp.changed() {
        ui.ctx().data_mut(|d| d.insert_temp(draft_id, text.clone()));
    }
    if resp.lost_focus() {
        account.model_mapping.clear();
        for line in text.lines() {
            if let Some((local, upstream)) = line.split_once('=') {
                let local = local.trim();
                let upstream = upstream.trim();
                if !local.is_empty() && !upstream.is_empty() {
                    account
                        .model_mapping
                        .insert(local.to_string(), upstream.to_string());
                }
            }
        }
        ui.ctx().data_mut(|d| d.remove::<String>(draft_id));
    }
}
fn serialize_model_mapping(account: &AuthAccountConfig) -> String {
    let mut entries = account.model_mapping.iter().collect::<Vec<_>>();
    entries.sort_by_key(|(key, _)| *key);
    entries
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}
fn default_auth_account() -> AuthAccountConfig {
    AuthAccountConfig {
        id: Uuid::new_v4().simple().to_string(),
        account_type: "openai".to_string(),
        name: "OpenAI Auth".to_string(),
        enabled: false,
        email: None,
        access_token: String::new(),
        refresh_token: None,
        account_id: None,
        client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_string(),
        token_url: "https://auth.openai.com/oauth/token".to_string(),
        base_url: String::new(),
        expires_at: None,
        models: crate::config::default_auth_account_models(),
        model_mapping: Default::default(),
        weight: 1,
        priority: 1,
        description: None,
    }
}
fn default_grok_account() -> AuthAccountConfig {
    AuthAccountConfig {
        id: Uuid::new_v4().simple().to_string(),
        account_type: "grok".to_string(),
        name: "Grok Auth".to_string(),
        enabled: false,
        email: None,
        access_token: String::new(),
        refresh_token: None,
        account_id: None,
        client_id: String::new(),
        token_url: String::new(),
        base_url: "https://api.x.ai/v1".to_string(),
        expires_at: None,
        models: vec!["grok-4.5".to_string()],
        model_mapping: Default::default(),
        weight: 1,
        priority: 1,
        description: Some("Grok OpenAI-compatible API Key 账号".to_string()),
    }
}

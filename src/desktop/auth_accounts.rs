use super::common::{
    accent, badge, border, confirm_delete_button, good, heading_color, muteds, primary_button,
    section, soft_button, surface, switch, text_color,
};
use super::{AppMessage, MessageKind};
use crate::auth_quota::{self, AuthQuotaSnapshot, GrokBillingSummary};
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

const AUTH_QUOTA_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

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
    /// 逐个额度窗口展示，标签由上游窗口时长决定。
    windows: Vec<UsageRow>,
    reset_credits: Option<i64>,
    applicable_reset_credits: Option<i64>,
    /// Grok/xAI 账单额度，仅免费档 cli-chat-proxy 授权能取到。
    billing: Option<GrokBillingSummary>,
    checked_at: Option<String>,
    error: Option<String>,
    refreshing: bool,
}

#[derive(Clone)]
struct UsageRow {
    label: String,
    used: Option<f64>,
    reset: Option<String>,
    window: Option<String>,
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
    auto_refresh_next: Option<Instant>,
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

/// 代理启动时立即刷新一次 OpenAI Auth 账号额度，之后每分钟刷新一次。
///
/// 这里只排队请求，实际网络操作在线程中执行，避免阻塞 egui 主线程。
pub fn start_background_auth_quota_refresh(
    config: &AppConfig,
    state: &mut AuthAccountsState,
) {
    state.auto_refresh_next = Some(Instant::now() + AUTH_QUOTA_REFRESH_INTERVAL);
    queue_enabled_accounts(config, state, "openai");
}

pub fn stop_background_auth_quota_refresh(state: &mut AuthAccountsState) {
    state.auto_refresh_next = None;
}

/// 在桌面主循环中回收额度刷新结果并驱动定时刷新。
pub fn tick_background_auth_quota(
    config: &mut AppConfig,
    state: &mut AuthAccountsState,
    proxy_running: bool,
) {
    apply_pending(config, state);

    if !proxy_running {
        state.auto_refresh_next = None;
        return;
    }

    let now = Instant::now();
    let Some(next_refresh) = state.auto_refresh_next else {
        return;
    };
    if now < next_refresh {
        return;
    }

    state.auto_refresh_next = Some(now + AUTH_QUOTA_REFRESH_INTERVAL);
    queue_enabled_accounts(config, state, "openai");
}

pub fn auth_accounts_section(
    ui: &mut egui::Ui,
    config: &mut AppConfig,
    message: &mut AppMessage,
    state: &mut AuthAccountsState,
) {
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
                        if active_type == "grok" && soft_button(ui, "全部刷新额度").clicked()
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
            if view.windows.is_empty() && view.billing.is_none() {
                ui.label(
                    egui::RichText::new(empty_quota_text(is_grok, view.checked_at.is_some()))
                        .size(12.0)
                        .color(muteds()),
                );
            } else {
                for row in &view.windows {
                    usage_row(
                        ui,
                        &row.label,
                        row.used,
                        row.reset.as_deref(),
                        row.window.as_deref(),
                    );
                }
                if let Some(billing) = view.billing.as_ref() {
                    billing_rows(ui, billing);
                }
                if let Some(credits) = view.reset_credits {
                    let applicable = view
                        .applicable_reset_credits
                        .map(|value| format!("（可用于当前额度 {value} 次）"))
                        .unwrap_or_default();
                    ui.label(
                        egui::RichText::new(format!("额度重置券 {credits} 张{applicable}"))
                            .size(11.0)
                            .color(muteds()),
                    );
                }
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
                    "刷新中..."
                } else {
                    "刷新额度"
                };
                if is_grok && soft_button(ui, check_label).clicked() && !view.refreshing {
                    grok::queue_single(account, state, proxy_url.clone());
                    *message = AppMessage::new(
                        format!("正在刷新额度: {}", account.name),
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

/// 纯 API Key 的 Grok 账号上游没有账单接口，刷新过一次后就不要再提示去点按钮。
fn empty_quota_text(is_grok: bool, checked: bool) -> &'static str {
    match (is_grok, checked) {
        (true, true) => "OpenAI-compatible API Key 授权 · 上游未提供额度数据",
        (true, false) => "点击「刷新额度」获取 xAI 账单额度",
        (false, true) => "上游未返回额度数据",
        (false, false) => "点击「刷新额度」获取账号额度",
    }
}

fn billing_rows(ui: &mut egui::Ui, billing: &GrokBillingSummary) {
    let period = billing.period_type.as_deref().unwrap_or("账单");
    let window = billing.period_seconds.map(format_window_seconds);
    let reset = billing
        .period_end
        .map(|end| auth_quota::format_reset_at(Some(end)));
    usage_row(
        ui,
        &format!("{period}额度"),
        billing.usage_percent,
        reset.as_deref(),
        window.as_deref(),
    );
    if let (Some(limit), Some(used)) = (billing.monthly_limit_cents, billing.used_cents) {
        ui.label(
            egui::RichText::new(format!(
                "已用 {} / 套餐 {}",
                format_cents(used),
                format_cents(limit)
            ))
            .size(11.0)
            .color(muteds()),
        );
    }
    if billing.on_demand_cap_cents.is_some() || billing.on_demand_used_cents.is_some() {
        usage_row(ui, "按量额度", billing.on_demand_used_percent, None, None);
        if let Some(cap) = billing.on_demand_cap_cents {
            let used = billing
                .on_demand_used_cents
                .map(format_cents)
                .unwrap_or_else(|| "-".to_string());
            ui.label(
                egui::RichText::new(format!("已用 {used} / 上限 {}", format_cents(cap)))
                    .size(11.0)
                    .color(muteds()),
            );
        }
    }
    for product in &billing.products {
        let percent = product
            .usage_percent
            .map(|value| format!("{value:.1}%"))
            .unwrap_or_else(|| "-".to_string());
        ui.label(
            egui::RichText::new(format!("{} · {percent}", product.product))
                .size(11.0)
                .color(muteds()),
        );
    }
}

/// xAI 账单金额单位是美分。
fn format_cents(cents: f64) -> String {
    format!("${:.2}", cents / 100.0)
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

fn apply_pending(config: &mut AppConfig, state: &mut AuthAccountsState) {
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
                view.windows = snapshot
                    .windows
                    .iter()
                    .map(|window| UsageRow {
                        label: window.label.clone(),
                        used: Some(window.used_percent),
                        reset: Some(auth_quota::format_reset_at(window.reset_at)),
                        window: window.limit_window_seconds.map(format_window_seconds),
                    })
                    .collect();
                view.reset_credits = snapshot.reset_credits;
                view.applicable_reset_credits = snapshot.applicable_reset_credits;
                view.checked_at = Some(snapshot.checked_at);
                view.error = None;
                view.availability = if snapshot.limit_reached || snapshot.allowed == Some(false) {
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
    let count = queue_enabled_accounts(config, state, account_type);
    if count == 0 {
        *message = AppMessage::new("没有已启用的 Auth 账号可刷新", MessageKind::Error);
    } else {
        *message = AppMessage::new(
            format!("正在刷新 {count} 个 Auth 账号额度"),
            MessageKind::Info,
        );
    }
}

fn queue_enabled_accounts(
    config: &AppConfig,
    state: &mut AuthAccountsState,
    account_type: &str,
) -> usize {
    let proxy_url = auth_proxy_url(config);
    let mut count = 0;
    for account in &config.auth_accounts {
        if account.enabled && account_type_matches(account, account_type) {
            queue_refresh(account, state, proxy_url.clone());
            count += 1;
        }
    }
    count
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

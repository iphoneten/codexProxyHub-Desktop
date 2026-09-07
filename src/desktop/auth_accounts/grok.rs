use super::{
    account_type_matches, auth_proxy_url, AccountQuotaView, AuthAccountsState, AuthAvailability,
};
use crate::{
    auth_quota::{self, GrokCheckResult, GrokQuotaResult},
    config::{AppConfig, AuthAccountConfig},
    desktop::{AppMessage, MessageKind},
};
use parking_lot::Mutex;
use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const CHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);

struct CheckResult {
    account_id: String,
    result: GrokQuotaResult,
    silent: bool,
}

#[derive(Default)]
pub(super) struct GrokCheckState {
    pending: Arc<Mutex<Vec<CheckResult>>>,
    last_background_check: Option<Instant>,
}

impl GrokCheckState {
    pub(super) fn has_pending(&self) -> bool {
        !self.pending.lock().is_empty()
    }
}

pub(super) fn tick_background(config: &mut AppConfig, state: &mut AuthAccountsState) {
    apply_pending(config, state, None, true);
    let now = Instant::now();
    let Some(last_check) = state.grok.last_background_check else {
        state.grok.last_background_check = Some(now);
        return;
    };
    if now.duration_since(last_check) < CHECK_INTERVAL {
        return;
    }
    state.grok.last_background_check = Some(now);
    queue_all(config, state, true);
}

pub(super) fn apply_pending(
    config: &mut AppConfig,
    state: &mut AuthAccountsState,
    message: Option<&mut AppMessage>,
    silent_only: bool,
) {
    let jobs = {
        let mut pending = state.grok.pending.lock();
        let all = std::mem::take(&mut *pending);
        if !silent_only {
            all
        } else {
            let (jobs, remain): (Vec<_>, Vec<_>) = all.into_iter().partition(|job| job.silent);
            pending.extend(remain);
            jobs
        }
    };
    if jobs.is_empty() {
        return;
    }
    let mut available = 0;
    let mut exhausted = 0;
    let mut disabled = 0;
    let mut silent = true;
    for job in jobs {
        silent &= job.silent;
        let view = state.views.entry(job.account_id.clone()).or_default();
        view.refreshing = false;
        view.checked_at = Some(chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string());
        if job.result.billing.is_some() {
            view.billing = job.result.billing;
        }
        match job.result.check {
            GrokCheckResult::Available => {
                set_available(view, &mut available);
                state
                    .quota_status_updates
                    .push((job.account_id.clone(), false));
            }
            GrokCheckResult::QuotaExhausted(err) => {
                view.availability = AuthAvailability::QuotaExhausted;
                view.error = Some(err);
                state
                    .quota_status_updates
                    .push((job.account_id.clone(), true));
                exhausted += 1;
            }
            GrokCheckResult::InvalidAuth(err) => {
                if let Some(account) = config
                    .auth_accounts
                    .iter_mut()
                    .find(|a| a.id == job.account_id)
                {
                    account.enabled = false;
                }
                view.availability = AuthAvailability::Unavailable;
                view.error = Some(err);
                disabled += 1;
            }
            GrokCheckResult::Unavailable(err) => {
                view.availability = AuthAvailability::Unavailable;
                view.error = Some(err);
            }
        }
    }
    if !silent {
        if let Some(message) = message {
            let kind = if disabled == 0 {
                MessageKind::Success
            } else {
                MessageKind::Error
            };
            let text = if disabled == 0 {
                format!("Grok 额度刷新完成，{available} 个可用，{exhausted} 个额度耗尽")
            } else {
                format!("Grok 额度刷新完成，{available} 个可用，{exhausted} 个额度耗尽，已停用 {disabled} 个授权失效账号")
            };
            *message = AppMessage::new(text, kind);
        }
    }
}

fn set_available(view: &mut AccountQuotaView, available: &mut usize) {
    view.availability = AuthAvailability::Available;
    view.error = None;
    *available += 1;
}

pub(super) fn check_all(
    config: &AppConfig,
    state: &mut AuthAccountsState,
    message: &mut AppMessage,
) {
    let count = queue_all(config, state, false);
    *message = if count == 0 {
        AppMessage::new("没有已启用的 Grok 账号可检查", MessageKind::Error)
    } else {
        AppMessage::new(
            format!("正在并发刷新 {count} 个 Grok 账号额度"),
            MessageKind::Info,
        )
    };
}

fn queue_all(config: &AppConfig, state: &mut AuthAccountsState, silent: bool) -> usize {
    let accounts = config
        .auth_accounts
        .iter()
        .filter(|account| account.enabled && account_type_matches(account, "grok"))
        .cloned()
        .collect::<Vec<_>>();
    for account in &accounts {
        let view = state.views.entry(account.id.clone()).or_default();
        view.refreshing = true;
        view.availability = AuthAvailability::Checking;
        view.error = None;
    }
    if accounts.is_empty() {
        return 0;
    }
    let count = accounts.len();
    let pending = Arc::clone(&state.grok.pending);
    let proxy_url = auth_proxy_url(config);
    thread::spawn(move || {
        pending.lock().extend(
            auth_quota::check_grok_accounts(accounts, proxy_url)
                .into_iter()
                .map(|(account_id, result)| CheckResult {
                    account_id,
                    result,
                    silent,
                }),
        );
    });
    count
}

pub(super) fn queue_single(
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
    let pending = Arc::clone(&state.grok.pending);
    thread::spawn(move || {
        let result = auth_quota::check_grok_account_quota(&account, proxy_url.as_deref());
        pending.lock().push(CheckResult {
            account_id: account.id,
            result,
            silent: false,
        });
    });
}

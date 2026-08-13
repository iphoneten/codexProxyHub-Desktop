use crate::config::AuthAccountConfig;
use chrono::{TimeZone, Utc};
use serde::Deserialize;
use serde_json::Value;

const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_USAGE_BASE: &str = "https://chatgpt.com/backend-api";
const REFRESH_EARLY_SECONDS: i64 = 300;
const FIVE_HOUR_SECONDS: i64 = 18_000;
const WEEK_SECONDS: i64 = 604_800;
const MIN_MONTH_SECONDS: i64 = 28 * 86_400;
const MAX_MONTH_SECONDS: i64 = 31 * 86_400;
const XAI_BILLING_WEEKLY_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const XAI_BILLING_MONTHLY_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing";
const GROK_CLIENT_VERSION: &str = "0.2.109";

#[derive(Debug, Clone, Default)]
pub struct AuthQuotaSnapshot {
    pub plan_type: Option<String>,
    pub allowed: Option<bool>,
    pub limit_reached: bool,
    /// 全部额度窗口：主/次窗口、代码审查以及 additional_rate_limits 展开后的结果。
    pub windows: Vec<AuthQuotaWindow>,
    /// rate_limit_reset_credits：可用于手动重置额度的次数。
    pub reset_credits: Option<i64>,
    pub applicable_reset_credits: Option<i64>,
    pub checked_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct AuthQuotaWindow {
    pub label: String,
    pub used_percent: f64,
    pub reset_at: Option<i64>,
    pub limit_window_seconds: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct AuthQuotaRefreshResult {
    pub snapshot: AuthQuotaSnapshot,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
}

#[derive(Debug)]
pub enum GrokCheckResult {
    Available,
    QuotaExhausted(String),
    InvalidAuth(String),
    Unavailable(String),
}

/// Grok/xAI 授权检查结果：可用性 + 可选的账单额度（免费档 cli-chat-proxy 才有）。
#[derive(Debug)]
pub struct GrokQuotaResult {
    pub check: GrokCheckResult,
    pub billing: Option<GrokBillingSummary>,
}

#[derive(Debug, Clone, Default)]
pub struct GrokBillingSummary {
    /// 计费周期名称：每周 / 每月，未知时为 None。
    pub period_type: Option<String>,
    pub usage_percent: Option<f64>,
    pub period_start: Option<i64>,
    pub period_end: Option<i64>,
    /// 周期长度（秒），由 start/end 推算而来。
    pub period_seconds: Option<i64>,
    pub monthly_limit_cents: Option<f64>,
    pub used_cents: Option<f64>,
    pub included_used_cents: Option<f64>,
    pub on_demand_cap_cents: Option<f64>,
    pub on_demand_used_cents: Option<f64>,
    pub on_demand_used_percent: Option<f64>,
    pub products: Vec<GrokProductUsage>,
}

#[derive(Debug, Clone)]
pub struct GrokProductUsage {
    pub product: String,
    pub usage_percent: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct UsageResponse {
    plan_type: Option<String>,
    rate_limit: Option<RateLimitBody>,
    code_review_rate_limit: Option<RateLimitBody>,
    /// 上游会显式给 null，所以必须用 Option 而不是 `#[serde(default)]` 的 Vec。
    additional_rate_limits: Option<Vec<AdditionalRateLimitBody>>,
    rate_limit_reset_credits: Option<ResetCreditsBody>,
}

#[derive(Debug, Deserialize)]
struct AdditionalRateLimitBody {
    limit_name: Option<String>,
    metered_feature: Option<String>,
    rate_limit: Option<RateLimitBody>,
}

#[derive(Debug, Deserialize)]
struct ResetCreditsBody {
    available_count: Option<i64>,
    applicable_available_count: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct RateLimitBody {
    allowed: Option<bool>,
    limit_reached: Option<bool>,
    primary_window: Option<WindowBody>,
    secondary_window: Option<WindowBody>,
}

#[derive(Debug, Deserialize)]
struct WindowBody {
    used_percent: Option<f64>,
    limit_window_seconds: Option<i64>,
    reset_after_seconds: Option<i64>,
    reset_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

pub fn refresh_auth_account_quota(
    account: &AuthAccountConfig,
    proxy_url: Option<&str>,
) -> Result<AuthQuotaRefreshResult, String> {
    let mut access_token = account.access_token.clone();
    let mut refresh_token = account.refresh_token.clone();
    let mut expires_at = account.expires_at.or_else(|| jwt_exp(&access_token));

    if access_token.trim().is_empty()
        || should_refresh(expires_at)
            && refresh_token
                .as_deref()
                .is_some_and(|token| !token.trim().is_empty())
    {
        let Some(current_refresh) = refresh_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
            .map(str::to_string)
        else {
            if access_token.trim().is_empty() {
                return Err("账号缺少 access_token 和 refresh_token".to_string());
            }
            // keep current token
            let snapshot = fetch_usage(&access_token, account.account_id.as_deref(), proxy_url)?;
            return Ok(AuthQuotaRefreshResult {
                snapshot,
                access_token,
                refresh_token,
                expires_at,
            });
        };
        let refreshed = refresh_access_token(account, &current_refresh, proxy_url)?;
        access_token = refreshed.access_token;
        refresh_token = refreshed.refresh_token.or(Some(current_refresh));
        expires_at = refreshed
            .expires_in
            .map(|seconds| Utc::now().timestamp() + seconds)
            .or_else(|| jwt_exp(&access_token));
    }

    if access_token.trim().is_empty() {
        return Err("账号缺少可用 access_token".to_string());
    }

    let snapshot = fetch_usage(&access_token, account.account_id.as_deref(), proxy_url)?;
    Ok(AuthQuotaRefreshResult {
        snapshot,
        access_token,
        refresh_token,
        expires_at,
    })
}

pub fn check_grok_account(account: &AuthAccountConfig, proxy_url: Option<&str>) -> GrokCheckResult {
    if account.access_token.trim().is_empty() {
        return GrokCheckResult::InvalidAuth("账号缺少 API Key".to_string());
    }
    let base_url = if account.base_url.trim().is_empty() {
        "https://api.x.ai/v1"
    } else {
        account.base_url.trim_end_matches('/')
    };
    let mut builder =
        reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(30));
    if let Some(proxy_url) = proxy_url.map(str::trim).filter(|value| !value.is_empty()) {
        let Ok(proxy) = reqwest::Proxy::all(proxy_url) else {
            return GrokCheckResult::Unavailable("Auth 代理地址无效".to_string());
        };
        builder = builder.no_proxy().proxy(proxy);
    }
    let Ok(client) = builder.build() else {
        return GrokCheckResult::Unavailable("创建 Grok 检查客户端失败".to_string());
    };
    let mut request = client
        .post(format!("{base_url}/chat/completions"))
        .bearer_auth(&account.access_token)
        .header("x-grok-client-version", "0.2.109")
        .header("x-grok-client-identifier", "grok-shell")
        .header("User-Agent", "xai-grok-build/0.2.109")
        .json(&serde_json::json!({
            "model": account.models.first().map(String::as_str).unwrap_or("grok-4.5"),
            "messages": [{"role": "user", "content": "Reply with exactly: hi"}],
            "stream": false,
            "max_tokens": 8
        }));
    if base_url
        .to_ascii_lowercase()
        .contains("cli-chat-proxy.grok.com")
    {
        request = request.header("X-XAI-Token-Auth", "xai-grok-cli");
    }
    let response = match request.send() {
        Ok(response) => response,
        Err(err) => {
            return GrokCheckResult::Unavailable(format!("Grok 授权检查请求失败: {err}"));
        }
    };
    let status = response.status();
    let text = response.text().unwrap_or_default();
    if status.is_success() {
        return GrokCheckResult::Available;
    }
    let detail: String = text.chars().take(180).collect();
    let error = format!("Grok 上游返回 {status}: {detail}");
    let normalized = text.to_ascii_lowercase();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || normalized.contains("insufficient_quota")
        || normalized.contains("quota exceeded")
        || normalized.contains("quota_exceeded")
        || normalized.contains("limit_reached")
        || normalized.contains("rate limit")
    {
        GrokCheckResult::QuotaExhausted(error)
    } else if status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::FORBIDDEN
    {
        GrokCheckResult::InvalidAuth(error)
    } else {
        GrokCheckResult::Unavailable(error)
    }
}

pub fn check_grok_accounts(
    accounts: Vec<AuthAccountConfig>,
    proxy_url: Option<String>,
) -> Vec<(String, GrokQuotaResult)> {
    let handles = accounts
        .into_iter()
        .map(|account| {
            let account_id = account.id.clone();
            let proxy_url = proxy_url.clone();
            let handle = std::thread::spawn(move || {
                let result = check_grok_account_quota(&account, proxy_url.as_deref());
                (account.id, result)
            });
            (account_id, handle)
        })
        .collect::<Vec<_>>();

    handles
        .into_iter()
        .map(|(account_id, handle)| {
            handle.join().unwrap_or_else(|_| {
                (
                    account_id,
                    GrokQuotaResult {
                        check: GrokCheckResult::Unavailable(
                            "Grok 授权检查线程异常".to_string(),
                        ),
                        billing: None,
                    },
                )
            })
        })
        .collect()
}

/// 优先读 xAI 账单额度（cli-chat-proxy），拿不到时退回 chat 探活。
pub fn check_grok_account_quota(
    account: &AuthAccountConfig,
    proxy_url: Option<&str>,
) -> GrokQuotaResult {
    if supports_grok_billing(account) {
        if let Ok(Some(summary)) = fetch_grok_billing(account, proxy_url) {
            let exhausted = summary
                .usage_percent
                .is_some_and(|percent| percent >= 100.0)
                && summary
                    .on_demand_used_percent
                    .map(|percent| percent >= 100.0)
                    .unwrap_or(true);
            let check = if exhausted {
                GrokCheckResult::QuotaExhausted("xAI 账单额度已用尽".to_string())
            } else {
                GrokCheckResult::Available
            };
            return GrokQuotaResult {
                check,
                billing: Some(summary),
            };
        }
    }
    GrokQuotaResult {
        check: check_grok_account(account, proxy_url),
        billing: None,
    }
}

/// 只有 Grok OAuth 令牌（JWT）或显式指向 cli-chat-proxy 的账号才有账单接口，
/// 纯 xAI API Key 直接探活，避免每次刷新都打两个必然 401 的请求。
fn supports_grok_billing(account: &AuthAccountConfig) -> bool {
    account
        .base_url
        .to_ascii_lowercase()
        .contains("cli-chat-proxy.grok.com")
        || jwt_exp(&account.access_token).is_some()
}

fn fetch_grok_billing(
    account: &AuthAccountConfig,
    proxy_url: Option<&str>,
) -> Result<Option<GrokBillingSummary>, String> {
    let client = build_blocking_client(proxy_url, 20)?;
    let weekly = request_grok_billing(&client, account, XAI_BILLING_WEEKLY_URL);
    let monthly = request_grok_billing(&client, account, XAI_BILLING_MONTHLY_URL);
    match (weekly, monthly) {
        (Err(weekly_err), Err(_)) => Err(weekly_err),
        (weekly, monthly) => Ok(merge_grok_billing(
            weekly.ok().flatten(),
            monthly.ok().flatten(),
        )),
    }
}

fn request_grok_billing(
    client: &reqwest::blocking::Client,
    account: &AuthAccountConfig,
    url: &str,
) -> Result<Option<GrokBillingSummary>, String> {
    let mut request = client
        .get(url)
        .bearer_auth(&account.access_token)
        .header("Accept", "application/json")
        .header("x-xai-token-auth", "xai-grok-cli")
        .header("x-grok-client-version", GROK_CLIENT_VERSION)
        .header(
            "User-Agent",
            format!(
                "grok-pager/{GROK_CLIENT_VERSION} grok-shell/{GROK_CLIENT_VERSION} (macos; aarch64)"
            ),
        );
    if let Some(user_id) = account
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        request = request.header("x-userid", user_id);
    }
    let response = request
        .send()
        .map_err(|err| format!("请求 xAI 账单失败: {err}"))?;
    let status = response.status();
    let text = response.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("xAI 账单接口返回 {status}: {}", truncate(&text)));
    }
    let payload: Value =
        serde_json::from_str(&text).map_err(|err| format!("xAI 账单响应无效: {err}"))?;
    Ok(parse_grok_billing(&payload))
}

fn parse_grok_billing(payload: &Value) -> Option<GrokBillingSummary> {
    let config = payload.get("config").filter(|value| value.is_object())?;
    let period = pick(config, &["currentPeriod", "current_period"]);
    let period_type = period
        .and_then(|period| json_string(pick(period, &["type"])))
        .map(|value| value.to_ascii_lowercase());
    let credit_usage_percent = json_number(pick(
        config,
        &["creditUsagePercent", "credit_usage_percent"],
    ));
    let products = pick(config, &["productUsage", "product_usage"])
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .enumerate()
                .map(|(index, item)| GrokProductUsage {
                    product: json_string(pick(item, &["product"]))
                        .unwrap_or_else(|| format!("产品 {}", index + 1)),
                    usage_percent: json_number(pick(item, &["usagePercent", "usage_percent"])),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let monthly_limit_cents = json_number(pick(config, &["monthlyLimit", "monthly_limit"]));
    let used_cents = json_number(pick(config, &["used"]));
    let on_demand_cap_cents = json_number(pick(config, &["onDemandCap", "on_demand_cap"]));
    let explicit_on_demand_used = json_number(pick(config, &["onDemandUsed", "on_demand_used"]));
    let billing_start = json_timestamp(pick(
        config,
        &["billingPeriodStart", "billing_period_start"],
    ));
    let billing_end = json_timestamp(pick(config, &["billingPeriodEnd", "billing_period_end"]));

    let included_used_cents = used_cents.map(|used| match monthly_limit_cents {
        Some(limit) if limit > 0.0 => used.min(limit),
        _ => used,
    });
    let on_demand_used_cents = explicit_on_demand_used.or_else(|| {
        match (used_cents, monthly_limit_cents) {
            (Some(used), Some(limit)) => Some((used - limit).max(0.0)),
            _ => None,
        }
    });
    let used_percent = match (monthly_limit_cents, included_used_cents) {
        (Some(limit), Some(used)) if limit > 0.0 => Some(used / limit * 100.0),
        _ => None,
    };
    let on_demand_used_percent = match (on_demand_cap_cents, on_demand_used_cents) {
        (Some(cap), Some(used)) if cap > 0.0 => Some(used / cap * 100.0),
        _ => None,
    };

    // 周额度（credits）和月账单是两套时钟，谁有数据就用谁自己的周期，避免把
    // 账单滚动误显示成额度重置。
    let is_weekly = credit_usage_percent.is_some()
        || !products.is_empty()
        || period_type
            .as_deref()
            .is_some_and(|value| value.contains("weekly"));
    let has_monthly = monthly_limit_cents.is_some()
        || used_cents.is_some()
        || (!is_weekly && (on_demand_cap_cents.is_some() || billing_end.is_some()));
    if !is_weekly && !has_monthly {
        return None;
    }

    let (period_start, period_end) = if is_weekly {
        (
            period
                .and_then(|period| json_timestamp(pick(period, &["start"])))
                .or(billing_start),
            period
                .and_then(|period| json_timestamp(pick(period, &["end"])))
                .or(billing_end),
        )
    } else {
        (billing_start, billing_end)
    };
    let period_seconds = match (period_start, period_end) {
        (Some(start), Some(end)) if end > start => Some(end - start),
        _ => None,
    };
    let period_type = if is_weekly {
        Some("每周".to_string())
    } else if has_monthly {
        Some("每月".to_string())
    } else {
        None
    };

    Some(GrokBillingSummary {
        period_type,
        usage_percent: if is_weekly {
            credit_usage_percent
        } else {
            used_percent
        },
        period_start,
        period_end,
        period_seconds,
        monthly_limit_cents,
        used_cents,
        included_used_cents,
        on_demand_cap_cents,
        on_demand_used_cents,
        on_demand_used_percent,
        products,
    })
}

/// 周额度接口和月账单接口各自可能缺字段，逐项取并集；周期字段以有周期类型的一侧为准。
fn merge_grok_billing(
    primary: Option<GrokBillingSummary>,
    fallback: Option<GrokBillingSummary>,
) -> Option<GrokBillingSummary> {
    let (primary, fallback) = match (primary, fallback) {
        (Some(primary), Some(fallback)) => (primary, fallback),
        (primary, fallback) => return primary.or(fallback),
    };
    Some(GrokBillingSummary {
        period_type: primary.period_type.or(fallback.period_type),
        usage_percent: primary.usage_percent.or(fallback.usage_percent),
        period_start: primary.period_start.or(fallback.period_start),
        period_end: primary.period_end.or(fallback.period_end),
        period_seconds: primary.period_seconds.or(fallback.period_seconds),
        monthly_limit_cents: primary.monthly_limit_cents.or(fallback.monthly_limit_cents),
        used_cents: primary.used_cents.or(fallback.used_cents),
        included_used_cents: primary.included_used_cents.or(fallback.included_used_cents),
        on_demand_cap_cents: primary.on_demand_cap_cents.or(fallback.on_demand_cap_cents),
        on_demand_used_cents: primary
            .on_demand_used_cents
            .or(fallback.on_demand_used_cents),
        on_demand_used_percent: primary
            .on_demand_used_percent
            .or(fallback.on_demand_used_percent),
        products: if primary.products.is_empty() {
            fallback.products
        } else {
            primary.products
        },
    })
}

fn pick<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .filter_map(|key| value.get(*key))
        .find(|value| !value.is_null())
}

/// 账单里的金额可能是数字、字符串或 `{ "val": ... }` 包装。
fn json_number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        Value::Object(_) => json_number(value.get("val")),
        _ => None,
    }
}

fn json_string(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn json_timestamp(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(number) = value.as_i64() {
        // 毫秒时间戳统一成秒。
        return Some(if number > 100_000_000_000 {
            number / 1000
        } else {
            number
        });
    }
    let text = value.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(time.timestamp());
    }
    text.parse::<i64>().ok()
}

fn build_blocking_client(
    proxy_url: Option<&str>,
    timeout_secs: u64,
) -> Result<reqwest::blocking::Client, String> {
    let mut builder =
        reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(timeout_secs));
    if let Some(proxy_url) = proxy_url.map(str::trim).filter(|value| !value.is_empty()) {
        let proxy =
            reqwest::Proxy::all(proxy_url).map_err(|err| format!("Auth 代理地址无效: {err}"))?;
        builder = builder.no_proxy().proxy(proxy);
    }
    builder.build().map_err(|err| err.to_string())
}

fn fetch_usage(
    access_token: &str,
    account_id: Option<&str>,
    proxy_url: Option<&str>,
) -> Result<AuthQuotaSnapshot, String> {
    let client = build_blocking_client(proxy_url, 20)?;

    let urls = [
        format!("{CODEX_USAGE_BASE}/wham/usage"),
        format!("{CODEX_USAGE_BASE}/codex/usage"),
    ];
    let mut last_error = "未获得有效额度响应".to_string();
    for url in urls {
        let mut request = client
            .get(&url)
            .bearer_auth(access_token)
            .header("Accept", "application/json");
        if let Some(account_id) = account_id.filter(|value| !value.trim().is_empty()) {
            request = request.header("ChatGPT-Account-Id", account_id);
        }
        match request.send() {
            Ok(response) => {
                let status = response.status();
                let text = response.text().unwrap_or_default();
                if !status.is_success() {
                    last_error = format!("上游返回 {status}: {}", truncate(&text));
                    continue;
                }
                match serde_json::from_str::<UsageResponse>(&text) {
                    Ok(body) if body.rate_limit.is_some() => {
                        return Ok(to_snapshot(body));
                    }
                    Ok(_) => {
                        last_error = format!("响应缺少 rate_limit: {}", truncate(&text));
                    }
                    Err(err) => {
                        last_error = format!("解析额度响应失败: {err}; body={}", truncate(&text));
                    }
                }
            }
            Err(err) => {
                last_error = format!("请求额度失败: {err}");
            }
        }
    }
    Err(last_error)
}

fn refresh_access_token(
    account: &AuthAccountConfig,
    refresh_token: &str,
    proxy_url: Option<&str>,
) -> Result<RefreshResponse, String> {
    let client = build_blocking_client(proxy_url, 20)?;
    let client_id = if account.client_id.trim().is_empty() {
        OPENAI_OAUTH_CLIENT_ID
    } else {
        account.client_id.as_str()
    };
    let token_url = if account.token_url.trim().is_empty() {
        OPENAI_OAUTH_TOKEN_URL
    } else {
        account.token_url.as_str()
    };
    let response = client
        .post(token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
        ])
        .send()
        .map_err(|err| format!("刷新 access_token 失败: {err}"))?;
    let status = response.status();
    let text = response.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "刷新 access_token 失败 ({status}): {}",
            truncate(&text)
        ));
    }
    serde_json::from_str(&text).map_err(|err| format!("刷新响应无效: {err}"))
}

fn to_snapshot(body: UsageResponse) -> AuthQuotaSnapshot {
    let rate_limit = body.rate_limit;
    let allowed = rate_limit.as_ref().and_then(|limit| limit.allowed);
    let explicit_limit_reached = rate_limit
        .as_ref()
        .and_then(|limit| limit.limit_reached)
        .unwrap_or(false);

    let mut windows = Vec::new();
    collect_windows(None, rate_limit, &mut windows);
    let main_exhausted = windows.iter().any(|window| window.used_percent >= 100.0);
    collect_windows(Some("代码审查"), body.code_review_rate_limit, &mut windows);
    for (index, extra) in body
        .additional_rate_limits
        .unwrap_or_default()
        .into_iter()
        .enumerate()
    {
        let name = extra
            .limit_name
            .or(extra.metered_feature)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("附加额度 {}", index + 1));
        collect_windows(Some(&name), extra.rate_limit, &mut windows);
    }

    let credits = body.rate_limit_reset_credits;
    AuthQuotaSnapshot {
        plan_type: body.plan_type,
        allowed,
        limit_reached: explicit_limit_reached || main_exhausted,
        windows,
        reset_credits: credits.as_ref().and_then(|value| value.available_count),
        applicable_reset_credits: credits
            .as_ref()
            .and_then(|value| value.applicable_available_count),
        checked_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    }
}

/// 把一组 rate_limit 的主/次窗口展开成带标签的额度窗口。
/// `group` 为 None 表示主额度，其余为代码审查/附加额度的名字前缀。
fn collect_windows(
    group: Option<&str>,
    limit: Option<RateLimitBody>,
    out: &mut Vec<AuthQuotaWindow>,
) {
    let Some(limit) = limit else {
        return;
    };
    let fallbacks = if group.is_some() {
        ["主", "次"]
    } else {
        ["主额度", "次额度"]
    };
    let bodies = [limit.primary_window, limit.secondary_window];
    for (body, fallback) in bodies.into_iter().zip(fallbacks) {
        if let Some(window) = window_from_body(body, group, fallback) {
            out.push(window);
        }
    }
}

fn window_from_body(
    body: Option<WindowBody>,
    group: Option<&str>,
    fallback: &str,
) -> Option<AuthQuotaWindow> {
    let body = body?;
    let used_percent = body.used_percent.unwrap_or(0.0);
    let reset_at = body.reset_at.or_else(|| {
        body.reset_after_seconds
            .map(|seconds| Utc::now().timestamp() + seconds)
    });
    Some(AuthQuotaWindow {
        label: window_label(group, body.limit_window_seconds, fallback),
        used_percent,
        reset_at,
        limit_window_seconds: body.limit_window_seconds,
    })
}

/// 额度名称以窗口时长为准：同一个 primary/secondary 字段在不同套餐里
/// 可能是 5 小时 / 每周 / 每月，只有缺少时长时才退回主/次顺序。
fn window_label(group: Option<&str>, seconds: Option<i64>, fallback: &str) -> String {
    match (group, window_period_name(seconds)) {
        (None, Some(period)) => format!("{period}额度"),
        (None, None) => fallback.to_string(),
        (Some(group), Some(period)) => format!("{group} · {period}"),
        (Some(group), None) => format!("{group} · {fallback}"),
    }
}

fn window_period_name(seconds: Option<i64>) -> Option<String> {
    let seconds = seconds.filter(|seconds| *seconds > 0)?;
    if seconds == FIVE_HOUR_SECONDS {
        return Some("5 小时".to_string());
    }
    if seconds == WEEK_SECONDS {
        return Some("每周".to_string());
    }
    if (MIN_MONTH_SECONDS..=MAX_MONTH_SECONDS).contains(&seconds) {
        return Some("每月".to_string());
    }
    Some(if seconds >= 86_400 {
        format!("{} 天", seconds / 86_400)
    } else if seconds >= 3_600 {
        format!("{} 小时", seconds / 3_600)
    } else if seconds >= 60 {
        format!("{} 分钟", seconds / 60)
    } else {
        format!("{seconds} 秒")
    })
}

fn should_refresh(expires_at: Option<i64>) -> bool {
    expires_at
        .is_some_and(|expires_at| expires_at <= Utc::now().timestamp() + REFRESH_EARLY_SECONDS)
}

fn jwt_exp(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = decode_base64_url(payload)?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get("exp").and_then(Value::as_i64)
}

fn decode_base64_url(value: &str) -> Option<Vec<u8>> {
    fn sextet(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let mut output = Vec::with_capacity(value.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for byte in value.bytes().filter(|byte| *byte != b'=') {
        buffer = (buffer << 6) | u32::from(sextet(byte)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1u32 << bits).saturating_sub(1);
        }
    }
    Some(output)
}

fn truncate(value: &str) -> String {
    let mut chars = value.chars();
    let head: String = chars.by_ref().take(180).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}

pub fn format_reset_at(reset_at: Option<i64>) -> String {
    let Some(reset_at) = reset_at else {
        return "未知".to_string();
    };
    match Utc.timestamp_opt(reset_at, 0).single() {
        Some(time) => time
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string(),
        None => reset_at.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_usage_snapshot() {
        let body = serde_json::from_str::<UsageResponse>(
            r#"{
              "plan_type": "plus",
              "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                  "used_percent": 12.5,
                  "limit_window_seconds": 18000,
                  "reset_at": 1900000000
                },
                "secondary_window": {
                  "used_percent": 40.0,
                  "limit_window_seconds": 604800,
                  "reset_at": 1900500000
                }
              },
              "code_review_rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                  "used_percent": 5.0,
                  "reset_at": 1900001000
                }
              },
              "additional_rate_limits": [
                {
                  "limit_name": "gpt-5-codex",
                  "metered_feature": "codex",
                  "rate_limit": {
                    "primary_window": {
                      "used_percent": 60.0,
                      "limit_window_seconds": 2592000,
                      "reset_at": 1900600000
                    }
                  }
                }
              ],
              "rate_limit_reset_credits": {
                "available_count": 3,
                "applicable_available_count": 1
              }
            }"#,
        )
        .unwrap();
        let snapshot = to_snapshot(body);
        assert_eq!(snapshot.plan_type.as_deref(), Some("plus"));
        assert_eq!(snapshot.allowed, Some(true));
        assert!(!snapshot.limit_reached);
        assert_eq!(snapshot.reset_credits, Some(3));
        assert_eq!(snapshot.applicable_reset_credits, Some(1));
        let labels = snapshot
            .windows
            .iter()
            .map(|window| (window.label.as_str(), window.used_percent))
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec![
                ("5 小时额度", 12.5),
                ("每周额度", 40.0),
                ("代码审查 · 主", 5.0),
                ("gpt-5-codex · 每月", 60.0),
            ]
        );
    }

    #[test]
    fn tolerates_null_collections_and_extra_fields() {
        // 免费账号的真实响应：additional_rate_limits 显式为 null，且带额外字段。
        let body = serde_json::from_str::<UsageResponse>(
            r#"{
              "user_id": "user-abc",
              "account_id": "76585f67-51a0-405e-bf1b-e81354590dc7",
              "email": "",
              "plan_type": "free",
              "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {"used_percent": 3.0, "limit_window_seconds": 18000},
                "secondary_window": null
              },
              "code_review_rate_limit": null,
              "additional_rate_limits": null,
              "rate_limit_reset_credits": null
            }"#,
        )
        .unwrap();
        let snapshot = to_snapshot(body);
        assert_eq!(snapshot.plan_type.as_deref(), Some("free"));
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].label, "5 小时额度");
        assert!(snapshot.reset_credits.is_none());
    }

    #[test]
    fn marks_limit_reached_from_main_window_only() {
        let body = serde_json::from_str::<UsageResponse>(
            r#"{
              "rate_limit": {
                "primary_window": {"used_percent": 100.0, "limit_window_seconds": 18000}
              },
              "code_review_rate_limit": {
                "primary_window": {"used_percent": 100.0}
              }
            }"#,
        )
        .unwrap();
        assert!(to_snapshot(body).limit_reached);

        let body = serde_json::from_str::<UsageResponse>(
            r#"{
              "rate_limit": {"primary_window": {"used_percent": 10.0}},
              "code_review_rate_limit": {"primary_window": {"used_percent": 100.0}}
            }"#,
        )
        .unwrap();
        assert!(!to_snapshot(body).limit_reached);
    }

    #[test]
    fn parses_weekly_grok_billing() {
        let payload = serde_json::json!({
            "config": {
                "currentPeriod": {
                    "type": "weekly",
                    "start": "2026-08-10T00:00:00Z",
                    "end": "2026-08-17T00:00:00Z"
                },
                "creditUsagePercent": "42.5",
                "productUsage": [
                    {"product": "grok-4.5", "usagePercent": 30.0},
                    {"usage_percent": 12.0}
                ]
            }
        });
        let summary = parse_grok_billing(&payload).unwrap();
        assert_eq!(summary.period_type.as_deref(), Some("每周"));
        assert_eq!(summary.usage_percent, Some(42.5));
        assert_eq!(summary.period_seconds, Some(7 * 86_400));
        assert_eq!(summary.products.len(), 2);
        assert_eq!(summary.products[0].product, "grok-4.5");
        assert_eq!(summary.products[1].product, "产品 2");
    }

    #[test]
    fn parses_monthly_grok_billing_with_on_demand() {
        let payload = serde_json::json!({
            "config": {
                "monthlyLimit": {"val": 2000},
                "used": 2500,
                "onDemandCap": "1000",
                "billing_period_start": 1_900_000_000i64,
                "billing_period_end": 1_902_592_000i64
            }
        });
        let summary = parse_grok_billing(&payload).unwrap();
        assert_eq!(summary.period_type.as_deref(), Some("每月"));
        assert_eq!(summary.included_used_cents, Some(2000.0));
        assert_eq!(summary.usage_percent, Some(100.0));
        assert_eq!(summary.on_demand_used_cents, Some(500.0));
        assert_eq!(summary.on_demand_used_percent, Some(50.0));
        assert_eq!(summary.period_end, Some(1_902_592_000));
    }

    #[test]
    fn ignores_empty_grok_billing() {
        assert!(parse_grok_billing(&serde_json::json!({"config": {}})).is_none());
        assert!(parse_grok_billing(&serde_json::json!({})).is_none());
    }

    #[test]
    fn merges_weekly_over_monthly_billing() {
        let weekly = parse_grok_billing(&serde_json::json!({
            "config": {"creditUsagePercent": 10.0, "currentPeriod": {"type": "weekly"}}
        }));
        let monthly = parse_grok_billing(&serde_json::json!({
            "config": {"monthlyLimit": 2000, "used": 400}
        }));
        let merged = merge_grok_billing(weekly, monthly).unwrap();
        assert_eq!(merged.period_type.as_deref(), Some("每周"));
        assert_eq!(merged.usage_percent, Some(10.0));
        assert_eq!(merged.monthly_limit_cents, Some(2000.0));
        assert_eq!(merged.used_cents, Some(400.0));
    }
}

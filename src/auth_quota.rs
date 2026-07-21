use crate::config::AuthAccountConfig;
use chrono::{TimeZone, Utc};
use serde::Deserialize;
use serde_json::Value;

const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_USAGE_BASE: &str = "https://chatgpt.com/backend-api";
const REFRESH_EARLY_SECONDS: i64 = 300;

#[derive(Debug, Clone, Default)]
pub struct AuthQuotaSnapshot {
    pub plan_type: Option<String>,
    pub allowed: Option<bool>,
    pub limit_reached: bool,
    pub primary: Option<AuthQuotaWindow>,
    pub secondary: Option<AuthQuotaWindow>,
    pub code_review: Option<AuthQuotaWindow>,
    pub checked_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct AuthQuotaWindow {
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

#[derive(Debug, Deserialize)]
struct UsageResponse {
    plan_type: Option<String>,
    rate_limit: Option<RateLimitBody>,
    code_review_rate_limit: Option<RateLimitBody>,
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
            let snapshot = fetch_usage(&access_token, account.account_id.as_deref())?;
            return Ok(AuthQuotaRefreshResult {
                snapshot,
                access_token,
                refresh_token,
                expires_at,
            });
        };
        let refreshed = refresh_access_token(account, &current_refresh)?;
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

    let snapshot = fetch_usage(&access_token, account.account_id.as_deref())?;
    Ok(AuthQuotaRefreshResult {
        snapshot,
        access_token,
        refresh_token,
        expires_at,
    })
}

fn fetch_usage(access_token: &str, account_id: Option<&str>) -> Result<AuthQuotaSnapshot, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|err| err.to_string())?;

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
) -> Result<RefreshResponse, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|err| err.to_string())?;
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
    let rate_limit = body.rate_limit.unwrap_or(RateLimitBody {
        allowed: None,
        limit_reached: None,
        primary_window: None,
        secondary_window: None,
    });
    let primary = window_from_body(rate_limit.primary_window);
    let secondary = window_from_body(rate_limit.secondary_window);
    let code_review = body
        .code_review_rate_limit
        .and_then(|value| window_from_body(value.primary_window));
    let limit_reached = rate_limit.limit_reached.unwrap_or(false)
        || primary
            .as_ref()
            .is_some_and(|window| window.used_percent >= 100.0)
        || secondary
            .as_ref()
            .is_some_and(|window| window.used_percent >= 100.0);
    AuthQuotaSnapshot {
        plan_type: body.plan_type,
        allowed: rate_limit.allowed,
        limit_reached,
        primary,
        secondary,
        code_review,
        checked_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    }
}

fn window_from_body(body: Option<WindowBody>) -> Option<AuthQuotaWindow> {
    let body = body?;
    let used_percent = body.used_percent.unwrap_or(0.0);
    let reset_at = body.reset_at.or_else(|| {
        body.reset_after_seconds
            .map(|seconds| Utc::now().timestamp() + seconds)
    });
    Some(AuthQuotaWindow {
        used_percent,
        reset_at,
        limit_window_seconds: body.limit_window_seconds,
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
                  "limit_window_seconds": 10800,
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
              }
            }"#,
        )
        .unwrap();
        let snapshot = to_snapshot(body);
        assert_eq!(snapshot.plan_type.as_deref(), Some("plus"));
        assert_eq!(snapshot.allowed, Some(true));
        assert!(!snapshot.limit_reached);
        assert_eq!(snapshot.primary.as_ref().unwrap().used_percent, 12.5);
        assert_eq!(snapshot.secondary.as_ref().unwrap().used_percent, 40.0);
        assert_eq!(snapshot.code_review.as_ref().unwrap().used_percent, 5.0);
    }
}

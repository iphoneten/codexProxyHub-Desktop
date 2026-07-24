use super::*;
use crate::config::AuthAccountConfig;

const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const GROK_BASE_URL: &str = "https://api.x.ai/v1";
const GROK_CLIENT_VERSION: &str = "0.2.109";
const REFRESH_EARLY_SECONDS: i64 = 300;

#[derive(Clone)]
pub(super) struct OAuthRuntimeToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
}

#[derive(serde::Deserialize)]
struct RefreshResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

pub(super) fn auth_account_as_provider(
    account: &AuthAccountConfig,
    auth: &crate::config::AuthConfig,
) -> ProviderConfig {
    let mut extra_headers = HashMap::new();
    let is_grok = is_grok_account(account);
    if is_grok {
        extra_headers.insert(
            "x-grok-client-version".to_string(),
            GROK_CLIENT_VERSION.to_string(),
        );
        extra_headers.insert(
            "x-grok-client-identifier".to_string(),
            "grok-shell".to_string(),
        );
        extra_headers.insert(
            "User-Agent".to_string(),
            format!("xai-grok-build/{GROK_CLIENT_VERSION}"),
        );
        if is_grok_cli_proxy(account) {
            extra_headers.insert("X-XAI-Token-Auth".to_string(), "xai-grok-cli".to_string());
        }
    } else {
        if let Some(account_id) = account
            .account_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            extra_headers.insert("ChatGPT-Account-Id".to_string(), account_id.to_string());
        }
    }

    let mut capabilities = HashMap::new();
    if is_grok {
        capabilities.insert("supports_chat".to_string(), true);
        capabilities.insert("supports_responses".to_string(), false);
    } else {
        capabilities.insert("supports_chat".to_string(), false);
        capabilities.insert("supports_responses".to_string(), true);
    }

    ProviderConfig {
        name: format!("auth:{}", account.id),
        enabled: account.enabled,
        provider_type: if is_grok { "openai" } else { "codex_only" }.to_string(),
        base_url: if is_grok {
            account_base_url(account, GROK_BASE_URL)
        } else {
            CODEX_BASE_URL.to_string()
        },
        website: None,
        api_key: account.access_token.clone(),
        models: auth_account_models(account, auth),
        model_mapping: account.model_mapping.clone(),
        extra_headers,
        capabilities,
        health_check_mode: "none".to_string(),
        model_sync_filter: "all".to_string(),
        responses_mode: if is_grok { "chat" } else { "native" }.to_string(),
        client_mode: "normal".to_string(),
        connect_timeout: 10,
        request_timeout: 120,
        stream_idle_timeout: 0,
        stream_max_duration: 0,
        debug_capture_sse: false,
        debug_sse_path: "logs/raw_sse".to_string(),
        debug_sse_max_events: 80,
        max_retries: 2,
        weight: account.weight.max(1),
        priority: account.priority,
        description: account.description.clone().or_else(|| {
            Some(if is_grok {
                "Grok Auth 账号".to_string()
            } else {
                format!(
                    "OpenAI Auth 账号{}",
                    account
                        .email
                        .as_deref()
                        .map(|email| format!(" ({email})"))
                        .unwrap_or_default()
                )
            })
        }),
        system_prompt_override: None,
        strip_thought: false,
        extra: Default::default(),
        auth_account_id: Some(account.id.clone()),
    }
}

fn is_grok_account(account: &AuthAccountConfig) -> bool {
    let ty = account.account_type.trim();
    ty.eq_ignore_ascii_case("grok") || ty.eq_ignore_ascii_case("gork")
}

fn is_grok_cli_proxy(account: &AuthAccountConfig) -> bool {
    account
        .base_url
        .trim()
        .to_ascii_lowercase()
        .contains("cli-chat-proxy.grok.com")
}

fn account_base_url(account: &AuthAccountConfig, default: &str) -> String {
    let value = account.base_url.trim();
    if value.is_empty() {
        default.to_string()
    } else {
        value.to_string()
    }
}

fn auth_account_models(
    account: &AuthAccountConfig,
    auth: &crate::config::AuthConfig,
) -> Vec<String> {
    if is_grok_account(account) {
        auth.grok_models.clone()
    } else {
        auth.openai_models.clone()
    }
}

impl AppState {
    pub(super) async fn provider_with_fresh_oauth(
        &self,
        provider: &ProviderConfig,
    ) -> Result<ProviderConfig, ProxyError> {
        let Some(account_id) = provider
            .auth_account_id
            .as_deref()
            .filter(|id| !id.is_empty())
        else {
            return Ok(provider.clone());
        };

        let account = self
            .snapshot()
            .auth_accounts
            .iter()
            .find(|account| account.id == account_id)
            .cloned();
        let Some(account) = account else {
            return Ok(provider.clone());
        };
        if is_grok_account(&account) {
            return Ok(provider.clone());
        }

        let cached = self.oauth_tokens.lock().get(account_id).cloned();
        let current = cached.unwrap_or_else(|| OAuthRuntimeToken {
            access_token: account.access_token.clone(),
            refresh_token: account.refresh_token.clone(),
            expires_at: account
                .expires_at
                .or_else(|| jwt_exp(&account.access_token)),
        });
        if !current.access_token.trim().is_empty() && !should_refresh(current.expires_at) {
            return Ok(apply_runtime_token(provider, current));
        }

        let refresh_lock = self
            .oauth_refresh_locks
            .lock()
            .entry(account_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = refresh_lock.lock().await;
        let current = self
            .oauth_tokens
            .lock()
            .get(account_id)
            .cloned()
            .unwrap_or(current);
        if !current.access_token.trim().is_empty() && !should_refresh(current.expires_at) {
            return Ok(apply_runtime_token(provider, current));
        }

        let Some(refresh_token) = current
            .refresh_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
        else {
            return Ok(apply_runtime_token(provider, current));
        };

        let refreshed = refresh_openai_token(self, provider, &account, refresh_token).await?;
        self.oauth_tokens
            .lock()
            .insert(account_id.to_string(), refreshed.clone());
        Ok(apply_runtime_token(provider, refreshed))
    }
}

async fn refresh_openai_token(
    state: &AppState,
    provider: &ProviderConfig,
    account: &AuthAccountConfig,
    refresh_token: &str,
) -> Result<OAuthRuntimeToken, ProxyError> {
    let client = state.client_for_provider(provider);
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
        .timeout(Duration::from_secs(provider.request_timeout.max(1)))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .map_err(|err| {
            ProxyError::new(
                StatusCode::BAD_GATEWAY,
                format!("OpenAI OAuth 刷新请求失败: {err}"),
            )
        })?;
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(ProxyError::new(
            StatusCode::BAD_GATEWAY,
            format!(
                "OpenAI OAuth 刷新失败 ({status}): {}",
                clean_upstream_error(&text)
            ),
        ));
    }
    let response: RefreshResponse = serde_json::from_str(&text).map_err(|err| {
        ProxyError::new(
            StatusCode::BAD_GATEWAY,
            format!("OpenAI OAuth 刷新响应无效: {err}"),
        )
    })?;
    let expires_at = response
        .expires_in
        .map(|seconds| chrono::Utc::now().timestamp() + seconds)
        .or_else(|| jwt_exp(&response.access_token));
    Ok(OAuthRuntimeToken {
        access_token: response.access_token,
        refresh_token: response
            .refresh_token
            .or_else(|| Some(refresh_token.to_string())),
        expires_at,
    })
}

fn apply_runtime_token(provider: &ProviderConfig, token: OAuthRuntimeToken) -> ProviderConfig {
    let mut provider = provider.clone();
    provider.api_key = token.access_token;
    provider
}

fn should_refresh(expires_at: Option<i64>) -> bool {
    expires_at.is_some_and(|expires_at| {
        expires_at <= chrono::Utc::now().timestamp() + REFRESH_EARLY_SECONDS
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn grok_account(base_url: &str) -> AuthAccountConfig {
        AuthAccountConfig {
            id: "grok-acct".into(),
            account_type: "grok".into(),
            name: "Grok Main".into(),
            enabled: true,
            email: None,
            access_token: "xai-key".into(),
            refresh_token: None,
            account_id: None,
            client_id: String::new(),
            token_url: String::new(),
            base_url: base_url.into(),
            expires_at: None,
            models: vec!["grok-4.5".into()],
            model_mapping: Default::default(),
            weight: 1,
            priority: 1,
            description: None,
        }
    }

    #[test]
    fn decodes_jwt_expiry() {
        let token = "header.eyJleHAiOjE5MDAwMDAwMDB9.signature";
        assert_eq!(jwt_exp(token), Some(1_900_000_000));
    }

    #[test]
    fn converts_auth_account_into_codex_provider() {
        let account = AuthAccountConfig {
            id: "acct".into(),
            account_type: "openai".into(),
            name: "user@example.com".into(),
            enabled: true,
            email: Some("user@example.com".into()),
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            account_id: Some("chatgpt-acct".into()),
            client_id: OPENAI_OAUTH_CLIENT_ID.into(),
            token_url: OPENAI_OAUTH_TOKEN_URL.into(),
            base_url: String::new(),
            expires_at: None,
            models: vec!["gpt-5.4".into()],
            model_mapping: Default::default(),
            weight: 1,
            priority: 1,
            description: None,
        };
        let provider = auth_account_as_provider(&account, &crate::config::AuthConfig::default());
        assert_eq!(provider.name, "auth:acct");
        assert_eq!(provider.api_key, "access");
        assert_eq!(provider.provider_type, "codex_only");
        assert_eq!(
            provider
                .extra_headers
                .get("ChatGPT-Account-Id")
                .map(String::as_str),
            Some("chatgpt-acct")
        );
    }

    #[test]
    fn converts_grok_account_into_openai_compatible_provider() {
        let account = grok_account("https://api.x.ai/v1");

        let provider = auth_account_as_provider(&account, &crate::config::AuthConfig::default());

        assert_eq!(provider.name, "auth:grok-acct");
        assert_eq!(provider.provider_type, "openai");
        assert_eq!(provider.base_url, "https://api.x.ai/v1");
        assert_eq!(provider.responses_mode, "chat");
        assert_eq!(provider.api_key, "xai-key");
        assert_eq!(
            provider
                .extra_headers
                .get("x-grok-client-version")
                .map(String::as_str),
            Some(GROK_CLIENT_VERSION)
        );
        assert_eq!(
            provider
                .extra_headers
                .get("x-grok-client-identifier")
                .map(String::as_str),
            Some("grok-shell")
        );
        assert_eq!(
            provider.extra_headers.get("User-Agent").map(String::as_str),
            Some("xai-grok-build/0.2.109")
        );
        assert!(!provider.extra_headers.contains_key("X-XAI-Token-Auth"));
        assert_eq!(provider.capabilities.get("supports_chat"), Some(&true));
        assert_eq!(
            provider.capabilities.get("supports_responses"),
            Some(&false)
        );
    }

    #[test]
    fn adds_cli_token_header_for_grok_cli_proxy_account() {
        let account = grok_account("https://cli-chat-proxy.grok.com/v1");

        let provider = auth_account_as_provider(&account, &crate::config::AuthConfig::default());

        assert_eq!(
            provider
                .extra_headers
                .get("X-XAI-Token-Auth")
                .map(String::as_str),
            Some("xai-grok-cli")
        );
    }
}

use crate::config::{AppConfig, AuthAccountConfig};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, NaiveDateTime};
use serde_json::{json, Value};
use std::{collections::HashMap, fs, path::Path};

const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

pub struct ImportResult {
    pub config: AppConfig,
    pub imported_accounts: usize,
    pub replaced_config: bool,
}

pub fn import_file(source: &Path, current: Option<&AppConfig>) -> Result<ImportResult> {
    let content = fs::read_to_string(source)
        .with_context(|| format!("读取导入文件失败: {}", source.display()))?;
    let is_json = source
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("json"));
    if !is_json {
        let config = serde_yaml::from_str::<AppConfig>(&content)
            .with_context(|| format!("解析 YAML 失败: {}", source.display()))?;
        return Ok(ImportResult {
            config,
            imported_accounts: 0,
            replaced_config: true,
        });
    }

    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("解析 JSON 失败: {}", source.display()))?;
    if value.get("providers").is_some() || value.get("server").is_some() {
        let config = serde_json::from_value::<AppConfig>(value)
            .with_context(|| format!("解析 RouteHub JSON 配置失败: {}", source.display()))?;
        return Ok(ImportResult {
            config,
            imported_accounts: 0,
            replaced_config: true,
        });
    }
    let imported = parse_auth_accounts(&value)?;
    let mut config = current
        .cloned()
        .ok_or_else(|| anyhow!("导入 OAuth JSON 前需要先加载当前配置"))?;
    let count = merge_accounts(&mut config.auth_accounts, imported);
    Ok(ImportResult {
        config,
        imported_accounts: count,
        replaced_config: false,
    })
}

pub fn import_auth_accounts_file(config: &mut AppConfig, source: &Path) -> Result<usize> {
    let content = fs::read_to_string(source)
        .with_context(|| format!("读取 Auth 账号文件失败: {}", source.display()))?;
    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("解析 Auth 账号 JSON 失败: {}", source.display()))?;
    let imported = parse_auth_accounts(&value)?;
    Ok(merge_accounts(&mut config.auth_accounts, imported))
}

pub fn export_auth_accounts_file(config: &AppConfig, target: &Path) -> Result<()> {
    if let Some(parent) = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let payload = json!({
        "type": "routehub-auth-accounts",
        "version": 1,
        "auth_accounts": config.auth_accounts,
    });
    let content = serde_json::to_string_pretty(&payload)?;
    fs::write(target, content)
        .with_context(|| format!("写入 Auth 账号文件失败: {}", target.display()))
}

pub fn parse_auth_accounts(value: &Value) -> Result<Vec<AuthAccountConfig>> {
    let entries = account_entries(value);
    let mut accounts = Vec::new();
    for (index, entry) in entries.into_iter().enumerate() {
        if let Some(account) = account_from_entry(entry, index + 1) {
            accounts.push(account);
        }
    }
    if accounts.is_empty() {
        return Err(anyhow!(
            "JSON 中未找到 access_token、refresh_token、token 或 refreshToken"
        ));
    }
    Ok(accounts)
}

fn account_entries(value: &Value) -> Vec<&Value> {
    if let Some(items) = value.as_array() {
        return items.iter().collect();
    }
    if let Some(items) = value
        .pointer("/data/accounts")
        .or_else(|| value.get("auth_accounts"))
        .or_else(|| value.get("accounts"))
        .and_then(Value::as_array)
    {
        return items.iter().collect();
    }
    vec![value]
}

fn account_from_entry(entry: &Value, index: usize) -> Option<AuthAccountConfig> {
    let credentials = entry.get("credentials").unwrap_or(entry);
    let access_token = string_field(credentials, &["access_token", "accessToken", "token"])
        .or_else(|| string_field(entry, &["access_token", "accessToken", "token"]));
    let refresh_token = string_field(credentials, &["refresh_token", "refreshToken"])
        .or_else(|| string_field(entry, &["refresh_token", "refreshToken"]));
    if access_token.is_none() && refresh_token.is_none() {
        return None;
    }

    let account_id = string_field(
        credentials,
        &["chatgpt_account_id", "account_id", "accountId"],
    )
    .or_else(|| string_field(entry, &["account_id", "accountId"]));
    let email = string_field(entry, &["email"]);
    let label = string_field(entry, &["name", "email", "label", "id"])
        .unwrap_or_else(|| format!("openai-oauth-{index}"));
    let id = string_field(entry, &["id"])
        .or_else(|| account_id.clone())
        .unwrap_or_else(|| stable_account_id(&label, index));
    let models = credential_models(credentials);
    let model_mapping = credentials
        .get("model_mapping")
        .and_then(Value::as_object)
        .map(|mapping| {
            mapping
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let expires_at = credentials
        .get("expires_at")
        .or_else(|| entry.get("expires_at"))
        .and_then(timestamp_value)
        .or_else(|| string_field(entry, &["expired"]).and_then(|value| parse_datetime(&value)));

    serde_json::from_value(json!({
        "id": id,
        "name": label,
        "enabled": true,
        "email": email,
        "access_token": access_token,
        "refresh_token": refresh_token,
        "client_id": string_field(credentials, &["client_id", "clientId"])
            .unwrap_or_else(|| OPENAI_OAUTH_CLIENT_ID.to_string()),
        "token_url": OPENAI_OAUTH_TOKEN_URL,
        "expires_at": expires_at,
        "account_id": account_id,
        "models": models,
        "model_mapping": model_mapping,
        "description": "从 CPA/sub2api JSON 导入的 OpenAI OAuth 账号"
    }))
    .ok()
}

fn credential_models(credentials: &Value) -> Vec<String> {
    if let Some(mapping) = credentials.get("model_mapping").and_then(Value::as_object) {
        let mut models = mapping.keys().cloned().collect::<Vec<_>>();
        models.sort();
        if !models.is_empty() {
            return models;
        }
    }
    crate::config::default_auth_account_models()
}

pub fn merge_accounts(
    existing: &mut Vec<AuthAccountConfig>,
    imported: Vec<AuthAccountConfig>,
) -> usize {
    let mut changed = 0;
    let mut insert_at = 0;
    for account in imported {
        let duplicate = existing.iter_mut().find(|current| {
            current.id == account.id
                || same_str(&current.access_token, &account.access_token)
                || same_secret(&current.refresh_token, &account.refresh_token)
                || same_secret(&current.account_id, &account.account_id)
        });
        if let Some(current) = duplicate {
            current.name = account.name;
            current.enabled = account.enabled;
            current.email = account.email;
            current.access_token = account.access_token;
            current.refresh_token = account.refresh_token;
            current.client_id = account.client_id;
            current.token_url = account.token_url;
            current.expires_at = account.expires_at;
            current.account_id = account.account_id;
            current.models = account.models;
            current.model_mapping = account.model_mapping;
            changed += 1;
        } else {
            // 新账号插到列表前面，并保持本次导入的相对顺序
            existing.insert(insert_at, account);
            insert_at += 1;
            changed += 1;
        }
    }
    changed
}

fn same_secret(left: &Option<String>, right: &Option<String>) -> bool {
    match (left.as_deref(), right.as_deref()) {
        (Some(left), Some(right)) => !left.is_empty() && left == right,
        _ => false,
    }
}

fn same_str(left: &str, right: &str) -> bool {
    !left.is_empty() && left == right
}

fn string_field(value: &Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        value
            .get(*name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn timestamp_value(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn parse_datetime(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.timestamp())
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S")
                .map(|value| value.and_utc().timestamp())
                .ok()
        })
}

fn stable_account_id(label: &str, index: usize) -> String {
    let label = label.trim();
    if label.is_empty() {
        format!("openai-oauth-{index}")
    } else {
        format!("openai-oauth-{label}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cpa_token_json() {
        let accounts = parse_auth_accounts(&json!({
            "type": "codex",
            "email": "user@example.com",
            "account_id": "acct_123",
            "access_token": "access",
            "refresh_token": "refresh"
        }))
        .unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].access_token, "access");
        assert_eq!(accounts[0].account_id.as_deref(), Some("acct_123"));
    }

    #[test]
    fn parses_nested_sub2api_data() {
        let accounts = parse_auth_accounts(&json!({
            "data": {
                "type": "sub2api-data",
                "accounts": [{
                    "name": "user@example.com",
                    "credentials": {
                        "access_token": "access",
                        "refresh_token": "refresh",
                        "chatgpt_account_id": "acct_456",
                        "model_mapping": {"gpt-5.4": "gpt-5.4"}
                    }
                }]
            }
        }))
        .unwrap();
        assert_eq!(accounts[0].models, vec!["gpt-5.4"]);
        assert_eq!(accounts[0].account_id.as_deref(), Some("acct_456"));
    }

    #[test]
    fn parses_routehub_auth_accounts_export() {
        let accounts = parse_auth_accounts(&json!({
            "type": "routehub-auth-accounts",
            "version": 1,
            "auth_accounts": [{
                "id": "auth-1",
                "name": "OpenAI Auth",
                "enabled": true,
                "access_token": "access",
                "refresh_token": "refresh",
                "models": ["gpt-5.4"]
            }]
        }))
        .unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, "auth-1");
        assert_eq!(accounts[0].refresh_token.as_deref(), Some("refresh"));
    }
}

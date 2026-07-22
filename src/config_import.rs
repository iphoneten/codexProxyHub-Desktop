use crate::config::{AppConfig, AuthAccountConfig};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, NaiveDateTime};
use flate2::read::DeflateDecoder;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

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

#[allow(dead_code)]
pub fn import_auth_accounts_file(config: &mut AppConfig, source: &Path) -> Result<usize> {
    import_auth_accounts_file_for_type(config, source, None)
}

pub fn import_auth_accounts_file_for_type(
    config: &mut AppConfig,
    source: &Path,
    account_type: Option<&str>,
) -> Result<usize> {
    if source
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("zip"))
    {
        return import_auth_accounts_zip_for_type(config, source, account_type);
    }
    let content = fs::read_to_string(source)
        .with_context(|| format!("读取 Auth 账号文件失败: {}", source.display()))?;
    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("解析 Auth 账号 JSON 失败: {}", source.display()))?;
    let mut imported = parse_auth_accounts(&value)?;
    if let Some(account_type) = account_type {
        for account in &mut imported {
            account.account_type = account_type.to_string();
        }
    }
    Ok(merge_accounts(&mut config.auth_accounts, imported))
}

pub fn import_auth_accounts_paths_for_type(
    config: &mut AppConfig,
    sources: &[PathBuf],
    account_type: Option<&str>,
) -> Result<(usize, Vec<String>)> {
    let mut total = 0usize;
    let mut errors = Vec::new();
    for source in sources {
        match import_auth_accounts_file_for_type(config, source, account_type) {
            Ok(count) => total += count,
            Err(err) => errors.push(format!("{}: {err}", source.display())),
        }
    }
    if total == 0 {
        return Err(anyhow!(
            "导入 Auth 账号失败: {}",
            if errors.is_empty() {
                "未选择有效文件".to_string()
            } else {
                errors.join("；")
            }
        ));
    }
    Ok((total, errors))
}

fn import_auth_accounts_zip_for_type(
    config: &mut AppConfig,
    source: &Path,
    account_type: Option<&str>,
) -> Result<usize> {
    let entries = read_zip_json_entries(source)?;
    if entries.is_empty() {
        return Err(anyhow!("ZIP 中未找到 JSON 文件: {}", source.display()));
    }

    let mut imported = Vec::new();
    let mut errors = Vec::new();
    for (name, content) in entries {
        match serde_json::from_slice::<Value>(&content)
            .with_context(|| format!("解析 ZIP 内 JSON 失败: {name}"))
            .and_then(|value| parse_auth_accounts(&value))
        {
            Ok(mut accounts) => {
                if let Some(account_type) = account_type {
                    for account in &mut accounts {
                        account.account_type = account_type.to_string();
                    }
                }
                imported.extend(accounts);
            }
            Err(err) => errors.push(format!("{name}: {err}")),
        }
    }

    if imported.is_empty() {
        let detail = errors.join("；");
        return Err(anyhow!("ZIP 中未导入任何 Auth 账号: {detail}"));
    }

    Ok(merge_accounts(&mut config.auth_accounts, imported))
}

fn read_zip_json_entries(source: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let data =
        fs::read(source).with_context(|| format!("读取 ZIP 文件失败: {}", source.display()))?;
    let mut offset = 0usize;
    let mut entries = Vec::new();

    while offset + 30 <= data.len() {
        let signature = read_u32_le(&data, offset)?;
        if signature == 0x0201_4b50 || signature == 0x0605_4b50 {
            break;
        }
        if signature != 0x0403_4b50 {
            return Err(anyhow!("ZIP 本地文件头无效: {}", source.display()));
        }

        let flags = read_u16_le(&data, offset + 6)?;
        if flags & 0x0008 != 0 {
            return Err(anyhow!(
                "暂不支持带 data descriptor 的 ZIP 文件: {}",
                source.display()
            ));
        }
        let method = read_u16_le(&data, offset + 8)?;
        let compressed_size = read_u32_le(&data, offset + 18)? as usize;
        let file_name_len = read_u16_le(&data, offset + 26)? as usize;
        let extra_len = read_u16_le(&data, offset + 28)? as usize;
        let name_start = offset + 30;
        let name_end = name_start + file_name_len;
        let data_start = name_end + extra_len;
        let data_end = data_start + compressed_size;
        if data_end > data.len() {
            return Err(anyhow!("ZIP 文件条目长度越界: {}", source.display()));
        }
        let name = String::from_utf8_lossy(&data[name_start..name_end]).to_string();
        if name.to_ascii_lowercase().ends_with(".json") && !name.ends_with('/') {
            let content = match method {
                0 => data[data_start..data_end].to_vec(),
                8 => {
                    let mut decoder = DeflateDecoder::new(&data[data_start..data_end]);
                    let mut out = Vec::new();
                    decoder.read_to_end(&mut out)?;
                    out
                }
                other => return Err(anyhow!("ZIP 条目 {name} 使用了不支持的压缩方法: {other}")),
            };
            entries.push((name, content));
        }
        offset = data_end;
    }

    Ok(entries)
}

fn read_u16_le(data: &[u8], offset: usize) -> Result<u16> {
    let bytes = data
        .get(offset..offset + 2)
        .ok_or_else(|| anyhow!("ZIP 文件截断"))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32_le(data: &[u8], offset: usize) -> Result<u32> {
    let bytes = data
        .get(offset..offset + 4)
        .ok_or_else(|| anyhow!("ZIP 文件截断"))?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[allow(dead_code)]
pub fn export_auth_accounts_file(config: &AppConfig, target: &Path) -> Result<()> {
    export_auth_accounts_file_for_type(config, target, None)
}

pub fn export_auth_accounts_file_for_type(
    config: &AppConfig,
    target: &Path,
    account_type: Option<&str>,
) -> Result<()> {
    if let Some(parent) = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let auth_accounts = config
        .auth_accounts
        .iter()
        .filter(|account| {
            account_type.is_none_or(|ty| account.account_type.trim().eq_ignore_ascii_case(ty))
        })
        .collect::<Vec<_>>();
    let payload = json!({
        "type": "routehub-auth-accounts",
        "version": 1,
        "auth_accounts": auth_accounts,
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
        "account_type": string_field(entry, &["account_type", "type", "provider"])
            .unwrap_or_else(|| "openai".to_string()),
        "name": label,
        "enabled": true,
        "email": email,
        "access_token": access_token,
        "refresh_token": refresh_token,
        "client_id": string_field(credentials, &["client_id", "clientId"])
            .unwrap_or_else(|| OPENAI_OAUTH_CLIENT_ID.to_string()),
        "token_url": OPENAI_OAUTH_TOKEN_URL,
        "base_url": string_field(credentials, &["base_url", "baseUrl"])
            .or_else(|| string_field(entry, &["base_url", "baseUrl"]))
            .unwrap_or_default(),
        "expires_at": expires_at,
        "account_id": account_id,
        "models": Vec::<String>::new(),
        "model_mapping": model_mapping,
        "description": "从 CPA/sub2api JSON 导入的 OpenAI OAuth 账号"
    }))
    .ok()
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
        assert!(accounts[0].models.is_empty());
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

    #[test]
    fn imports_auth_accounts_from_zip_batch() {
        let dir = std::env::temp_dir().join(format!(
            "routehub-auth-zip-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("accounts.zip");
        write_stored_zip(
            &zip_path,
            &[
                (
                    "one.json",
                    br#"{"name":"one","access_token":"access-1","refresh_token":"refresh-1"}"#
                        .as_slice(),
                ),
                (
                    "nested/two.json",
                    br#"{"name":"two","access_token":"access-2","refresh_token":"refresh-2"}"#
                        .as_slice(),
                ),
            ],
        );

        let mut config = AppConfig::load("config.example.yaml").unwrap();
        let count =
            import_auth_accounts_file_for_type(&mut config, &zip_path, Some("grok")).unwrap();

        assert_eq!(count, 2);
        assert_eq!(config.auth_accounts.len(), 2);
        assert!(config
            .auth_accounts
            .iter()
            .all(|account| account.account_type == "grok"));

        let _ = std::fs::remove_file(zip_path);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn write_stored_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut data = Vec::new();
        let mut central = Vec::new();
        for (name, content) in entries {
            let local_offset = data.len() as u32;
            let size = content.len() as u32;
            let name_len = name.len() as u16;
            data.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            data.extend_from_slice(&20u16.to_le_bytes());
            data.extend_from_slice(&0u16.to_le_bytes());
            data.extend_from_slice(&0u16.to_le_bytes());
            data.extend_from_slice(&0u16.to_le_bytes());
            data.extend_from_slice(&0u16.to_le_bytes());
            data.extend_from_slice(&0u32.to_le_bytes());
            data.extend_from_slice(&size.to_le_bytes());
            data.extend_from_slice(&size.to_le_bytes());
            data.extend_from_slice(&name_len.to_le_bytes());
            data.extend_from_slice(&0u16.to_le_bytes());
            data.extend_from_slice(name.as_bytes());
            data.extend_from_slice(content);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&name_len.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&local_offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let central_offset = data.len() as u32;
        let central_size = central.len() as u32;
        data.extend_from_slice(&central);
        data.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        data.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        data.extend_from_slice(&central_size.to_le_bytes());
        data.extend_from_slice(&central_offset.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        std::fs::write(path, data).unwrap();
    }
}

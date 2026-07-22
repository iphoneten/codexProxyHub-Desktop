use crate::config::AuthAccountConfig;
use parking_lot::Mutex;
use rand::RngCore;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_OAUTH_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const OPENAI_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OAUTH_CALLBACK_PORT: u16 = 1455;
const OAUTH_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const OAUTH_SESSION_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone)]
pub struct OAuthAccountTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone)]
pub enum OAuthLoginOutcome {
    Success(OAuthAccountTokens),
    Failed(String),
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    id_token: Option<String>,
}

/// 启动 ChatGPT OAuth（PKCE）流程：
/// 1. 监听 localhost:1455/auth/callback
/// 2. 浏览器打开 auth.openai.com 授权页
/// 3. 回调后用 code 换取 access_token / refresh_token
pub fn start_chatgpt_oauth_login(
    pending: Arc<Mutex<Vec<OAuthLoginOutcome>>>,
    cancelled: Arc<AtomicBool>,
    proxy_url: Option<String>,
) -> Result<(), String> {
    let session = create_session()?;
    let listener = TcpListener::bind(("127.0.0.1", OAUTH_CALLBACK_PORT)).map_err(|err| {
        format!(
            "无法监听 OAuth 回调端口 {OAUTH_CALLBACK_PORT}: {err}。请确认该端口未被占用后重试。"
        )
    })?;
    listener
        .set_nonblocking(false)
        .map_err(|err| format!("设置回调监听失败: {err}"))?;

    open_browser(&session.auth_url).map_err(|err| {
        let _ = listener;
        format!("打开浏览器失败: {err}")
    })?;

    let pending_clone = Arc::clone(&pending);
    thread::spawn(move || {
        run_callback_server(listener, session, pending_clone, cancelled, proxy_url);
    });
    Ok(())
}

struct OAuthSession {
    state: String,
    code_verifier: String,
    auth_url: String,
}

fn create_session() -> Result<OAuthSession, String> {
    let (code_verifier, code_challenge) = generate_pkce();
    let state = random_hex(16);
    let auth_url = build_auth_url(OAUTH_REDIRECT_URI, &state, &code_challenge);
    Ok(OAuthSession {
        state,
        code_verifier,
        auth_url,
    })
}

fn run_callback_server(
    listener: TcpListener,
    session: OAuthSession,
    pending: Arc<Mutex<Vec<OAuthLoginOutcome>>>,
    cancelled: Arc<AtomicBool>,
    proxy_url: Option<String>,
) {
    if let Err(err) = listener.set_nonblocking(true) {
        pending.lock().push(OAuthLoginOutcome::Failed(format!(
            "设置回调监听失败: {err}"
        )));
        return;
    }
    let started = Instant::now();
    let mut handled = false;

    while started.elapsed() < OAUTH_SESSION_TTL && !handled && !cancelled.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                handled =
                    handle_connection(stream, &session, &pending, &cancelled, proxy_url.as_deref());
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                thread::sleep(Duration::from_millis(100));
            }
            Err(err) => {
                pending.lock().push(OAuthLoginOutcome::Failed(format!(
                    "OAuth 回调服务异常: {err}"
                )));
                break;
            }
        }
    }

    if !handled && !cancelled.load(Ordering::Relaxed) {
        pending.lock().push(OAuthLoginOutcome::Failed(
            "OAuth 授权超时，请重新点击新增账号".to_string(),
        ));
    }
}

fn handle_connection(
    mut stream: TcpStream,
    session: &OAuthSession,
    pending: &Arc<Mutex<Vec<OAuthLoginOutcome>>>,
    cancelled: &Arc<AtomicBool>,
    proxy_url: Option<&str>,
) -> bool {
    let mut buffer = [0u8; 8192];
    let n = match stream.read(&mut buffer) {
        Ok(0) => return false,
        Ok(n) => n,
        Err(_) => return false,
    };
    let request = String::from_utf8_lossy(&buffer[..n]);
    let Some(request_line) = request.lines().next() else {
        write_html(&mut stream, 400, callback_html(false, "无效请求"));
        return false;
    };
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();

    if !path.starts_with("/auth/callback") {
        write_html(&mut stream, 404, callback_html(false, "Not found"));
        return false;
    }

    let query = path.split('?').nth(1).unwrap_or("");
    let params = parse_query(query);
    if let Some(error) = params.get("error") {
        let desc = params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| error.clone());
        pending
            .lock()
            .push(OAuthLoginOutcome::Failed(format!("OAuth 授权失败: {desc}")));
        write_html(&mut stream, 200, callback_html(false, &desc));
        return true;
    }

    let code = params.get("code").cloned().unwrap_or_default();
    let state = params.get("state").cloned().unwrap_or_default();
    if code.is_empty() || state.is_empty() {
        pending.lock().push(OAuthLoginOutcome::Failed(
            "回调缺少 code 或 state 参数".to_string(),
        ));
        write_html(
            &mut stream,
            400,
            callback_html(false, "Missing code or state parameter"),
        );
        return true;
    }
    if state != session.state {
        pending.lock().push(OAuthLoginOutcome::Failed(
            "OAuth state 不匹配，请重新授权".to_string(),
        ));
        write_html(&mut stream, 400, callback_html(false, "Invalid state"));
        return true;
    }

    match exchange_code(&code, &session.code_verifier, OAUTH_REDIRECT_URI, proxy_url) {
        Ok(tokens) => {
            if cancelled.load(Ordering::Relaxed) {
                write_html(&mut stream, 200, callback_html(false, "授权已取消"));
            } else {
                pending.lock().push(OAuthLoginOutcome::Success(tokens));
                write_html(&mut stream, 200, callback_html(true, ""));
            }
        }
        Err(err) => {
            if !cancelled.load(Ordering::Relaxed) {
                pending.lock().push(OAuthLoginOutcome::Failed(err.clone()));
                write_html(&mut stream, 200, callback_html(false, &err));
            }
        }
    }
    true
}

fn exchange_code(
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    proxy_url: Option<&str>,
) -> Result<OAuthAccountTokens, String> {
    let mut builder = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30));
    if let Some(proxy_url) = proxy_url.filter(|value| !value.trim().is_empty()) {
        let proxy = reqwest::Proxy::all(proxy_url.trim())
            .map_err(|err| format!("OAuth 代理地址无效: {err}"))?;
        builder = builder.proxy(proxy);
    }
    let client = builder
        .build()
        .map_err(|err| format!("创建 HTTP 客户端失败: {err}"))?;
    let response = client
        .post(OPENAI_OAUTH_TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", OPENAI_OAUTH_CLIENT_ID),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", code_verifier),
        ])
        .send()
        .map_err(|err| format!("换取 token 请求失败: {err}"))?;
    let status = response.status();
    let text = response.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "换取 token 失败 ({status}): {}",
            truncate_error(&text)
        ));
    }
    let parsed: TokenResponse =
        serde_json::from_str(&text).map_err(|err| format!("token 响应无效: {err}"))?;
    let expires_at = parsed
        .expires_in
        .map(|seconds| chrono::Utc::now().timestamp() + seconds)
        .or_else(|| jwt_exp(&parsed.access_token));
    let account_id = extract_chatgpt_account_id(&parsed.access_token).or_else(|| {
        parsed
            .id_token
            .as_deref()
            .and_then(extract_chatgpt_account_id)
    });
    let email = extract_email(&parsed.access_token)
        .or_else(|| parsed.id_token.as_deref().and_then(extract_email));
    let name = email.clone().unwrap_or_else(|| "OpenAI Auth".to_string());
    Ok(OAuthAccountTokens {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        expires_at,
        account_id,
        email,
        name,
    })
}

pub fn check_proxy_exit_ip(proxy_url: Option<&str>) -> Result<String, String> {
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(12))
        .user_agent("RouteHub-OAuth-ProxyCheck/0.1");
    if let Some(proxy_url) = proxy_url.map(str::trim).filter(|value| !value.is_empty()) {
        let proxy =
            reqwest::Proxy::all(proxy_url).map_err(|err| format!("OAuth 代理地址无效: {err}"))?;
        // 显式代理时禁用系统代理，避免叠加
        builder = builder.no_proxy().proxy(proxy);
    }
    let client = builder
        .build()
        .map_err(|err| format!("创建 HTTP 客户端失败: {err}"))?;

    let endpoints = [
        "https://api.ipify.org",
        "https://ifconfig.me/ip",
        "https://ipinfo.io/ip",
    ];
    let mut last_error = String::from("未能获取出口 IP");
    for endpoint in endpoints {
        match client.get(endpoint).send() {
            Ok(response) => {
                let status = response.status();
                let text = response.text().unwrap_or_default();
                if !status.is_success() {
                    last_error = format!("{endpoint} 返回 {status}: {}", truncate_error(&text));
                    continue;
                }
                if let Some(ip) = extract_ip_text(&text) {
                    return Ok(ip);
                }
                last_error = format!("{endpoint} 响应不是有效 IP: {}", truncate_error(&text));
            }
            Err(err) => {
                last_error = format!("{endpoint} 请求失败: {err}");
            }
        }
    }
    Err(last_error)
}

fn extract_ip_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 允许纯 IPv4 / IPv6，或 JSON 里的 "ip":"..."
    if looks_like_ip(trimmed) {
        return Some(trimmed.to_string());
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        for key in ["ip", "origin", "query"] {
            if let Some(ip) = value.get(key).and_then(Value::as_str).map(str::trim) {
                if looks_like_ip(ip) {
                    return Some(ip.to_string());
                }
            }
        }
    }
    // 从文本中抓第一个 IPv4
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let candidate = &trimmed[start..i];
            if looks_like_ipv4(candidate) {
                return Some(candidate.to_string());
            }
        } else {
            i += 1;
        }
    }
    None
}

fn looks_like_ip(value: &str) -> bool {
    looks_like_ipv4(value) || looks_like_ipv6(value)
}

fn looks_like_ipv4(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    parts.iter().all(|part| {
        !part.is_empty()
            && part.len() <= 3
            && part.chars().all(|c| c.is_ascii_digit())
            && part.parse::<u16>().is_ok_and(|n| n <= 255)
    })
}

fn looks_like_ipv6(value: &str) -> bool {
    if !value.contains(':') || value.contains(' ') {
        return false;
    }
    value
        .chars()
        .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
        && value.split(':').count() >= 3
}

pub fn tokens_to_account(tokens: OAuthAccountTokens) -> AuthAccountConfig {
    AuthAccountConfig {
        id: Uuid::new_v4().simple().to_string(),
        account_type: "openai".to_string(),
        name: tokens.name,
        enabled: true,
        email: tokens.email,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        account_id: tokens.account_id,
        client_id: OPENAI_OAUTH_CLIENT_ID.to_string(),
        token_url: OPENAI_OAUTH_TOKEN_URL.to_string(),
        base_url: String::new(),
        expires_at: tokens.expires_at,
        models: crate::config::default_auth_account_models(),
        model_mapping: Default::default(),
        weight: 1,
        priority: 1,
        description: None,
    }
}

pub fn merge_oauth_account(
    config: &mut crate::config::AppConfig,
    account: AuthAccountConfig,
) -> String {
    if let Some(existing) = config.auth_accounts.iter_mut().find(|current| {
        (!account.account_id.as_deref().unwrap_or("").is_empty()
            && current.account_id.as_deref() == account.account_id.as_deref())
            || (!account.email.as_deref().unwrap_or("").is_empty()
                && current.email.as_deref() == account.email.as_deref())
            || (!account.access_token.is_empty() && current.access_token == account.access_token)
    }) {
        existing.name = account.name.clone();
        existing.enabled = true;
        existing.email = account.email.or(existing.email.clone());
        existing.access_token = account.access_token;
        if account.refresh_token.is_some() {
            existing.refresh_token = account.refresh_token;
        }
        if account.account_id.is_some() {
            existing.account_id = account.account_id;
        }
        existing.expires_at = account.expires_at.or(existing.expires_at);
        if existing.client_id.trim().is_empty() {
            existing.client_id = OPENAI_OAUTH_CLIENT_ID.to_string();
        }
        if existing.token_url.trim().is_empty() {
            existing.token_url = OPENAI_OAUTH_TOKEN_URL.to_string();
        }
        return existing.name.clone();
    }
    let name = account.name.clone();
    config.auth_accounts.insert(0, account);
    name
}

fn build_auth_url(redirect_uri: &str, state: &str, code_challenge: &str) -> String {
    // OpenAI 要求空格编码为 %20，不能用 +。
    let params = [
        ("response_type", "code"),
        ("client_id", OPENAI_OAUTH_CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", code_challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", "codex_cli_rs"),
    ];
    let qs = params
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{OPENAI_OAUTH_AUTHORIZE_URL}?{qs}")
}

fn generate_pkce() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let code_verifier = base64url_encode(&bytes);
    let challenge = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = base64url_encode(&challenge);
    (code_verifier, code_challenge)
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|err| err.to_string())?;
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
            .map_err(|err| err.to_string())?;
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map_err(|err| err.to_string())?;
        return Ok(());
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", unix)))]
    {
        let _ = url;
        Err("当前平台不支持自动打开浏览器".to_string())
    }
}

fn write_html(stream: &mut TcpStream, status: u16, body: impl AsRef<str>) {
    let body = body.as_ref();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

fn callback_html(ok: bool, message: &str) -> String {
    if ok {
        r#"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="UTF-8"><title>授权成功</title>
<style>
body{font-family:-apple-system,BlinkMacSystemFont,Segoe UI,sans-serif;background:#0f172a;color:#e2e8f0;display:flex;align-items:center;justify-content:center;min-height:100vh;margin:0}
.card{background:#1e293b;border:1px solid #334155;border-radius:12px;padding:28px 32px;max-width:420px;text-align:center}
h1{margin:0 0 8px;font-size:22px;color:#4ade80}
p{margin:0;color:#94a3b8;line-height:1.5}
</style></head><body><div class="card"><h1>授权成功</h1><p>可以关闭此页面，返回 RouteHub 查看新增账号。</p></div></body></html>"#
            .to_string()
    } else {
        let safe = html_escape(message);
        format!(
            r#"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="UTF-8"><title>授权失败</title>
<style>
body{{font-family:-apple-system,BlinkMacSystemFont,Segoe UI,sans-serif;background:#0f172a;color:#e2e8f0;display:flex;align-items:center;justify-content:center;min-height:100vh;margin:0}}
.card{{background:#1e293b;border:1px solid #334155;border-radius:12px;padding:28px 32px;max-width:480px;text-align:center}}
h1{{margin:0 0 8px;font-size:22px;color:#f87171}}
p{{margin:0;color:#94a3b8;line-height:1.5;word-break:break-word}}
</style></head><body><div class="card"><h1>授权失败</h1><p>{safe}</p></div></body></html>"#
        )
    }
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in query.split('&').filter(|part| !part.is_empty()) {
        let mut parts = pair.splitn(2, '=');
        let key = url_decode(parts.next().unwrap_or(""));
        let value = url_decode(parts.next().unwrap_or(""));
        map.insert(key, value);
    }
    map
}

fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = from_hex(bytes[i + 1]);
                let lo = from_hex(bytes[i + 2]);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn base64url_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | (data[i + 2] as u32);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push(TABLE[(n & 63) as usize] as char);
        i += 3;
    }
    let rem = data.len() - i;
    if rem == 1 {
        let n = (data[i] as u32) << 16;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
    } else if rem == 2 {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
    }
    out
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

fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = decode_base64_url(payload)?;
    serde_json::from_slice(&bytes).ok()
}

fn jwt_exp(token: &str) -> Option<i64> {
    jwt_payload(token)?.get("exp")?.as_i64()
}

fn extract_chatgpt_account_id(token: &str) -> Option<String> {
    let payload = jwt_payload(token)?;
    payload
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn extract_email(token: &str) -> Option<String> {
    let payload = jwt_payload(token)?;
    if let Some(email) = payload
        .get("https://api.openai.com/profile")
        .and_then(|profile| profile.get("email"))
        .and_then(Value::as_str)
    {
        return Some(email.to_string());
    }
    payload
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn truncate_error(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() > 240 {
        format!("{}…", &trimmed[..240])
    } else {
        trimmed.to_string()
    }
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_authorize_url_with_percent_encoding() {
        let url = build_auth_url(
            "http://localhost:1455/auth/callback",
            "state123",
            "challenge456",
        );
        assert!(url.starts_with(OPENAI_OAUTH_AUTHORIZE_URL));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge=challenge456"));
        assert!(url.contains("scope=openid%20profile%20email%20offline_access"));
        assert!(!url.contains("scope=openid+profile"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
    }

    #[test]
    fn pkce_challenge_is_base64url_without_padding() {
        let (verifier, challenge) = generate_pkce();
        assert!(!verifier.is_empty());
        assert!(!challenge.contains('='));
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
    }

    #[test]
    fn extracts_ip_from_plain_text() {
        assert_eq!(extract_ip_text("1.2.3.4\n").as_deref(), Some("1.2.3.4"));
        assert_eq!(
            extract_ip_text(r#"{"ip":"8.8.8.8"}"#).as_deref(),
            Some("8.8.8.8")
        );
    }

    #[test]
    fn extracts_account_claims_from_jwt() {
        // {"https://api.openai.com/auth":{"chatgpt_account_id":"acct-1"},"https://api.openai.com/profile":{"email":"a@b.com"},"exp":1900000000}
        let payload = base64url_encode(
            br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-1"},"https://api.openai.com/profile":{"email":"a@b.com"},"exp":1900000000}"#,
        );
        let token = format!("h.{payload}.s");
        assert_eq!(
            extract_chatgpt_account_id(&token).as_deref(),
            Some("acct-1")
        );
        assert_eq!(extract_email(&token).as_deref(), Some("a@b.com"));
        assert_eq!(jwt_exp(&token), Some(1_900_000_000));
    }
}

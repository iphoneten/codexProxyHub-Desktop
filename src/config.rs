use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub usage_log: UsageLogConfig,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
    #[serde(skip)]
    config_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub admin_key: Option<String>,
    #[serde(default)]
    pub api_keys: Vec<ApiKeyConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyConfig {
    pub key: String,
    #[serde(default = "default_key_name")]
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoutingConfig {
    #[serde(default)]
    pub model_fallbacks: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageLogConfig {
    #[serde(default = "default_usage_backend")]
    pub backend: String,
    #[serde(default = "default_sqlite_path")]
    pub sqlite_path: String,
    #[serde(default = "default_true")]
    pub import_legacy_jsonl: bool,
    #[serde(default = "default_usage_path")]
    pub path: String,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
    #[serde(default = "default_max_chunks")]
    pub max_chunks: usize,
}

impl Default for UsageLogConfig {
    fn default() -> Self {
        Self {
            backend: default_usage_backend(),
            sqlite_path: default_sqlite_path(),
            import_legacy_jsonl: true,
            path: default_usage_path(),
            max_bytes: default_max_bytes(),
            max_chunks: default_max_chunks(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_provider_type")]
    pub provider_type: String,
    pub base_url: String,
    #[serde(default)]
    pub website: Option<String>,
    pub api_key: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub model_mapping: HashMap<String, String>,
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
    #[serde(default)]
    pub capabilities: HashMap<String, bool>,
    #[serde(default = "default_health_check_mode")]
    pub health_check_mode: String,
    #[serde(default = "default_model_sync_filter")]
    pub model_sync_filter: String,
    #[serde(default = "default_responses_mode")]
    pub responses_mode: String,
    #[serde(default = "default_client_mode")]
    pub client_mode: String,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: u64,
    #[serde(default = "default_request_timeout", alias = "timeout")]
    pub request_timeout: u64,
    #[serde(default)]
    pub max_retries: usize,
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(default = "default_priority")]
    pub priority: i32,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub system_prompt_override: Option<String>,
    #[serde(default)]
    pub strip_thought: bool,
    #[serde(default)]
    pub persistent_session: bool,
    #[serde(default = "default_persist_interval")]
    pub persist_interval: f64,
    #[serde(default)]
    pub persist_max_wait: u64,
    #[serde(default)]
    pub persist_keepalive: bool,
    #[serde(default = "default_keepalive_interval")]
    pub persist_keepalive_interval: u64,
    #[serde(default)]
    pub persist_keepalive_model: Option<String>,
    #[serde(default = "default_keepalive_prompt")]
    pub persist_keepalive_prompt: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl AppConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)
            .with_context(|| format!("读取配置失败: {}", path.display()))?;
        let mut cfg: Self = serde_yaml::from_str(&content)
            .with_context(|| format!("解析 YAML 失败: {}", path.display()))?;
        cfg.config_dir = config_parent_dir(path);
        Ok(cfg)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let content = serde_yaml::to_string(self)?;
        fs::write(path, content).with_context(|| format!("保存配置失败: {}", path.display()))
    }

    pub fn usage_log_sqlite_path(&self) -> PathBuf {
        self.resolve_runtime_path(&self.usage_log.sqlite_path)
    }

    pub fn usage_log_jsonl_path(&self) -> PathBuf {
        self.resolve_runtime_path(&self.usage_log.path)
    }

    fn resolve_runtime_path(&self, path: &str) -> PathBuf {
        let path_buf = PathBuf::from(path);
        if path_buf.is_absolute() || path.trim().is_empty() {
            return path_buf;
        }
        self.config_dir
            .as_ref()
            .map(|dir| dir.join(path_buf.clone()))
            .unwrap_or(path_buf)
    }
}

fn config_parent_dir(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    if parent.as_os_str().is_empty() {
        return None;
    }
    Some(parent.to_path_buf())
}

pub fn default_config_path() -> PathBuf {
    let cwd_config = PathBuf::from("config.yaml");
    if cwd_config.exists() {
        return cwd_config;
    }

    if let Some(app_support) = app_support_config_path() {
        if app_support.exists() {
            return app_support;
        }
        if let Some(resource_config) = bundled_resource_config_path() {
            if resource_config.exists() {
                if let Some(parent) = app_support.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::copy(resource_config, &app_support);
                return app_support;
            }
        }
        return app_support;
    }

    bundled_resource_config_path().unwrap_or(cwd_config)
}

fn app_support_config_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("RouteHub")
                .join("config.yaml"),
        )
    }

    #[cfg(target_os = "windows")]
    {
        let app_data = env::var_os("APPDATA")?;
        Some(PathBuf::from(app_data).join("RouteHub").join("config.yaml"))
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let config_home = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(config_home.join("RouteHub").join("config.yaml"))
    }
}

fn bundled_resource_config_path() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;

    #[cfg(target_os = "macos")]
    {
        let macos_dir = exe.parent()?;
        let contents_dir = macos_dir.parent()?;
        Some(contents_dir.join("Resources").join("config.yaml"))
    }

    #[cfg(not(target_os = "macos"))]
    {
        Some(exe.parent()?.join("config.yaml"))
    }
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}
fn default_port() -> u16 {
    8000
}
fn default_true() -> bool {
    true
}
fn default_key_name() -> String {
    "default".to_string()
}
fn default_provider_type() -> String {
    "openai".to_string()
}
fn default_health_check_mode() -> String {
    "models".to_string()
}
fn default_model_sync_filter() -> String {
    "all".to_string()
}
fn default_responses_mode() -> String {
    "auto".to_string()
}
fn default_client_mode() -> String {
    "normal".to_string()
}
fn default_connect_timeout() -> u64 {
    10
}
fn default_request_timeout() -> u64 {
    60
}
fn default_weight() -> u32 {
    1
}
fn default_priority() -> i32 {
    1
}
fn default_persist_interval() -> f64 {
    3.0
}
fn default_keepalive_interval() -> u64 {
    30
}
fn default_keepalive_prompt() -> String {
    "Hi".to_string()
}
fn default_usage_backend() -> String {
    "sqlite".to_string()
}
fn default_sqlite_path() -> String {
    "logs/proxy_usage.sqlite3".to_string()
}
fn default_usage_path() -> String {
    "logs/proxy_usage.jsonl".to_string()
}
fn default_max_bytes() -> u64 {
    5 * 1024 * 1024
}
fn default_max_chunks() -> usize {
    20
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_example_config_is_valid() {
        let cfg: AppConfig = serde_yaml::from_str(include_str!("../config.example.yaml")).unwrap();

        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.server.port, 8000);
        assert!(cfg.providers.is_empty());
    }

    #[test]
    fn legacy_timeout_maps_to_request_timeout() {
        let cfg: AppConfig = serde_yaml::from_str(
            r#"
providers:
  - name: legacy
    base_url: https://example.test/v1
    api_key: sk-test
    timeout: 120
"#,
        )
        .unwrap();

        let provider = &cfg.providers[0];
        assert_eq!(provider.connect_timeout, 10);
        assert_eq!(provider.request_timeout, 120);
    }

    #[test]
    fn relative_usage_log_paths_resolve_from_config_directory() {
        let mut cfg: AppConfig = serde_yaml::from_str(
            r#"
usage_log:
  backend: sqlite
  sqlite_path: logs/proxy_usage.sqlite3
  path: logs/proxy_usage.jsonl
"#,
        )
        .unwrap();
        cfg.config_dir = Some(PathBuf::from("/tmp/RouteHub"));

        assert_eq!(
            cfg.usage_log_sqlite_path(),
            PathBuf::from("/tmp/RouteHub/logs/proxy_usage.sqlite3")
        );
        assert_eq!(
            cfg.usage_log_jsonl_path(),
            PathBuf::from("/tmp/RouteHub/logs/proxy_usage.jsonl")
        );
    }

    #[test]
    fn absolute_usage_log_paths_are_preserved() {
        let mut cfg: AppConfig = serde_yaml::from_str(
            r#"
usage_log:
  backend: sqlite
  sqlite_path: /var/tmp/proxy_usage.sqlite3
  path: /var/tmp/proxy_usage.jsonl
"#,
        )
        .unwrap();
        cfg.config_dir = Some(PathBuf::from("/tmp/RouteHub"));

        assert_eq!(
            cfg.usage_log_sqlite_path(),
            PathBuf::from("/var/tmp/proxy_usage.sqlite3")
        );
        assert_eq!(
            cfg.usage_log_jsonl_path(),
            PathBuf::from("/var/tmp/proxy_usage.jsonl")
        );
    }
}

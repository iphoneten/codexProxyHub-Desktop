下面这份方案我会直接以你现在的 Rust recodexProxyHub 架构为基础来设计，而不是从零假设一个普通 HTTP 代理。

你的产品本质上可以定义为：

高性能多渠道 AI API Gateway + 账号反代/账号池 + 智能路由 + 故障转移 + 熔断 + 会话粘性 + 用量统计 + OAuth 账号管理 + Mac/Windows 桌面控制端

重点不仅是“中转”，而是把它做成一个完整的 AI Provider Gateway。

Rust 高性能 AI 渠道中转与账号反代桌面软件完整设计方案

项目代号：ReCodeX Proxy Hub

目标平台：

* Server：Linux / macOS / Windows
* Desktop：macOS / Windows
* Core：Rust
* HTTP Framework：Axum
* Async Runtime：Tokio
* HTTP Client：Reqwest
* Database：SQLite / PostgreSQL
* Desktop UI：Tauri + React
* API：OpenAI Compatible / Anthropic Compatible
* 配置：YAML / JSON
* 通信：HTTP / HTTPS / WebSocket（可选）

⸻

一、产品定位

1.1 产品目标

构建一个高性能 AI API Gateway，用于统一接入：

* OpenAI
* Anthropic
* Google Gemini
* OpenAI Compatible Provider
* Claude OAuth / Account
* Codex OAuth / Account
* 自定义 API Provider
* 企业内部 AI Gateway
* 第三方 AI 中转渠道

客户端只需要连接一个统一 Endpoint：

https://your-domain.com/v1

Gateway 根据：

API Key
        ↓
请求模型
        ↓
路由策略
        ↓
Provider / Account
        ↓
代理
        ↓
上游 AI 服务

完成请求转发。

⸻

二、总体架构

整体建议拆成：

                         ┌──────────────────────┐
                         │      Desktop UI      │
                         │   Tauri + React      │
                         └──────────┬───────────┘
                                    │
                             Local IPC / HTTP
                                    │
                                    ▼
┌───────────────────────────────────────────────────────────────┐
│                     Rust Gateway Core                         │
│                                                               │
│  ┌──────────┐   ┌───────────┐   ┌────────────┐              │
│  │ HTTP API │ → │ Auth      │ → │ Rate Limit │              │
│  └──────────┘   └───────────┘   └────────────┘              │
│         │               │                │                    │
│         ▼               ▼                ▼                    │
│  ┌──────────────────────────────────────────────┐            │
│  │              Request Router                  │            │
│  │                                              │            │
│  │ Model → Provider → Account → Proxy           │            │
│  └──────────────────────┬───────────────────────┘            │
│                         │                                    │
│              ┌──────────┼───────────┐                        │
│              ▼          ▼           ▼                        │
│          Provider A Provider B Provider C                     │
│              │          │           │                         │
│              ▼          ▼           ▼                         │
│          Account Pool / OAuth / API Key                       │
│                         │                                    │
│                         ▼                                    │
│                    Proxy Layer                               │
│                         │                                    │
│                         ▼                                    │
│                   Internet / Upstream                         │
│                                                               │
│  ┌──────────┐ ┌────────────┐ ┌────────────┐ ┌─────────────┐ │
│  │ Circuit  │ │ Session    │ │ Usage Log  │ │ Metrics     │ │
│  │ Breaker  │ │ Affinity   │ │            │ │             │ │
│  └──────────┘ └────────────┘ └────────────┘ └─────────────┘ │
└───────────────────────────────────────────────────────────────┘

⸻

三、核心设计原则

整个系统应该遵循以下原则。

3.1 请求路径绝不访问数据库

正常请求：

HTTP
 ↓
Auth
 ↓
Route
 ↓
Provider
 ↓
Upstream

不能：

HTTP
 ↓
SQLite
 ↓
Route
 ↓
Provider

否则高并发情况下数据库会成为瓶颈。

数据库只负责：

* 配置持久化
* API Key
* Account
* Usage Log
* Statistics
* Audit Log

运行时全部进入内存。

⸻

四、Runtime State

你当前：

struct AppState {
    config: ConfigHandle,
    clients: Arc<Mutex<HashMap<(u64, String), Client>>>,
    counters: Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>>,
    api_key_limiters: Arc<Mutex<HashMap<String, ApiKeyLimiter>>>,
    provider_circuits: Arc<Mutex<HashMap<String, ProviderCircuit>>>,
    provider_loads: Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>>,
    provider_statuses: ProviderCircuitStatusHandle,
    session_affinity: Arc<Mutex<HashMap<String, SessionAffinity>>>,
    usage_injection: Arc<Mutex<HashMap<String, bool>>>,
    oauth_tokens: Arc<Mutex<HashMap<String, OAuthRuntimeToken>>>,
    oauth_refresh_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

这个方向是正确的。

但后续建议进一步拆成：

AppState
│
├── ConfigManager
├── ProviderRegistry
├── AccountRegistry
├── Router
├── AuthManager
├── RateLimiter
├── CircuitBreaker
├── SessionAffinity
├── OAuthManager
├── ProxyManager
├── HttpClientPool
├── UsageManager
├── MetricsManager
└── EventBus

不要让 AppState 最终变成一个几千行代码的 God Object。

⸻

五、Provider 模型

Provider 是系统最核心的实体。

建议：

struct Provider {
    id: String,
    name: String,
    provider_type: ProviderType,
    enabled: bool,
    base_url: String,
    api_key: Option<String>,
    models: Vec<ModelMapping>,
    proxy: ProxyConfig,
    timeout: TimeoutConfig,
    concurrency: ConcurrencyConfig,
    retry: RetryConfig,
    circuit_breaker: CircuitBreakerConfig,
    health_check: HealthCheckConfig,
}

ProviderType：

enum ProviderType {
    OpenAI,
    Anthropic,
    Gemini,
    OpenAICompatible,
    AnthropicCompatible,
    OAuth,
    Custom,
}

⸻

六、Account 与 Provider 分离

这是后续非常重要的一步。

不要把：

Provider
API Key
OAuth Account
Proxy

全部设计成一个对象。

应该：

Provider
    │
    ├── Account A
    ├── Account B
    ├── Account C
    └── Account D

例如：

Anthropic
│
├── claude-account-001
├── claude-account-002
├── claude-account-003
└── claude-account-004

然后：

Provider
    ↓
Account Pool
    ↓
Proxy
    ↓
Upstream

这样才能真正实现账号池。

⸻

七、Account Pool

Account Pool 建议支持：

struct Account {
    id: String,
    provider_id: String,
    account_type: AccountType,
    status: AccountStatus,
    credential: Credential,
    quota: Option<Quota>,
    concurrency_limit: usize,
    current_concurrency: AtomicUsize,
    last_used_at: Instant,
    last_success_at: Option<Instant>,
    last_failure_at: Option<Instant>,
    failure_count: usize,
}

AccountStatus：

Healthy
Busy
RateLimited
QuotaExceeded
Unauthorized
Forbidden
Expired
Disabled
Cooldown

⸻

八、账号选择策略

建议实现 Strategy：

AccountSelector

支持：

1. Round Robin

A
B
C
A
B
C

2. Least Connections

优先：

current_concurrency 最低

3. Weighted

例如：

A = 50
B = 30
C = 20

4. Quota First

优先选择剩余额度最多的账号。

5. Health First

只选择：

Healthy

6. Sticky Session

同一个：

API Key + Model + Session

尽可能保持：

Account A

⸻

九、最终推荐的账号选择算法

建议：

1. Session Affinity
        ↓
2. Healthy Account
        ↓
3. Quota Check
        ↓
4. Concurrency Check
        ↓
5. Circuit Breaker
        ↓
6. Least Load
        ↓
7. Weighted Random

这样比简单 Round Robin 稳定很多。

⸻

十、Model Router

系统应该把：

请求模型

和：

实际 Provider 模型

彻底分开。

例如用户：

{
  "model": "gpt-5"
}

可以路由：

gpt-5
 ↓
Provider A
 ↓
gpt-5

或者：

gpt-5
 ↓
Provider B
 ↓
gpt-5.1

甚至：

claude-opus
 ↓
Provider A
 ↓
claude-opus-4-1

定义：

struct ModelRoute {
    request_model: String,
    provider: String,
    upstream_model: String,
    priority: i32,
    weight: u32,
    enabled: bool,
}

⸻

十一、路由优先级

推荐：

API Key Route
        ↓
Model Route
        ↓
Provider Route
        ↓
Account Route
        ↓
Global Route

例如：

API Key: user-001
Model: claude-opus
        ↓
Provider A
        ↓
Account 1

另一个：

API Key: user-002
Model: claude-opus
        ↓
Provider B
        ↓
Account 5

⸻

十二、Failover

系统必须支持自动故障转移。

例如：

Request
  ↓
Provider A
  ↓
429
  ↓
Provider B
  ↓
成功

但不能所有错误都 Failover。

建议：

错误	Failover
400	❌
401	✅
403	✅
408	✅
429	✅
500	✅
502	✅
503	✅
Timeout	✅
DNS Error	✅
用户参数错误	❌
Token 超长	❌

⸻

十三、Retry 与 Failover 必须分开

这是非常重要的设计。

Retry

是：

Provider A
 ↓
Provider A
 ↓
Provider A

而：

Failover

是：

Provider A
 ↓
Provider B
 ↓
Provider C

建议：

同 Provider Retry ≤ 2
Provider Failover ≤ 3

最终：

A retry
 ↓
A retry
 ↓
B
 ↓
B retry
 ↓
C

⸻

十四、Circuit Breaker

你当前已经实现：

Closed
Open
HalfOpen

这是正确的。

建议升级成：

Closed
    │
    │ failures >= threshold
    ▼
Open
    │
    │ cooldown
    ▼
HalfOpen
    │
    ├── success → Closed
    │
    └── failure → Open

建议默认：

failure_threshold = 3
cooldown = 30s
half_open_requests = 1

⸻

十五、429 特殊处理

429 不应该和普通 500 完全一样。

如果上游：

HTTP 429
Retry-After: 60

应该：

Provider
 ↓
RateLimited
 ↓
cooldown = 60s

而不是简单：

30s

你现在的：

retry_after.unwrap_or(PROVIDER_CIRCUIT_DEFAULT_COOLDOWN)

方向是正确的。

⸻

十六、Quota Exhausted

需要区分：

429 Rate Limit

和：

Quota Exhausted

例如：

insufficient_quota
quota_exceeded
limit_reached

前者：

短时间冷却

后者：

账号长期不可用

所以 Account 状态应该：

RateLimited

和：

QuotaExhausted

分离。

⸻

十七、Session Affinity

你目前：

session_affinity: Arc<Mutex<HashMap<String, SessionAffinity>>>

这个设计可以继续。

但是建议 key 不要只依赖 API Key。

推荐：

tenant
+
api_key
+
model
+
conversation/session

例如：

tenant:user001
api-key:key001
model:claude-opus
session:abc123

最终：

affinity_key =
sha256(...)

避免 HashMap Key 无限增长。

⸻

十八、Session Affinity TTL

当前系统如果永久保存：

session_affinity

最终可能产生内存泄漏。

必须增加：

expires_at: Instant

建议：

TTL = 30min ~ 2h

同时后台定期清理：

每 5 分钟

⸻

十九、API Key 系统

建议 API Key：

rk_live_xxxxxxxxx

数据库保存：

id
name
key_hash
status
concurrency_limit
daily_token_limit
allowed_models
allowed_providers
created_at
updated_at
last_used_at

不要保存明文 API Key。

只保存：

SHA256(key)

或者：

HMAC-SHA256

⸻

二十、API Key 权限模型

建议：

API Key
│
├── Models
├── Providers
├── Concurrency
├── Daily Token Limit
├── RPM
├── TPM
└── Expiration

例如：

api_key:
  name: developer
  allowed_models:
    - gpt-5
    - claude-opus
  allowed_providers:
    - openai
    - anthropic
  concurrency: 10
  rpm: 100
  daily_tokens: 10000000

⸻

二十一、限流系统

至少实现三层。

Global

Gateway
 ↓
Global Concurrency

Provider

Provider A
 ↓
Concurrency = 20

API Key

API Key A
 ↓
Concurrency = 5

最终：

Global
 ↓
API Key
 ↓
Provider
 ↓
Account

⸻

二十二、不要使用 Mutex 做高频计数

例如：

Arc<Mutex<HashMap<String, usize>>>

只适合低频控制。

高频统计建议：

AtomicUsize
AtomicU64

例如：

AtomicU64::fetch_add()

对于大量请求：

Atomic

比：

Mutex

更合适。

⸻

二十三、HTTP Client Pool

你当前：

HashMap<(u64, String), Client>

这个思路是对的。

建议进一步：

HttpClientPool
│
├── Direct Client
├── Proxy Client
├── SOCKS5 Client
├── HTTP Proxy Client
└── Custom Proxy Client

Client Key：

connect_timeout
proxy
tls_mode

⸻

二十四、Proxy Manager

代理建议独立抽象：

trait ProxyProvider {
    async fn get_proxy(&self) -> Result<Proxy>;
}

实现：

DirectProxy
HttpProxy
Socks5Proxy
ProxyPool
DynamicProxy

ProxyPool：

Proxy A
Proxy B
Proxy C

也应该拥有：

Health
Latency
Failure
Cooldown

因此最终形成：

Provider
 ↓
Account
 ↓
Proxy
 ↓
Internet

三级容错。

⸻

二十五、账号反代

账号反代是这个产品和普通 API Gateway 最大的区别之一。

推荐模型：

Client
 ↓
API Key
 ↓
Virtual Model
 ↓
Provider
 ↓
Account Pool
 ↓
Proxy
 ↓
Upstream

例如：

Claude Pool
│
├── Account A
│   └── Proxy 1
│
├── Account B
│   └── Proxy 2
│
└── Account C
    └── Proxy 3

这样可以做到：

账号 A 429
 ↓
账号 B

而不是：

整个 Provider 429

⸻

二十六、OAuth Account Manager

OAuth 建议单独设计。

struct OAuthAccount {
    id: String,
    provider: String,
    access_token: Secret,
    refresh_token: Secret,
    expires_at: DateTime,
    scope: Vec<String>,
    status: OAuthStatus,
}

核心：

OAuthManager

负责：

Get Token
Refresh Token
Refresh Lock
Token Expiration
Invalid Token
Re-authentication

你目前已经有：

oauth_tokens
oauth_refresh_locks

这是非常好的方向。

⸻

二十七、OAuth Refresh 防并发击穿

必须保证：

100 requests
      ↓
Token expired

不能：

100 requests
 ↓
100 refresh

而应该：

100 requests
      ↓
     Lock
      ↓
1 request refresh
      ↓
99 requests reuse token

你当前：

oauth_refresh_locks

就是为了实现这个。

⸻

二十八、Streaming

AI Gateway 最重要的性能指标之一：

TTFT

即：

Time To First Token

所以流式请求不要：

上游完整读取
 ↓
转换
 ↓
返回

必须：

Upstream
 ↓
Bytes
 ↓
Transform
 ↓
Client

实时透传。

⸻

二十九、SSE Pipeline

建议：

reqwest Response
       ↓
Byte Stream
       ↓
SSE Parser
       ↓
Normalizer
       ↓
Usage Extractor
       ↓
Client

同时：

                    ┌→ Usage
                    │
SSE Stream ──────────┼→ Metrics
                    │
                    └→ Client

不要为了记录 usage 把整个 stream 缓存在内存。

⸻

三十、SSE Buffer

建议：

MAX_SSE_BUFFER = 1MB

防止恶意上游发送：

无限不完整 event

造成内存增长。

⸻

三十一、Usage Injection

你当前：

usage_injection

已经考虑到了：

stream_options.include_usage

建议把它升级成 Provider Capability：

struct ProviderCapabilities {
    streaming: bool,
    tool_calling: bool,
    vision: bool,
    reasoning: bool,
    usage_in_stream: bool,
    response_api: bool,
}

这样以后不用不断：

if provider_type == ...

⸻

三十二、协议转换层

建议不要把协议转换写进 Provider。

独立：

Protocol Layer

结构：

OpenAI Request
      ↓
Canonical Request
      ↓
Provider Adapter
      ↓
Anthropic Request

响应：

Anthropic Response
      ↓
Canonical Response
      ↓
OpenAI Response

⸻

三十三、Canonical Model

系统内部定义统一请求：

struct CanonicalRequest {
    model: String,
    messages: Vec<Message>,
    system: Option<String>,
    tools: Vec<Tool>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    stream: bool,
    metadata: Metadata,
}

Provider 只负责：

Canonical
 ↓
Provider Specific

这样以后支持新 Provider 非常快。

⸻

三十四、Provider Adapter

建议：

trait ProviderAdapter: Send + Sync {
    fn transform_request(
        &self,
        request: CanonicalRequest,
    ) -> Result<UpstreamRequest>;
    fn transform_response(
        &self,
        response: UpstreamResponse,
    ) -> Result<CanonicalResponse>;
    fn transform_stream(
        &self,
        stream: UpstreamStream,
    ) -> ProviderStream;
}

实现：

OpenAIAdapter
AnthropicAdapter
GeminiAdapter
OpenAICompatibleAdapter

⸻

三十五、路由引擎

建议单独建立：

RoutingEngine

输入：

RequestContext

例如：

struct RequestContext {
    api_key_id: String,
    model: String,
    session_id: Option<String>,
    stream: bool,
    estimated_tokens: u64,
}

输出：

Vec<RouteCandidate>

例如：

[
    ProviderA / Account1,
    ProviderA / Account2,
    ProviderB / Account3
]

然后：

RouteSelector

决定最终执行顺序。

⸻

三十六、Route Candidate

建议：

struct RouteCandidate {
    provider_id: String,
    account_id: Option<String>,
    score: f64,
    priority: i32,
    estimated_latency: Duration,
    available: bool,
}

评分：

score =
priority
+ health
+ quota
+ latency
+ load
+ affinity

⸻

三十七、智能路由评分

可以设计：

Score =
Priority × 100
+
Health × 30
+
Affinity × 50
+
Quota × 20
-
Latency × 0.1
-
Load × 10

不过第一版不建议过度复杂。

第一版：

Priority
→ Affinity
→ Health
→ Load

足够。

⸻

三十八、请求生命周期

完整请求：

Client
 ↓
HTTP Parse
 ↓
Request Size Limit
 ↓
Authentication
 ↓
API Key Permission
 ↓
Rate Limit
 ↓
Parse Request
 ↓
Normalize Request
 ↓
Model Route
 ↓
Session Affinity
 ↓
Provider Selection
 ↓
Account Selection
 ↓
Proxy Selection
 ↓
Circuit Breaker
 ↓
HTTP Client
 ↓
Upstream
 ↓
Stream Transform
 ↓
Client
 ↓
Usage Collector
 ↓
Metrics
 ↓
Async Usage Log

⸻

三十九、错误处理

统一：

enum ProxyError {
    Authentication,
    Authorization,
    InvalidRequest,
    RateLimited,
    ProviderUnavailable,
    UpstreamTimeout,
    UpstreamError,
    NoRoute,
    QuotaExceeded,
}

最终转换：

ProxyError
 ↓
HTTP Status
 ↓
OpenAI / Anthropic Error Format

⸻

四十、错误不能泄漏内部信息

不要直接返回：

reqwest::Error

例如：

proxy://user:password@1.2.3.4

绝对不能返回给客户端。

应该：

{
  "error": {
    "message": "upstream request failed",
    "type": "proxy_error"
  }
}

详细错误只进入：

Log

⸻

四十一、日志系统

建议使用：

tracing
tracing-subscriber

而不是大量：

println!
eprintln!

日志：

TRACE
DEBUG
INFO
WARN
ERROR

Request ID：

X-Request-ID

例如：

req_01J...

整个请求链路统一：

Request ID
 ↓
Gateway
 ↓
Provider
 ↓
Account
 ↓
Proxy

⸻

四十二、结构化日志

推荐 JSON：

{
  "timestamp": "...",
  "level": "INFO",
  "request_id": "...",
  "api_key_id": "...",
  "provider": "anthropic",
  "account": "account-001",
  "model": "claude-opus",
  "status": 200,
  "ttft_ms": 842,
  "duration_ms": 12430,
  "input_tokens": 1024,
  "output_tokens": 2300
}

⸻

四十三、Usage Log

你目前使用 SQLite 是合理的。

建议表：

usage_logs

字段：

id
request_id
api_key_id
api_key_name
provider_id
account_id
request_model
upstream_model
status
error_type
stream
input_tokens
output_tokens
total_tokens
first_token_ms
duration_ms
created_at
completed_at

⸻

四十四、Usage Log 不应该阻塞请求

错误方式：

Response
 ↓
SQLite INSERT
 ↓
返回

正确：

Response
 ↓
Client
Usage
 ↓
Channel
 ↓
Background Worker
 ↓
SQLite

例如：

mpsc::Sender<UsageEvent>

后台：

UsageWriter

批量：

100 events
 ↓
transaction
 ↓
SQLite

性能会明显更好。

⸻

四十五、SQLite WAL

SQLite：

PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA busy_timeout=5000;

读写分离：

Runtime
 ↓
Usage Channel
 ↓
Writer
 ↓
SQLite

⸻

四十六、数据库设计

建议至少：

api_keys
providers
accounts
models
routes
proxies
usage_logs
oauth_accounts
system_settings
audit_logs

关系：

Provider
   │
   ├── Models
   ├── Accounts
   └── Routes
API Key
   │
   └── Routes

⸻

四十七、配置中心

你现在：

Arc<RwLock<Arc<AppConfig>>>

这个设计很好。

建议建立：

ConfigManager

负责：

Load
Validate
Snapshot
Update
Persist
Reload
Broadcast

修改：

Web UI
 ↓
ConfigManager
 ↓
Validate
 ↓
Persist
 ↓
Swap Arc<AppConfig>
 ↓
Broadcast

⸻

四十八、配置热更新

热更新不能所有东西都动态修改。

建议：

可以热更新

Provider
API Key
Route
Account
Proxy
Rate Limit
Circuit Config

必须重启

Listen Port
Listen Host
TLS Certificate
Database Path

⸻

四十九、配置版本

每次修改：

config_version++

例如：

v103

支持：

Rollback

以后桌面软件可以：

配置历史
 ↓
v100
v101
v102
v103

一键恢复。

⸻

五十、Admin API

建议：

/admin

例如：

GET    /admin/providers
POST   /admin/providers
PUT    /admin/providers/:id
DELETE /admin/providers/:id
GET    /admin/accounts
POST   /admin/accounts
GET    /admin/api-keys
POST   /admin/api-keys
GET    /admin/routes
PUT    /admin/routes/:id
GET    /admin/usage
GET    /admin/stats
GET    /admin/health
GET    /admin/circuits

⸻

五十一、User API 与 Admin API 分离

不要：

/v1/*

直接暴露管理能力。

建议：

/v1/*

业务 API。

/admin/*

管理 API。

/user/*

用户管理。

⸻

五十二、管理后台

Web UI：

Dashboard
Providers
Accounts
Models
Routes
API Keys
Proxies
Usage
Logs
OAuth
Settings

Dashboard：

Requests
RPM
TPM
Error Rate
TTFT
Latency
Active Connections
Provider Health
Account Health

⸻

五十三、桌面软件架构

Mac / Windows 建议：

Tauri
│
├── React
│
└── Rust Core

不要：

React
 ↓
HTTP
 ↓
Rust Server

作为唯一架构。

桌面版最好：

Tauri
 ↓
Rust Core

直接管理本地 Gateway。

⸻

五十四、桌面端进程架构

建议：

Application
│
├── UI Process
│
└── Gateway Process

更推荐：

Tauri
│
├── Frontend
│
└── Rust Core
      │
      └── Gateway

Gateway 运行：

127.0.0.1:PORT

UI 通过：

Tauri IPC

访问。

⸻

五十五、为什么桌面端不建议只做 UI

因为 Gateway 本身就是 Rust。

如果 UI 只是：

Electron / React

然后再：

启动另一个 Rust Server

会增加：

* 进程管理
* 生命周期管理
* 崩溃恢复
* 配置同步
* IPC
* 安装包复杂度

Tauri 可以让 Rust Core 和 UI 更紧密。

⸻

五十六、桌面端功能

建议：

Dashboard

显示：

Gateway Status
Running
Port
Requests
Latency
Error Rate

⸻

五十七、渠道管理

Providers

支持：

新增渠道
编辑渠道
启用/禁用
测试连接
测试模型
查看延迟
查看错误率

⸻

五十八、账号管理

Accounts

显示：

Account
Provider
Status
Quota
Concurrency
Last Used
Last Error
Token Expire

操作：

Login
Refresh
Disable
Delete
Test

⸻

五十九、代理管理

Proxy Pool

支持：

Direct
HTTP
HTTPS
SOCKS5

测试：

Latency
IP
Country
ASN
Connectivity

⸻

六十、健康检查

后台 Worker：

HealthChecker

周期：

30s

检查：

Provider
Account
Proxy

结果：

Healthy
Degraded
Unhealthy

注意：

健康检查不能真的调用昂贵模型。

优先：

/models

或者：

HEAD /

或者 Provider 特定轻量接口。

⸻

六十一、Metrics

建议：

Prometheus

指标：

proxy_requests_total
proxy_request_duration_seconds
proxy_request_errors_total
proxy_provider_requests_total
proxy_provider_errors_total
proxy_provider_inflight
proxy_provider_circuit_status
proxy_account_requests_total
proxy_account_quota
proxy_tokens_input_total
proxy_tokens_output_total
proxy_ttft_seconds

⸻

六十二、性能目标

第一阶段建议目标：

10,000+ RPS

纯转发情况下。

P99：

< 20ms

不包含上游响应时间。

Streaming：

TTFT overhead < 5ms

目标：

Gateway 本身几乎不成为瓶颈。

⸻

六十三、性能原则

避免：

Mutex
数据库
JSON Serialize/Deserialize

出现在高频路径。

能：

Bytes

就不要：

String

能：

Atomic

就不要：

Mutex

能：

Arc

就不要频繁 Clone 大对象。

⸻

六十四、JSON 优化

正常：

serde_json

已经够用。

但是对于 streaming：

不要完整 JSON parse

尽量：

Bytes
→ SSE Frame
→ 必要字段解析

⸻

六十五、连接池

Reqwest：

pool_max_idle_per_host(20)

建议进一步做成配置：

http:
  pool_max_idle_per_host: 50
  pool_idle_timeout: 90
  connect_timeout: 10

⸻

六十六、Backpressure

必须考虑：

Client
 ↓
Gateway
 ↓
Provider

如果：

Client 读取速度 < Provider 输出速度

不能无限 Buffer。

Streaming 必须具备：

bounded channel

例如：

mpsc::channel(128)

形成自然 backpressure。

⸻

六十七、请求 Body 限制

你目前：

MAX_REQUEST_BODY_BYTES = 64MB
MAX_ANTHROPIC_REQUEST_BODY_BYTES = 32MB

方向正确。

但是建议：

Global Limit
Provider Limit
API Key Limit

同时：

Content-Length

和实际读取量都需要控制。

⸻

六十八、安全架构

必须考虑：

API Key
OAuth Token
Refresh Token
Proxy Password

全部属于 Secret。

不能：

log

不能：

error response

不能：

普通 config export

直接泄露。

⸻

六十九、Secret Storage

桌面端：

macOS

使用：

Keychain

Windows

使用：

Windows Credential Manager

Linux：

Secret Service

不要把：

OAuth refresh_token

明文存：

config.yaml

⸻

七十、远程 Admin 安全

如果 Admin API 暴露公网：

必须：

HTTPS
+
Admin Authentication
+
RBAC
+
Rate Limit
+
Audit Log

最好：

Cloudflare / Zero Trust

或者：

VPN

访问。

⸻

七十一、RBAC

建议：

Owner
Admin
Operator
Viewer

权限：

Owner
 ├── All
Admin
 ├── Provider
 ├── Account
 ├── API Key
 └── Usage
Operator
 ├── Health
 ├── Logs
 └── Restart
Viewer
 └── Read Only

⸻

七十二、审计日志

所有敏感操作：

Add Provider
Delete Provider
Add Account
Delete Account
Create API Key
Delete API Key
Change Route
OAuth Login
Config Change

记录：

operator
action
resource
resource_id
ip
timestamp
result

⸻

七十三、桌面端自动更新

建议：

Tauri Updater

流程：

启动
 ↓
检查版本
 ↓
发现新版本
 ↓
下载
 ↓
验证签名
 ↓
安装
 ↓
Restart

Rust Gateway 本身也应该支持：

graceful shutdown

避免请求中断。

⸻

七十四、Graceful Shutdown

退出：

SIGTERM
 ↓
Stop accepting requests
 ↓
等待 active requests
 ↓
等待 streaming
 ↓
flush usage logs
 ↓
关闭 DB
 ↓
退出

建议：

shutdown_timeout = 30s

⸻

七十五、Crash Recovery

桌面端 Gateway 崩溃：

Supervisor
 ↓
detect process exit
 ↓
restart

最好：

Crash loop protection

例如：

5 次 / 60 秒

则：

停止自动重启

避免死循环。

⸻

七十六、配置文件结构

建议：

server:
  host: 127.0.0.1
  port: 8787
auth:
  enabled: true
routing:
  strategy: priority
providers:
  - id: openai
    type: openai
    base_url: https://api.openai.com/v1
accounts:
  - id: account-001
    provider: anthropic
models:
  - name: claude-opus
    routes:
      - provider: anthropic
        account_pool: claude-main
proxy:
  pools: []
limits:
  global:
    concurrency: 100
logging:
  level: info

⸻

七十七、模块目录

建议最终 Rust 项目：

src/
│
├── main.rs
│
├── app/
│   ├── mod.rs
│   ├── state.rs
│   └── lifecycle.rs
│
├── api/
│   ├── mod.rs
│   ├── openai.rs
│   ├── anthropic.rs
│   ├── responses.rs
│   ├── models.rs
│   └── health.rs
│
├── auth/
│   ├── mod.rs
│   ├── api_key.rs
│   └── rbac.rs
│
├── routing/
│   ├── mod.rs
│   ├── engine.rs
│   ├── selector.rs
│   ├── affinity.rs
│   └── failover.rs
│
├── provider/
│   ├── mod.rs
│   ├── openai.rs
│   ├── anthropic.rs
│   ├── gemini.rs
│   ├── adapter.rs
│   └── capability.rs
│
├── account/
│   ├── mod.rs
│   ├── pool.rs
│   ├── selector.rs
│   └── health.rs
│
├── oauth/
│   ├── mod.rs
│   ├── token.rs
│   ├── refresh.rs
│   └── provider.rs
│
├── proxy/
│   ├── mod.rs
│   ├── http.rs
│   ├── socks5.rs
│   └── pool.rs
│
├── circuit/
│   ├── mod.rs
│   └── breaker.rs
│
├── streaming/
│   ├── mod.rs
│   ├── sse.rs
│   └── usage.rs
│
├── usage/
│   ├── mod.rs
│   ├── writer.rs
│   ├── sqlite.rs
│   └── metrics.rs
│
├── config/
│   ├── mod.rs
│   ├── loader.rs
│   ├── validator.rs
│   └── watcher.rs
│
├── database/
│   ├── mod.rs
│   ├── migrations.rs
│   └── repository.rs
│
├── observability/
│   ├── mod.rs
│   ├── logging.rs
│   └── metrics.rs
│
└── error.rs

⸻

七十八、Desktop 项目

推荐：

desktop/
│
├── src/
│   ├── pages/
│   │   ├── Dashboard
│   │   ├── Providers
│   │   ├── Accounts
│   │   ├── Models
│   │   ├── Routes
│   │   ├── APIKeys
│   │   ├── Proxies
│   │   ├── Usage
│   │   ├── Logs
│   │   └── Settings
│   │
│   ├── components/
│   ├── stores/
│   ├── services/
│   └── types/
│
└── src-tauri/
    ├── commands/
    ├── state/
    └── main.rs

⸻

七十九、桌面端状态管理

如果使用 React：

Zustand

管理 UI State：

useAppStore
useProviderStore
useAccountStore
useUsageStore

服务器数据：

TanStack Query

因此：

Zustand
→ UI State
TanStack Query
→ Server State

不要全部塞进 Zustand。

⸻

八十、实时状态

桌面 Dashboard 不建议每秒 HTTP Polling。

建议：

Gateway
 ↓
Event Bus
 ↓
WebSocket / Tauri Event
 ↓
UI

实时推送：

Provider Status
Account Status
Request Count
Active Connections
Circuit State

⸻

八十一、事件系统

内部建议：

enum GatewayEvent {
    ProviderStatusChanged,
    AccountStatusChanged,
    CircuitOpened,
    CircuitClosed,
    RequestCompleted,
    ConfigChanged,
    OAuthUpdated,
}

通过：

broadcast::channel

发送。

⸻

八十二、核心线程模型

Tokio：

HTTP Worker
      │
      ├── Request
      │
      ├── Router
      │
      ├── Upstream
      │
      └── Stream

后台：

Usage Writer
Health Checker
Session Cleanup
Metrics Aggregator
OAuth Refresh
Config Watcher

⸻

八十三、后台任务生命周期

统一：

TaskManager

管理：

usage_writer
health_checker
metrics
oauth_refresh
session_cleanup

避免：

tokio::spawn(...)

散落整个项目。

⸻

八十四、测试体系

至少：

Unit Test
Integration Test
Protocol Test
Load Test
Failure Test

⸻

八十五、Unit Test

重点：

Routing
CircuitBreaker
AccountSelector
Retry
Failover
Auth
ModelMapping
SSE Parser
Usage Parser
OAuth Refresh

⸻

八十六、Integration Test

使用：

Mock Provider

例如：

Mock OpenAI
Mock Anthropic
Mock 429
Mock 500
Mock Timeout
Mock SSE

验证：

A 429
 ↓
B

以及：

A timeout
 ↓
B success

⸻

八十七、Load Test

推荐：

k6

测试：

100 RPS
500 RPS
1000 RPS
5000 RPS
10000 RPS

关注：

CPU
Memory
Latency
TTFT
Connections
Lock Contention
SQLite

⸻

八十八、Streaming Load Test

重点测试：

1000 concurrent streaming

因为普通：

RPS

不能代表 AI Gateway 实际压力。

真正重要的是：

Concurrent Streams

⸻

八十九、故障测试

必须模拟：

Provider 429
Provider 401
Provider 500
Provider Timeout
Proxy Timeout
Proxy Disconnect
OAuth Expired
OAuth Refresh Failure
SQLite Locked
Client Disconnect
Upstream Disconnect

⸻

九十、Client Disconnect

这是 Streaming 最容易被忽略的问题。

如果：

Client
 ↓
Gateway
 ↓
Provider

Client 断开后：

Provider request

也应该尽快取消。

使用：

CancellationToken

实现：

Client disconnect
 ↓
CancellationToken
 ↓
Upstream request cancel

否则会浪费 Provider 额度。

⸻

九十一、请求取消

完整：

RequestContext
{
    cancellation_token
}

所有下游：

Provider
Account
Proxy
Stream

都监听 cancellation。

⸻

九十二、成本控制

如果未来支持多个 Provider：

可以增加：

Cost Router

例如：

Provider A
$10 / 1M
Provider B
$5 / 1M

根据策略：

cheapest
balanced
fastest
quality

选择。

⸻

九十三、智能模型降级

例如：

claude-opus

不可用：

claude-sonnet

不可用：

gpt-5

但这属于高级功能。

必须显式配置：

fallback:
  - claude-opus
  - claude-sonnet
  - gpt-5

不能系统自动随便降级。

⸻

九十四、虚拟模型

这是产品化非常重要的能力。

用户看到：

my-claude
my-gpt
my-coding

内部：

my-claude
 ↓
Claude Provider Pool
my-coding
 ↓
GPT + Claude + Gemini

用户不需要知道后端到底是谁。

⸻

九十五、虚拟模型路由

例如：

coding
│
├── Claude 60%
├── GPT 30%
└── Gemini 10%

或者：

coding
 ↓
Primary Claude
 ↓
Fallback GPT

⸻

九十六、租户系统

如果未来商业化，建议从现在开始预留：

Tenant

关系：

Tenant
│
├── Users
├── API Keys
├── Routes
├── Usage
└── Limits

即：

Gateway
 ↓
Tenant
 ↓
API Key
 ↓
Model
 ↓
Provider
 ↓
Account

⸻

九十七、多租户隔离

每个 Tenant：

Concurrency
RPM
TPM
Models
Providers
Daily Limit

独立。

⸻

九十八、商业化版本

可以最终分：

Community

Local Gateway
Basic Provider
Basic API Key
SQLite

Pro

Account Pool
OAuth
Smart Routing
Usage
Dashboard

Enterprise

Multi Tenant
RBAC
PostgreSQL
Redis
Cluster
HA
Prometheus
Audit
SSO

⸻

九十九、分布式架构

单机版：

Gateway
 ├── Memory
 └── SQLite

企业版：

             ┌── Gateway A
Client ─ LB ─┼── Gateway B
             └── Gateway C
                   │
          ┌────────┼────────┐
          ▼        ▼        ▼
       Redis    PostgreSQL  Metrics

Redis：

Rate Limit
Session Affinity
Distributed Lock
Provider State

PostgreSQL：

Config
Account
Usage
Tenant
Audit

⸻

一百、为什么不要一开始就 Redis

第一版：

单机

完全可以：

Atomic
Mutex
SQLite

不要为了所谓“高性能”一开始加入：

Redis
Kafka
RabbitMQ
PostgreSQL
Kubernetes

否则复杂度会快速上升。

⸻

一百零一、推荐产品演进路线

Phase 1

目标：

稳定 API Gateway

实现：

OpenAI
Anthropic
API Key
Provider
Streaming
Retry
Failover
Circuit Breaker
Usage
SQLite

⸻

Phase 2

目标：

账号池

增加：

Account
Account Pool
OAuth
Proxy Pool
Session Affinity
Health Check

⸻

Phase 3

目标：

桌面软件

增加：

Tauri
Dashboard
Provider Management
Account Management
Logs
Usage
Settings
Auto Update

⸻

Phase 4

目标：

智能路由

增加：

Virtual Model
Weighted Routing
Cost Routing
Latency Routing
Quota Routing

⸻

Phase 5

目标：

商业化

增加：

Tenant
RBAC
Billing
Cloud Console
Remote Gateway

⸻

Phase 6

目标：

Enterprise

增加：

Redis
PostgreSQL
Cluster
HA
Prometheus
Grafana
SSO
Audit

⸻

一百零二、当前代码建议优先改造

根据你目前的 proxy.rs，我认为当前最值得做的不是继续往 proxy.rs 里面加功能，而是拆模块。

现在这个文件已经承担：

HTTP Server
State
Auth
Circuit Breaker
Provider
Proxy
OAuth
Usage
Routing
Streaming
Config

继续增加功能会迅速失控。

建议第一步：

proxy.rs

逐渐缩减成：

pub async fn run_server(...) -> Result<()> {
    let state = AppState::new(...);
    let app = build_router(state);
    serve(app).await
}

⸻

一百零三、建议第一轮拆分

优先拆：

proxy.rs
 ↓
app/state.rs
routing/circuit.rs
routing/affinity.rs
provider/client.rs
auth/api_key.rs
oauth/manager.rs
streaming/sse.rs
usage/writer.rs

尤其：

ProviderCircuitGuard

应该独立出去。

⸻

一百零四、最终 Core Architecture

最终希望达到：

                    ┌───────────────┐
                    │    Client     │
                    └───────┬───────┘
                            │
                            ▼
                    ┌───────────────┐
                    │ HTTP Gateway  │
                    └───────┬───────┘
                            │
                    ┌───────▼───────┐
                    │ Authentication│
                    └───────┬───────┘
                            │
                    ┌───────▼───────┐
                    │ Rate Limiter  │
                    └───────┬───────┘
                            │
                    ┌───────▼───────┐
                    │ Routing Engine │
                    └───────┬───────┘
                            │
             ┌──────────────┼──────────────┐
             ▼              ▼              ▼
        Provider A      Provider B      Provider C
             │              │              │
         Account Pool   Account Pool   Account Pool
             │              │              │
          Proxy Pool     Proxy Pool     Proxy Pool
             │              │              │
             └──────────────┼──────────────┘
                            │
                            ▼
                         Upstream
                            │
                            ▼
                      Stream Adapter
                            │
                ┌───────────┼───────────┐
                ▼           ▼           ▼
             Client       Usage       Metrics
                            │
                            ▼
                       Async Writer
                            │
                            ▼
                          SQLite

⸻

一百零五、最终产品形态

最终产品不是简单的：

API Proxy

而应该定位为：

AI Gateway

完整能力：

                    ReCodeX Gateway
                           │
       ┌───────────────────┼───────────────────┐
       │                   │                   │
    Protocol            Routing             Account
       │                   │                   │
 OpenAI              Smart Route          Account Pool
 Anthropic            Failover             OAuth
 Gemini               Affinity             Quota
 Responses             Weight              Health
       │                   │                   │
       └───────────────────┼───────────────────┘
                           │
                        Network
                           │
                     Proxy / Pool
                           │
                        Upstream
                           │
       ┌───────────────────┼───────────────────┐
       │                   │                   │
    Security            Usage              Desktop
       │                   │                   │
   API Key             Token Stats          Tauri
   RBAC                 Cost                macOS
   Audit                Metrics             Windows

⸻

一百零六、我对你当前项目的技术路线建议

如果现在就开始继续开发，我建议按照下面顺序：

第一阶段
│
├── proxy.rs 拆分
├── Provider Registry
├── Account Registry
├── Routing Engine
└── ProviderCircuit 独立
        ↓
第二阶段
│
├── Account Pool
├── OAuth Manager
├── Proxy Pool
├── Health Checker
└── Session Affinity TTL
        ↓
第三阶段
│
├── Canonical Request
├── Provider Adapter
├── OpenAI ↔ Anthropic
└── Capability System
        ↓
第四阶段
│
├── Usage Worker
├── Metrics
├── Event Bus
└── Admin API
        ↓
第五阶段
│
├── Tauri
├── Dashboard
├── Account UI
├── Provider UI
└── Auto Update
        ↓
第六阶段
│
├── Multi Tenant
├── Redis
├── PostgreSQL
└── Cluster

一百零七、最关键的架构决策

整个项目最重要的不是 Axum，也不是 Tokio，而是下面几个抽象是否建立正确：

Provider
Account
Proxy
Model
Route
API Key
Session

最终应该形成：

API Key
   ↓
Virtual Model
   ↓
Route
   ↓
Provider
   ↓
Account
   ↓
Proxy
   ↓
Upstream

而：

Circuit Breaker
Rate Limiter
Health Check
Retry
Failover
Usage
Metrics

横向作用于整个链路。

如果这几个核心抽象设计正确，后面增加：

OpenAI
Anthropic
Gemini
Claude OAuth
Codex OAuth
更多第三方渠道

都只是增加 Adapter / Provider，而不需要重新设计整个系统。

⸻

一百零八、第一版 MVP 建议

如果目标是尽快做出稳定可用版本，我建议 MVP 暂时只保留：

Rust
Axum
Tokio
Reqwest
SQLite
Tauri
React
Zustand
TanStack Query

核心能力：

✓ OpenAI Compatible
✓ Anthropic
✓ API Key
✓ Provider
✓ Account
✓ Account Pool
✓ Proxy
✓ Streaming
✓ Retry
✓ Failover
✓ Circuit Breaker
✓ Session Affinity
✓ OAuth
✓ Usage Log
✓ Health Check
✓ Admin API
✓ Desktop Dashboard

暂时不要：

✗ Kubernetes
✗ Kafka
✗ Redis
✗ PostgreSQL
✗ 微服务
✗ 分布式配置中心

等单机 Gateway 达到：

5000~10000 concurrent streams

以后，再考虑分布式。

⸻

一百零九、最终目标

这个项目最终可以形成三个产品层：

                ReCodeX
                   │
       ┌───────────┴───────────┐
       │                       │
 ReCodeX Gateway          ReCodeX Desktop
       │                       │
       │                  macOS / Windows
       │
       ├── Provider
       ├── Account
       ├── OAuth
       ├── Proxy
       ├── Routing
       ├── Failover
       ├── Circuit Breaker
       ├── Usage
       └── Metrics
                   │
                   ▼
              ReCodeX Cloud
                   │
             ┌─────┼─────┐
             │     │     │
           Tenant Billing Admin

核心思想是：

Rust Core 保证性能，Provider/Account/Proxy 三层解耦保证扩展能力，Routing Engine 保证智能调度，Tauri 保证桌面体验，SQLite 保证单机简单可靠，未来再通过 Redis/PostgreSQL 扩展到多节点。

这套架构比较适合你现在已经拥有的代码基础，不需要推倒重来；当前 proxy.rs 里的 Circuit Breaker、Session Affinity、OAuth Runtime Token、Usage Injection、API Key Limiter、HTTP Client Pool 都可以保留，只需要逐步把职责从 proxy.rs 中拆出来。
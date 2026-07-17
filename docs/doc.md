# config.yaml 配置说明

本文档说明 `RouteHub` 当前支持的 `config.yaml` 字段。配置使用 YAML 格式，未知字段会被保留，用于兼容旧版配置或后续扩展。

## 最小配置示例

```yaml
server:
  host: 127.0.0.1
  port: 8000

web:
  enabled: false
  session_ttl_hours: 24

auth:
  enabled: false
  api_keys: []

routing:
  model_fallbacks: {}

usage_log:
  backend: sqlite
  sqlite_path: logs/proxy_usage.sqlite3

providers:
  - name: openai-main
    enabled: true
    provider_type: openai
    base_url: https://api.openai.com/v1
    api_key: sk-xxx
    models:
      - gpt-5.5
    connect_timeout: 10
    request_timeout: 60
    max_retries: 1
    priority: 1
    weight: 1
```

## 顶层字段

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `server` | object | 见下文 | 本地代理服务监听配置。 |
| `web` | object | 见下文 | 用户 Web 控制台监听配置。 |
| `auth` | object | 见下文 | 本地代理鉴权配置。 |
| `routing` | object | 见下文 | 模型 fallback 和路由配置。 |
| `usage_log` | object | 见下文 | 请求日志和用量统计配置。 |
| `providers` | array | `[]` | 上游渠道列表。 |
| 其它未知字段 | any | 无 | 会被读取并保存，当前核心代理逻辑不消费。 |

## server

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `host` | string | `127.0.0.1` | 本地代理监听地址。只允许本机访问用 `127.0.0.1`；局域网访问可用 `0.0.0.0`。 |
| `port` | number | `8000` | 本地代理监听端口。运行中修改端口需要重启代理。 |

## web

Web 控制台与代理共用同一个监听端口。用户控制台挂载在 `/user`，用户使用已启用
的 API Key 登录，只能查看该 Key 产生的新请求日志。管理控制台挂载在 `/admin`，
使用 `auth.admin_key` 登录，可查看网关汇总、渠道用量、API Key 用量和全量实时
日志。当前管理控制台为只读，不修改配置。登录后均使用进程内会话。

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | 是否启用 Web 控制台。用户端访问 `/user`，管理端访问 `/admin`。 |
| `host` | string | `127.0.0.1` | 兼容旧配置保留，当前同端口模式不单独监听。 |
| `port` | number | `8001` | 兼容旧配置保留，当前同端口模式不单独监听。 |
| `session_ttl_hours` | number | `24` | 登录会话有效时间，支持 `1` 到 `720` 小时。 |

## auth

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | 是否启用本地代理 Bearer Token 鉴权。 |
| `admin_key` | string/null | `null` | Admin Web 登录密钥。未配置或为空时管理端拒绝登录。 |
| `api_keys` | array | `[]` | 允许访问本地代理的 API Key 列表。 |

### auth.api_keys[]

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `key` | string | 必填 | 客户端请求本地代理时使用的 Bearer Token。 |
| `name` | string | `default` | Key 名称，仅用于展示和区分。 |
| `enabled` | bool | `true` | 是否启用该 Key。 |
| `created_at` | string | `""` | 创建时间，仅用于展示。 |
| `max_concurrency` | number | `5` | 该 API Key 允许同时处理的最大请求数，超过会返回 429。 |
| `daily_token_limit` | number/null | `null` | 每日 Token 上限，按本地自然日累计 `input_tokens + output_tokens`。为空或 `0` 表示不限，达到上限后新请求返回 429。 |
| `allowed_models` | array<string> | `[]` | 允许访问的模型；空列表或 `*` 表示允许全部模型。 |
| `allowed_providers` | array<string> | `[]` | 允许使用的渠道名称；空列表或 `*` 表示允许全部渠道，路由与故障转移不会越过该列表。 |

客户端请求示例：

```http
Authorization: Bearer sk-proxy-xxx
```

## routing

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `model_fallbacks` | map<string,array<string>> | `{}` | 模型 fallback 列表。请求模型不可用或对应渠道失败时，会按列表尝试替代模型。 |

示例：

```yaml
routing:
  model_fallbacks:
    gpt-5.5:
      - gpt-5.6-sol
      - claude-opus-4-7
```

对于请求 `gpt-5.5`，候选模型顺序是：`gpt-5.5`、`gpt-5.6-sol`、`claude-opus-4-7`。

## usage_log

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `backend` | string | `sqlite` | 日志后端。目前使用 SQLite。 |
| `sqlite_path` | string | `logs/proxy_usage.sqlite3` | SQLite 日志文件路径。相对路径按配置文件所在目录解析。 |

日志字段里，`first_token_ms` 是流式请求从代理开始请求该渠道到首个有效文本或工具调用输出的耗时；`latency_ms` 是完整流结束后的代理端到端耗时。长输出、工具调用等待、客户端读取和 SSE 连接持续时间都会让 `latency_ms` 大于上游平台显示的模型实际耗时。

## providers[]

`providers` 是上游渠道数组。代理会先按模型和 API 能力过滤渠道，再按 `priority`、`weight` 和轮询顺序尝试。

### 基础字段

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | 渠道唯一名称，用于日志和路由标识。 |
| `enabled` | bool | `true` | 是否启用该渠道。 |
| `provider_type` | string | `openai` | 渠道协议类型。支持 `openai` 和 `anthropic`。 |
| `base_url` | string | 必填 | 上游 API Base URL，不要带具体接口路径之外的重复后缀。OpenAI 兼容一般形如 `https://example.com/v1`。 |
| `website` | string/null | `null` | 渠道官网或管理地址，仅用于展示。 |
| `api_key` | string | 必填 | 上游渠道 API Key。`openai` 类型使用 Bearer；`anthropic` 类型使用 `x-api-key`。 |
| `description` | string/null | `null` | 渠道备注，仅用于展示。 |

### 模型字段

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `models` | array<string> | `[]` | 该渠道对外声明支持的模型列表。路由时会用请求模型匹配这里的值。 |
| `model_mapping` | map<string,string> | `{}` | 模型映射。Key 是客户端请求模型，Value 是实际发给上游的模型。 |

示例：

```yaml
models:
  - gpt-5.5
model_mapping:
  gpt-5.5: gpt-5.6-sol
```

客户端请求 `gpt-5.5` 时会命中该渠道，真正上游请求模型会改成 `gpt-5.6-sol`。

### 请求头字段

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `extra_headers` | map<string,string> | `{}` | 额外发给上游的请求头。敏感/协议头如 `authorization`、`content-type`、`host` 会被代理过滤。 |

说明：客户端请求里的 Codex/OpenAI/Stainless 相关签名头会按白名单透传，上游校验 Codex 请求时通常依赖这些头。

### 能力字段

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `capabilities` | map<string,bool> | `{}` | 渠道能力声明。当前核心路由主要使用 `supports_chat` 和 `supports_responses`。 |
| `capabilities.supports_chat` | bool | `true` | 是否支持 `/v1/chat/completions`。设置为 `false` 会让 chat 路由跳过该 OpenAI 兼容渠道。 |
| `capabilities.supports_responses` | bool | `true` | 是否支持 `/v1/responses`。`responses_mode: auto` 时可配合 chat fallback。 |
| `capabilities.supports_stream` | bool | 无强制过滤 | 流式能力声明，当前主要用于展示/兼容。 |
| `capabilities.supports_tools` | bool | 无强制过滤 | 工具调用能力声明，当前主要用于展示/兼容。 |
| `capabilities.supports_tool_outputs` | bool | 无强制过滤 | 工具输出能力声明，当前主要用于展示/兼容。 |
| `capabilities.supports_models` | bool | 无强制过滤 | 模型列表能力声明，当前主要用于展示/兼容。 |

Anthropic 渠道会通过 `/messages` 协议翻译，因此即使 `supports_chat: false`，也可以作为代理侧 chat/responses 候选。

### 路由字段

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `priority` | number | `1` | 优先级，数字越小越优先。只有低数字优先级全部失败后，才会尝试更大数字优先级。 |
| `weight` | number | `1` | 同一优先级内的轮询权重。当前同一次请求内每个 provider 只尝试一次，权重只影响不同请求的起始顺序。 |
| `max_retries` | number | `0` | 单个渠道内部最大重试次数。重试耗尽后才切换下一个渠道。 |
| `responses_mode` | string | `auto` | Responses API 路由模式：`auto`、`native`、`chat`。 |

`responses_mode` 说明：

| 值 | 说明 |
| --- | --- |
| `auto` | 优先尝试原生 `/responses`，必要时按错误自动 fallback 到 chat 兼容路径。 |
| `native` | 只按原生 `/responses` 能力判断。 |
| `chat` | 强制将 `/responses` 请求转换为 chat 路径处理。Anthropic 渠道通常使用该模式。 |

### 超时字段

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `connect_timeout` | number | `10` | 连接超时秒数，限制 DNS/TCP/TLS/连接建立阶段。代理会按该值缓存 reqwest Client。 |
| `request_timeout` | number | `60` | 请求超时秒数。非流式请求限制整个请求；流式请求分别限制等待响应头和等待首个有效输出，SSE 已开始正常输出后不限制总时长。 |
| `stream_idle_timeout` | number | `0` | 流式响应开始后，连续多少秒没有收到上游新数据就主动中断。`0` 表示关闭，本地会继续等待上游返回完成事件。 |
| `stream_max_duration` | number | `0` | 流式响应开始后允许持续的最大秒数。`0` 表示关闭。设置过短会误杀长输出或工具任务。 |
| `timeout` | number | 无 | 旧字段。仍可读取，会映射到 `request_timeout`；保存新配置时建议使用 `request_timeout`。 |

建议值：

```yaml
connect_timeout: 10
request_timeout: 60
stream_idle_timeout: 60
stream_max_duration: 300
max_retries: 0
```

如果希望坏渠道尽快切换，可以降低 `request_timeout`，但不要设得太小，否则首包慢的上游会被误判失败。
如果希望避免上游进入流式后长时间拖住任务，可以给不稳定渠道设置 `stream_idle_timeout` 或 `stream_max_duration`。流已经开始后无法无缝切换渠道，这类保护会主动中断并在日志中记为 `error`。默认情况下本地不会主动收尾，会继续等待上游返回完成事件。

### 原始 SSE 抓取

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `debug_capture_sse` | bool | `false` | 是否抓取该渠道流式响应的尾部原始 SSE 事件。只建议排查问题时临时开启。 |
| `debug_sse_path` | string | `logs/raw_sse` | 抓取文件目录。相对路径按配置文件所在目录解析。 |
| `debug_sse_max_events` | number | `80` | 每次请求最多保留最后多少个 SSE event，避免文件过大。 |

示例：

```yaml
debug_capture_sse: true
debug_sse_path: logs/raw_sse
debug_sse_max_events: 120
```

说明：原始 SSE 可能包含对话内容、工具参数或上游错误详情。问题定位完成后建议关闭 `debug_capture_sse`。

### 协议和兼容字段

| 字段 | 类型 | 默认值 | 当前状态 | 说明 |
| --- | --- | --- | --- | --- |
| `health_check_mode` | string | `models` | 兼容保留 | 健康检查模式字段，当前请求路由不基于健康检查结果动态跳过渠道。 |
| `model_sync_filter` | string | `all` | 部分使用 | 桌面端同步模型时的过滤模式字段，当前核心代理路由不消费。 |
| `client_mode` | string | `normal` | 兼容保留 | 客户端模式字段，当前核心代理逻辑不消费。 |
| `strip_thought` | bool | `false` | UI 可编辑，核心有限使用 | 保留给思考内容处理/兼容旧配置。 |
| `system_prompt_override` | string/null | `null` | 生效 | chat 请求发往该渠道前，可覆盖系统提示词。空字符串等同于不覆盖。 |

### 持久会话字段

| 字段 | 类型 | 默认值 | 当前状态 | 说明 |
| --- | --- | --- | --- | --- |
| `persistent_session` | bool | `false` | 兼容保留 | 旧版持久会话字段，当前核心代理逻辑未启用完整保活机制。 |
| `persist_interval` | number | `3.0` | 兼容保留 | 持久会话间隔。 |
| `persist_max_wait` | number | `0` | 兼容保留 | 持久会话最大等待时间。 |
| `persist_keepalive` | bool | `false` | 已生效 | 是否对该渠道启用后台心跳。默认关闭，开启后会产生上游请求和少量 token 消耗。 |
| `persist_keepalive_interval` | number | `30` | 已生效 | 心跳间隔秒数，最小 5 秒。 |
| `persist_keepalive_model` | string/null | `null` | 已生效 | 心跳使用的模型。为空时使用该渠道模型列表的第一个模型。 |
| `persist_keepalive_prompt` | string | `Hi` | 已生效 | 心跳请求使用的提示词。 |

## 路由和故障转移行为

1. 根据请求模型生成候选模型列表：原模型优先，然后追加 `routing.model_fallbacks`。
2. 对每个候选模型过滤 provider：必须 `enabled: true`，且 `models` 或 `model_mapping` 能匹配模型。
3. 按 `priority` 从小到大尝试。
4. 同一 `priority` 内按 `weight` 做轮询排序。
5. 单个 provider 失败后，先按 `max_retries` 在该 provider 内重试。
6. 重试耗尽后继续尝试下一个 provider。
7. 流式请求只有拿到首个有效文本或工具调用事件后才判定 provider 成功；在此之前超时、报错或断流会继续故障转移。
8. 一旦某个 provider 成功，立即返回，不再尝试后续 provider。

同优先级同权重示例：

```text
请求 1: A -> B -> C
请求 2: B -> C -> A
请求 3: C -> A -> B
```

如果 `A.max_retries: 1` 且 A 不可用：

```text
A 第 1 次 -> A retry 第 1 次 -> B 第 1 次
```

## OpenAI 与 Anthropic 渠道差异

### OpenAI 兼容渠道

```yaml
provider_type: openai
base_url: https://example.com/v1
api_key: sk-xxx
```

代理会使用：

```http
Authorization: Bearer sk-xxx
```

支持的上游路径取决于请求和 `responses_mode`，常见为 `/chat/completions`、`/responses`、`/models`。

### Anthropic 渠道

```yaml
provider_type: anthropic
base_url: https://api.anthropic.com/v1
api_key: sk-ant-xxx
responses_mode: chat
```

代理会使用：

```http
x-api-key: sk-ant-xxx
anthropic-version: 2023-06-01
```

部分 Anthropic 兼容网关使用 Bearer Token，可通过渠道 `extra_headers` 覆盖默认认证：

```yaml
extra_headers:
  Authorization: Bearer {api_key}
```

`{api_key}` 会在请求发送前替换为该渠道的 `api_key`。配置 Bearer 认证后不会再发送 `x-api-key`。

代理侧仍暴露 OpenAI 兼容接口，上游实际走 Anthropic `/messages`，并在请求/响应之间做协议转换。

## 旧配置兼容说明

| 旧字段 | 新字段/行为 | 说明 |
| --- | --- | --- |
| `timeout` | `request_timeout` | 仍可读取。建议后续改为 `request_timeout`。 |
| 未知顶层字段 | 保留 | 例如旧版 OAuth/Antigravity 字段会被读取和保存，但核心代理不消费。 |
| 未知 provider 字段 | 保留 | 用于兼容旧配置或未来扩展。 |

### antigravity_auth

`antigravity_auth` 是旧版兼容扩展字段，当前 Rust 核心代理不会使用，但导入、编辑和保存配置时会保留。

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `enabled` | bool | 旧版 Antigravity OAuth 功能开关。 |
| `access_token` | string/null | OAuth Access Token。 |
| `refresh_token` | string/null | OAuth Refresh Token。 |
| `token_type` | string/null | Token 类型，例如 `Bearer`。 |
| `expires_at` | string/number/null | Token 过期时间。 |
| `account_email` | string/null | 关联账号邮箱。 |
| `authorization_url` | string | OAuth 授权地址。 |
| `token_url` | string | OAuth Token 交换地址。 |
| `client_id` | string | OAuth Client ID。 |
| `client_secret` | string/null | OAuth Client Secret。 |
| `redirect_uri` | string | OAuth 回调地址。 |
| `scope` | string/null | OAuth Scope。 |
| `endpoint` | string | 旧版 Antigravity API 地址。 |
| `note` | string/null | 备注。 |

## 推荐配置实践

- 多个可用渠道设置相同 `priority: 1`、`weight: 1` 时，会按请求轮询。
- 不稳定但可兜底的渠道建议使用更大的 `priority`，例如 `priority: 3`。
- 如果希望故障转移更快，优先降低 `request_timeout`，再考虑 `max_retries`。
- 流式 Codex/opencode 任务不建议使用过短的 `request_timeout`，否则首包慢时会误切渠道。
- 不要把真实 `api_key` 提交到仓库；发布包建议使用无密钥的 `config.example.yaml`。

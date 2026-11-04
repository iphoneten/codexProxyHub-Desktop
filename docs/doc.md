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
  auth_preference: provider_first
  auth_proxy: ""
  model_fallbacks: {}
  auth_preference: provider_first

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
    max_retries: 3
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
| `auth_preference` | string | `provider_first` | 同优先级的来源偏好。`provider_first` 先普通渠道，`auth_first` 先 Auth 账号；不能越过更小的 `priority`。同级成功渠道偏好可优先于默认来源顺序。 |
| `auth_proxy` | string | `""` | 统一上游代理。用于 OAuth、Auth 账号请求，以及显式开启 `use_proxy` 的普通渠道。支持 `http://`/`https://`/`socks5h://`。留空不强制代理。 |

示例：

```yaml
routing:
  model_fallbacks:
    gpt-5.5:
      - gpt-5.6-sol
      - claude-opus-4-7
  auth_preference: provider_first
  auth_proxy: ""
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
| `use_proxy` | bool | `false` | 是否使用 `routing.auth_proxy` 访问该渠道。关闭时保持直连/系统代理行为。 |
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
| `priority` | number | `1` | 同一候选模型内数字越小越优先。较小数字的候选用完本轮次数后，才尝试更大数字；成功渠道及来源偏好均不能跨优先级。原模型仍先于回退模型。 |
| `weight` | number | `1` | 同优先级候选的起始排序权重；每个候选的尝试次数仍由 `max_retries` 决定，不将权重乘入次数。 |
| `max_retries` | number | `3` | 每轮在该渠道上的业务尝试次数，包含首次；0 按 1 次兼容。用完后切换下一个候选，一轮全失败则继续下一轮。 |
| `persistent_retry` | bool | `false` | 旧版兼容字段，仍读写保存；业务请求统一持续轮询，不再由该开关控制。 |
| `responses_mode` | string | `auto` | Responses API 路由模式：`auto`、`native`、`chat`。 |

`responses_mode` 说明：

| 值 | 说明 |
| --- | --- |
| `auto` | 优先尝试原生 `/responses`，必要时按错误自动 fallback 到 chat 兼容路径。 |
| `native` | 显式尝试原生 `/responses`，不被旧 `supports_responses: false` 提前过滤；保留 Anthropic / Google 的现有协议特例。 |
| `chat` | 强制将 `/responses` 请求转换为 chat 路径处理。Anthropic 渠道通常使用该模式。 |

### 超时字段

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `connect_timeout` | number | `10` | 连接超时秒数，限制 DNS/TCP/TLS/连接建立阶段。代理会按该值缓存 reqwest Client。 |
| `request_timeout` | number | `60` | 单次上游尝试的超时秒数，不是持续轮询整个请求的总时限；流式分别限制等待响应头和首个有效输出。正常输出后使用以下流式超时字段。 |
| `stream_idle_timeout` | number | `0` | 流式响应开始后，连续多少秒没有收到上游新数据就主动中断。`0` 表示关闭，本地会继续等待上游返回完成事件。 |
| `stream_max_duration` | number | `0` | 流式响应开始后允许持续的最大秒数。`0` 表示关闭。设置过短会误杀长输出或工具任务。 |
| `timeout` | number | 无 | 旧字段。仍可读取，会映射到 `request_timeout`；保存新配置时建议使用 `request_timeout`。 |

建议值：

```yaml
connect_timeout: 10
request_timeout: 60
stream_idle_timeout: 60
stream_max_duration: 300
max_retries: 3
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

## 路由和故障转移行为

1. 根据请求模型生成候选模型列表：原模型优先，然后追加 `routing.model_fallbacks`。
2. 对每个候选模型过滤 provider：必须 `enabled: true`，且 `models` 或 `model_mapping` 能匹配模型。
3. 同一候选模型内，所有普通渠道和 Auth 账号先统一按 `priority` 从小到大排序。
4. 优先级相同时使用 `routing.auth_preference`，同一来源内保留权重和负载排序。普通请求的已有成功渠道偏好只能在同一模型、同一优先级内调整顺序；携带稳定会话标识的 Responses 请求例外，会优先回到生成其加密状态的账号、项目和实际模型。Auth 账号使用 `routing.auth_proxy`；普通渠道仅在 `use_proxy: true` 时使用。
5. 每个 provider 按 `max_retries.max(1)` 尝试，包含首次；业务重试由统一调度层控制，底层不再额外叠加次数。
6. 本候选次数用完后切换下一个候选；所有候选失败后，重新获取当前配置开始下一轮。取消熔断、冷却和半开状态对请求的阻挡。
7. 流式请求拿到首个有效文本或工具调用事件后交付；Chat 提供合法结束原因但无正文时，也返回其结束/截断状态，不判为网络故障。首个输出或合法结束之前的超时、报错或断流仍继续故障转移。
8. 一旦某个 provider 成功，立即返回，不再尝试后续 provider。普通请求下一次仍先尝试更高优先级渠道；“本地 API Key＋模型”的成功渠道偏好只在同一模型、同一优先级内生效。Responses 会话优先使用 `prompt_cache_key`、`session_id`、`conversation_id` 建立独立绑定；缺少这些字段时，再按已有加密 item 的 ID 尽力绑定。绑定只保存哈希和渠道身份，写入配置目录的 `responses-state/responses-affinity.json`，重启后继续生效。仅使用不断变化的 `previous_response_id`、且历史已裁剪掉原 opaque item 的客户端无法保证稳定绑定，但仍可使用下述降级恢复。
9. 除客户端取消外，上游错误通常参与重试，包括 400/401/403/404/429/5xx、连接或读取失败、超时、无效响应和首个输出前的流内错误。`invalid_encrypted_content`、密文无法验证及 item ID 不匹配属于例外：同一渠道只在移除被拒绝的 opaque 状态后降级重试一次，仍失败则返回 400，不进入无限轮询，也不误降级到 Chat。已有其它协议协商（如 auto 切换接口、include_usage 兼容修正）仍保留。
10. 重试退避仅属于当前请求的该渠道，不是共享熔断状态。一个渠道的 Retry-After 不延迟轮到的其他渠道；下一次再轮到该渠道时仍遵守剩余等待时间。
11. 客户端取消或服务关停结束重试；停用/删除渠道、更新密钥、收紧本地密钥权限会在后续尝试时生效。无可用候选时返回错误，不向未授权或不匹配的渠道发送请求。Auth 账号额度保护保留。
12. 已经向客户端输出业务内容后不重放请求。Responses 会话切换到不同账号、项目或实际模型时，代理不会把原安全域的密文继续发给新上游；会保留可见消息、工具调用和工具结果，移除 `previous_response_id` 及无法复用的 reasoning/compaction opaque item。该恢复会损失相应隐藏推理，但避免会话因不可解密密文永久卡死；普通无状态请求的故障转移不变。

同优先级同权重示例：

```text
请求 1: A -> B -> C
请求 2: B -> C -> A
请求 3: C -> A -> B
```

如果 A 的 Priority=0、配置 5 次，B 的 Priority=10、配置 3 次，C 的 Priority=20、配置 1 次且全部失败：

```text
A1 -> A2 -> A3 -> A4 -> A5 -> B1 -> B2 -> B3 -> C1 -> 下一轮
```

任意一次成功即结束本次请求。只有 A 时则按轮重复尝试 A。旧字段名保留，但 `max_retries: 3` 现在表示每轮共 3 次，不是首次再加 3 次；`0` 仍至少尝试一次。持续轮询可能产生费用，并受客户端自己的超时限制。
即使上次由 C 成功返回，下一次也从可用的高优先级 A 开始；C 不因历史成功跳到 A 前面。配置文件中的排列顺序不覆盖 Priority。

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

## 入站 Anthropic Messages API

除 OpenAI 兼容接口外，代理还直接暴露 Anthropic 协议端点，供 Claude Code、Anthropic 官方 SDK
等原生客户端接入：

| 端点 | 说明 |
| --- | --- |
| `POST /v1/messages` | Anthropic Messages API，支持流式与非流式 |
| `POST /v1/messages/count_tokens` | Token 估算，只读、不新增用量日志，不受每日限额拦截；鉴权与模型/渠道权限仍生效 |

鉴权同时接受两种客户端惯例，任选其一：

```http
x-api-key: <代理的 API Key>
Authorization: Bearer <代理的 API Key>
```

Anthropic 官方 SDK 只发 `x-api-key`。这里填的是**代理自己的** `auth.api_keys` 里的 key，
不是上游渠道的 key；代理会替换成所选渠道的凭据再转发，客户端 key 不会外泄给上游。

行为要点：

- **只直通 `provider_type: anthropic` 渠道。** 请求体与响应体都不做协议翻译，原样转发。
  OpenAI / Google 等渠道没有 `/messages` 端点，即使声明了同一个模型也会被排除在候选之外。
- 客户端的 `anthropic-version` 与 `anthropic-beta` 头会透传给上游；未提供 `anthropic-version`
  时代理补 `2023-06-01`。
- 复用统一的模型匹配、优先级、权重、模型 fallback、持续轮询与故障转移、运行统计和用量日志。
  用量日志里 `api` 字段记为 `messages`。
- 渠道的 `system_prompt_override` 生效，写入 Anthropic 的顶层 `system` 字段。
- 错误按 Anthropic 形状返回，客户端 SDK 可以正常解析：

```json
{"type": "error", "error": {"type": "authentication_error", "message": "无效或缺失 API Key"}}
```

Claude Code 接入示例：

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8000
export ANTHROPIC_AUTH_TOKEN=<代理的 API Key>
```

注意 `strip_thought` 只作用于 OpenAI 协议面，对入站 `/messages` 的原生直通不生效。

## 旧配置兼容说明

| 旧字段 | 新字段/行为 | 说明 |
| --- | --- | --- |
| `timeout` | `request_timeout` | 仍可读取。建议后续改为 `request_timeout`。 |
| 未知顶层字段 | 保留 | 例如旧版 OAuth/Antigravity 字段会被读取和保存，但核心代理不消费。 |
| 未知 provider 字段 | 保留 | 用于兼容旧配置或未来扩展。 |

## Auth 账号

OpenAI OAuth 账号独立保存在顶层 `auth_accounts`，不属于普通 `providers` 渠道。代理运行时会把启用账号适配为 Codex Responses 上游，并在 JWT 临近过期时通过 `refresh_token` 刷新：

```yaml
auth_accounts:
  - id: acct-local-1
    name: user@example.com
    enabled: true
    email: user@example.com
    access_token: eyJ...
    refresh_token: rt_...
    account_id: acct_...
    client_id: app_EMoamEEZ73f0CkXaXp7hrann
    token_url: https://auth.openai.com/oauth/token
    models:
      - gpt-5.4
    priority: 1
    weight: 1
```

桌面端“导入配置”支持：

- CPA 单账号 Token JSON。
- CPA 导出的账号 JSON 数组。
- sub2api 的 `accounts` 或 `data.accounts` JSON。
- `token`/`refreshToken` 驼峰账号格式。

OAuth JSON 导入采用追加或更新 Auth 账号，不覆盖现有服务、渠道、鉴权和路由配置；YAML 配置导入仍保持完整替换行为。

Auth 账号页的“导出账号”生成 RouteHub 专用 JSON，包含 `auth_accounts` 数组，可直接再次导入；导入 CPA/sub2api JSON 时会自动识别其账号数组和 `credentials` 嵌套结构。

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

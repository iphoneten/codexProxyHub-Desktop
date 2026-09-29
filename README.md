# RouteHub

Rust 桌面版 OpenAI 兼容中转管理工具，基于旧版 `codeProxyHub` 的配置格式重构。

完整配置字段说明见 [docs/doc.md](docs/doc.md)。

## 目标

- 使用 Rust 实现本机 OpenAI 兼容代理服务。
- 使用桌面端管理配置，不再提供 `/admin` 或 `/chat` WebUI。
- 继续兼容旧版 `config.yaml` 的核心字段和未知扩展字段。

## 已实现

- 桌面端启动/停止本机代理。
- 桌面端编辑服务监听地址、鉴权开关、代理 API Key、渠道启用状态、优先级、权重、超时、模型列表。
- 桌面端通过原生文件对话框导入和导出 YAML 配置。
- `GET /health`
- `GET /v1/models`
- `GET /v1/models/{model}`
- `POST /v1/chat/completions`
- `POST /v1/completions`
- `POST /v1/embeddings`
- `POST /v1/responses`
- `POST /v1/messages`（入站 Anthropic Messages API，原生直通到 `provider_type: anthropic` 渠道）
- `POST /v1/messages/count_tokens`
- Bearer API Key 鉴权（入站 Anthropic 协议同时接受 `x-api-key`）。
- 按模型匹配、优先级、权重和模型 fallback 选择 provider。
- 上游 429、5xx、网络错误时重试和故障转移。
- 渠道编辑页可独立设置“开启心跳”和“开启无限重试”，两个开关默认关闭。开启无限重试（配置字段 `persistent_retry: true`，默认关闭）：HTTP 429、500、502、503、504 不受重试次数限制，等待成功或客户端断开/服务关闭。遵守 Retry-After，否则采用带抖动的指数退避；其他错误仍使用原有重试及故障转移。开启后停用该渠道的熔断冷却，后续请求可越过已有冷却及半开探测限制，迟到失败或取消也不再触发冷却；关闭后恢复正常熔断规则。此设置不解除 Auth 账号额度耗尽限制；保活独立运行，其失败不影响客户端候选渠道。持续等待期间占用该请求的并发名额。
- “开启心跳”用于空闲上游保活（`heartbeat_enabled`），间隔由 `heartbeat_interval_secs` 设置，默认 10 秒，可设 1–3600 秒。有真实请求执行时跳过；真实请求到达时取消进行中的保活。使用桌面端选择的心跳模型（`heartbeat_model`，留空使用渠道第一个有效模型）发送 `Hi`，请求输出上限为 1 Token；按渠道类型、Responses 模式与能力选择 Responses、Chat Completions 或 Anthropic Messages，均使用流式请求；`auto` 模式优先使用该渠道成功真实请求已验证的 Responses 接口。
- Responses 保活沿用 Python 版的真实请求模板流程：普通渠道的真实 Responses 请求通过上游校验后即缓存模板，捕获不依赖心跳开关；保留工具定义、推理、输出格式等字段，将 instructions 替换为短文本并移除 `previous_response_id`。输入只保留最后一条用户消息，保留该消息的上下文块和附件，仅将最后一个文本块替换为 `Hi`，因此输入用量取决于保留的上下文；`store` 未指定时使用 `false`。渠道自定义请求头优先，每次保活生成独立请求 ID。与 Python 相同，新的成功请求未携带可复用协议头时，保留同渠道、同上游地址已有的请求头模板。没有请求体或请求头模板时跳过保活；新模板首次保活发生非临时错误，或上游返回 `invalid codex request` / `invalid_responses_request` 时，暂停该模板，等待新的成功真实请求。模板按渠道和上游地址隔离，自动保存至当前配置文件同目录下的 `keepalive-templates/<渠道标识>.json`，服务启动时读取，已有可用模板无需再次等待真实请求；仍不自动读取 Python 项目的模板文件。失败暂停状态一并保存，新的成功真实请求会更新文件并解除暂停；HTTP 408、429 和 5xx（包括连接失败、超时对应的网关错误）不会暂停模板，继续按心跳间隔及 Retry-After 后台重试。旧版已经写入文件的暂停状态仍会保留，需新的成功真实请求更新；文件持久化本身不会解决上游 400 参数不兼容。关闭心跳或暂时停用渠道不会清除模板；删除渠道或修改上游地址后清理旧内存模板，旧文件保留但不匹配新渠道/地址。
- 模板文件写入采用同目录临时文件原子替换，macOS/Linux 的模板目录权限为 `0700`、文件为 `0600`。不保存 Authorization、Cookie 等认证头，发送时使用当前渠道配置的凭据；模板仍包含保留的上下文、附件、工具定义及会话标识，请作为本地私有数据保管，不要提交或共享。读取失败时保留原文件并等待真实请求；保存失败不阻断真实请求，会提示本次更新仅在内存中可用。捕获、恢复、读写失败和保活接口会显示在运行时日志中，模板内容不写入日志。保活 HTTP 错误分类同时识别 `error.message` 与已知的 `error.code`、`error.param`，不输出完整错误响应或未知字段值。
- 保活与真实请求使用同一网络客户端选择逻辑：渠道开启“使用设置代理”且 `routing.auth_proxy` 有效时，保活也使用该代理；地址为空或无效时沿用默认网络设置。发送保活的运行日志会显示网络配置状态，不输出代理地址或凭据。保活失败不排除客户端渠道，也不触发渠道熔断；其他可重试的保活失败在后台按心跳间隔重试，并遵守更长的 Retry-After。保活状态仅写入运行时日志，不记录模板内容，也不写请求用量日志；实际用量以上游计费为准。关闭开关或停止服务会结束保活。
- 上游连接使用连接池和 TCP 保活。无限重试的流式请求等待期间另有固定 10 秒的客户端 SSE 心跳；首个心跳之后的失败通过 SSE 错误事件返回。客户端心跳不受空闲上游保活间隔影响。非流式请求仍受客户端超时限制，已输出内容后不会重放请求。
- Chat Completions 与 Responses 流式响应中途断开时，在同一客户端 SSE 连接内续接下一个可用渠道。
- `/v1/responses` 对 `responses_mode: chat` 或上游不支持 Responses API 的情况做基础 Chat Completions 兼容包装。
- SQLite 用量日志（`usage_log.backend: sqlite`）。

## 启动

```bash
cargo run
```

默认读取当前目录的 `config.yaml`。桌面端里可以修改配置路径并重新加载。

## 开发时自动重启

Rust/egui 桌面 UI 不支持像 Web HMR 一样的运行时热替换；修改 Rust 布局代码后仍然需要重新编译。开发时可以用 `cargo-watch` 自动监听文件变化并重启应用：

```bash
cargo install cargo-watch
./scripts/dev-watch.sh
```

这会监听 `src/` 和 `Cargo.toml`，保存后自动执行 `cargo run`。

## 多平台打包

### macOS Apple Silicon

```bash
rustup target add aarch64-apple-darwin
./scripts/build-dmg.sh aarch64-apple-darwin
```

产物为：

```text
dist/RouteHub-v1.2.3-macos-arm64-20260713-123456.dmg
```

### macOS Intel

在 macOS 上执行：

```bash
rustup target add x86_64-apple-darwin
./scripts/build-dmg.sh x86_64-apple-darwin
```

产物为：

```text
dist/RouteHub-v1.2.3-macos-x86_64-20260713-123456.dmg
```

### Windows x64

在安装了 Rust 和 Visual Studio C++ Build Tools 的 Windows PowerShell 中执行：

```powershell
rustup target add x86_64-pc-windows-msvc
.\scripts\build-windows.ps1
```

产物为：

```text
dist/RouteHub-v1.2.3-windows-x86_64-20260713-123456.zip
```

文件名中的末尾时间为 UTC 构建时间，格式为 `YYYYMMDD-HHMMSS`。也可以在 GitHub 仓库的 Actions 页面手动运行 `Build release packages`，一次生成上述三种包；同一次工作流的三个产物共享同一个时间戳。手动从普通分支运行时，构建结果位于对应工作流的 Artifacts。

发布 tag 必须使用 `vX.Y.Z` 格式，例如：

```bash
git tag v1.2.3
git push origin v1.2.3
```

推送 tag 后，GitHub Actions 会构建三个平台，全部成功后自动创建 `v1.2.3` Release、生成发布说明并上传两个 DMG 和一个 Windows ZIP。重新运行同一 tag 的工作流会上传带新时间戳的产物，并清理该 tag 中对应平台的旧附件。

打包脚本会自动将 tag 解析为软件版本 `1.2.3`，并写入应用标题、macOS Bundle 版本和 Windows 包内的 `VERSION.txt`。非 tag 构建会回退到 `Cargo.toml` 的 package version，也可以通过 `ROUTEHUB_VERSION=1.2.3` 显式覆盖。构建时间默认使用当前 UTC 时间，也可以通过 `ROUTEHUB_BUILD_TIME=20260713-123456` 固定。

macOS 脚本会生成 `RouteHub.app` 和带 Applications 快捷方式的 DMG。打包时如果当前目录存在 `config.yaml`，会内置到 App 的 `Resources` 中；首次从 App 启动时会复制到：

```text
~/Library/Application Support/RouteHub/config.yaml
```

后续桌面端默认读写这个用户配置文件，避免直接修改 `.app` 或 DMG 内的只读资源。

### macOS 首次打开提示"已损坏"

因为当前版本没有 Apple Developer ID 签名和公证，从浏览器下载 DMG 后 macOS 会给 `.app` 打上 `com.apple.quarantine` 隔离标记，双击可能出现：

> "RouteHub" 已损坏，无法打开。你应该将它移到废纸篓。

这不是安装包损坏，而是 Gatekeeper 拒绝运行未签名程序。任选一种方式解除：

方式一（推荐，一次性）：把 `.app` 拖入"应用程序"后，在终端执行：

```bash
xattr -dr com.apple.quarantine /Applications/RouteHub.app
```

方式二：在"访达"里对 `.app` 右键选择"打开"，在弹窗中再次点击"打开"。

打包脚本已经对 `.app` 做了 ad-hoc 签名，从源码本地打包后直接双击运行不会出现该提示。

Windows 首次运行时会将压缩包中的 `config.yaml` 复制到：

```text
%APPDATA%\RouteHub\config.yaml
```

仓库中的 `config.yaml` 不会提交，以防泄露渠道密钥。自动构建找不到本地配置时，会将无密钥的 `config.example.yaml` 作为初始配置打包。

客户端 Base URL：

```text
http://127.0.0.1:8000/v1
```

## 日志

默认配置使用 SQLite 保存请求日志：

```yaml
usage_log:
  backend: sqlite
  sqlite_path: logs/proxy_usage.sqlite3
```

桌面端“日志”页会读取 SQLite 请求记录。
流式请求里，`首字(秒)` 表示代理从开始请求该渠道到收到首个有效文本或工具调用输出的时间；`总耗时(秒)`/`latency_ms` 表示代理侧完整流连接结束后的端到端耗时，可能明显大于上游平台显示的模型处理耗时。
相对日志路径会按当前加载的配置文件所在目录解析；DMG 首次启动后默认写入：

```text
~/Library/Application Support/RouteHub/logs/
```

Codex 示例：

```toml
model_provider = "RouteHub"
model = "gpt-5.5"
review_model = "gpt-5.5"

[model_providers.RouteHub]
name = "RouteHub"
base_url = "http://127.0.0.1:8000/v1"
wire_api = "responses"
requires_openai_auth = true
```

运行日志：日志页的“运行时日志”独立显示服务启停、上游 HTTP 重试和流式等待结束事件。仅保留内存中最近 1000 条，应用退出后清空；重试按第 1、2、4、8… 次采样。上游保活状态写入运行时日志，客户端 SSE 心跳不记录；运行事件不写请求用量数据库。

## 与旧版差异

- 移除了旧版 FastAPI 管理后台和 Web 聊天测试页。
- 桌面端直接读写 YAML 配置。
- 当前版本优先覆盖 Codex/CLI 真实调用闭环；旧版的健康诊断、模型同步、SQLite 报表、Anthropic 原生完整事件转换属于后续迁移项。

# recodexProxyHub

Rust 桌面版 OpenAI 兼容中转管理工具，基于旧版 `codeProxyHub` 的配置格式重构。

## 目标

- 使用 Rust 实现本机 OpenAI 兼容代理服务。
- 使用桌面端管理配置，不再提供 `/admin` 或 `/chat` WebUI。
- 继续兼容旧版 `config.yaml` 的核心字段和未知扩展字段。

## 已实现

- 桌面端启动/停止本机代理。
- 桌面端编辑服务监听地址、鉴权开关、代理 API Key、渠道启用状态、优先级、权重、超时、模型列表。
- `GET /health`
- `GET /v1/models`
- `GET /v1/models/{model}`
- `POST /v1/chat/completions`
- `POST /v1/completions`
- `POST /v1/embeddings`
- `POST /v1/responses`
- Bearer API Key 鉴权。
- 按模型匹配、优先级、权重和模型 fallback 选择 provider。
- 上游 429、5xx、网络错误时重试和故障转移。
- `/v1/responses` 对 `responses_mode: chat` 或上游不支持 Responses API 的情况做基础 Chat Completions 兼容包装。
- SQLite 用量日志（`usage_log.backend: sqlite`），并保留 JSONL 兼容写入模式。

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

## 打包 DMG

在 macOS 上安装 Rust 后执行：

```bash
./scripts/build-dmg.sh
```

产物位置：

```text
dist/recodexProxyHub.dmg
```

脚本会先执行 `cargo build --release`，再生成 `recodexProxyHub.app` 和 DMG。打包时如果当前目录存在 `config.yaml`，会内置到 App 的 `Resources` 中；首次从 App 启动时会复制到：

```text
~/Library/Application Support/recodexProxyHub/config.yaml
```

后续桌面端默认读写这个用户配置文件，避免直接修改 `.app` 或 DMG 内的只读资源。

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

桌面端“日志”页会按当前 backend 读取最近 200 条请求记录。若将 `backend` 改为其他值，则使用 `usage_log.path` 写入/读取 JSONL。

Codex 示例：

```toml
model_provider = "recodexProxyHub"
model = "gpt-5.5"
review_model = "gpt-5.5"

[model_providers.recodexProxyHub]
name = "recodexProxyHub"
base_url = "http://127.0.0.1:8000/v1"
wire_api = "responses"
requires_openai_auth = true
```

## 与旧版差异

- 移除了旧版 FastAPI 管理后台和 Web 聊天测试页。
- 桌面端直接读写 YAML 配置。
- 当前版本优先覆盖 Codex/CLI 真实调用闭环；旧版的健康诊断、模型同步、SQLite 报表、Anthropic 原生完整事件转换和持久保活属于后续迁移项。

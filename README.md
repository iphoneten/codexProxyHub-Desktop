# recodexProxyHub

Rust 桌面版 OpenAI 兼容中转管理工具，基于旧版 `codeProxyHub` 的配置格式重构。

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
- Bearer API Key 鉴权。
- 按模型匹配、优先级、权重和模型 fallback 选择 provider。
- 上游 429、5xx、网络错误时重试和故障转移。
- Chat Completions 与 Responses 流式响应中途断开时，在同一客户端 SSE 连接内续接下一个可用渠道。
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

## 多平台打包

### macOS Apple Silicon

```bash
rustup target add aarch64-apple-darwin
./scripts/build-dmg.sh aarch64-apple-darwin
```

产物为：

```text
dist/recodexProxyHub-v1.2.3-macos-arm64-20260713-123456.dmg
```

### macOS Intel

在 macOS 上执行：

```bash
rustup target add x86_64-apple-darwin
./scripts/build-dmg.sh x86_64-apple-darwin
```

产物为：

```text
dist/recodexProxyHub-v1.2.3-macos-x86_64-20260713-123456.dmg
```

### Windows x64

在安装了 Rust 和 Visual Studio C++ Build Tools 的 Windows PowerShell 中执行：

```powershell
rustup target add x86_64-pc-windows-msvc
.\scripts\build-windows.ps1
```

产物为：

```text
dist/recodexProxyHub-v1.2.3-windows-x86_64-20260713-123456.zip
```

文件名中的末尾时间为 UTC 构建时间，格式为 `YYYYMMDD-HHMMSS`。也可以在 GitHub 仓库的 Actions 页面手动运行 `Build release packages`，一次生成上述三种包；同一次工作流的三个产物共享同一个时间戳。手动从普通分支运行时，构建结果位于对应工作流的 Artifacts。

发布 tag 必须使用 `vX.Y.Z` 格式，例如：

```bash
git tag v1.2.3
git push origin v1.2.3
```

推送 tag 后，GitHub Actions 会构建三个平台，全部成功后自动创建 `v1.2.3` Release、生成发布说明并上传两个 DMG 和一个 Windows ZIP。重新运行同一 tag 的工作流会上传带新时间戳的产物，并清理该 tag 中对应平台的旧附件。

打包脚本会自动将 tag 解析为软件版本 `1.2.3`，并写入应用标题、macOS Bundle 版本和 Windows 包内的 `VERSION.txt`。非 tag 构建会回退到 `Cargo.toml` 的 package version，也可以通过 `RECODEX_VERSION=1.2.3` 显式覆盖。构建时间默认使用当前 UTC 时间，也可以通过 `RECODEX_BUILD_TIME=20260713-123456` 固定。

macOS 脚本会生成 `recodexProxyHub.app` 和带 Applications 快捷方式的 DMG。打包时如果当前目录存在 `config.yaml`，会内置到 App 的 `Resources` 中；首次从 App 启动时会复制到：

```text
~/Library/Application Support/recodexProxyHub/config.yaml
```

后续桌面端默认读写这个用户配置文件，避免直接修改 `.app` 或 DMG 内的只读资源。

### macOS 首次打开提示"已损坏"

因为当前版本没有 Apple Developer ID 签名和公证，从浏览器下载 DMG 后 macOS 会给 `.app` 打上 `com.apple.quarantine` 隔离标记，双击可能出现：

> "recodexProxyHub" 已损坏，无法打开。你应该将它移到废纸篓。

这不是安装包损坏，而是 Gatekeeper 拒绝运行未签名程序。任选一种方式解除：

方式一（推荐，一次性）：把 `.app` 拖入"应用程序"后，在终端执行：

```bash
xattr -dr com.apple.quarantine /Applications/recodexProxyHub.app
```

方式二：在"访达"里对 `.app` 右键选择"打开"，在弹窗中再次点击"打开"。

打包脚本已经对 `.app` 做了 ad-hoc 签名，从源码本地打包后直接双击运行不会出现该提示。

Windows 首次运行时会将压缩包中的 `config.yaml` 复制到：

```text
%APPDATA%\recodexProxyHub\config.yaml
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

桌面端“日志”页会按当前 backend 读取最近 200 条请求记录。若将 `backend` 改为其他值，则使用 `usage_log.path` 写入/读取 JSONL。
相对日志路径会按当前加载的配置文件所在目录解析；DMG 首次启动后默认写入：

```text
~/Library/Application Support/recodexProxyHub/logs/
```

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

# RabbitControl macOS (`TokenBoom/rabbitcontrol-macos`)

RabbitControl macOS 客户端与硬件适配仓库，基于 [`TokenBoom/rabbitcontrol-core`](https://github.com/TokenBoom/rabbitcontrol-core) 构建，包含全部苹果原生低延迟硬件后端、Tauri 桌面应用与前后台分离系统守护。

## 仓库结构（11 crates）

| 分层 | Crate | 职责 |
|---|---|---|
| macOS 硬件与系统后端 | `rc-capture-macos`, `rc-encode-vt`, `rc-decode-vt`, `rc-render-macos`, `rc-input-macos`, `rc-audio-macos`, `rc-clipboard-macos` | ScreenCaptureKit 视频/音频、VideoToolbox 软硬一体零拷贝 H.264、Metal 零拷贝渲染、CGEventTap 输入捕获/注入、AudioQueue 播放、Carbon Pasteboard |
| 被控驻留与平台总成 | `rc-host-macos`, `rc-platform-macos` | AppKit 状态栏托盘/审批窗，以及将全部 macOS 提供者统一注入 `rabbitcontrol-core` 的注册总入口 |
| 桌面端与后台守护 | `rc-ui` (`rc-daemon`, `rc-host-agent`) | Tauri v2 桌面端（纯静态 HTML/CSS/JS）、`CAMetalLayer` 主线程桥接、TCC 权限检查向导、`LaunchDaemon` / 双会话 `LaunchAgent` |
| 诊断命令行 | `rc-cli` | 硬件/回环探测、性能 Gate 压测与独立双端运行 |

## 本地联合调试 `rabbitcontrol-core`

默认 `Cargo.toml` 从 GitHub `TokenBoom/rabbitcontrol-core` 拉取 `tag = "v0.1.0"`。若本地同时修改了相邻的 `../rabbitcontrol-core`，在本地未跟踪的 `.cargo/config.toml` 中加入：

```toml
[patch."https://github.com/TokenBoom/rabbitcontrol-core.git"]
rc-types = { path = "../rabbitcontrol-core/crates/rc-types" }
rc-diff = { path = "../rabbitcontrol-core/crates/rc-diff" }
rc-sched = { path = "../rabbitcontrol-core/crates/rc-sched" }
rc-fec = { path = "../rabbitcontrol-core/crates/rc-fec" }
rc-proto = { path = "../rabbitcontrol-core/crates/rc-proto" }
rc-trace = { path = "../rabbitcontrol-core/crates/rc-trace" }
rc-loopback = { path = "../rabbitcontrol-core/crates/rc-loopback" }
rc-capture = { path = "../rabbitcontrol-core/crates/rc-capture" }
rc-encode = { path = "../rabbitcontrol-core/crates/rc-encode" }
rc-decode = { path = "../rabbitcontrol-core/crates/rc-decode" }
rc-render = { path = "../rabbitcontrol-core/crates/rc-render" }
rc-input = { path = "../rabbitcontrol-core/crates/rc-input" }
rc-audio = { path = "../rabbitcontrol-core/crates/rc-audio" }
rc-pipeline = { path = "../rabbitcontrol-core/crates/rc-pipeline" }
rc-net = { path = "../rabbitcontrol-core/crates/rc-net" }
rc-session = { path = "../rabbitcontrol-core/crates/rc-session" }
rc-tools = { path = "../rabbitcontrol-core/crates/rc-tools" }
rc-client-core = { path = "../rabbitcontrol-core/crates/rc-client-core" }
```

## 验证与打包

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --test-threads=1

# 打包签名 macOS .app 与 .dmg（target/dist/）
./scripts/package-macos.sh --release
```

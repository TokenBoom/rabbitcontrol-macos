# RabbitControl macOS 客户端全量能力清单与跨端功能对齐基线

> **文档用途**：作为 **Windows（`rabbitcontrol-win`）**、**iOS（`rabbitcontrol-ios`）** 与 **Android（`rabbitcontrol-android`）** 的单一权威功能参考清单，确保各端在功能范围、交互语义、协议契约、安全红线与错误体验上与 macOS 参考实现保持一致。

---

## 基线版本与上游契约

| 组件 / 仓库 | 参考基线版本 / Commit | 关键依赖与说明 |
|---|---|---|
| `rabbitcontrol-macos` | Tag `v0.2.2`（`46b7d9e`，workspace `0.2.2`） | Tauri GUI（`rc-ui`）+ 系统守护双进程（`rc-daemon` / `rc-host-agent`）+ 原生硬件/OS 后端 + 诊断 CLI（`rc-cli`） |
| `rabbitcontrol-core` | Tag `v0.2.3`（`CORE_ABI_VERSION = 2`） | 18 个纯跨平台 Rust crate（协议、QUIC/mTLS、帧调度、工具引擎、通用客户端内核 `rc-client-core`） |
| `rabbitcontrol-apple-media` | Tag `v0.2.1`（`e5f9aac`） | Apple VideoToolbox AV1/HEVC/H.264 硬件解码及移动端 C FFI 适配 |
| `rabbitcontrol-spec` | Tag `v1.0.0` | 统一错误目录（116 个规范错误码、127 个线协议别名、17 个域、4 端平台覆写）与多语言代码生成 |

---

## 一、客户端总体架构、进程与窗口拓扑

### 1.1 四大控制反转（IoC）平台注册入口

所有跨平台客户端均禁止在 `rabbitcontrol-core` 内引入 OS/UI 依赖，必须通过以下 4 个入口由宿主平台注入原生能力（macOS 集中在 `crates/rc-platform-macos/src/lib.rs::register_all()`）：

1. `rc_pipeline::register_platform_provider(...)`（`PlatformMediaProvider`）：采集、编码、解码、硬件渲染器、主机单调时钟、线程 QoS、编解码与静帧细化能力上报；
2. `rc_audio::register_audio_provider(...)`（`AudioBackendProvider`）：系统音频采集与静音播控；
3. `rc_session::register_clipboard_provider(...)`：剪贴板读写与变更监听；
4. `rc_client_core::platform::register_platform_hooks(...)`（`ClientPlatformHooks`）：显示分辨率枚举/切换/恢复、屏保壁纸抓图、锁屏/熄屏检测与唤醒、系统账号口令校验、防窥幕布、被控静音快照/恢复、多屏/虚拟屏、系统代理检测（`resolve_system_proxy`）、主网卡 MAC 获取（`primary_mac_address`）、QoS 活动声明等。

### 1.2 进程与可执行文件角色（前后台分离 + 独立被控托盘 + 诊断 CLI）

| 组件 | 形态与职责 | 跨端借鉴重点 |
|---|---|---|
| `rc-ui`（前台 GUI） | 按需开合的桌面主界面、远控渲染窗与工具窗；经 Unix Domain Socket 与后台服务通信；未安装系统服务时平滑回退为**单进程嵌入式托管模式** | Windows 对标 Electron/WinUI/Tauri 主进程 + Windows Service IPC；移动端走单进程嵌入式 |
| `rc-daemon`（系统级守护） | 随系统开机常驻：**单点串行刷新 Refresh Token**（防止多进程/多开重放吊销整族会话）、设备证书自动续签、独立复核升级包 SHA-256 与签名后原子替换 `.app`、`boot-marker` 与 `manual-stop` 防误重启标记 | Windows 对应 System Windows Service 负责开机在线与提权升级；移动端无独立 daemon |
| `rc-host-agent`（双会话被控代理） | 同时覆盖 `LoginWindow`（重启未输密码登录界面，只放行同账号 `OwnerTicket` Ed25519 自控或固定自定义密码，严禁无密码与随机临时码）与 `Aqua`（登录后桌面上下文，经 `ByeReason::SessionSwitching` 无缝交接并接管顶部状态栏）；使用 `ipc.secret` 256-bit 随机十六进制共享密钥做常量时间鉴权 | Windows 对应 Session 0 / Winlogon 桌面前置接管，保证重启/锁屏/UAC 界面均可控 |
| `rc-host-macos`（托盘与审批弹窗） | 状态栏（`NSStatusItem`）：展示当前状态、**「断开当前会话」**（仅会话中启用）、**「退出」**；非阻塞模态连接审批弹窗（支持拒绝/放行与超时自动撤窗） | Windows 托盘与来电审批气泡/弹窗须提供对等控制权 |
| `rc-cli`（诊断 CLI） | 提供 `probe`（权限与后端 JSON 探测）、`version`、`host`、`client`、`loopback`、`bench` 等命令行能力，便于自动化回归与排障 | 桌面端建议保留轻量无头诊断子命令 |

### 1.3 六大 UI 窗口与前端 IPC 契约

macOS 桌面端（`crates/rc-ui`）共暴露 **117 个 IPC 命令**（`#[tauri::command]`）与 **15 个实时推送事件**，支撑 6 个独立 HTML 视图：

| 窗口 / 视图 | 文件路径 | 核心用途 |
|---|---|---|
| 主窗口（P1 / P2 / P3） | `index.html` | 左侧导航「我的设备 / 远程协助 / 全部设备」，含启动门禁（服务器配置 / 登录 / 注册 / 改密）、客户端本地锁屏遮罩、系统权限引导条 |
| 远控会话窗口（S1） | `session.html` | 硬件渲染舞台（`CAMetalLayer`）、顶部控制条（屏幕 Tab、计时器、安全盾、新建虚拟屏、远程更新提示、控制中心按钮）、多级悬浮控制中心菜单、连接模式/自动重连/失败常驻浮层、实时网络状态 HUD |
| 文件传输窗口（W2） | `transfer.html` | 双栏（本机 ↔ 远端）文件管理与传输队列进度控制 |
| 远程终端窗口（W3） | `terminal.html` | 浅色会话列表首屏 + `xterm.js` 多会话终端、字体设置、锁屏系统口令验证弹窗 |
| 端口映射窗口（W4） | `portmap.html` | 本地 TCP 端口转发规则列表与启停/增删弹窗 |
| 设置窗口（W1） | `settings.html` | 「常规 / 安全 / 键盘 / 网络」四栏配置中心 |

**15 个核心实时事件通道**：
- 门禁与设备：`gate://status`、`server://status`、`service://status`、`client://lock-changed`、`device://status-changed`、`device://snapshots`、`device://wol-changed`、`tray://open-permissions`
- 会话与被控：`session://active-state`、`session://update`、`host://update`
- 工具与更新：`file://event`、`terminal://event`、`portmap://update`、`updater://progress`

---

## 二、启动门禁、账号体系、PKI 与客户端锁定

1. **服务器探测与配置（Gate 阶段 1）**
   - `test_server_connection` / `probe_and_save_server`：支持域/IP、HTTPS(TLS) 开关与自签证书 TOFU/指纹固定、自定义 REST API（默认 `:21118`）、信令（`:21116`）与中继（`:21117`）端口；连通后实时回显服务器状态。
2. **账号认证体系（Gate 阶段 2）**
   - `fetch_auth_config` 动态读取服务端 `allow_registration` 开关（决定是否允许注册新账号）；
   - `auth_register`、`auth_login`、`auth_change_password`、`auth_logout`；
   - **首次登录强制改密**：服务端返回 `must_change_password = true` 时进入改密拦截页，改密完成前阻塞进入主界面；
   - **单一 Refresh Token 轮换保活**：多组件并发刷新互斥，避免单次令牌重放吊销整族会话。
3. **全链 ECC PKI 与同账号免密自控（`OwnerTicket`）**
   - 登录后自动申请并持久化 7 天有效期 **ECDSA P-256** 设备证书（URI SAN `urn:rabbitcontrol:user:{uid}:device:{did}`），支持手动/后台自动续签（`renew_device_cert`）；
   - 同账号设备间发起控制默认采用 **Ed25519 PoP `OwnerTicket`** 免输入对方临时验证码直接认证通过（仍受被控端 `allow_remote_control` 总开关策略约束）。
4. **客户端本地应用锁**
   - `get_client_lock_status` / `lock_client` / `unlock_client` / `change_client_lock_password`；
   - 锁定状态下后台核心安全边界（`state.require_unlocked()`）阻断一切出站连接、工具操作与敏感设置读取。
5. **系统权限预检与非阻塞引导（macOS 特色，移动端对应系统授权）**
   - `check_permissions` / `request_screen_recording_permission` / `open_permission_settings`：分别检查 **屏幕录制**、**辅助功能**、**输入监控**；
   - 启动时弹出权限检查条/引导，但**始终保留「跳过，直接进入主界面」**（确保主控其他设备不受本机被控权限阻塞）；若授权后较进程启动期变化则给出重启生效提示；缺失权限映射为规范 `platform.<os>.*` `UiError`（如 Android 的 `MediaProjection` / `AccessibilityService` 对应映射）。

---

## 三、主界面三视图（P1 / P2 / P3）与设备管理

### 3.1 P1「我的设备」
- **侧栏过滤与排序红线**：
  - 通过 `canShowInSidebar` 严格过滤仅展示 **`allow_remote_control == true`** 的同账号设备（未开启被控的设备如手机端默认隐藏在 P3）；
  - **在线优先稳定排序**：在线设备置顶、离线设备沉底，组内按 `last_seen_at DESC, device_id ASC` 排序；收到 `device://status-changed` 实时重排，设备离线时自动清除 `inSession` 占位状态。
- **右侧双态详情页**：
  - 展示设备最近会话抓取并缓存的**桌面壁纸快照**（`get_device_snapshots` / `device://snapshots`）；
  - **纯工具在途态**（仅打开 W2/W3/W4 未开桌面）：保留壁纸预览 + 侧栏 `[1]` 活跃角标 + 顶部蓝色连接横幅，主按钮为**「进入桌面 →」**（支持原地热升级出画面）；
  - **桌面控制态**：叠加磨砂半透明遮罩与「本机正在控制此电脑」，主按钮为**「显示远控窗口」**与**「断开远控」**；
  - **快捷工具栏**：在设备页即可秒开「文件传输 / 观看模式 / 终端 / 端口映射」，无需先开桌面视频窗口；
  - **离线远程开机（#7 WoL）**：设备离线且 `wol_capable == true`（由信令快照 + `MyDeviceInfo` REST 兜底实时同步 `device://wol-changed`）时展示**「远程开机」**按钮。

### 3.2 P2「远程协助」
- **本机被控身份与策略**：展示永久 9 位设备码（`9ZGQ30ES` 样式，支持重置 `refresh_device_id`）与一键「复制并分享」；
- **协助开关与验证方式**：`允许他人远程协助`（`set_host_enabled`，独立于同账号自控策略）；访问验证支持 **8 位临时验证码定时/手动轮换**、**固定自定义密码**、**仅机主弹窗审批** 三种模式（`set_host_password`）；
- **主动连接伙伴**：输入 9 位设备码或直连 `ip:port`（`parse_connect_target` + 验证码），可选「开始协助」桌面连接或快速工具入口；
- **来电审批弹窗 & 踢出**：`HostApprovalPrompt`（带代次校验防旧弹出确认，超时/断连自动撤销），已连入可显示对端描述并一键「断开客户端」（`disconnect_host_client`）。

### 3.3 P3「全部设备」
- 展示同账号下**全部设备**（含 `allow_remote_control == false` 的 iOS/Android 设备）；
- 对未开启远控的设备：呈现**「未开启远控」**徽标、隐藏进入详情箭头 `>`、禁用点击连接与右键远控菜单，但保留信息展示与设备管理操作；
- 通用设备管理：**修改设备备注名**（`rename_my_device`）与**移除设备**（`remove_my_device`）。

---

## 四、网络路由、断网自愈、代理与 Wake-on-LAN

1. **三层路由与分级回退（含 #4 握手修复）**
   - 路由优先级：**静默局域网发现（UDP LAN）→ P2P UDP 打洞 → 强制 TLS 1.3 mTLS TCP 中继（`:21117`）**；局域网设备仅在连接路由决策时静默使用，**默认不在设备列表暴露**；
   - **#4 握手稳定性**：解耦 QUIC Accept 短分片与 TLS 握手超时，`Welcome` 收到前遇到底层超时/断连自动回退下一级链路；**严守红线**：一旦对方拒绝 mTLS/鉴权、处于 `AuthChallenge` 审批等待期或模式为 `P2pOnly`，绝不误降级。
2. **三档持久化「连接模式」（热切换不断流恢复）**
   - `AppPrefs.connection_mode`（`Auto` 自动 / `P2pOnly` 仅 P2P / `TcpRelay` TCP 中继），持久化在 `prefs.json`；
   - 在 S1 会话窗口内、重连浮层与失败页面均可点击分段切换（`set_session_connection_mode`），切换后等待旧连接发完 `Bye` 即立刻按新模式重建（`TcpRelay` 走专用 `RelayAlloc → RelayReady`；`P2pOnly` 禁止回退中继）。
3. **断网/切网快检与全路径自动重连**
   - `network_monitor`（系统网络路由/网卡变更监听，切网 1–2s 内刷新信令、清空重探 LAN、续刷 UPnP，并立刻触发活跃会话探针与重连 500ms 快试）；
   - `ClientSession` 6s / `HostSession` 8s 双向活性看门狗、QUIC `10s` 空闲超时、Relay `10s/3s` 保活、信令 `10s/25s` 心跳；
   - 被控端支持同对端/失活会话入站即时抢占（消灭占线黑屏）；
   - 控制端 `AutoReconnectTask` 支持 8 次指数退避重连 + `trigger_reconnect_now` / `reconnect_last_session` 手动秒重试；**S1 会话窗口遇到错误时常驻展示失败原因与「重新连接 / 关闭窗口」，绝不自动关闭弹窗**。
4. **UPnP IGD v1/v2 + NAT-PMP 自动端口映射**
   - `rc_client_core::upnp`：纯安全 Rust 实现，启动与切网时自动将本地 QUIC UDP 端口映射至网关公网端口、半租期自动续租、退网/退出发送退租删除映射；严格内置 **SSRF 拦截**（只允许私有 LAN IPv4 + 禁止 HTTP 3xx 跳转）与 **CGNAT（`100.64.0.0/10`）过滤**；可在 W1 设置「网络」实时查看映射协议/公网端口/状态。
5. **出站网络代理（#5）**
   - 支持 **`none`（不使用） / `system`（跟随系统） / `manual`（手动 SOCKS5 或 HTTP CONNECT + 可选认证）**；
   - 覆盖全部出站 TCP 链路：REST（`:21118`）、信令（`:21116`）、TCP 中继（`:21117`）与自更新下载器；
   - **安全与路由约定**：已保存密码绝不在接口响应或日志中明文回显；代理激活时自动跳过 UDP 探测/UPnP/P2P 打洞直接走 `:21117` mTLS 中继（与 `P2pOnly` 冲突时明确返回 `proxy.config_conflict`）。
6. **Wake-on-LAN 远程开机（#7）**
   - 被控端仅在用户打开「允许通过远程开机启动（`allow_wol`）」时采集主网卡 MAC 并上报，关闭时不采集也不上报；
   - 控制端调用 `request_device_wol` 自动经服务端协调三路唤醒：同局域网直发 UDP 102B Magic Packet → 同账号同网段在线节点 `WolRelay` 代发 → 可选服务端直发兜底；结果以 `WolAckResult` 或结构化 `UiError` 返回。

---

## 五、远控桌面（S1）与低延迟媒体/显示/音频/剪贴板

1. **零拷贝硬件媒体管线与色彩/画质调优**
   - 零拷贝链路：`ScreenCaptureKit` → `VideoToolbox`（2-deep 流水线编码与解码）→ `Metal`（`CAMetalLayer`）；
   - **BT.709 色彩空间对齐** + H.264 **High Profile** + 内容自适应码率控制（静态零编码 + 局域变化降码率）+ **真实脏区提取**（`SCStreamFrameInfoDirtyRects`）；
   - 核心指标遵循 Q1–Q6（高起始帧率、不对称降升收敛、接收端极小队列避免帧积压）。
2. **非对称编解码协商与画质/帧率实时调控**
   - 能力真实上报：支持硬件端到端协商（例：macOS 上报 H.264/HEVC 硬编 + M3+ AV1/HEVC/H.264 硬解，**绝不虚报未支持的编码格式**；Windows NVENC/QSV/AMF 或 Android MediaCodec 可按实际硬编/硬解能力上报）；
   - 内置**会话级编解码熔断器**（解码失败或过载按 `(Codec, ResolutionTier)` 自动降级并锁定不闪烁）；
   - 控制中心「画质」子菜单提供：
     - **6 档码率预设**：极致 `150M` / 旗舰 `100M` / 臻享 `60M` / 原画 `30M` / 超清 `14M` / 高清 `8M`；
     - **7 档帧率预设**：`380` / `240` / `144` / `120` / `90` / `60` / `30` 帧；
     - **编解码器选择**：`AV1（高清省流）`（按本机解码能力 fail-closed）/ `HEVC (H.265)（高刷推荐）` / `H.264（兼容）`。
3. **多显示器、虚拟屏与分辨率匹配**
   - 顶栏屏幕 Tab 切换 + 快捷键 **「切换下一个屏幕（`⌥⌘N`）」** + 右键菜单切被控端逻辑/HiDPI 分辨率（支持**会话结束自动恢复原分辨率** `DisplayModeSessionGuard`）；
   - **「一键消除黑边」**：按远端当前纵横比智能匹配缩放与自适应窗口比例；
   - **虚拟屏管理**（`list_session_displays` / `switch_session_display` / `create_session_virtual_display` / `remove_session_virtual_display`）：支持在无实体副屏或熄屏无头 Mac 上创建/销毁虚拟显示器；
   - **全屏模式**：一键进入/退出原生全屏。
4. **熄屏唤醒与锁屏远控（Issue #2 + `v0.2.1`）**
   - 远控连接建立时自动检测显示器休眠并触发用户活动断言（`IOPMAssertion`）唤醒亮屏；
   - 锁屏桌面与未登录的登录窗口界面支持无阻塞采集与远程解锁操作，不再被错误挂起卡在静态等待画面。
5. **音频与双向剪贴板**
   - 48 kHz 立体声 IMA-ADPCM 实时音频，内置 **DTX 静音门控**（静音 200ms 后降至 `0 Kbps`）；
   - **音频采集自愈**：当被控端变更屏幕分辨率/拔插显示器导致采集流停机时，`AudioCapture::is_stopped()` 自动触发退避重启恢复（无需重连会话）；
   - **控制中心声音与安全项**：「播放远端声音」开关、「被控端静音运行」开关（`set_session_host_mute`，避免远程办公室外放泄露声音）及「远程结束后恢复被控端声音设置」；
   - **双向剪贴板同步**：支持文本实时同步及菜单栏一键启停开关。
6. **实时网络状态 HUD 与安全盾悬浮窗**
   - 实时展示传输通道（`UDP LAN` / `UDP P2P` / `Relay`）、实时 FPS、实时码率、RTT、帧耗时、丢包率与当前分辨率档位，可由「更多 → 显示网络状态」切换；
   - 安全盾按钮弹出：当前加密状态（`mTLS` 是否激活与对端证书 SHA-256 指纹）、防窥状态、静音与自动锁屏状态摘要。

---

## 六、输入、外设、安全防窥与权限门控

1. **高精度鼠标与三种光标模式**
   - 按 `aspect-fit` 视口做归一化亚像素坐标映射；`InputBatcher` 支持高频（1 kHz）移动包合批 + 首包前沿零延迟直发；
   - **三种鼠标模式**：`智能鼠标` / `使用被控端鼠标` / `使用主控端鼠标`。
2. **键盘与快捷键映射**
   - 无粘滞修饰键注入（完整跟踪并释放 `Shift`/`Option`/`Control`/`Command`）；
   - 支持**「发送系统快捷键到被控端」**开关 + 自定义快捷键配置弹窗；
   - 控制中心**常用系统组合键一键发送**：`Spotlight (⌘Space)`、`切换应用 (⌘Tab)`、`强制退出应用 (⌥⌘Esc)`、`截屏 (⇧⌘3)`、`锁定屏幕 (⌃⌘Q)`。
3. **会话级安全与被控端防窥模式**
   - **观看模式（`set_session_watch_only`）**：控制端只看不注入输入；
   - **立即锁定被控端屏幕（`⌃⌘Q`）** 与 **远程结束后自动锁屏**；
   - **被控端防窥模式（`set_session_privacy_mode`）**：支持 `不启用` / `隐私屏保`（可上传、裁剪预览自定义图片 `get_privacy_custom_image` / `save_privacy_custom_image`）/ `黑屏防窥`；
   - **会话内加密连接 (mTLS) 开关（`set_session_mtls_mode`）**（默认开启）。
4. **双维度权限与工具通道硬闸门（#6 + `v0.2.1`）**
   - **连接权限分层**：`allow_remote_control`（是否允许作为被控端出现在对方「我的设备」及被远控）与 `allow_remote_assist`（是否允许他人在 P2 协助连接）完全解耦；
   - **被控端工具通道强制鉴权**：
     - 同账号免密自控（`authenticated_owner()`）豁免受限；
     - 访客协助（Guest）：**默认禁止远程终端（`allow_terminal = false`）** 与端口映射策略门控；**文件互传权限由设置项「被远程协助时允许文件互传（`allow_assist_file_transfer`）」热控制**，运行期关闭即刻中止在途传输、清理 `.rcpart` 临时文件并通知控制端灰化 W2 入口与弹出 `tool.forbidden`。

---

## 七、三大独立工具窗口与纯工具无头模式（W2 / W3 / W4）

### 7.0 纯工具无头模式（Tools-Only）与热升级
- 从设备卡片直接打开 W2 / W3 / W4 时，采用 `tools_only = true`（或发 `SetMediaPaused(true)`），**不启动屏幕采集与视频编解码管线**，秒开工具窗并支持毫秒级低开销通道通信；
- 纯工具期间可点击「进入桌面 →」调用 `upgrade_to_desktop` **原地平滑拉起 S1 视频流**，工具窗口不断连；反之关闭 S1 窗口但保留任意工具窗口时，自动发 `SetMediaPaused(true)` 停视频保工具存活。

### 7.1 W2 文件传输（13 个 Tauri 命令 + `file://event`）
- **双栏浏览**：本机（`file_list_local` / `file_mkdir_local` / `file_rename_local` / `file_delete_local`）与远端（`file_list_remote` / `file_mkdir_remote` / `file_rename_remote` / `file_delete_remote`）对称浏览与目录管理；
- **双向并发传输**：支持多选全选、批量上传（`file_upload`）、下载（`file_download`）、单任务取消（`file_cancel`）、清理已完成（`file_clear_finished`）与移除记录（`file_remove_transfer`）；
- **存储与拖拽增强**：支持在 W1 设置接收目录（`pick_transfer_storage_dir` / `open_transfer_storage_dir`）与拖拽文件浮窗开关；实时受 `ToolPermissions.allow_file_transfer` 门控。

### 7.2 W3 远程终端（17 个 Tauri 命令 + `terminal://event`）
- **浅色会话列表首屏**：展示当前活跃 PTY 会话与历史列表，`...` 溢出菜单支持**「重命名（`terminal_rename`）」**与**「终止会话（`terminal_close`）」**；
- **多会话后台保活**：同会话最多同时保活 `8` 个后台 PTY，断线重附着（`terminal_attach`）自动回放最近 **`256 KiB` 滚动输出**，`xterm.js` 渲染并严禁 `.innerHTML`；支持脱离保留后台（`terminal_detach`）、新建（`terminal_open`）、输入（`terminal_input`）与窗口大小同步（`terminal_resize`）；
- **字体体验同步**：支持读取本机字体与拉取/缓存远端字体字节流（`terminal_list_local_fonts` / `terminal_list_remote_fonts` / `terminal_fetch_remote_font` / `terminal_get_remote_font_bytes` / `terminal_set_font_prefs`），支持等宽过滤与字号调节；
- **按 `user_id` 持久化归属者终端**：同一登录用户重连自动认领已有终端会话，与访客隔离；
- **锁屏状态系统口令验证解锁（`v0.2.1` 新增）**：
  - 当被控端处于锁屏或 `LoginWindow` 时打开终端，先通过 `terminal_get_lock_state` 获知锁定态与系统用户名，弹出「验证系统密码」门禁（`terminal_unlock` 经 `verify_system_password` 真实验证 OS 口令，内置错误重试退避冷却）；
  - 支持勾选「记住此设备系统密码」加密存入本机系统钥匙串（`rc-ui/src/keychain.rs`），下次自动解锁（`terminal_try_auto_unlock`）并支持一键清除（`terminal_clear_saved_password`）。

### 7.3 W4 端口映射（5 个 Tauri 命令 + `portmap://update`）
- 列表管理、新建/编辑表单校验（`validate_map_form`）、增删与热启停（`portmap_list` / `portmap_add` / `portmap_toggle` / `portmap_delete`）；
- 映射规则本地持久化，并在远控或纯工具重连成功后按会话自动恢复生效（`portmaps_restored`）。

---

## 八、统一错误回显规范（`rabbitcontrol-spec v1.0.0`）

1. **统一结构化模型 `UiError`（10 个 camelCase 字段）**：
   `code` / `title` / `message` / `suggestion` / `severity`（`info|warning|error|critical`） / `presentation`（`inline|toast|banner|modal`） / `action` / `retryable` / `status?` / `detail?`。
2. **目录与平台覆写**：所有 REST、信令、中继、工具、代理、WoL 与本地系统权限错误统一走 `rabbitcontrol-spec v1.0.0`（116 错误码），各端不得再维护分散硬编码错误文案表；各端自带 `ErrorPlatform` 专属提示（如 macOS TCC / Windows DXGI-UAC / iOS Keychain / Android MediaProjection）。
3. **四级无 XSS 渲染器（`error-display.js`）**：按 `presentation` 自动路由至表单行内提示（`inline`）、浮层轻提示（`toast`）、顶部告警横幅（`banner`）或阻塞模态弹窗（`modal`，自带焦点捕获并在 S1 会话窗口弹窗期间暂停远程键鼠输入防穿透）。

---

## 九、自更新、远程升级被控端、版本诊断与后台服务

1. **本机应用自更新与镜像加速**
   - `check_for_updates` / `get_cached_release` / `get_update_mirror_url` / `save_update_mirror_url` / `start_self_update` / `cancel_self_update`；
   - 支持 HTTPS 更新镜像前缀、遵循代理配置、下载进度实时事件（`updater://progress`）、**防降级 SemVer 校验 + `SHA256SUMS` 校验 + 代码签名/证书钉扎复核 + 守护进程提权原子替换**。
2. **远程升级同账号被控端（Owner-Only）**
   - S1 顶栏与「更多」菜单在检测到对端版本落后时亮起「升级被控端」（`get_session_host_version` / `trigger_remote_update` / `cancel_remote_update`），支持仅同账号归属者触发，弱网下可由控制端直接通过工具通道中转推送安装包分片。
3. **版本与构建元数据透出（#8-1）**
   - `get_app_info` 统一暴露 `CoreBuildInfo`（`coreVersion`、`gitCommit`、`buildTimestamp`、`sessionProtocolVersion`、`punchVersion`、`beaconVersion`、`abiVersion`），供「关于」与排障报告消费。
4. **系统后台服务生命周期（macOS 系统服务 / Windows Service 对标）**
   - `get_service_status` / `install_system_service` / `uninstall_system_service` / `quit_app_completely`；
   - 自动探测 FileVault 全盘加密状态并给出提示（冷重启磁盘解锁后免登录即可被控）；检测到磁盘可执行文件 SHA-256 升级后触发 `RestartForUpgrade` 平滑重启接管。

---

## 十、设置中心（W1）全项清单

| 分栏 | 设置项 | 存储与生效机制 |
|---|---|---|
| **常规** | 开机自启动 | 本地偏好 + 系统启动项 |
| **常规** | 让这台 Mac 保持唤醒状态（永不 / 10m / 30m / 1h） | 防休眠断言设置 |
| **常规** | 关闭窗口时（隐藏到菜单栏 / 退出程序） | `AppPrefs.close_to_tray`（启用后台服务时自动锁定为隐藏菜单栏） |
| **常规** | 允许通过远程开机启动（WoL） | `AppPrefs.allow_wol`（实时开关主网卡 MAC 采集与上报） |
| **常规** | 系统级后台被控服务（安装 / 卸载 / 状态 / FileVault 说明） | `launchd` Daemon/Agent + 整体退出 `quit_app_completely` |
| **常规** | 被控时拖拽文件显示发送浮窗 | 本地设置 |
| **常规** | 移动设备传输文件存储路径（修改路径 / 打开文件夹） | `AppPrefs.file_transfer_dir`（热同步至主机引擎，含防穿越校验） |
| **安全** | 默认开启加密连接 (mTLS) | `AppPrefs.encrypt_by_default` |
| **安全** | 被远程协助时允许文件互传 | `AppPrefs.allow_assist_file_transfer`（热同步运行中的主进程与后台 Agent） |
| **安全** | 本机 mTLS 设备证书（状态与有效期 / 立即续签） | `CertManager` + REST CSR 签发 |
| **键盘** | 快捷键扩展配置（当前桌面侧栏空态预留，核心快捷键已在 S1 外设子菜单就绪） | — |
| **网络** | 启用中继服务器 / 服务器地址 / 信令端口 / 中继端口 / 连接状态测试与保存 | `server.json` + 信令热重连 |
| **网络** | UPnP / NAT-PMP 端口映射开关与实时外网映射状态 | `AppPrefs.upnp_enabled` |
| **网络** | 代理设置（不使用 / 系统代理 / 手动 SOCKS5 & HTTP CONNECT + 认证脱敏 + 清除已存密码） | `AppPrefs.proxy` + 活跃信令即时重连 |
| **网络** | 更新镜像加速前缀 | HTTPS 前缀校验落盘 |

---

## 十一、跨端功能对齐总矩阵（macOS / Windows / iOS / Android）

> 图例：**全量对齐** = 与 macOS 保持对等体验与参数；**系统等价实现** = 交互与能力一致，底层替换为对应 OS API；**仅主控对齐** = 移动端默认作为控制端运行（除非明确开放移动被控）；**平台不适用** = 受系统沙盒/架构限制不强求。

| 能力域 | macOS（当前参考基线） | Windows（P1 桌面全栈对标） | iOS（移动主控优先） | Android（移动主控 + 可选协助被控） |
|---|---|---|---|---|
| **启动门禁 / 账号 / ECC PKI / `OwnerTicket`** | ✅ 已落地 | ✅ 全量对齐 | ✅ 全量对齐（Keychain） | ✅ 全量对齐（Keystore） |
| **客户端本地锁屏** | ✅ 已落地 | ✅ 全量对齐 | 🟡 生物识别/PIN 锁（推荐） | 🟡 生物识别/PIN 锁（推荐） |
| **P1 我的设备 / P2 协助 / P3 全部设备** | ✅ 已落地 | ✅ 全量对齐 | ✅ 全量对齐（侧栏过滤/P3 只读/默认 `allow_remote_control=false`） | ✅ 全量对齐（P1 仅受控端、P3 全部设备、默认仅协助模式） |
| **三层路由 + 三档连接模式 + 自动重连** | ✅ 已落地 | ✅ 全量对齐 | ✅ 全量对齐（前后台切网刷新 + 重连面板） | ✅ 全量对齐（ConnectivityManager 切网监听 + 重连面板） |
| **UPnP IGD / NAT-PMP + 出站代理（#5）** | ✅ 已落地 | ✅ 系统等价实现（WinHTTP 系统代理 + 手动 SOCKS5/HTTP） | ✅ `rc_proxy_set_config_json` 接入（#3 跟进中） | ✅ JNI 代理 + UPnP 开关（`android#1` 后已可接） |
| **Wake-on-LAN 远程开机（#7）** | ✅ 已落地 | ✅ 系统等价实现（主网卡 MAC 上报 + 离线按钮） | ✅ 主控发 WOL（#3 跟进中） | ✅ 主控发 WOL + 可选本地 MAC 上报 |
| **硬件零拷贝管线 + 非对称 AV1/HEVC/H.264** | ✅ SCK + VT + Metal（M3+ AV1 硬解） | ✅ DXGI + NVENC/QSV/AMF（可接入 AV1 硬编/硬解）+ D3D11 | ✅ Metal + VT（A17 Pro+ AV1 硬解，`apple-media v0.2.0`） | ✅ MediaCodec AV1/HEVC/H.264 硬解 + ANativeWindow |
| **多屏切换 / 改分辨率 / 虚拟屏 / 熄屏唤醒** | ✅ 已落地 | ✅ DXGI 多屏 + 虚拟屏驱动 + 唤醒防休眠 | 🟡 远端屏幕切换与分辨率菜单（控别人可用） | 🟡 远端屏幕切换与分辨率菜单（控别人可用） |
| **防窥模式 / 自动锁屏 / 被控静音 / 观看模式** | ✅ 已落地 | ✅ 系统等价实现（遮罩窗 / `LockWorkStation` / WASAPI 静音） | 🟡 控制端触发控制中心指令（Watch-only / 锁对方屏 / 静音对方） | 🟡 控制端触发控制中心指令 + 协助被控侧基础提示 |
| **精确光标 / 快捷键映射 / 常用组合键** | ✅ 已落地 | ✅ 系统等价实现（`SendInput` + `Win`/组合键映射） | 🟡 触控/虚拟鼠标/蓝牙键鼠 + 快捷功能条 | 🟡 触控/虚拟鼠标/全虚拟键盘 + 快捷组合键 |
| **音频 48kHz + 静音门控 + 采集自愈** | ✅ 已落地 | ✅ WASAPI（覆写 `AudioCapture::is_stopped` 实现设备热插拔自愈） | ✅ 远端音频播放与静音 | ✅ 远端音频播放 + `AAudio` 采集（协助被控） |
| **W2 文件传输 + #6 权限硬闸门** | ✅ 已落地 | ✅ 全量对齐 | ✅ 全量对齐（`tool_permissions` 实时禁用 + Files 导入导出） | ✅ 全量对齐（SAF/下载目录 + 权限门控） |
| **W3 远程终端（8 PTY / 256KiB / 字体 / 锁屏验证）** | ✅ 已落地（`v0.2.1`） | ✅ 全量对齐（ConPTY 被控 + xterm + DPAPI 凭据） | ✅ 远程终端主控面板 + 锁屏口令弹窗 | ✅ 远程终端主控面板 + 锁屏口令弹窗 |
| **W4 端口映射 + 纯工具热升级（`tools_only`）** | ✅ 已落地 | ✅ 全量对齐 | ✅ 工具页秒开 + 热切全屏桌面 | ✅ 工具页秒开 + 热切全屏桌面 |
| **统一 `UiError` 4 级 Presenter（`spec v1.0.0`）** | ✅ 已落地 | ⬜ `#1` 跟进中（替换 `format_auth_error`） | ⬜ `#3` 跟进中（`RCErrorModels.swift` + 4 级 Presenter） | ✅ 已合入 `47b476a` |
| **系统后台开机服务 + 提权自更新 + 远程升级被控** | ✅ `launchd` + 证书钉扎 | ✅ Windows Service + 签名校验 MSI/NSIS | 🔵 受 App Store 2.5.2 限制，CI 绑定 `core` 发布静态版本 | ✅ 运行期 `release-manifest.json` + `.sig` 热更新 `.so` + APK 自更新 |

---

## 十二、跨端迭代核对铁律（Checklist）

任何新需求在 macOS 落地并同步至其他端时，请按以下顺序核对：

1. **核心优先下沉**：凡可跨平台复用的状态机、协议字段、错误码、路由策略、权限校验均留在 `rabbitcontrol-core` 或 `rabbitcontrol-spec`，平台仓仅实现 UI 与 4 个 IoC 钩子；
2. **协议前向兼容**：新增控制面字段使用可选或 `#[serde(default)]`，禁止破坏低延迟媒体热路径；
3. **能力如实上报**：各端 `encode_capabilities()` / `decode_capabilities()` / `refinement_capability()` 必须与实际已验证硬件后端严格一致，绝不虚报；
4. **权限与错误统一**：新增错误统一入 `rabbitcontrol-spec` 并按 4 级 Presenter 呈现；工具权限与被控开关必须由**被控端核心硬闸门 + 控制端 UI 实时联动**双重保障；
5. **版本元数据透出**：「关于 / 诊断」统一展示 `CoreBuildInfo`（`coreVersion` + `gitCommit` + `abiVersion` + 协议版本号），确保四端可一键比对协议一致性。

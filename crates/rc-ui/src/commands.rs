//! Tauri IPC 命令集合（T21 API 契约 §6）。
//!
//! 全模块 `#![deny(unsafe_code)]`：
//! - 所有纯逻辑校验委托 [`rc_ui::model`]；
//! - 所有输入映射委托 [`rc_ui::input_bridge`]；
//! - Retina 物理像素换算在 Rust 侧统一完成（T21 坑 3）；
//! - AppKit / CoreAnimation 层操作统一经 `app.run_on_main_thread` 派发到主线程（T21 坑 2）。

#![deny(unsafe_code)]

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rc_input::{InputEvent, MouseButton};
use rc_session::auth::{OwnerTicketSigner, OwnerTicketVerifier};
use rc_tools::{fonts::TerminalFontInfo, FileEntry, PortMapRuleSnapshot, TransferItem};
use rc_ui::app_prefs::{
    self, AppPrefs, ConnectionMode, DeviceSnapshotView, SavedPortMapRule, SecurityPrefsPatch,
    StreamPrefsPatch, TerminalPrefsPatch,
};
use rc_ui::auth_client::{
    AuthClientError, AuthHttpClient, AuthSessionManager, BindDeviceRequest, MyDeviceInfo,
};
use rc_ui::cert_manager::{OwnerGrantPayload, RcOwnerTicketVerifier};
use rc_ui::config_paths;
use rc_ui::host_bridge::{self, HostHandle, HostUpdate, SessionMtlsBundle, UiHostConfig};
use rc_ui::input_bridge;
use rc_ui::lan_discovery::{self, LanPeer};
use rc_ui::metal_attach::{self, Frame, StatusBarAction, StatusBarDisplay};
use rc_ui::model::{self, ConnectTarget};
use rc_ui::permissions::{self, PermissionSnapshot};
use rc_ui::proxy::{self, ProxyConfig, ProxyMode, ProxyProtocol};
use rc_ui::server_config::{self, ServerConfig, ServerConnectionState, ServerStatusSnapshot};
use rc_ui::service_ipc::{IpcClient, ServiceRequest, ServiceResponse};
use rc_ui::service_manager::{self, ServiceStatusReport};
use rc_ui::session_bridge::{
    SessionCommand, SessionHandle, SessionUpdate, ToolClientHandle, UiSessionConfig,
};
use rc_ui::signal_client::{self, SignalClientHandle, SignalClientOptions, WolAckResult};
use rc_ui::ui_error::{self, codes, ErrorPlatform, IntoUiError, UiError};
use rc_ui::upnp::UpnpStatus;
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::state::{
    self, ActiveSession, ActiveSessionInfo, ActiveSessionMeta, AppGateState, AppState, DeviceEntry,
    GateCertStatus, GateStatusSnapshot, HostRuntime, HostStatusSnapshot,
};
use crate::window;

/// Tauri IPC 结构化错误信封（增量附加 4 级 [`UiError`]，同时保留顶层 `message` 兼容字符串回退）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiCommandError {
    /// 面向兼容路径的人类可读主错误文案。
    pub message: String,
    /// 基于 `rabbitcontrol-spec` 与 `ErrorPlatform::Macos` 生成的结构化 4 级 UI 错误对象。
    pub ui_error: Box<UiError>,
}

impl std::fmt::Display for UiCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl UiCommandError {
    pub(crate) fn from_ui_error(ui_error: UiError) -> Self {
        let message = if !ui_error.message.trim().is_empty() {
            ui_error.message.clone()
        } else {
            ui_error.title.clone()
        };
        Self {
            message,
            ui_error: Box::new(ui_error),
        }
    }

    pub(crate) fn from_ui_error_with_message(
        message: impl Into<String>,
        ui_error: UiError,
    ) -> Self {
        Self {
            message: message.into(),
            ui_error: Box::new(ui_error),
        }
    }

    pub(crate) fn from_auth_error(err: &AuthClientError) -> Self {
        let ui_error = err.to_ui_error(ErrorPlatform::Macos);
        Self::from_ui_error(ui_error)
    }

    pub(crate) fn from_host_error(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let ui_error = ui_error::classify_host_error(&raw, ErrorPlatform::Macos);
        Self::from_ui_error_with_message(raw, ui_error)
    }

    pub(crate) fn from_validation_error(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let ui_error = ui_error::resolve_ui_error(
            codes::VALIDATION_INVALID_INPUT,
            ErrorPlatform::Macos,
            &[],
            None,
            Some(raw.clone()),
        );
        Self::from_ui_error_with_message(raw, ui_error)
    }

    pub(crate) fn from_auth_gate_error(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let code = if raw.contains("修改密码") {
            codes::AUTH_PASSWORD_CHANGE_REQUIRED
        } else if raw.contains("服务器") {
            codes::VALIDATION_INVALID_CONFIG
        } else {
            codes::AUTH_UNAUTHORIZED
        };
        let ui_error =
            ui_error::resolve_ui_error(code, ErrorPlatform::Macos, &[], None, Some(raw.clone()));
        Self::from_ui_error_with_message(raw, ui_error)
    }

    pub(crate) fn from_file_tool_error_with_policy(
        raw: impl Into<String>,
        allow_file_transfer: bool,
    ) -> Self {
        let raw = raw.into();
        let core_forbidden_msg = ui_error::tool_forbidden_ui_error(
            rc_session::TOOL_CHANNEL_FILE,
            ErrorPlatform::Macos,
            None,
        )
        .message;
        let is_tool_forbidden = !allow_file_transfer
            || raw == core_forbidden_msg
            || raw.contains(codes::TOOL_FORBIDDEN)
            || raw.contains("TOOL_FORBIDDEN")
            || raw.contains("已禁用工具");
        let ui_error = if is_tool_forbidden {
            ui_error::tool_forbidden_ui_error(
                rc_session::TOOL_CHANNEL_FILE,
                ErrorPlatform::Macos,
                Some(raw.clone()),
            )
        } else {
            ui_error::resolve_ui_error(
                codes::TOOL_FILE_TRANSFER_FAILED,
                ErrorPlatform::Macos,
                &[],
                None,
                Some(raw.clone()),
            )
        };
        Self::from_ui_error_with_message(raw, ui_error)
    }
}

fn active_session_file_transfer_allowed(state: &AppState) -> bool {
    let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
    slot.as_ref()
        .filter(|a| !a.disconnecting)
        .map(|a| a.meta.tool_permissions.allow_file_transfer)
        .unwrap_or(true)
}

fn classify_macos_session_or_signal_error(raw: &str) -> UiError {
    let session_err = ui_error::classify_session_reason(raw, ErrorPlatform::Macos);
    if session_err.code != codes::COMMON_UNKNOWN {
        session_err
    } else {
        ui_error::classify_signal_error(raw, ErrorPlatform::Macos)
    }
}

fn session_ended_user_cancelled(reason: impl Into<String>) -> SessionUpdate {
    let reason = reason.into();
    let ui_error = ui_error::resolve_ui_error(
        codes::SESSION_CANCELLED,
        ErrorPlatform::Macos,
        &[],
        None,
        Some(reason.clone()),
    );
    SessionUpdate::Ended {
        reason,
        recoverable: false,
        ui_error: Some(ui_error),
    }
}

fn session_ended_with_error(reason: impl Into<String>, recoverable: bool) -> SessionUpdate {
    let reason = reason.into();
    let ui_error = classify_macos_session_or_signal_error(&reason);
    SessionUpdate::Ended {
        reason,
        recoverable,
        ui_error: Some(ui_error),
    }
}

/// `open_session` 默认请求宽度（0 表示采用被控端当前分辨率）。
const DEFAULT_SESSION_WIDTH: u32 = 0;
/// `open_session` 默认请求高度（0 表示采用被控端当前分辨率）。
const DEFAULT_SESSION_HEIGHT: u32 = 0;
/// `open_session` 默认优先硬件后端。
fn default_prefer_hardware() -> bool {
    true
}
/// `open_session` 默认开启剪贴板与音频。
fn default_true() -> bool {
    true
}

/// 前端发起远控会话的请求负载。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenSessionRequest {
    /// 对端地址（M2 直连格式 `ip:port`，如 `"127.0.0.1:9901"`，或 9 位设备码）。
    pub target: String,
    /// 可选访问密码（对端被控端启用密码鉴权时传入）。
    #[serde(default)]
    pub password: Option<String>,
    /// 观看模式（`true` 时只读不注入输入，T21 U9）。
    #[serde(default)]
    pub watch_only: bool,
    /// 请求宽度（像素；未传时使用默认占位，实际以 Welcome 裁定为准）。
    #[serde(default)]
    pub width: Option<u32>,
    /// 请求高度（像素；未传时使用默认占位，实际以 Welcome 裁定为准）。
    #[serde(default)]
    pub height: Option<u32>,
    /// 请求帧率（fps；未传时从持久化的 `AppPrefs.fps` 读取）。
    #[serde(default)]
    pub fps: Option<u32>,
    /// 请求码率（bps；未传时从持久化的 `AppPrefs.bitrate_bps` 读取）。
    #[serde(default)]
    pub bitrate_bps: Option<u32>,
    /// 优先硬件后端（VT 解码 + Metal 呈现）。
    #[serde(default = "default_prefer_hardware")]
    pub prefer_hardware: bool,
    /// 是否启用双向剪贴板同步（默认 `true`）。
    #[serde(default = "default_true")]
    pub clipboard_enabled: bool,
    /// 是否启用远端音频流（默认 `true`）。
    #[serde(default = "default_true")]
    pub audio_enabled: bool,
    /// 对端设备名称（用于 S1 窗标题、主窗控制状态与壁纸快照归档）。
    #[serde(default)]
    pub device_name: Option<String>,
    /// 是否强制通过 TCP 中继连接（忽略 P2P 直连）。
    #[serde(default)]
    pub force_relay: bool,
    /// 连接路由模式（`"auto"` / `"p2p_only"` / `"tcp_relay"`；未传时读取持久化偏好）。
    #[serde(default)]
    pub connection_mode: Option<String>,
    /// 后台静默工具会话模式（`true` 时仅建立底层传输会话，不弹出 S1 远程桌面窗口，供 W2/W3/W4 工具按需静默建连）。
    #[serde(default)]
    pub headless: bool,
    /// 本次会话是否开启端到端 mTLS 加密握手（未传时读取 `AppPrefs.default_encrypt_connection`）。
    #[serde(default)]
    pub encrypt_connection: Option<bool>,
}

/// WebView DOM 输入事件负载（经 [`input_bridge`] 映射为 [`InputEvent`]）。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebInputEvent {
    /// 指针移动（前端传 stage 内 CSS 坐标与 stage CSS 宽高）。
    MouseMove {
        /// 相对 stage 左上角的 X（CSS 像素）。
        x: f64,
        /// 相对 stage 左上角的 Y（CSS 像素）。
        y: f64,
        /// stage 宽度（CSS 像素）。
        stage_w: f64,
        /// stage 高度（CSS 像素）。
        stage_h: f64,
    },
    /// 鼠标按键按下/抬起（DOM `MouseEvent.button`：0=左 1=中 2=右 3=后退 4=前进）。
    MouseButton {
        /// DOM 按键编号。
        button: u8,
        /// `true` = 按下，`false` = 抬起。
        pressed: bool,
    },
    /// 鼠标滚轮（DOM `WheelEvent.deltaY` / `deltaX`，CSS 像素）。
    MouseWheel {
        /// 垂直滚动量（CSS 像素，向下为正）。
        delta_y_px: f64,
        /// 前端实测行高（CSS 像素，注入参数）。
        line_height_px: f64,
        /// 可选水平滚动量（CSS 像素，向右为正）。
        #[serde(default)]
        delta_x_px: f64,
    },
    /// 键盘按键按下/抬起（DOM `KeyboardEvent.code` + 修饰键精确快照）。
    Key {
        /// DOM `KeyboardEvent.code`（如 `"KeyA"` / `"ArrowLeft"`）。
        code: String,
        /// `true` = 按下，`false` = 抬起。
        pressed: bool,
        /// Ctrl 是否处于按下态。
        #[serde(default)]
        ctrl: bool,
        /// Shift 是否处于按下态。
        #[serde(default)]
        shift: bool,
        /// Alt / Option 是否处于按下态。
        #[serde(default)]
        alt: bool,
        /// Meta / Command 是否处于按下态。
        #[serde(default)]
        meta: bool,
    },
}

/// 前端 `ResizeObserver` 上报的 stage 占位矩形（CSS 像素）。
#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StageRectMsg {
    /// 左上角 X（CSS 像素）。
    pub x: f64,
    /// 左上角 Y（CSS 像素）。
    pub y: f64,
    /// 宽度（CSS 像素）。
    pub width: f64,
    /// 高度（CSS 像素）。
    pub height: f64,
}

/// 前端「设置 → 网络」保存信令与中继服务器配置的请求负载。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveServerConfigRequest {
    /// 是否启用信令与中继服务器。
    pub enabled: bool,
    /// 服务器主机名或 IP（支持可选 `:端口`）。
    pub server_host: String,
    /// 信令端口（1..=65535，默认 `21116`）。
    pub signal_port: u16,
    /// 中继端口（1..=65535，默认 `21117`）。
    pub relay_port: u16,
}

/// W4 添加端口映射表单输入。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MapForm {
    /// 映射名称。
    pub name: String,
    /// 本地端口字符串。
    pub local_port: String,
    /// 目标地址字符串。
    pub target_addr: String,
    /// 目标端口字符串。
    pub target_port: String,
}

/// W4 添加端口映射表单校验结果（错误文案原样返给前端红框）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MapFormErrors {
    /// 名称错误文案（通过为 `None`）。
    pub name: Option<String>,
    /// 本地端口错误文案（通过为 `None`）。
    pub local_port: Option<String>,
    /// 目标地址错误文案（通过为 `None`）。
    pub target_addr: Option<String>,
    /// 目标端口错误文案（通过为 `None`）。
    pub target_port: Option<String>,
    /// 四个字段全部通过时为 `true`。
    pub valid: bool,
}

/// 将 DOM `MouseEvent.button` 映射为 `rc_input::MouseButton`。
fn map_dom_mouse_button(button: u8) -> Option<MouseButton> {
    match button {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Middle),
        2 => Some(MouseButton::Right),
        3 => Some(MouseButton::X2),
        4 => Some(MouseButton::X1),
        _ => None,
    }
}

/// 将 `WebInputEvent` 映射为 0..=2 条 `InputEvent`（纯映射，便于离线单测）。
///
/// 返回 `(events, unmapped_key)`；若遇到未收录的 DOM `KeyboardEvent.code`，
/// `unmapped_key` 为 `true` 且 `events` 为空。
#[cfg_attr(not(test), allow(dead_code))]
fn map_web_input_event(ev: &WebInputEvent) -> (Vec<InputEvent>, bool) {
    map_web_input_event_for_video(ev, None)
}

/// 按当前协商视频分辨率 `video_size` 将 `WebInputEvent` 映射为 `InputEvent`。
///
/// 指针移动采用 [`input_bridge::normalize_pointer_in_video`]，与 `metal_attach` 共享同一套
/// `fit_video_rect` 等比居中视口几何，确保任意窗口缩放下鼠标点击与远端画面像素严格重合。
fn map_web_input_event_for_video(
    ev: &WebInputEvent,
    video_size: Option<(u32, u32)>,
) -> (Vec<InputEvent>, bool) {
    match ev {
        WebInputEvent::MouseMove {
            x,
            y,
            stage_w,
            stage_h,
        } => {
            let (nx, ny) =
                input_bridge::normalize_pointer_in_video(*x, *y, *stage_w, *stage_h, video_size);
            (vec![InputEvent::MouseMove { x: nx, y: ny }], false)
        }
        WebInputEvent::MouseButton { button, pressed } => match map_dom_mouse_button(*button) {
            Some(btn) => (
                vec![InputEvent::MouseButton {
                    button: btn,
                    pressed: *pressed,
                }],
                false,
            ),
            None => (Vec::new(), false),
        },
        WebInputEvent::MouseWheel {
            delta_y_px,
            line_height_px,
            delta_x_px,
        } => {
            let mut out = Vec::new();
            let dy = input_bridge::wheel_lines(*delta_y_px, *line_height_px);
            if dy != 0 {
                out.push(InputEvent::MouseWheel {
                    delta: dy,
                    horizontal: false,
                });
            }
            if delta_x_px.is_finite() && *delta_x_px != 0.0 {
                let dx = input_bridge::wheel_lines(-*delta_x_px, *line_height_px);
                if dx != 0 {
                    out.push(InputEvent::MouseWheel {
                        delta: dx,
                        horizontal: true,
                    });
                }
            }
            (out, false)
        }
        WebInputEvent::Key {
            code,
            pressed,
            ctrl,
            shift,
            alt,
            meta,
        } => match input_bridge::scancode_for(code) {
            Some(scancode) => {
                let modifiers = input_bridge::modifier_bits(*ctrl, *shift, *alt, *meta);
                (
                    vec![InputEvent::Key {
                        scancode,
                        pressed: *pressed,
                        modifiers,
                    }],
                    false,
                )
            }
            None => (Vec::new(), true),
        },
    }
}

/// 将 CSS 像素矩形乘以 `scale_factor` 换算为物理像素 [`Frame`]（T21 坑 3）。
fn css_rect_to_physical_frame(rect: StageRectMsg, scale_factor: f64) -> Frame {
    let scale = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    Frame {
        x: if rect.x.is_finite() {
            rect.x * scale
        } else {
            0.0
        },
        y: if rect.y.is_finite() {
            rect.y * scale
        } else {
            0.0
        },
        width: if rect.width.is_finite() {
            (rect.width * scale).max(0.0)
        } else {
            0.0
        },
        height: if rect.height.is_finite() {
            (rect.height * scale).max(0.0)
        } else {
            0.0
        },
    }
}

/// 安全回收旧会话：若持有待挂载/已挂载的 `CAMetalLayer` 租约，先在主线程 `detach_layer`
/// （期间由 [`rc_ui::session_bridge::LayerLease`] 保活底层 `MetalRenderer`，即使会话线程已
/// 退出或 `"session"` 窗口已销毁也不会 UAF），再在后台短线程 drop `lease` 与 `SessionHandle`
/// （避免主线程阻塞等待 QUIC `Bye` / `join`）。
pub(crate) fn teardown_session(app: &AppHandle, mut old: ActiveSession) {
    stop_shortcut_tap_runtime(&app.state::<AppState>());
    if let Some(lease) = old.layer_state.take_lease() {
        let _ = app.run_on_main_thread(move || {
            metal_attach::detach_leased_layer(&lease);
            std::thread::spawn(move || {
                drop(lease);
                drop(old);
            });
        });
    } else {
        std::thread::spawn(move || drop(old));
    }
}

/// 同步等待旧会话发完 `Bye(Normal)` / 断开并摘除 Metal 图层（有界超时 300ms），
/// 避免在切换传输模式或加密模式时旧连接尚未退场就向服务端申请新中继凭据，同时绝不阻塞新连接启动。
async fn teardown_session_and_wait(app: &AppHandle, mut old: ActiveSession) {
    stop_shortcut_tap_runtime(&app.state::<AppState>());
    let app_bg = app.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || {
        if let Some(lease) = old.layer_state.take_lease() {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let scheduled = app_bg.run_on_main_thread(move || {
                metal_attach::detach_leased_layer(&lease);
                let _ = tx.send(lease);
            });
            let returned_lease = if scheduled.is_ok() {
                rx.recv_timeout(Duration::from_millis(500)).ok()
            } else {
                None
            };
            drop(returned_lease);
        }
        old.handle.detach_and_abort(Duration::from_millis(300));
    })
    .await;
}

/// 断开并回收当前活动会话；可按需覆写或在未传入时读取 `prefs.lock_on_exit` 在断线前发送锁屏指令。
/// 返回递增后的 `session_open_epoch`。
pub(crate) fn disconnect_active_session_with_lock(
    app: &AppHandle,
    lock_override: Option<bool>,
) -> u64 {
    let (old, should_lock, new_epoch) = {
        let state = app.state::<AppState>();
        let lock_pref = state
            .prefs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lock_on_exit;
        let should_lock = lock_override.unwrap_or(lock_pref);
        let mut slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        // H9：递增会话打开代次，使任何在途的 `open_session` / 自动重连任务醒来后自动作废。
        let new_epoch = state.session_open_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        state.connecting_epoch.store(0, Ordering::Release);
        state.clear_terminal_error();
        cancel_auto_reconnect(&state);
        state::clear_session_snapshot_locked(&state.last_session_update);
        *state.host_version.lock().unwrap_or_else(|e| e.into_inner()) = None;
        (slot.take(), should_lock, new_epoch)
    };
    if let Some(session) = old {
        if should_lock {
            session.handle.request_lock_on_disconnect();
        }
        let _ = session.handle.tools().detach_all_terminals();
        let _ = app.emit("portmap://update", &Vec::<PortMapRuleSnapshot>::new());
        let _ = app.emit(
            "session://update",
            &session_ended_user_cancelled("已主动断开远控会话"),
        );
        teardown_session(app, session);
    }
    let state = app.state::<AppState>();
    stop_shortcut_tap_runtime(&state);
    let _ = app.emit("session://active-state", &ActiveSessionInfo::inactive());
    new_epoch
}

/// 断开并回收当前活动会话（供 S1 窗口关闭事件兜底调用，T21 U10 + U-H2 + H9）。
pub(crate) fn disconnect_active_session(app: &AppHandle) {
    let _ = disconnect_active_session_with_lock(app, None);
}

/// 脱离当前活动会话上已附着的远端终端视图（供 W3 `"terminal"` 窗口销毁事件兜底调用，保持被控端后台 PTY 存活）。
pub(crate) fn detach_active_terminals(app: &AppHandle) {
    let tools = {
        let state = app.state::<AppState>();
        let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref().map(|active| active.handle.tools())
    };
    if let Some(tools) = tools {
        let _ = tools.detach_all_terminals();
    }
}

/// IPC 命令：返回 §6.0 mock 设备表（`state.rs` 单一数据源）。
#[tauri::command]
pub fn list_devices(state: State<'_, AppState>) -> Result<Vec<DeviceEntry>, String> {
    state.require_authenticated()?;
    Ok(state.devices())
}

/// IPC 命令：校验连接目标（`model::parse_connect_target` 的 IPC 壳）。
#[tauri::command]
pub fn parse_connect_target(target: String) -> Result<String, String> {
    model::parse_connect_target(&target).map(|addr| addr.to_string())
}

fn resolve_session_target_meta(
    state: &AppState,
    raw_target: &str,
    parsed: &ConnectTarget,
    requested_name: Option<&str>,
) -> (String, String, Option<String>) {
    let trimmed = raw_target.trim();
    let show_lan = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .show_lan_devices;
    let peer = if show_lan {
        let guard = state.lan.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().and_then(|lan| {
            let peers = lan.peers();
            match parsed {
                ConnectTarget::Direct(addr) => {
                    let addr_str = addr.to_string();
                    peers.into_iter().find(|p| p.addr == addr_str)
                }
                ConnectTarget::DeviceId(id) => peers
                    .into_iter()
                    .find(|p| p.device_id.as_deref() == Some(id.as_str())),
            }
        })
    } else {
        None
    };
    let relay_target_id = match parsed {
        ConnectTarget::DeviceId(id) => Some(id.clone()),
        ConnectTarget::Direct(_) => peer.as_ref().and_then(|p| p.device_id.clone()),
    };
    let device_key = match (parsed, &peer) {
        (ConnectTarget::DeviceId(id), _) => id.clone(),
        (ConnectTarget::Direct(_), Some(p)) => format!("lan:{}", p.instance_id),
        (ConnectTarget::Direct(addr), None) => addr.to_string(),
    };
    let device_name = requested_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| peer.as_ref().map(|p| p.name.clone()))
        .unwrap_or_else(|| match parsed {
            ConnectTarget::DeviceId(id) => server_config::format_device_id(id),
            ConnectTarget::Direct(addr) => addr.to_string(),
            #[allow(unreachable_patterns)]
            _ => trimmed.to_string(),
        });
    (device_key, device_name, relay_target_id)
}

/// 单次信令连接请求超时（收紧至 5 秒，降低切网或节点离线时的等待感）。
const SIGNAL_REQUEST_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// 当信令通道正处于 `Connecting`（例如刚切网恢复瞬间）时，最多短等 4 秒待信令就绪再发起请求。
const SIGNAL_CONNECTING_WAIT_CAP: Duration = Duration::from_secs(4);

async fn resolve_endpoint_and_grant(
    state: &AppState,
    target: ConnectTarget,
    force_relay: bool,
    expected_epoch: Option<u64>,
) -> Result<(SocketAddr, Option<rc_ui::signal_client::ConnectGrant>), String> {
    match target {
        ConnectTarget::Direct(addr) => Ok((addr, None)),
        ConnectTarget::DeviceId(target_id) => {
            if state.is_service_proxy() {
                let wait_deadline = Instant::now() + SIGNAL_CONNECTING_WAIT_CAP;
                loop {
                    if expected_epoch
                        .is_some_and(|ep| state.session_open_epoch.load(Ordering::Acquire) != ep)
                    {
                        return Err("连接请求已取消".to_string());
                    }
                    let srv_state = tauri::async_runtime::spawn_blocking(|| {
                        match call_agent_ipc(
                            ServiceRequest::GetServerStatus,
                            Duration::from_millis(350),
                        ) {
                            Ok(ServiceResponse::ServerStatus(s)) => Some(s.state),
                            _ => None,
                        }
                    })
                    .await
                    .ok()
                    .flatten();
                    if srv_state != Some(ServerConnectionState::Connecting)
                        || Instant::now() >= wait_deadline
                    {
                        break;
                    }
                    let _ = tauri::async_runtime::spawn_blocking(|| {
                        std::thread::sleep(Duration::from_millis(150));
                    })
                    .await;
                }
                if expected_epoch
                    .is_some_and(|ep| state.session_open_epoch.load(Ordering::Acquire) != ep)
                {
                    return Err("连接请求已取消".to_string());
                }
                let timeout_ms = SIGNAL_REQUEST_CONNECT_TIMEOUT.as_millis() as u64;
                let grant = tauri::async_runtime::spawn_blocking(move || {
                    match call_agent_ipc(
                        ServiceRequest::RequestConnect {
                            target_id,
                            timeout_ms,
                            force_relay,
                        },
                        SIGNAL_REQUEST_CONNECT_TIMEOUT + Duration::from_secs(2),
                    )? {
                        ServiceResponse::ConnectGrant(grant) => Ok(*grant),
                        ServiceResponse::Error { message } => Err(message),
                        other => Err(format!("后台服务返回异常连接应答: {other:?}")),
                    }
                })
                .await
                .map_err(|e| format!("发起中继连接任务异常：{e}"))??;
                return Ok((SocketAddr::from(([0, 0, 0, 0], 0)), Some(grant)));
            }
            let client = {
                let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
                rt.signal_client.as_ref().cloned()
            };
            let Some(client) = client else {
                return Err("请先在「设置 → 网络」配置并连接服务器".to_string());
            };
            if client.status().state == ServerConnectionState::Connecting {
                let wait_deadline = Instant::now() + SIGNAL_CONNECTING_WAIT_CAP;
                while client.status().state == ServerConnectionState::Connecting
                    && Instant::now() < wait_deadline
                {
                    if expected_epoch
                        .is_some_and(|ep| state.session_open_epoch.load(Ordering::Acquire) != ep)
                    {
                        return Err("连接请求已取消".to_string());
                    }
                    let _ = tauri::async_runtime::spawn_blocking(|| {
                        std::thread::sleep(Duration::from_millis(100));
                    })
                    .await;
                }
            }
            if expected_epoch
                .is_some_and(|ep| state.session_open_epoch.load(Ordering::Acquire) != ep)
            {
                return Err("连接请求已取消".to_string());
            }
            if client.status().state != ServerConnectionState::Connected {
                return Err("请先在「设置 → 网络」配置并连接服务器".to_string());
            }
            let grant = tauri::async_runtime::spawn_blocking(move || {
                if force_relay {
                    client.request_relay(&target_id, SIGNAL_REQUEST_CONNECT_TIMEOUT)
                } else {
                    client.request_connect(&target_id, SIGNAL_REQUEST_CONNECT_TIMEOUT)
                }
            })
            .await
            .map_err(|e| format!("发起中继连接任务异常：{e}"))??;
            Ok((SocketAddr::from(([0, 0, 0, 0], 0)), Some(grant)))
        }
    }
}

pub(crate) fn preallocate_metal_layer_for_session(
    app: &AppHandle,
    prefer_hardware: bool,
) -> Option<metal_attach::MainThreadMetalLayer> {
    if cfg!(target_os = "macos") && prefer_hardware {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let _ = app.run_on_main_thread(move || {
            let _ = tx.send(metal_attach::new_metal_layer_on_main_thread().ok());
        });
        rx.recv_timeout(Duration::from_secs(5)).ok().flatten()
    } else {
        None
    }
}

/// 根据 `AppPrefs.still_refinement_enabled` 计算开启/升级桌面会话时是否需要下发关闭补帧指令。
///
/// - 仅在桌面会话（`!tools_only`）且用户显式关闭（`still_refinement_enabled == false`）时返回
///   `Some(SessionCommand::SetStillRefinementEnabled { enabled: false })`；
/// - 纯工具无头会话（`tools_only == true`）或保持默认开启（`true`）时返回 `None`。
pub(crate) fn still_refinement_command_for_desktop_open(
    state: &AppState,
    tools_only: bool,
) -> Option<SessionCommand> {
    if tools_only {
        return None;
    }
    let enabled = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .still_refinement_enabled;
    (!enabled).then_some(SessionCommand::SetStillRefinementEnabled { enabled: false })
}

fn activate_session_with_cfg(
    app: &AppHandle,
    state: &AppState,
    my_epoch: u64,
    ui_cfg: UiSessionConfig,
    watch_only: bool,
    meta: ActiveSessionMeta,
    preallocated_override: Option<Option<metal_attach::MainThreadMetalLayer>>,
) -> Result<(), String> {
    state.require_authenticated()?;
    if state.session_open_epoch.load(Ordering::Acquire) != my_epoch {
        return Ok(());
    }

    // 锁顺序约定：在获取 `state.session` 锁之前读取 `state.prefs`，杜绝锁顺序反转。
    let still_refine_cmd = still_refinement_command_for_desktop_open(state, ui_cfg.tools_only);

    let preallocated_layer = match preallocated_override {
        Some(layer) => layer,
        None if meta.tools_only => None,
        None => {
            window::ensure_session_window(app, state, watch_only)?;
            window::set_session_window_title(app, &meta.device_name);
            preallocate_metal_layer_for_session(app, ui_cfg.prefer_hardware)
        }
    };

    let (previous, active_info) = {
        let mut slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        if state.require_authenticated().is_err()
            || state.session_open_epoch.load(Ordering::Acquire) != my_epoch
        {
            return Ok(());
        }
        *state
            .last_session_params
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((watch_only, meta.clone()));
        let old = slot.take();
        // 在 `session` 锁内原子清空旧快照（U-H2 / U-H3），防止新窗口拉到旧会话的 `Ended` / `Negotiated`。
        state::clear_session_snapshot_locked(&state.last_session_update);
        *state.host_version.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let handle = SessionHandle::spawn_with_surface(
            ui_cfg,
            preallocated_layer.map(metal_attach::MainThreadMetalLayer::into_render_surface),
        );
        if let Some(cmd) = still_refine_cmd {
            handle.send(cmd);
        }
        let sid = state.next_session_id();
        let mut active = ActiveSession::new(sid, handle, watch_only);
        active.set_meta(meta);
        let info = active.info();
        *slot = Some(active);
        state.clear_connecting_epoch(my_epoch);
        state.clear_terminal_error();
        (old, info)
    };

    let _ = app.emit("session://active-state", &active_info);

    if let Some(old) = previous {
        // 替换旧会话前脱离其已附着的终端视图并清空 W4 端口映射列表，同时保留主机侧后台 PTY。
        let _ = old.handle.tools().detach_all_terminals();
        let _ = app.emit("portmap://update", &Vec::<PortMapRuleSnapshot>::new());
        teardown_session(app, old);
    }

    Ok(())
}

fn local_device_id_snapshot(state: &AppState) -> Option<String> {
    let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
    rt.current_status()
        .device_id
        .or_else(|| rt.config.cached_device_id.clone())
        .or_else(|| state.auth.server_config().cached_device_id)
}

fn build_session_mtls_bundle(
    state: &AppState,
    expected_peer_device_id: Option<String>,
    expected_peer_user_id: Option<String>,
) -> Option<SessionMtlsBundle> {
    let ca_cert_pem = {
        let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        rt.config
            .ca_cert_pem
            .clone()
            .or_else(|| state.auth.server_config().ca_cert_pem)
    }?;
    let current_uid = state.auth.current_session()?.user_id;
    let local_did = local_device_id_snapshot(state);
    let cert_mgr = state.auth.cert_manager();
    let (cert_pem, meta) = cert_mgr.load_device_cert()?;
    if !meta.user_id.eq_ignore_ascii_case(current_uid.trim()) {
        return None;
    }
    if let Some(ref did) = local_did {
        if meta.device_id != *did {
            return None;
        }
    }
    let key_pem = cert_mgr.tls_key_pem().ok()?;
    Some(SessionMtlsBundle {
        ca_cert_pem,
        cert_pem,
        key_pem,
        local_user_id: meta.user_id,
        expected_peer_device_id,
        expected_peer_user_id,
    })
}

pub(crate) fn build_relay_reject_mtls(
    state: &AppState,
) -> Option<rc_ui::relay_transport::RelayMtlsConfig> {
    build_session_mtls_bundle(state, None, None).map(|b| b.to_relay_mtls_config(None))
}

fn build_owner_verifier(state: &AppState) -> Option<Arc<dyn OwnerTicketVerifier>> {
    let server_auth_pubkey_hex = {
        let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        rt.config
            .server_auth_pubkey_hex
            .clone()
            .or_else(|| state.auth.server_config().server_auth_pubkey_hex)
    }?;
    let user_id = state.auth.current_session()?.user_id;
    let local_device_id = local_device_id_snapshot(state)?;
    RcOwnerTicketVerifier::new(&server_auth_pubkey_hex, user_id, local_device_id)
        .ok()
        .map(|v| Arc::new(v) as Arc<dyn OwnerTicketVerifier>)
}

fn build_signal_client_options(state: &AppState) -> SignalClientOptions {
    let auth = Arc::clone(&state.auth);
    let token_provider: rc_ui::signal_client::TokenProvider =
        Arc::new(move || auth.ensure_valid_access_token().map_err(format_auth_error));
    let auth_refresh = Arc::clone(&state.auth);
    let token_refresher: rc_ui::signal_client::TokenRefresher = Arc::new(move |stale| {
        auth_refresh
            .refresh_after_unauthorized(stale)
            .map_err(format_auth_error)
    });
    let device_pubkey = Some(state.auth.cert_manager().device_pubkey_hex());
    let prefs_snap = sync_prefs_from_disk_if_proxy(state);
    let allow_remote_assist = prefs_snap.allow_remote_assist;
    let upnp_enabled = prefs_snap.upnp_enabled;
    let allow_remote_control = state
        .host
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .allow_owner_control;
    SignalClientOptions {
        token_provider: Some(token_provider),
        token_refresher: Some(token_refresher),
        device_pubkey,
        allow_remote_assist,
        allow_remote_control: Some(allow_remote_control),
        dynamic_lan_discovery: true,
        upnp_enabled,
    }
}

fn normalize_device_id_for_match(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect()
}

/// 若收到的 `DeviceStatusChanged` 指向本机设备，则热同步本机 `HostRuntime` 与活跃 `HostHandle` 的
/// `allow_owner_control` 门控状态（防止远端 REST `PATCH` 关闭本机远控后局域网直连仍放行）。
pub(crate) fn apply_local_device_status_policy(
    state: &AppState,
    ev: &rc_ui::signal_client::DeviceStatusEvent,
) -> bool {
    let Some(local_did) = local_device_id_snapshot(state) else {
        return false;
    };
    let norm_local = normalize_device_id_for_match(&local_did);
    let norm_ev = normalize_device_id_for_match(&ev.device_id);
    if norm_local.is_empty() || norm_local != norm_ev {
        return false;
    }
    let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
    host.allow_owner_control = ev.allow_remote_control;
    if let Some(handle) = host.handle.as_ref() {
        handle.set_allow_owner_control(ev.allow_remote_control);
    }
    true
}

/// 将最新的 `allow_remote_assist` / `allow_assist_file_transfer` / `allow_owner_control` / `OwnerTicketVerifier` / `mTLS` 策略热同步至正在运行的被控端监听服务。
pub(crate) fn refresh_host_security_runtime(state: &AppState) {
    let (allow_remote_assist, allow_assist_file_transfer, default_encrypt) = {
        let prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        (
            prefs.allow_remote_assist,
            prefs.allow_assist_file_transfer,
            prefs.default_encrypt_connection,
        )
    };
    let owner_verifier = build_owner_verifier(state);
    let mtls_bundle = build_session_mtls_bundle(state, None, None);
    let accept_mtls_lan = default_encrypt;
    let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
    host.allow_remote_assist = allow_remote_assist;
    if let Some(handle) = host.handle.as_ref() {
        handle.set_allow_owner_control(host.allow_owner_control);
        handle.set_allow_remote_assist(allow_remote_assist);
        handle.set_allow_assist_file_transfer(allow_assist_file_transfer);
        handle.set_owner_verifier(owner_verifier);
        handle.set_mtls_config(mtls_bundle, accept_mtls_lan);
    }
    sync_lan_advertising_locked(state, &host);
}

fn sync_assist_state_to_server(app: &AppHandle, state: &AppState, allow: bool) {
    let signal_client = {
        let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        rt.signal_client.as_ref().cloned()
    };
    if let Some(client) = signal_client {
        let _ = client.set_allow_remote_assist(allow);
    }
    if let Some(device_id) = local_device_id_snapshot(state) {
        let app_bg = app.clone();
        let _ = std::thread::Builder::new()
            .name("rc-sync-assist-rest".to_string())
            .spawn(move || {
                let st_bg = app_bg.state::<AppState>();
                let _ = with_valid_token_in_app(&app_bg, &st_bg, |client, token| {
                    client.set_device_allow_remote_assist(token, &device_id, allow)
                });
            });
    }
}

struct PreparedSessionSecurity {
    mtls_config: Option<SessionMtlsBundle>,
    owner_grant: Option<OwnerGrantPayload>,
    owner_signer: Option<Arc<dyn OwnerTicketSigner>>,
    lan_direct_addr: Option<SocketAddr>,
}

fn prepare_session_security(
    state: &AppState,
    relay_target_id: Option<&str>,
    relay_grant: Option<&rc_ui::signal_client::ConnectGrant>,
    force_relay: bool,
    encrypt_connection: bool,
) -> Result<PreparedSessionSecurity, String> {
    let lan_direct_addr = if !force_relay {
        relay_target_id.and_then(|did| {
            let lan = state.lan.lock().unwrap_or_else(|e| e.into_inner());
            lan.as_ref().and_then(|h| h.lookup_lan_addr(did))
        })
    } else {
        None
    };

    let owner_grant = relay_grant.and_then(|g| g.owner_grant.clone());
    let owner_signer = match owner_grant.as_ref() {
        Some(grant) => Some(Arc::new(
            state
                .auth
                .cert_manager()
                .create_owner_ticket_signer(grant.clone())
                .map_err(|e| format!("构造本人自控签名器失败：{e}"))?,
        ) as Arc<dyn OwnerTicketSigner>),
        None => None,
    };

    if encrypt_connection && relay_target_id.is_none() {
        return Err(
            "直连 IP 模式未知对端设备码，无法校验 mTLS 证书身份；请改用 9 位设备码连接，或关闭此会话的加密连接开关".to_string(),
        );
    }

    let expected_peer_user_id = if owner_grant.is_some() {
        state.auth.current_session().map(|s| s.user_id)
    } else {
        None
    };
    let mtls_config = build_session_mtls_bundle(
        state,
        relay_target_id.map(str::to_string),
        expected_peer_user_id,
    );

    if encrypt_connection && mtls_config.is_none() {
        return Err(
            "已开启加密连接 (mTLS)，但本机设备证书或服务器 Root CA 未就绪，请先在「设置 → 安全」续签设备证书".to_string(),
        );
    }

    Ok(PreparedSessionSecurity {
        mtls_config,
        owner_grant,
        owner_signer,
        lan_direct_addr,
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn resolve_connection_mode(
    force_relay: bool,
    requested: Option<&str>,
    persisted: ConnectionMode,
) -> ConnectionMode {
    if force_relay {
        return ConnectionMode::TcpRelay;
    }
    requested
        .and_then(ConnectionMode::parse_lossy)
        .unwrap_or(persisted)
}

/// 解析本次会话实际使用的连接路由模式：
/// - 若本次请求显式传了 `force_relay: true` 或有效的 `connection_mode`，优先采用本次显式模式；
/// - 否则使用全局持久化偏好 `persisted`，但若目标为纯 IP 直连（`ConnectTarget::Direct` 且无
///   `relay_target_id`）且全局偏好为 `TcpRelay`，本次自动回退为 `ConnectionMode::Auto`（允许直连，
///   不锁死纯 IP 且不覆盖全局偏好）。
fn resolve_connection_mode_for_target(
    force_relay: bool,
    requested: Option<&str>,
    persisted: ConnectionMode,
    parsed: &ConnectTarget,
    relay_target_id: Option<&str>,
) -> ConnectionMode {
    let explicit_mode = if force_relay {
        Some(ConnectionMode::TcpRelay)
    } else {
        requested.and_then(ConnectionMode::parse_lossy)
    };
    match explicit_mode {
        Some(mode) => mode,
        None => {
            if persisted == ConnectionMode::TcpRelay
                && matches!(parsed, ConnectTarget::Direct(_))
                && relay_target_id.is_none()
            {
                ConnectionMode::Auto
            } else {
                persisted
            }
        }
    }
}

fn connection_mode_label(mode: ConnectionMode) -> &'static str {
    match mode {
        ConnectionMode::Auto => "自动模式",
        ConnectionMode::P2pOnly => "仅 P2P",
        ConnectionMode::TcpRelay => "TCP Relay",
    }
}

fn effective_target_for_mode(
    parsed: ConnectTarget,
    relay_target_id: Option<&str>,
    mode: ConnectionMode,
) -> Result<ConnectTarget, String> {
    if mode == ConnectionMode::TcpRelay {
        match (&parsed, relay_target_id) {
            (ConnectTarget::DeviceId(_), _) => Ok(parsed),
            (ConnectTarget::Direct(_), Some(id)) => Ok(ConnectTarget::DeviceId(id.to_string())),
            (ConnectTarget::Direct(_), None) => Err(
                "直连地址未绑定 9 位设备码，无法强制开启 TCP 中继；请使用对方 9 位设备码连接"
                    .to_string(),
            ),
        }
    } else {
        Ok(parsed)
    }
}

fn is_same_session_target(
    active_meta: &ActiveSessionMeta,
    raw_target: &str,
    parsed: &ConnectTarget,
    device_key: &str,
    relay_target_id: Option<&str>,
) -> bool {
    let active_key = active_meta.device_key.trim();
    let next_key = device_key.trim();
    if !active_key.is_empty() && !next_key.is_empty() && active_key == next_key {
        return true;
    }
    let active_did = active_meta
        .relay_target_id
        .as_deref()
        .map(normalize_device_id_for_match)
        .filter(|s| !s.is_empty())
        .or_else(|| match model::parse_connect_input(&active_meta.target) {
            Ok(ConnectTarget::DeviceId(id)) => Some(normalize_device_id_for_match(&id)),
            _ => None,
        });
    let next_did = relay_target_id
        .map(normalize_device_id_for_match)
        .filter(|s| !s.is_empty())
        .or_else(|| match parsed {
            ConnectTarget::DeviceId(id) => Some(normalize_device_id_for_match(id)),
            ConnectTarget::Direct(_) => None,
        });
    if let (Some(a), Some(b)) = (active_did.as_deref(), next_did.as_deref()) {
        if a == b {
            return true;
        }
    }
    let active_target = active_meta.target.trim();
    let next_target = raw_target.trim();
    if !active_target.is_empty() && !next_target.is_empty() {
        if active_target == next_target {
            return true;
        }
        if let Ok(active_parsed) = model::parse_connect_input(active_target) {
            if active_parsed == *parsed {
                return true;
            }
        }
    }
    false
}

fn can_reuse_headless_session<H>(
    active: &ActiveSession<H>,
    raw_target: &str,
    parsed: &ConnectTarget,
    device_key: &str,
    relay_target_id: Option<&str>,
) -> bool {
    !active.disconnecting
        && !active.watch_only
        && is_same_session_target(
            &active.meta,
            raw_target,
            parsed,
            device_key,
            relay_target_id,
        )
}

fn save_last_session_params(state: &AppState, watch_only: bool, meta: ActiveSessionMeta) {
    *state
        .last_session_params
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some((watch_only, meta));
}

/// 信令/安全协商期 RAII 守卫：在异常早退、取消或完成后通过 CAS 仅清零自身代次的 `connecting_epoch`。
struct ConnectingEpochGuard<'a> {
    state: &'a AppState,
    epoch: u64,
}

impl<'a> ConnectingEpochGuard<'a> {
    fn begin(state: &'a AppState, epoch: u64) -> Self {
        state.connecting_epoch.store(epoch, Ordering::Release);
        Self { state, epoch }
    }
}

impl Drop for ConnectingEpochGuard<'_> {
    fn drop(&mut self) {
        self.state.clear_connecting_epoch(self.epoch);
    }
}

fn cancel_auto_reconnect(state: &AppState) {
    // 仅重置重试计数并唤醒睡眠，让已过期的旧任务根据调用方递增的 `session_open_epoch` 自行退出并 CAS 释放自身令牌，
    // 绝不强行 `store(0)` 覆盖可能并发启动的后代重连任务令牌。
    state.auto_reconnect_attempts.store(0, Ordering::Release);
    state.wake_auto_reconnect();
}

/// IPC 命令：打开远控会话（支持纯工具同目标免拆复用、纯工具原地热升级桌面、三档连接模式与失败常驻绝不关窗）。
#[tauri::command]
pub async fn open_session(
    cfg: OpenSessionRequest,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.require_unlocked()?;
    state.require_authenticated()?;

    let parsed = model::parse_connect_input(&cfg.target)?;
    let (device_key, device_name, relay_target_id) =
        resolve_session_target_meta(&state, &cfg.target, &parsed, cfg.device_name.as_deref());

    // 在拆旧会话前优先判定同目标复用或纯工具原地热升级，避免工具秒开窗竞态或重复建连。
    enum ReuseDecision {
        None,
        ReuseHeadless,
        WakeReconnectingHeadless,
        UpgradeToDesktop {
            sid: u64,
            device_name: String,
            prefer_hardware: bool,
            video_size: Option<(u32, u32)>,
        },
    }

    let reuse = {
        let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = slot.as_ref() {
            if cfg.headless
                && can_reuse_headless_session(
                    active,
                    &cfg.target,
                    &parsed,
                    &device_key,
                    relay_target_id.as_deref(),
                )
            {
                ReuseDecision::ReuseHeadless
            } else if !cfg.headless
                && !active.disconnecting
                && !active.meta.media_active
                && is_same_session_target(
                    &active.meta,
                    &cfg.target,
                    &parsed,
                    &device_key,
                    relay_target_id.as_deref(),
                )
            {
                ReuseDecision::UpgradeToDesktop {
                    sid: active.id,
                    device_name: if !active.meta.device_name.is_empty() {
                        active.meta.device_name.clone()
                    } else {
                        device_name.clone()
                    },
                    prefer_hardware: active.meta.prefer_hardware,
                    video_size: active.video_size,
                }
            } else {
                ReuseDecision::None
            }
        } else if cfg.headless && state.is_auto_reconnect_active() {
            let matches_reconnecting = state
                .last_session_params
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|(watch_only, meta)| {
                    !*watch_only
                        && is_same_session_target(
                            meta,
                            &cfg.target,
                            &parsed,
                            &device_key,
                            relay_target_id.as_deref(),
                        )
                });
            if matches_reconnecting {
                ReuseDecision::WakeReconnectingHeadless
            } else {
                ReuseDecision::None
            }
        } else if cfg.headless && state.is_connecting_current_epoch() {
            let matches_connecting = state
                .last_session_params
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|(watch_only, meta)| {
                    !*watch_only
                        && is_same_session_target(
                            meta,
                            &cfg.target,
                            &parsed,
                            &device_key,
                            relay_target_id.as_deref(),
                        )
                });
            if matches_connecting {
                ReuseDecision::ReuseHeadless
            } else {
                ReuseDecision::None
            }
        } else {
            ReuseDecision::None
        }
    };

    match reuse {
        ReuseDecision::ReuseHeadless => return Ok(()),
        ReuseDecision::WakeReconnectingHeadless => {
            state.wake_auto_reconnect();
            return Ok(());
        }
        ReuseDecision::UpgradeToDesktop {
            sid,
            device_name: upgrade_name,
            prefer_hardware,
            video_size,
        } => {
            window::ensure_session_window(&app, &state, cfg.watch_only)?;
            window::set_session_window_title(&app, &upgrade_name);
            if let Some((vw, vh)) = video_size {
                if vw > 0 && vh > 0 {
                    let _ = window::fit_session_window_to_video(&app, vw, vh, false);
                }
            }
            let preallocated = preallocate_metal_layer_for_session(&app, prefer_hardware)
                .map(metal_attach::MainThreadMetalLayer::into_render_surface);
            let still_refine_cmd = still_refinement_command_for_desktop_open(&state, false);
            let upgrade_result = {
                let mut slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(active) = slot
                    .as_mut()
                    .filter(|a| a.id == sid && !a.disconnecting && !a.meta.media_active)
                {
                    if active.watch_only != cfg.watch_only {
                        active.watch_only = cfg.watch_only;
                        active.handle.set_watch_only(cfg.watch_only);
                    }
                    let tools_to_detach = cfg.watch_only.then(|| active.handle.tools());
                    if let Some(cmd) = still_refine_cmd {
                        active.handle.send(cmd);
                    }
                    active.handle.upgrade_to_desktop(preallocated);
                    if active.meta.audio_enabled {
                        active
                            .handle
                            .send(SessionCommand::SetAudioMuted { muted: false });
                    }
                    active.handle.send_display_ctrl(
                        rc_client_core::display_ctrl::DisplayControlMsg::ListDisplays,
                    );
                    active.meta.media_active = true;
                    active.meta.tools_only = false;
                    save_last_session_params(&state, active.watch_only, active.meta.clone());
                    let fallback = Frame {
                        x: 0.0,
                        y: 0.0,
                        width: f64::from(active.meta.width.max(1)),
                        height: f64::from(active.meta.height.max(1)),
                    };
                    let attach_req = active
                        .handle
                        .layer_lease()
                        .and_then(|lease| active.begin_attach(lease, fallback));
                    Some((active.info(), attach_req, tools_to_detach))
                } else {
                    None
                }
            };
            if let Some((info, attach_req, tools_to_detach)) = upgrade_result {
                if let Some(tools) = tools_to_detach {
                    let _ = tools.detach_all_terminals();
                }
                let _ = app.emit("session://active-state", &info);
                if let Some((attach_sid, lease, frame)) = attach_req {
                    crate::attach_layer_on_main_thread(&app, attach_sid, lease, frame);
                }
                let _ = sync_shortcut_tap_runtime(&app, &state);
                return Ok(());
            }
            // 若并发期间该纯工具会话恰好断开或已被替换，落回正常建连路径。
        }
        ReuseDecision::None => {}
    }

    let prefs_snap = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let connection_mode = resolve_connection_mode_for_target(
        cfg.force_relay,
        cfg.connection_mode.as_deref(),
        prefs_snap.connection_mode,
        &parsed,
        relay_target_id.as_deref(),
    );
    let force_relay = connection_mode == ConnectionMode::TcpRelay;
    let effective_target =
        effective_target_for_mode(parsed, relay_target_id.as_deref(), connection_mode)?;

    let (req_width, req_height) = match (cfg.width, cfg.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
        _ => (DEFAULT_SESSION_WIDTH, DEFAULT_SESSION_HEIGHT),
    };
    let req_fps = cfg.fps.unwrap_or(prefs_snap.fps).max(1);
    let req_bitrate_bps = cfg.bitrate_bps.unwrap_or(prefs_snap.bitrate_bps).max(1);
    let password = cfg.password.filter(|p| !p.is_empty());
    let encrypt_connection = cfg
        .encrypt_connection
        .unwrap_or(prefs_snap.default_encrypt_connection);

    let mut meta = ActiveSessionMeta {
        target: cfg.target.trim().to_string(),
        device_key,
        device_name: device_name.clone(),
        relay_target_id: relay_target_id.clone(),
        password: password.clone(),
        width: req_width,
        height: req_height,
        fps: req_fps,
        bitrate_bps: req_bitrate_bps,
        prefer_hardware: cfg.prefer_hardware,
        clipboard_enabled: cfg.clipboard_enabled,
        audio_enabled: cfg.audio_enabled,
        force_relay,
        connection_mode,
        media_active: !cfg.headless,
        phase: "connecting".to_string(),
        tools_only: cfg.headless,
        encrypt_connection,
        mtls_active: false,
        peer_cert_fingerprint: None,
        connected: false,
        transport: None,
        tool_permissions: state::ToolPermissionsSnapshot::default(),
    };

    // H9：在打开窗口与 `request_connect` 之前递增代次并取消任何在途自动重连；
    // 同时立即落盘 `last_session_params` 并置位 `connecting_epoch`，确保秒开工具窗与晚加载 S1 都能读到协商态，
    // 且即便后续信令或安全准备失败，用户在失败面板切换模式/立即重试仍能拿到目标。
    let my_epoch = state.session_open_epoch.fetch_add(1, Ordering::AcqRel) + 1;
    cancel_auto_reconnect(&state);
    state.clear_terminal_error();
    save_last_session_params(&state, cfg.watch_only, meta.clone());
    let _connecting_guard = ConnectingEpochGuard::begin(&state, my_epoch);

    // 先回收旧会话并推送初始 Connecting 状态，再立即创建/展示 S1 窗口，使控制界面秒开。
    let previous = {
        let mut slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        state::clear_session_snapshot_locked(&state.last_session_update);
        *state.host_version.lock().unwrap_or_else(|e| e.into_inner()) = None;
        slot.take()
    };
    if let Some(old) = previous {
        let _ = old.handle.tools().detach_all_terminals();
        let _ = app.emit("portmap://update", &Vec::<PortMapRuleSnapshot>::new());
        let _ = app.emit("session://active-state", &ActiveSessionInfo::inactive());
        teardown_session_and_wait(&app, old).await;
    }

    let preallocated_layer = if !cfg.headless {
        window::ensure_session_window(&app, &state, cfg.watch_only)?;
        window::set_session_window_title(&app, &device_name);
        let _ = app.emit(
            "session://update",
            &SessionUpdate::Connecting {
                phase: rc_ui::session_bridge::ConnectPhase::EndpointCreating,
                watch_only: cfg.watch_only,
            },
        );
        preallocate_metal_layer_for_session(&app, cfg.prefer_hardware)
    } else {
        None
    };

    // 推送初始 connecting 快照，供秒开工具窗或主窗展示「正在建立安全连接…」。
    let connecting_info = ActiveSessionInfo::connecting_from_meta(cfg.watch_only, &meta);
    let _ = app.emit("session://active-state", &connecting_info);

    let (connect_addr, relay_grant) =
        match resolve_endpoint_and_grant(&state, effective_target, force_relay, Some(my_epoch))
            .await
        {
            Ok(res) => res,
            Err(err) => {
                state.clear_connecting_epoch(my_epoch);
                if state.session_open_epoch.load(Ordering::Acquire) == my_epoch {
                    let ended = session_ended_with_error(err.clone(), false);
                    let ui_err = match &ended {
                        SessionUpdate::Ended { ui_error, .. } => ui_error.clone(),
                        _ => None,
                    };
                    state.record_terminal_error_with_ui_if_epoch(my_epoch, err.clone(), ui_err);
                    let _ = app.emit("session://active-state", &ActiveSessionInfo::inactive());
                    // 铁律：连接失败绝不自动关闭或销毁 S1 远控窗口，仅推送不可恢复终态由界面常驻展示错误。
                    let _ = app.emit("session://update", &ended);
                }
                return Err(err);
            }
        };

    // 若等待 `request_connect` 期间已被更新的 `open_session` 或 `disconnect_active_session` 取代，静默丢弃。
    if state.session_open_epoch.load(Ordering::Acquire) != my_epoch {
        return Ok(());
    }

    let relay_target_for_sec = relay_target_id.clone();
    let grant_for_sec = relay_grant.clone();
    let app_sec = app.clone();
    let sec_res = tauri::async_runtime::spawn_blocking(move || {
        let st_sec = app_sec.state::<AppState>();
        prepare_session_security(
            &st_sec,
            relay_target_for_sec.as_deref(),
            grant_for_sec.as_ref(),
            force_relay,
            encrypt_connection,
        )
    })
    .await
    .map_err(|e| format!("准备会话安全配置异常：{e}"))
    .and_then(|res| res);
    let security = match sec_res {
        Ok(sec) => sec,
        Err(err) => {
            state.clear_connecting_epoch(my_epoch);
            if state.session_open_epoch.load(Ordering::Acquire) == my_epoch {
                let ended = session_ended_with_error(err.clone(), false);
                let ui_err = match &ended {
                    SessionUpdate::Ended { ui_error, .. } => ui_error.clone(),
                    _ => None,
                };
                state.record_terminal_error_with_ui_if_epoch(my_epoch, err.clone(), ui_err);
                let _ = app.emit("session://active-state", &ActiveSessionInfo::inactive());
                // 铁律：安全配置失败同样绝不自动销毁 S1 远控窗口。
                let _ = app.emit("session://update", &ended);
            }
            return Err(err);
        }
    };

    let upgraded_to_desktop_while_connecting =
        cfg.headless && app.get_webview_window("session").is_some();
    let effective_headless = cfg.headless && !upgraded_to_desktop_while_connecting;
    if upgraded_to_desktop_while_connecting {
        meta.tools_only = false;
        meta.media_active = true;
    }

    let signal_client = state
        .server
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .signal_client
        .clone();
    let ui_cfg = UiSessionConfig {
        connect_addr,
        password,
        watch_only: cfg.watch_only,
        width: req_width,
        height: req_height,
        fps: req_fps,
        bitrate_bps: req_bitrate_bps,
        prefer_hardware: cfg.prefer_hardware,
        clipboard_enabled: cfg.clipboard_enabled,
        audio_enabled: if effective_headless {
            false
        } else {
            cfg.audio_enabled
        },
        relay_grant,
        force_relay,
        connection_mode,
        tools_only: effective_headless,
        encrypt_connection,
        mtls_config: security.mtls_config,
        owner_grant: security.owner_grant,
        owner_signer: security.owner_signer,
        lan_direct_addr: security.lan_direct_addr,
        signal_client,
        target_device_id: relay_target_id,
    };

    let preallocated_override = if upgraded_to_desktop_while_connecting {
        None
    } else {
        Some(preallocated_layer)
    };

    activate_session_with_cfg(
        &app,
        &state,
        my_epoch,
        ui_cfg,
        cfg.watch_only,
        meta,
        preallocated_override,
    )
}

/// IPC 命令：切换连接路由模式（`"auto"` / `"p2p_only"` / `"tcp_relay"`），持久化偏好并立即按新模式重连；
/// 当前无活动会话（如刚连接失败）时自动从 `last_session_params` 取回当前目标参数，绝不因失败关闭窗口。
#[tauri::command]
pub async fn set_session_connection_mode(
    mode: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    state.require_unlocked()?;
    state.require_authenticated()?;

    let parsed_mode = ConnectionMode::parse_lossy(&mode)
        .ok_or_else(|| format!("无效的连接模式：{mode}（仅支持 auto / p2p_only / tcp_relay）"))?;

    let (watch_only, mut meta) = {
        let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = slot.as_ref().filter(|a| !a.disconnecting) {
            (active.watch_only, active.meta.clone())
        } else {
            drop(slot);
            let saved = state
                .last_session_params
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            saved.ok_or_else(|| "当前无活动或可重连的远控会话".to_string())?
        }
    };
    if meta.target.trim().is_empty() {
        return Err("当前会话缺少连接目标，无法切换连接模式".to_string());
    }

    let parsed = model::parse_connect_input(&meta.target)?;
    if meta.relay_target_id.is_none() {
        let (_, _, found_relay_id) =
            resolve_session_target_meta(&state, &meta.target, &parsed, Some(&meta.device_name));
        meta.relay_target_id = found_relay_id;
    }

    let force_relay = parsed_mode == ConnectionMode::TcpRelay;
    let effective_target =
        effective_target_for_mode(parsed, meta.relay_target_id.as_deref(), parsed_mode)?;

    // 目标与路由模式校验全部通过后再落盘持久化偏好，防止纯 IP 会话误点 TCP 中继污染全局偏好。
    let app_persist = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let st = app_persist.state::<AppState>();
        persist_prefs_mutation(&st, &app_prefs::default_prefs_path(), |prefs| {
            prefs.connection_mode = parsed_mode;
        })?;
        notify_background_services_reload(&st, false);
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| format!("保存连接模式偏好任务异常：{e}"))??;

    let my_epoch = state.session_open_epoch.fetch_add(1, Ordering::AcqRel) + 1;
    cancel_auto_reconnect(&state);
    state.clear_terminal_error();

    let prefs_snap = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    meta.fps = prefs_snap.fps.max(1);
    meta.bitrate_bps = prefs_snap.bitrate_bps.max(1);
    meta.connection_mode = parsed_mode;
    meta.force_relay = force_relay;
    meta.phase = "connecting".to_string();
    meta.mtls_active = false;
    meta.peer_cert_fingerprint = None;
    meta.connected = false;
    meta.transport = None;
    meta.tool_permissions = state::ToolPermissionsSnapshot::default();
    save_last_session_params(&state, watch_only, meta.clone());
    let _connecting_guard = ConnectingEpochGuard::begin(&state, my_epoch);

    // 切换传输模式前先断开并回收旧会话（有界 300ms 非阻塞），避免旧连接与新配对竞争。
    let previous = {
        let mut slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        state::clear_session_snapshot_locked(&state.last_session_update);
        *state.host_version.lock().unwrap_or_else(|e| e.into_inner()) = None;
        slot.take()
    };
    if let Some(old) = previous {
        let _ = old.handle.tools().detach_all_terminals();
        let _ = app.emit("portmap://update", &Vec::<PortMapRuleSnapshot>::new());
        teardown_session_and_wait(&app, old).await;
    }
    let _ = app.emit(
        "session://active-state",
        &ActiveSessionInfo::connecting_from_meta(watch_only, &meta),
    );
    let _ = app.emit(
        "session://update",
        &SessionUpdate::Connecting {
            phase: rc_ui::session_bridge::ConnectPhase::Connecting,
            watch_only,
        },
    );

    let (connect_addr, relay_grant) =
        match resolve_endpoint_and_grant(&state, effective_target, force_relay, Some(my_epoch))
            .await
        {
            Ok(res) => res,
            Err(err) => {
                state.clear_connecting_epoch(my_epoch);
                if state.session_open_epoch.load(Ordering::Acquire) == my_epoch {
                    let ended = session_ended_with_error(err.clone(), false);
                    let ui_err = match &ended {
                        SessionUpdate::Ended { ui_error, .. } => ui_error.clone(),
                        _ => None,
                    };
                    state.record_terminal_error_with_ui_if_epoch(my_epoch, err.clone(), ui_err);
                    let _ = app.emit("session://active-state", &ActiveSessionInfo::inactive());
                    let _ = app.emit("session://update", &ended);
                }
                return Err(err);
            }
        };
    if state.session_open_epoch.load(Ordering::Acquire) != my_epoch {
        return Err("会话状态已变更，请重试".to_string());
    }

    let relay_target_for_sec = meta.relay_target_id.clone();
    let grant_for_sec = relay_grant.clone();
    let encrypt_conn = meta.encrypt_connection;
    let app_sec = app.clone();
    let sec_res = tauri::async_runtime::spawn_blocking(move || {
        let st_sec = app_sec.state::<AppState>();
        prepare_session_security(
            &st_sec,
            relay_target_for_sec.as_deref(),
            grant_for_sec.as_ref(),
            force_relay,
            encrypt_conn,
        )
    })
    .await
    .map_err(|e| format!("准备会话安全配置异常：{e}"))
    .and_then(|res| res);
    let security = match sec_res {
        Ok(sec) => sec,
        Err(err) => {
            state.clear_connecting_epoch(my_epoch);
            if state.session_open_epoch.load(Ordering::Acquire) == my_epoch {
                let ended = session_ended_with_error(err.clone(), false);
                let ui_err = match &ended {
                    SessionUpdate::Ended { ui_error, .. } => ui_error.clone(),
                    _ => None,
                };
                state.record_terminal_error_with_ui_if_epoch(my_epoch, err.clone(), ui_err);
                let _ = app.emit("session://active-state", &ActiveSessionInfo::inactive());
                let _ = app.emit("session://update", &ended);
            }
            return Err(err);
        }
    };

    if meta.tools_only && app.get_webview_window("session").is_some() {
        meta.tools_only = false;
        meta.media_active = true;
    }

    let (req_width, req_height) = if meta.width > 0 && meta.height > 0 {
        (meta.width, meta.height)
    } else {
        (DEFAULT_SESSION_WIDTH, DEFAULT_SESSION_HEIGHT)
    };
    let signal_client = state
        .server
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .signal_client
        .clone();
    let ui_cfg = UiSessionConfig {
        connect_addr,
        password: meta.password.clone(),
        watch_only,
        width: req_width,
        height: req_height,
        fps: meta.fps,
        bitrate_bps: meta.bitrate_bps,
        prefer_hardware: meta.prefer_hardware,
        clipboard_enabled: meta.clipboard_enabled,
        audio_enabled: if meta.tools_only {
            false
        } else {
            meta.audio_enabled
        },
        relay_grant,
        force_relay,
        connection_mode: parsed_mode,
        tools_only: meta.tools_only,
        encrypt_connection: meta.encrypt_connection,
        mtls_config: security.mtls_config,
        owner_grant: security.owner_grant,
        owner_signer: security.owner_signer,
        lan_direct_addr: security.lan_direct_addr,
        signal_client,
        target_device_id: meta.relay_target_id.clone(),
    };

    activate_session_with_cfg(&app, &state, my_epoch, ui_cfg, watch_only, meta, None)?;
    Ok(connection_mode_label(parsed_mode).to_string())
}

/// IPC 命令：兼容旧版「使用安全中转连接」布尔开关，内部转发至 [`set_session_connection_mode`]。
#[tauri::command]
pub async fn set_session_relay_mode(
    force_relay: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let mode = if force_relay {
        ConnectionMode::TcpRelay
    } else {
        ConnectionMode::Auto
    };
    set_session_connection_mode(mode.as_str().to_string(), app, state).await
}

/// IPC 命令：在当前活动会话中动态切换观看模式（只读禁止本地输入注入），并向主窗推送状态快照。
#[tauri::command]
pub fn set_session_watch_only(
    enabled: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ActiveSessionInfo, String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let (info, tools_to_detach, restore_req) = {
        let mut slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        let Some(active) = slot.as_mut().filter(|a| !a.disconnecting) else {
            return Err("当前无活动远控会话".to_string());
        };
        active.watch_only = enabled;
        active.handle.set_watch_only(enabled);
        let tools_to_detach = enabled.then(|| active.handle.tools());
        let restore_req = if !enabled && active.meta.connected && !active.portmaps_restored {
            active.portmaps_restored = true;
            let raw_key = if !active.meta.device_key.trim().is_empty() {
                active.meta.device_key.trim()
            } else {
                active.meta.target.trim()
            };
            (!raw_key.is_empty()).then(|| (active.id, raw_key.to_string()))
        } else {
            None
        };
        let active_info = active.info();
        *state
            .last_session_params
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((enabled, active.meta.clone()));
        (active_info, tools_to_detach, restore_req)
    };
    if let Some(tools) = tools_to_detach {
        let _ = tools.detach_all_terminals();
    }
    if let Some((sid, restore_key)) = restore_req {
        let app_handle = app.clone();
        let _ = std::thread::Builder::new()
            .name("rc-portmap-restore".to_string())
            .spawn(move || {
                let st = app_handle.state::<AppState>();
                restore_saved_portmaps_for_active_session(&st, sid, &restore_key);
            });
    }
    let _ = sync_shortcut_tap_runtime(&app, &state);
    let _ = app.emit("session://active-state", &info);
    Ok(info)
}

/// IPC 命令：在当前活动会话中动态切换「加密连接 (mTLS)」模式并触发安全握手重建会话。
#[tauri::command]
pub async fn set_session_mtls_mode(
    enabled: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ActiveSessionInfo, String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let (watch_only, mut meta) = {
        let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        let Some(active) = slot.as_ref().filter(|a| !a.disconnecting) else {
            return Err("当前无活动远控会话".to_string());
        };
        (active.watch_only, active.meta.clone())
    };
    if meta.target.trim().is_empty() {
        return Err("当前会话缺少连接目标，无法切换加密连接模式".to_string());
    }

    let parsed = model::parse_connect_input(&meta.target)?;
    if meta.relay_target_id.is_none() {
        let (_, _, found_relay_id) =
            resolve_session_target_meta(&state, &meta.target, &parsed, Some(&meta.device_name));
        meta.relay_target_id = found_relay_id;
    }

    let effective_target = if meta.force_relay {
        match (&parsed, &meta.relay_target_id) {
            (ConnectTarget::DeviceId(_), _) => parsed,
            (ConnectTarget::Direct(_), Some(id)) => ConnectTarget::DeviceId(id.clone()),
            (ConnectTarget::Direct(_), None) => {
                return Err("当前直连会话未绑定 9 位设备码，无法在强制中转模式下重建".to_string());
            }
        }
    } else {
        parsed
    };

    // 若开启 mTLS 但本机证书或 Root CA 缺失，在断开旧会话之前提前拦截（零静默降级）。
    if enabled && build_session_mtls_bundle(&state, meta.relay_target_id.clone(), None).is_none() {
        return Err(
            "已开启加密连接 (mTLS)，但本机设备证书或服务器 Root CA 未就绪，请先在「设置 → 安全」续签设备证书".to_string(),
        );
    }

    let my_epoch = state.session_open_epoch.fetch_add(1, Ordering::AcqRel) + 1;
    cancel_auto_reconnect(&state);
    state.clear_terminal_error();
    let prefs_snap = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    meta.fps = prefs_snap.fps.max(1);
    meta.bitrate_bps = prefs_snap.bitrate_bps.max(1);
    meta.encrypt_connection = enabled;
    meta.phase = "connecting".to_string();
    meta.mtls_active = false;
    meta.peer_cert_fingerprint = None;
    meta.connected = false;
    meta.transport = None;
    meta.tool_permissions = state::ToolPermissionsSnapshot::default();
    save_last_session_params(&state, watch_only, meta.clone());
    let _connecting_guard = ConnectingEpochGuard::begin(&state, my_epoch);

    let previous = {
        let mut slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        state::clear_session_snapshot_locked(&state.last_session_update);
        *state.host_version.lock().unwrap_or_else(|e| e.into_inner()) = None;
        slot.take()
    };
    if let Some(old) = previous {
        let _ = old.handle.tools().detach_all_terminals();
        let _ = app.emit("portmap://update", &Vec::<PortMapRuleSnapshot>::new());
        teardown_session_and_wait(&app, old).await;
    }
    let _ = app.emit(
        "session://active-state",
        &ActiveSessionInfo::connecting_from_meta(watch_only, &meta),
    );
    let _ = app.emit(
        "session://update",
        &SessionUpdate::Connecting {
            phase: rc_ui::session_bridge::ConnectPhase::Connecting,
            watch_only,
        },
    );

    let (connect_addr, relay_grant) = match resolve_endpoint_and_grant(
        &state,
        effective_target,
        meta.force_relay,
        Some(my_epoch),
    )
    .await
    {
        Ok(res) => res,
        Err(err) => {
            state.clear_connecting_epoch(my_epoch);
            if state.session_open_epoch.load(Ordering::Acquire) == my_epoch {
                let ended = session_ended_with_error(err.clone(), false);
                let ui_err = match &ended {
                    SessionUpdate::Ended { ui_error, .. } => ui_error.clone(),
                    _ => None,
                };
                state.record_terminal_error_with_ui_if_epoch(my_epoch, err.clone(), ui_err);
                let _ = app.emit("session://active-state", &ActiveSessionInfo::inactive());
                let _ = app.emit("session://update", &ended);
            }
            return Err(err);
        }
    };
    if state.session_open_epoch.load(Ordering::Acquire) != my_epoch {
        return Err("会话状态已变更，请重试".to_string());
    }

    let security = match prepare_session_security(
        &state,
        meta.relay_target_id.as_deref(),
        relay_grant.as_ref(),
        meta.force_relay,
        enabled,
    ) {
        Ok(sec) => sec,
        Err(err) => {
            state.clear_connecting_epoch(my_epoch);
            if state.session_open_epoch.load(Ordering::Acquire) == my_epoch {
                let ended = session_ended_with_error(err.clone(), false);
                let ui_err = match &ended {
                    SessionUpdate::Ended { ui_error, .. } => ui_error.clone(),
                    _ => None,
                };
                state.record_terminal_error_with_ui_if_epoch(my_epoch, err.clone(), ui_err);
                let _ = app.emit("session://active-state", &ActiveSessionInfo::inactive());
                let _ = app.emit("session://update", &ended);
            }
            return Err(err);
        }
    };

    let (req_width, req_height) = if meta.width > 0 && meta.height > 0 {
        (meta.width, meta.height)
    } else {
        (DEFAULT_SESSION_WIDTH, DEFAULT_SESSION_HEIGHT)
    };
    let signal_client = state
        .server
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .signal_client
        .clone();
    let ui_cfg = UiSessionConfig {
        connect_addr,
        password: meta.password.clone(),
        watch_only,
        width: req_width,
        height: req_height,
        fps: meta.fps,
        bitrate_bps: meta.bitrate_bps,
        prefer_hardware: meta.prefer_hardware,
        clipboard_enabled: meta.clipboard_enabled,
        audio_enabled: if meta.tools_only {
            false
        } else {
            meta.audio_enabled
        },
        relay_grant,
        force_relay: meta.force_relay,
        connection_mode: meta.connection_mode,
        tools_only: meta.tools_only,
        encrypt_connection: enabled,
        mtls_config: security.mtls_config,
        owner_grant: security.owner_grant,
        owner_signer: security.owner_signer,
        lan_direct_addr: security.lan_direct_addr,
        signal_client,
        target_device_id: meta.relay_target_id.clone(),
    };

    activate_session_with_cfg(&app, &state, my_epoch, ui_cfg, watch_only, meta, None)?;
    Ok(state.active_session_info())
}

/// 纯状态读取 S1 窗口初始化所需的会话快照与被控端锁屏状态（便于离线单测）。
///
/// 当活动会话快照为空且无正在运行的未断开会话时，按优先级回退补发：
/// 1) 当前代次正在自动重连 → `SessionUpdate::Reconnecting`；
/// 2) 当前代次正在信令/安全协商 → `SessionUpdate::Connecting`；
/// 3) 当前代次存在不可恢复终态错误 → `SessionUpdate::Ended { recoverable: false }`。
pub(crate) fn resolve_session_snapshot(
    state: &AppState,
) -> (Option<SessionUpdate>, Option<(bool, bool)>) {
    let (snap, cached_screen_state) = state::current_session_snapshot_and_screen_state(
        &state.session,
        &state.last_session_update,
    );
    if snap.is_some() {
        return (snap, cached_screen_state);
    }
    let has_live_session = {
        let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref().is_some_and(|a| !a.disconnecting)
    };
    if has_live_session {
        return (None, cached_screen_state);
    }
    let cur_epoch = state.session_open_epoch.load(Ordering::Acquire);
    if state.is_auto_reconnect_active() {
        let (attempt, reason) = state.current_auto_reconnect_status(AUTO_RECONNECT_MAX_ATTEMPTS);
        return (
            Some(SessionUpdate::Reconnecting {
                attempt,
                max_attempts: AUTO_RECONNECT_MAX_ATTEMPTS,
                reason,
            }),
            None,
        );
    }
    if state.is_connecting_current_epoch() {
        let watch_only = state
            .last_session_params
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|(w, _)| *w)
            .unwrap_or(false);
        return (
            Some(SessionUpdate::Connecting {
                phase: rc_ui::session_bridge::ConnectPhase::Connecting,
                watch_only,
            }),
            None,
        );
    }
    if let Some((err_epoch, reason, stored_ui_error)) = state
        .last_terminal_error
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        if err_epoch == cur_epoch {
            let ui_error =
                stored_ui_error.or_else(|| Some(classify_macos_session_or_signal_error(&reason)));
            return (
                Some(SessionUpdate::Ended {
                    reason,
                    recoverable: false,
                    ui_error,
                }),
                None,
            );
        }
    }
    (None, cached_screen_state)
}

/// IPC 命令：拉取最近一次会话状态快照（Review M8 + U-H2 / U-H3：
/// 在单次 `session` 锁内校验 `!disconnecting` 与匹配 `sid`，同时原子取出缓存的被控端锁屏/挂起状态；
/// 若处于锁屏或采集挂起态，则向 S1 窗口补发一次 `host_screen_state`；
/// 若当前活动会话有工具权限限制，亦补发一次 `tool_permissions_updated`；
/// 若当前无活动会话但处于重连中 / 协商中 / 当前代次终态失败，则补发对应状态避免 S1 永久转圈）。
#[tauri::command]
pub fn session_snapshot(app: AppHandle, state: State<'_, AppState>) -> Option<SessionUpdate> {
    let (snap, cached_screen_state) = resolve_session_snapshot(&state);
    if let Some((locked, capture_suspended)) = cached_screen_state {
        let _ = app.emit_to(
            "session",
            "session://update",
            &SessionUpdate::HostScreenState {
                locked,
                capture_suspended,
            },
        );
    }
    let active_perms = {
        let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref()
            .filter(|a| !a.disconnecting)
            .map(|a| a.meta.tool_permissions)
    };
    if let Some(perms) = active_perms {
        if !perms.allow_file_transfer || !perms.allow_terminal || !perms.allow_port_map {
            let _ = app.emit_to(
                "session",
                "session://update",
                &SessionUpdate::ToolPermissionsUpdated(rc_session::ToolPermissions {
                    allow_file_transfer: perms.allow_file_transfer,
                    allow_terminal: perms.allow_terminal,
                    allow_port_map: perms.allow_port_map,
                }),
            );
        }
    }
    snap
}

/// `SessionCommand::Disconnect` 原子前置状态收敛结果（便于离线单测）。
enum DisconnectCommandOutcome {
    /// 槽位为空（例如处于自动重连退避期或信令协商等待期），需立即广播 `inactive` + `Ended`。
    EmptySlot,
    /// 槽位中会话已在断开中（幂等忽略）。
    AlreadyDisconnecting,
    /// 已向无图层的活动会话发送 `Disconnect`。
    SentNow,
    /// 活动会话持有 Metal 图层租约，需先在主线程摘除再按 `sid` 发送 `Disconnect`。
    NeedsMainThreadDetach(u64, rc_ui::session_bridge::LayerLease),
}

fn prepare_disconnect_command(state: &AppState, lock_on_exit: bool) -> DisconnectCommandOutcome {
    let mut slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
    let _new_epoch = state.session_open_epoch.fetch_add(1, Ordering::AcqRel) + 1;
    state.connecting_epoch.store(0, Ordering::Release);
    state.clear_terminal_error();
    cancel_auto_reconnect(state);

    let Some(active) = slot.as_mut() else {
        state::clear_session_snapshot_locked(&state.last_session_update);
        return DisconnectCommandOutcome::EmptySlot;
    };
    if active.disconnecting {
        return DisconnectCommandOutcome::AlreadyDisconnecting;
    }
    active.disconnecting = true;
    if let Some(lease) = active.layer_state.take_lease() {
        DisconnectCommandOutcome::NeedsMainThreadDetach(active.id, lease)
    } else {
        if lock_on_exit {
            active.handle.request_lock_on_disconnect();
        }
        active.handle.send(SessionCommand::Disconnect);
        DisconnectCommandOutcome::SentNow
    }
}

/// 兼容 `{ "type": "..." }`（规范）与 `{ "kind": "..." }`（历史别名）的会话控制命令反序列化包装。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionCommandArg(pub SessionCommand);

impl<'de> serde::Deserialize<'de> for SessionCommandArg {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut val = serde_json::Value::deserialize(deserializer)?;
        if let Some(obj) = val.as_object_mut() {
            // L5：若 `"type"` 缺失或为 `null`，回退采用 `"kind"` 作为标签字段。
            if !obj.get("type").is_some_and(|v| !v.is_null()) {
                if let Some(kind) = obj.remove("kind") {
                    obj.insert("type".to_string(), kind);
                }
            }
        }
        serde_json::from_value(val)
            .map(SessionCommandArg)
            .map_err(serde::de::Error::custom)
    }
}

/// IPC 命令：转发低频控制命令（`Disconnect` / `SetBitrate` / `StageRect` /
/// `SetClipboardEnabled` / `SetAudioMuted`）。
///
/// 收到 `Disconnect` 时先在主线程摘除已挂载的 `CAMetalLayer`，再发送 `Disconnect`
/// 让会话线程走 `Bye` 优雅收尾并经轮询器推送 `Ended`（T21 U10 / 坑 2）。
#[tauri::command]
pub fn session_command(
    cmd: SessionCommandArg,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let cmd = cmd.0;
    match cmd {
        SessionCommand::Disconnect => {
            stop_shortcut_tap_runtime(&state);
            let lock_on_exit = state
                .prefs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .lock_on_exit;
            match prepare_disconnect_command(&state, lock_on_exit) {
                DisconnectCommandOutcome::EmptySlot => {
                    let _ = app.emit("session://active-state", &ActiveSessionInfo::inactive());
                    let _ = app.emit(
                        "session://update",
                        &session_ended_user_cancelled("已主动断开远控会话"),
                    );
                }
                DisconnectCommandOutcome::NeedsMainThreadDetach(sid, lease) => {
                    let app_clone = app.clone();
                    let _ = app.run_on_main_thread(move || {
                        metal_attach::detach_leased_layer(&lease);
                        let st = app_clone.state::<AppState>();
                        let _ = state::finish_disconnect_on_main_thread(&st.session, sid, |h| {
                            if lock_on_exit {
                                h.request_lock_on_disconnect();
                            }
                            h.send(SessionCommand::Disconnect);
                        });
                        drop(lease);
                    });
                }
                DisconnectCommandOutcome::SentNow
                | DisconnectCommandOutcome::AlreadyDisconnecting => {}
            }
        }
        other => {
            let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(active) = slot.as_ref() {
                if !active.disconnecting {
                    active.handle.send(other);
                }
            }
        }
    }
    Ok(())
}

/// IPC 命令：WebView DOM 输入事件 → `input_bridge` 映射 → `SessionHandle::send_input`。
///
/// 未收录键码静默丢弃并计数；观看模式（`watch_only`）或无活动会话时静默忽略（T21 U9）。
#[tauri::command]
pub fn send_input_event(ev: WebInputEvent, state: State<'_, AppState>) -> Result<(), String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = slot.as_ref() else {
        return Ok(());
    };
    if active.watch_only {
        return Ok(());
    }
    let (mapped, unmapped_key) = map_web_input_event_for_video(&ev, active.video_size);
    if unmapped_key {
        drop(slot);
        let _ = state.record_dropped_key();
        return Ok(());
    }
    if mapped.is_empty() {
        return Ok(());
    }
    for input_ev in mapped {
        active.handle.send_input(input_ev);
    }
    Ok(())
}

/// IPC 命令：前端 `ResizeObserver` 上报 stage 矩形（CSS 像素）。
///
/// Rust 侧乘以 `window.scale_factor()` 换算物理像素（T21 坑 3），按协商视频宽高比计算等比居中
/// 视口后转发 `StageRect` 命令，并在 `CAMetalLayer` 已挂载时经主线程调用
/// `metal_attach::sync_layer_frame`（T21 坑 2）。
#[tauri::command]
pub fn stage_rect(
    rect: StageRectMsg,
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, AppState>,
) {
    let scale = window.scale_factor().unwrap_or(1.0);
    let frame = css_rect_to_physical_frame(rect, scale);

    let sync_target = state::prepare_stage_rect(&state.session, frame, |handle, effective| {
        let phys_w = effective.width.round() as u32;
        let phys_h = effective.height.round() as u32;
        if phys_w > 0 && phys_h > 0 {
            handle.send(SessionCommand::StageRect {
                width: phys_w,
                height: phys_h,
            });
        }
    });

    if let Some((sid, lease, effective_frame)) = sync_target {
        let app_clone = app.clone();
        let _ = app.run_on_main_thread(move || {
            let st = app_clone.state::<AppState>();
            if state::should_sync_layer_on_main_thread(&st.session, sid, &lease) {
                let _ = metal_attach::sync_leased_layer_frame(&lease, effective_frame);
            }
            drop(lease);
        });
    }
}

/// IPC 命令：W4 添加端口映射表单校验（`model` 三函数的 IPC 壳）。
#[tauri::command]
pub fn validate_map_form(form: MapForm) -> MapFormErrors {
    let name = model::validate_map_name(&form.name).err();
    let local_port = model::validate_port(&form.local_port).err();
    let target_addr = model::validate_target_addr(&form.target_addr).err();
    let target_port = model::validate_port(&form.target_port).err();
    let valid =
        name.is_none() && local_port.is_none() && target_addr.is_none() && target_port.is_none();
    MapFormErrors {
        name,
        local_port,
        target_addr,
        target_port,
        valid,
    }
}

/// IPC 命令：写入系统剪贴板（P2「复制并分享」，经 `tauri-plugin-clipboard-manager`）。
#[tauri::command]
pub fn copy_text(text: String, app: AppHandle) -> Result<(), String> {
    app.clipboard().write_text(text).map_err(|e| e.to_string())
}

// ─────────────────────────────────────────────────────────────────
// 被控端服务与局域网发现控制（All-in-One Phase 2）
// ─────────────────────────────────────────────────────────────────

/// 向后台 `rc-host-agent` 发送单次 IPC 请求。
pub(crate) fn call_agent_ipc(
    req: ServiceRequest,
    timeout: Duration,
) -> Result<ServiceResponse, String> {
    IpcClient::call(
        &config_paths::agent_socket_path(),
        &config_paths::ipc_secret_path(),
        req,
        timeout,
    )
}

fn send_background_services_reload_ipc(auth_changed: bool) {
    let secret_path = config_paths::ipc_secret_path();
    let req = if auth_changed {
        ServiceRequest::RefreshAuthAndCerts
    } else {
        ServiceRequest::ReloadPrefs
    };
    let _ = IpcClient::call(
        &config_paths::daemon_socket_path(),
        &secret_path,
        req.clone(),
        Duration::from_secs(2),
    );
    let _ = IpcClient::call(
        &config_paths::agent_socket_path(),
        &secret_path,
        req,
        Duration::from_secs(2),
    );
}

/// 异步通知后台 `rc-daemon` 与 `rc-host-agent` 重新加载配置/认证状态。
pub(crate) fn notify_background_services_reload(state: &AppState, auth_changed: bool) {
    if !state.is_service_proxy() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("rc-notify-bg-reload".to_string())
        .spawn(move || send_background_services_reload_ipc(auth_changed));
}

/// 同步通知后台 `rc-daemon` 与 `rc-host-agent` 重新加载配置/认证状态（登录/改密/登出/刷新设备码等关键切换路径使用）。
pub(crate) fn notify_background_services_reload_sync(state: &AppState, auth_changed: bool) {
    if !state.is_service_proxy() {
        return;
    }
    send_background_services_reload_ipc(auth_changed);
}

/// 在独立模式下将当前本机状态同步到顶部菜单栏 `NSStatusItem`。
pub(crate) fn sync_standalone_status_bar(state: &AppState) {
    if state.is_service_proxy() {
        return;
    }
    let server_snap = {
        let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        rt.current_status()
    };
    let connected_peer = {
        let host = state.host.lock().unwrap_or_else(|e| e.into_inner());
        host.connected_peer.clone()
    };
    let perms = permissions::check_permissions();
    metal_attach::update_status_bar(StatusBarDisplay {
        device_id: server_snap.device_id.or_else(|| {
            state
                .server
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .config
                .cached_device_id
                .clone()
        }),
        signal_online: server_snap.state == ServerConnectionState::Connected,
        connected_peer,
        missing_permissions: perms.platform_supported && !perms.host_ready,
        session_label: "独立托管模式".to_string(),
    });
}

/// 在已持有 `state.host` 锁的前提下同步局域网信标广播状态（U-M6：严格遵守 `host -> lan` 锁序，消除 TOCTOU）。
pub(crate) fn sync_lan_advertising_locked(state: &AppState, host: &HostRuntime) {
    let target = if state.is_service_proxy() {
        None
    } else {
        host.handle.as_ref().map(|h| {
            (
                h.listen_addr().port(),
                host.allow_remote_assist && !host.password.is_empty(),
            )
        })
    };
    if let Some(lan) = state.lan.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        lan.set_advertising(target);
    }
}

/// 启动被控端服务（可指定绑定地址、后端偏好与是否允许他人远程协助）。
pub(crate) fn start_host_service_with_policy(
    state: &AppState,
    listen_addr: SocketAddr,
    prefer_hardware: bool,
    allow_remote_assist: bool,
) -> Result<HostStatusSnapshot, String> {
    if state.is_service_proxy() {
        return match call_agent_ipc(
            ServiceRequest::SetRemoteAssist {
                enabled: allow_remote_assist,
            },
            Duration::from_secs(3),
        )? {
            ServiceResponse::Snapshot(snap) => Ok(HostStatusSnapshot::from_agent_snapshot(*snap)),
            ServiceResponse::Error { message } => Err(message),
            other => Err(format!("后台服务返回异常响应: {other:?}")),
        };
    }
    let (default_encrypt, allow_assist_file_transfer, file_transfer_dir) = {
        let prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        (
            prefs.default_encrypt_connection,
            prefs.allow_assist_file_transfer,
            prefs.resolved_file_transfer_dir(),
        )
    };
    let owner_verifier = build_owner_verifier(state);
    let mtls_config = build_session_mtls_bundle(state, None, None);
    let accept_mtls_lan = default_encrypt;
    let signal_client = state
        .server
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .signal_client
        .clone();

    let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(handle) = host.handle.as_ref() {
        handle.set_allow_owner_control(host.allow_owner_control);
        handle.set_allow_remote_assist(allow_remote_assist);
        handle.set_allow_assist_file_transfer(allow_assist_file_transfer);
        handle.set_owner_verifier(owner_verifier);
        handle.set_mtls_config(mtls_config, accept_mtls_lan);
        handle.set_file_transfer_dir(Some(file_transfer_dir));
        apply_host_signal_client(handle, signal_client);
        host.allow_remote_assist = allow_remote_assist;
    } else {
        let password = (!host.password.is_empty()).then(|| host.password.clone());
        let handle = HostHandle::spawn(UiHostConfig {
            listen_addr,
            password,
            approval_timeout_us: rc_session::auth::DEFAULT_AUTH_TIMEOUT_US,
            prefer_hardware,
            require_approval: true,
            auto_accept_with_password: true,
            auto_rotate_password: !host.custom_password,
            clipboard_enabled: true,
            audio_enabled: true,
            allow_owner_control: host.allow_owner_control,
            allow_remote_assist,
            owner_verifier,
            mtls_config,
            accept_mtls_lan,
            forbid_passwordless_auto_approve: false,
            file_transfer_dir: Some(file_transfer_dir),
            allow_assist_file_transfer,
            signal_client,
        })?;
        host.allow_remote_assist = allow_remote_assist;
        let actual = handle.listen_addr();
        host.listen_addr = Some(actual.to_string());
        host.display_addr = Some(HostRuntime::format_display_addr(actual));
        host.backend = if prefer_hardware {
            "auto".to_string()
        } else {
            "fake".to_string()
        };
        host.connected_peer = None;
        host.pending_approval = None;
        host.handle = Some(handle);
    }
    sync_lan_advertising_locked(state, &host);
    Ok(host.snapshot())
}

/// 启动被控端服务（可指定绑定地址与后端偏好，便于测试使用 `127.0.0.1:0`）。
#[cfg(test)]
pub(crate) fn start_host_service_on(
    state: &AppState,
    listen_addr: SocketAddr,
    prefer_hardware: bool,
) -> Result<HostStatusSnapshot, String> {
    start_host_service_with_policy(state, listen_addr, prefer_hardware, true)
}

/// 停止被控端服务并撤回局域网广播，同时返回被取消的在途审批代次（U-M5 / U-M6）；
/// `blocking = true` 时同步回收工作线程（供退出时调用）。
pub(crate) fn stop_host_service_with_cancelled(
    state: &AppState,
    blocking: bool,
) -> (HostStatusSnapshot, Option<u64>) {
    let (old_handle, snapshot, cancelled_gen) = {
        let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
        let old = host.handle.take();
        let cancelled = host.pending_approval.take().map(|p| p.generation);
        host.listen_addr = None;
        host.display_addr = None;
        host.connected_peer = None;
        host.backend.clear();
        sync_lan_advertising_locked(state, &host);
        (old, host.snapshot(), cancelled)
    };

    if let Some(mut handle) = old_handle {
        if blocking {
            handle.stop();
        } else {
            std::thread::spawn(move || {
                handle.stop();
            });
        }
    }

    (snapshot, cancelled_gen)
}

/// 停止被控端服务并撤回局域网广播；`blocking = true` 时同步回收工作线程（供退出时调用）。
pub(crate) fn stop_host_service(state: &AppState, blocking: bool) -> HostStatusSnapshot {
    stop_host_service_with_cancelled(state, blocking).0
}

/// 应用退出或主窗口销毁时幂等回收认证维护线程、被控端服务、局域网发现线程与信令服务器客户端。
pub(crate) fn shutdown_host_and_lan(app: &AppHandle) {
    let state = app.state::<AppState>();
    stop_shortcut_tap_runtime(&state);
    disconnect_active_session(app);
    window::close_session_and_tool_windows(app);
    // 退出前在主线程主动清理物理防窥隐私幕布，防止后续 `stop_host_service` join 线程时主队列已停而超时。
    #[cfg(target_os = "macos")]
    rc_capture_macos::clear_privacy_curtain();
    stop_auth_maintainer(&state);
    if let Some(mut mon) = state
        .network_monitor
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        mon.stop();
    }
    shutdown_signal_client(&state);
    let _ = stop_host_service(&state, true);
    let old_lan = state.lan.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(mut lan) = old_lan {
        lan.stop();
    }
}

/// 更新被控端密码的纯状态实现（便于离线单元测试）。
fn set_host_password_inner(
    state: &AppState,
    password: Option<String>,
    regenerate: bool,
) -> Result<HostStatusSnapshot, String> {
    if state.is_service_proxy() {
        let req = if regenerate {
            ServiceRequest::RotateTempCode
        } else {
            ServiceRequest::SetPassword {
                password: password.clone(),
            }
        };
        match call_agent_ipc(req, Duration::from_secs(3))? {
            ServiceResponse::PasswordChanged { .. } | ServiceResponse::Ok => {}
            ServiceResponse::Error { message } => return Err(message),
            other => return Err(format!("后台服务密码更新返回异常: {other:?}")),
        }
        return match call_agent_ipc(ServiceRequest::GetSnapshot, Duration::from_secs(2))? {
            ServiceResponse::Snapshot(snap) => Ok(HostStatusSnapshot::from_agent_snapshot(*snap)),
            ServiceResponse::Error { message } => Err(message),
            other => Err(format!("读取后台状态失败: {other:?}")),
        };
    }
    let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
    if regenerate {
        host.password = host_bridge::generate_temp_code();
        host.custom_password = false;
    } else {
        match password.filter(|p| !p.is_empty()) {
            Some(pw) => {
                host.password = pw;
                host.custom_password = true;
            }
            None => {
                host.password.clear();
                host.custom_password = true;
            }
        }
    }
    let opt_pw = (!host.password.is_empty()).then(|| host.password.clone());
    if let Some(handle) = host.handle.as_ref() {
        handle.set_password_mode(opt_pw, !host.custom_password);
    }
    sync_lan_advertising_locked(state, &host);
    Ok(host.snapshot())
}

/// 回写机主审批裁决的纯状态实现。
fn respond_approval_inner(state: &AppState, generation: u64, accepted: bool) -> Result<(), String> {
    if state.is_service_proxy() {
        return match call_agent_ipc(
            ServiceRequest::RespondApproval {
                generation,
                accept: accepted,
            },
            Duration::from_secs(2),
        )? {
            ServiceResponse::Ok => Ok(()),
            ServiceResponse::Error { message } => Err(message),
            other => Err(format!("后台审批应答异常: {other:?}")),
        };
    }
    let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
    let Some(handle) = host.handle.as_ref() else {
        return Err("被控端服务未启动".to_string());
    };
    let applied = handle.respond_approval(generation, accepted);
    if host
        .pending_approval
        .as_ref()
        .is_some_and(|p| p.generation == generation)
    {
        host.pending_approval = None;
    }
    if applied {
        Ok(())
    } else {
        Err("审批请求已过期或不存在".to_string())
    }
}

/// 断开当前控制端连接的纯状态实现。
fn disconnect_host_client_inner(state: &AppState) -> Result<(), String> {
    if state.is_service_proxy() {
        return match call_agent_ipc(ServiceRequest::DisconnectClient, Duration::from_secs(2))? {
            ServiceResponse::Ok => Ok(()),
            ServiceResponse::Error { message } => Err(message),
            other => Err(format!("后台断开控制端返回异常: {other:?}")),
        };
    }
    let host = state.host.lock().unwrap_or_else(|e| e.into_inner());
    let Some(handle) = host.handle.as_ref() else {
        return Err("被控端服务未启动".to_string());
    };
    handle.disconnect_client();
    Ok(())
}

/// IPC 命令：获取当前本机被控端状态快照。
#[tauri::command]
pub fn get_host_status(state: State<'_, AppState>) -> Result<HostStatusSnapshot, String> {
    state.require_authenticated()?;
    if state.is_service_proxy() {
        if let Ok(ServiceResponse::Snapshot(snap)) =
            call_agent_ipc(ServiceRequest::GetSnapshot, Duration::from_millis(500))
        {
            return Ok(HostStatusSnapshot::from_agent_snapshot(*snap));
        }
    }
    Ok(state
        .host
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .snapshot())
}

/// IPC 命令：检测当前系统权限状态（代理模式下优先探测后台 `rc-host-agent` 进程真实权限；非 macOS 返回全放行占位快照）。
#[tauri::command]
pub fn check_permissions(state: State<'_, AppState>) -> PermissionSnapshot {
    if state.is_service_proxy() {
        if let Ok(ServiceResponse::Snapshot(snap)) = call_agent_ipc(
            ServiceRequest::TriggerPermissionCheck,
            Duration::from_secs(2),
        ) {
            return snap.permissions;
        }
    }
    let perms = permissions::check_permissions();
    if !state.is_service_proxy()
        && state.gate_state() == AppGateState::Authenticated
        && permissions::ensure_host_ready(&perms).is_ok()
        && state
            .host
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .handle
            .is_none()
    {
        auto_start_host_if_permitted(&state);
    }
    perms
}

/// IPC 命令：请求 macOS 屏幕录制权限并返回最新系统权限快照。
#[tauri::command]
pub fn request_screen_recording_permission(state: State<'_, AppState>) -> PermissionSnapshot {
    let perms = permissions::request_screen_recording();
    if !state.is_service_proxy()
        && state.gate_state() == AppGateState::Authenticated
        && permissions::ensure_host_ready(&perms).is_ok()
        && state
            .host
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .handle
            .is_none()
    {
        auto_start_host_if_permitted(&state);
    }
    perms
}

/// IPC 命令：打开指定权限对应的系统设置隐私面板（`"screen_recording"` / `"accessibility"` / `"input_monitoring"`）。
#[tauri::command]
pub fn open_permission_settings(kind: String) -> Result<(), UiCommandError> {
    permissions::open_permission_settings(&kind).map_err(|err| {
        let ui_err = permissions::permission_kind_ui_error(&kind, Some(err.clone()));
        UiCommandError::from_ui_error_with_message(err, ui_err)
    })
}

fn persist_prefs_mutation(
    state: &AppState,
    path: &Path,
    mutate: impl FnOnce(&mut AppPrefs),
) -> Result<(), String> {
    let _io = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _file_lock = config_paths::acquire_file_lock(path, Duration::from_secs(3));
    let next = {
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        if state.is_service_proxy() && path.exists() {
            *prefs = app_prefs::load_from_path(path);
        }
        mutate(&mut prefs);
        prefs.clone()
    };
    app_prefs::save_to_path(path, &next)
}

#[cfg(test)]
fn persist_allow_remote_assist_to_path(
    state: &AppState,
    enabled: bool,
    path: &Path,
) -> Result<(), String> {
    persist_prefs_mutation(state, path, |prefs| {
        prefs.allow_remote_assist = enabled;
    })
}

fn persist_custom_host_password_to_path(
    state: &AppState,
    custom_password: Option<String>,
    path: &Path,
) -> Result<(), String> {
    persist_prefs_mutation(state, path, |prefs| {
        prefs.custom_host_password = custom_password;
    })
}

/// 认证放行且系统权限均就绪时自动启用被控端监听（默认始终承接同账号本人免密自控；是否放行他人由 `prefs.allow_remote_assist` 控制）。
pub(crate) fn auto_start_host_if_permitted(state: &AppState) {
    let allow_assist = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .allow_remote_assist;
    let perms = permissions::check_permissions();
    if permissions::ensure_host_ready(&perms).is_ok() {
        let default_addr = SocketAddr::from(([0, 0, 0, 0], host_bridge::DEFAULT_HOST_PORT));
        let _ = start_host_service_with_policy(state, default_addr, true, allow_assist);
    }
}

/// IPC 命令：开启或关闭本机「允许他人远程协助」服务（入口强门禁校验 `gate == Authenticated`；
/// 开启前在 macOS 校验必需系统权限；关闭时始终保留仅本人免密自控通道监听并拒绝外来协助；
/// 同步更新局域网信标、信令服务器在线状态与持久化偏好，关闭时取消在途外来审批弹窗 U-M5）。
#[tauri::command]
pub fn set_host_enabled(
    enabled: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<HostStatusSnapshot, UiCommandError> {
    state
        .require_unlocked()
        .map_err(UiCommandError::from_auth_gate_error)?;
    state
        .require_authenticated()
        .map_err(UiCommandError::from_auth_gate_error)?;
    if enabled {
        let prefs_path = app_prefs::default_prefs_path();
        let snapshot = {
            if state.is_service_proxy() {
                let snap = match call_agent_ipc(
                    ServiceRequest::SetRemoteAssist { enabled: true },
                    Duration::from_secs(3),
                )
                .map_err(UiCommandError::from_host_error)?
                {
                    ServiceResponse::Snapshot(s) => HostStatusSnapshot::from_agent_snapshot(*s),
                    ServiceResponse::Error { message } => {
                        return Err(UiCommandError::from_host_error(message));
                    }
                    other => {
                        return Err(UiCommandError::from_host_error(format!(
                            "后台开启远程协助返回异常: {other:?}"
                        )));
                    }
                };
                let _io = state
                    .prefs_io_lock
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let _file_lock =
                    config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
                let loaded = app_prefs::load_from_path(&prefs_path);
                *state.prefs.lock().unwrap_or_else(|e| e.into_inner()) = loaded;
                snap
            } else {
                let perms = permissions::check_permissions();
                if let Err(ui_err) = permissions::ensure_host_ready_ui_error(&perms) {
                    let msg = permissions::ensure_host_ready(&perms)
                        .err()
                        .unwrap_or_else(|| ui_err.message.clone());
                    return Err(UiCommandError::from_ui_error_with_message(msg, *ui_err));
                }

                let _io = state
                    .prefs_io_lock
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let _file_lock =
                    config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
                let prev_allow = {
                    let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
                    let prev = prefs.allow_remote_assist;
                    prefs.allow_remote_assist = true;
                    let snapshot_prefs = prefs.clone();
                    drop(prefs);
                    app_prefs::save_to_path(&prefs_path, &snapshot_prefs).map_err(|err| {
                        let mut revert = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
                        revert.allow_remote_assist = prev;
                        UiCommandError::from_host_error(err)
                    })?;
                    prev
                };
                let default_addr = SocketAddr::from(([0, 0, 0, 0], host_bridge::DEFAULT_HOST_PORT));
                match start_host_service_with_policy(&state, default_addr, true, true) {
                    Ok(snap) => snap,
                    Err(err) => {
                        let reverted = {
                            let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
                            prefs.allow_remote_assist = prev_allow;
                            prefs.clone()
                        };
                        let _ = app_prefs::save_to_path(&prefs_path, &reverted);
                        return Err(UiCommandError::from_host_error(err));
                    }
                }
            }
        };
        sync_assist_state_to_server(&app, &state, true);
        Ok(snapshot)
    } else {
        let prefs_path = app_prefs::default_prefs_path();
        let (snapshot, cancelled_gen) = {
            if state.is_service_proxy() {
                let snap = match call_agent_ipc(
                    ServiceRequest::SetRemoteAssist { enabled: false },
                    Duration::from_secs(3),
                )
                .map_err(UiCommandError::from_host_error)?
                {
                    ServiceResponse::Snapshot(s) => HostStatusSnapshot::from_agent_snapshot(*s),
                    ServiceResponse::Error { message } => {
                        return Err(UiCommandError::from_host_error(message));
                    }
                    other => {
                        return Err(UiCommandError::from_host_error(format!(
                            "后台关闭远程协助返回异常: {other:?}"
                        )));
                    }
                };
                {
                    let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
                    host.allow_remote_assist = false;
                }
                let _io = state
                    .prefs_io_lock
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let _file_lock =
                    config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
                let loaded = app_prefs::load_from_path(&prefs_path);
                *state.prefs.lock().unwrap_or_else(|e| e.into_inner()) = loaded;
                (snap, None)
            } else {
                let _io = state
                    .prefs_io_lock
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let _file_lock =
                    config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
                let _prev_allow = {
                    let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
                    let prev = prefs.allow_remote_assist;
                    prefs.allow_remote_assist = false;
                    let snapshot_prefs = prefs.clone();
                    drop(prefs);
                    app_prefs::save_to_path(&prefs_path, &snapshot_prefs).map_err(|err| {
                        let mut revert = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
                        revert.allow_remote_assist = prev;
                        UiCommandError::from_host_error(err)
                    })?;
                    prev
                };
                let (snapshot, cancelled_gen) = {
                    let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
                    host.allow_remote_assist = false;
                    let cancelled = host.pending_approval.take().map(|p| p.generation);
                    if let Some(handle) = host.handle.as_ref() {
                        if let Some(gen) = cancelled {
                            let _ = handle.respond_approval(gen, false);
                        }
                        handle.set_allow_remote_assist(false);
                    }
                    sync_lan_advertising_locked(&state, &host);
                    (host.snapshot(), cancelled)
                };
                (snapshot, cancelled_gen)
            }
        };
        refresh_host_security_runtime(&state);
        sync_assist_state_to_server(&app, &state, false);
        if let Some(generation) = cancelled_gen {
            let _ = app.emit(
                "host://update",
                &HostUpdate::ApprovalCancelled { generation },
            );
        }
        Ok(snapshot)
    }
}

/// IPC 命令：设置自定义访问密码或重新生成临时验证码，并立即同步至活动被控端及持久化偏好。
#[tauri::command]
pub fn set_host_password(
    password: Option<String>,
    regenerate: bool,
    state: State<'_, AppState>,
) -> Result<HostStatusSnapshot, String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let snap = set_host_password_inner(&state, password, regenerate)?;
    if state.is_service_proxy() {
        let _io = state
            .prefs_io_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let loaded = app_prefs::load_from_path(&app_prefs::default_prefs_path());
        *state.prefs.lock().unwrap_or_else(|e| e.into_inner()) = loaded;
        return Ok(snap);
    }
    let persisted_pw = if regenerate {
        None
    } else {
        Some(snap.password.clone())
    };
    if let Err(err) =
        persist_custom_host_password_to_path(&state, persisted_pw, &app_prefs::default_prefs_path())
    {
        eprintln!("[rc-ui] 持久化 custom_host_password 失败：{err}");
    }
    sync_standalone_status_bar(&state);
    Ok(snap)
}

/// IPC 命令：回写机主连接审批弹窗的同意/拒绝裁决。
#[tauri::command]
pub fn respond_approval(
    generation: u64,
    accepted: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    respond_approval_inner(&state, generation, accepted)
}

/// IPC 命令：主动断开当前接入本机的控制端会话（被控端继续监听新连接）。
#[tauri::command]
pub fn disconnect_host_client(state: State<'_, AppState>) -> Result<(), String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    disconnect_host_client_inner(&state)
}

fn sync_prefs_from_disk_if_proxy(state: &AppState) -> AppPrefs {
    if !state.is_service_proxy() {
        return state
            .prefs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
    }
    let prefs_path = app_prefs::default_prefs_path();
    let _io = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _file_lock = config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
    let latest = app_prefs::load_from_path(&prefs_path);
    *state.prefs.lock().unwrap_or_else(|e| e.into_inner()) = latest.clone();
    latest
}

/// IPC 命令：返回当前局域网发现的在线被控端设备列表快照（仅当启用 `show_lan_devices` 时返回，默认隐藏不暴露）。
#[tauri::command]
pub fn list_lan_devices(state: State<'_, AppState>) -> Result<Vec<LanPeer>, String> {
    state.require_authenticated()?;
    let show_lan = sync_prefs_from_disk_if_proxy(&state).show_lan_devices;
    if !show_lan {
        return Ok(Vec::new());
    }
    Ok(state
        .lan
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|lan| lan.peers())
        .unwrap_or_default())
}

// ─────────────────────────────────────────────────────────────────
// 信令与中继服务器配置与生命周期控制
// ─────────────────────────────────────────────────────────────────

fn filter_usable_display_lan_addr(addr: &str) -> Option<String> {
    let trimmed = addr.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed: SocketAddr = trimmed.parse().ok()?;
    let canonical_ip = parsed.ip().to_canonical();
    let usable = match canonical_ip {
        std::net::IpAddr::V4(v4) => lan_discovery::is_usable_lan_ipv4(v4),
        std::net::IpAddr::V6(v6) => {
            !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_multicast()
                && (v6.segments()[0] & 0xffc0) != 0xfe80
        }
    };
    usable.then(|| SocketAddr::new(canonical_ip, parsed.port()).to_string())
}

fn collect_local_lan_addrs(state: &AppState) -> Vec<String> {
    let mut out = Vec::new();
    let display = state
        .host
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .display_addr
        .clone();
    if let Some(addr) = display.as_deref().and_then(filter_usable_display_lan_addr) {
        out.push(addr);
    } else if let Some(ip) = lan_discovery::detect_local_lan_ipv4() {
        out.push(format!("{ip}:{}", host_bridge::DEFAULT_HOST_PORT));
    }
    out
}

/// 在独立后台短线程中异步停机并回收旧 `SignalClientHandle`，绝不在持锁状态或 UI/轮询线程上 `join`（H2 / H8）。
fn reap_signal_client_async(old: Option<Arc<SignalClientHandle>>) {
    if let Some(arc) = old {
        arc.request_stop();
        std::thread::spawn(move || {
            drop(arc);
        });
    }
}

#[cfg(test)]
std::thread_local! {
    static TEST_HOST_SIGNAL_SYNC_LOG: std::cell::RefCell<Option<Vec<Option<usize>>>> =
        const { std::cell::RefCell::new(None) };
}

fn apply_host_signal_client(handle: &HostHandle, signal_client: Option<Arc<SignalClientHandle>>) {
    #[cfg(test)]
    TEST_HOST_SIGNAL_SYNC_LOG.with(|slot| {
        if let Some(ref mut log) = *slot.borrow_mut() {
            log.push(signal_client.as_ref().map(|s| Arc::as_ptr(s) as usize));
        }
    });
    handle.set_signal_client(signal_client);
}

/// 将最新信令客户端句柄同步至运行中的独立模式 `HostHandle`（绝不在持有 `state.server` 锁时调用）。
fn sync_running_host_signal_client(
    state: &AppState,
    signal_client: Option<Arc<SignalClientHandle>>,
) {
    let host = state.host.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(handle) = host.handle.as_ref() {
        apply_host_signal_client(handle, signal_client);
    }
}

/// 退出应用时停机并同步回收信令客户端：先置位 `shutting_down = true` 阻止新客户端派生，
/// 再释放 `ServerRuntime` 锁后调用 `request_stop()` 与 `stop()` 同步等待回收（可中断拨号 <= 150ms）。
pub(crate) fn shutdown_signal_client(state: &AppState) {
    let old = {
        let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        rt.shutting_down = true;
        rt.signal_client.take()
    };
    sync_running_host_signal_client(state, None);
    if let Some(arc) = old {
        arc.request_stop();
        if let Some(mut handle) = Arc::into_inner(arc) {
            handle.stop();
        }
    }
}

/// 登出时停机并回收信令客户端（不置 `shutting_down = true`，支持同进程重新登录恢复连接）。
fn stop_signal_client_for_logout(state: &AppState) -> ServerStatusSnapshot {
    let (old, snap) = {
        let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        let old = rt.signal_client.take();
        let snap = rt.config.disabled_snapshot();
        rt.last_status = snap.clone();
        (old, snap)
    };
    sync_running_host_signal_client(state, None);
    reap_signal_client_async(old);
    snap
}

/// 将本机 Wake-on-LAN 隐私开关与主网卡 MAC 同步至活跃信令句柄：
/// - `allow_wol == true`：上报已探测到的合法单播 MAC 并开启 `wol_enabled`；
/// - `allow_wol == false`：显式清空缓存 MAC（`set_mac_addr(None)`）并关闭 `wol_enabled`，避免后续心跳携带 MAC。
fn apply_wol_state_to_signal(sig: &SignalClientHandle, allow_wol: bool, local_mac: Option<String>) {
    if allow_wol {
        if let Some(mac) = local_mac {
            let _ = sig.set_mac_addr(Some(mac));
        }
        let _ = sig.set_wol_enabled(true);
    } else {
        let _ = sig.set_mac_addr(None);
        let _ = sig.set_wol_enabled(false);
    }
}

/// 根据当前 `state.server.config` 启动或停用信令服务器长连接客户端，并返回最新状态快照。
pub(crate) fn start_signal_client_if_enabled(state: &AppState) -> ServerStatusSnapshot {
    let lan_addrs = collect_local_lan_addrs(state);
    let authenticated = state.gate_state() == AppGateState::Authenticated;
    let allow_wol = sync_prefs_from_disk_if_proxy(state).allow_wol;
    // 严格遵守隐私承诺：仅当用户开启 allow_wol 时才在锁外探测主网卡 MAC。
    let local_mac = if authenticated && !state.is_service_proxy() && allow_wol {
        rc_ui::platform::primary_mac_address()
    } else {
        None
    };
    let options = build_signal_client_options(state);
    let (old, new_signal, snap) = {
        let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        let old = rt.signal_client.take();
        let can_start = authenticated
            && !state.is_service_proxy()
            && !rt.shutting_down
            && rt.config.enabled
            && !rt.config.server_host.trim().is_empty()
            && rt.config.signal_port != 0;
        let snap = if can_start {
            let handle = Arc::new(SignalClientHandle::spawn_with_options(
                rt.config.clone(),
                lan_discovery::local_hostname(),
                lan_addrs,
                options,
                |_| {},
            ));
            apply_wol_state_to_signal(&handle, allow_wol, local_mac);
            let snap = handle.status();
            rt.signal_client = Some(handle);
            snap
        } else {
            rt.config.disabled_snapshot()
        };
        rt.last_status = snap.clone();
        (old, rt.signal_client.clone(), snap)
    };
    sync_running_host_signal_client(state, new_signal);
    reap_signal_client_async(old);
    snap
}

/// 获取当前服务器状态快照的纯状态实现（代理模式下优先从后台 Agent 获取，否则读取活动 `SignalClientHandle::status()`）。
pub(crate) fn get_server_config_inner(state: &AppState) -> ServerStatusSnapshot {
    if state.is_service_proxy() {
        if let Ok(ServiceResponse::ServerStatus(mut snap)) =
            call_agent_ipc(ServiceRequest::GetServerStatus, Duration::from_millis(350))
        {
            snap.revision = server_config::next_status_revision();
            return *snap;
        }
    }
    state
        .server
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .current_status()
}

/// 保存信令/中继服务器配置到指定路径并重启信令客户端的纯状态实现（H2 + H8，便于离线单元测试）。
fn save_server_config_to_path(
    state: &AppState,
    req: SaveServerConfigRequest,
    path: &Path,
) -> Result<ServerStatusSnapshot, String> {
    if req.signal_port == 0 || req.relay_port == 0 {
        return Err("信令端口与中继端口必须为 1–65535".to_string());
    }

    let (clean_host, effective_signal_port) = if req.enabled {
        server_config::parse_server_input(&req.server_host, req.signal_port)?
    } else {
        let trimmed = req.server_host.trim();
        if trimmed.is_empty() {
            (String::new(), req.signal_port)
        } else {
            server_config::parse_server_input(trimmed, req.signal_port)
                .unwrap_or_else(|_| (trimmed.to_string(), req.signal_port))
        }
    };

    // 锁序恒为 `server_io_lock -> server`，绝不在持有 `server` 锁期间执行磁盘 I/O。
    let _io_guard = state
        .server_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let (next_cfg, should_wipe_auth) = {
        let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        let same_endpoint =
            rt.config.server_host == clean_host && rt.config.signal_port == effective_signal_port;
        let next = ServerConfig {
            enabled: req.enabled,
            server_host: clean_host,
            signal_port: effective_signal_port,
            relay_port: req.relay_port,
            api_port: rt.config.api_port,
            use_https: rt.config.use_https,
            ca_fingerprint_sha256: if same_endpoint {
                rt.config.ca_fingerprint_sha256.clone()
            } else {
                None
            },
            ca_cert_pem: if same_endpoint {
                rt.config.ca_cert_pem.clone()
            } else {
                None
            },
            server_auth_pubkey_hex: if same_endpoint {
                rt.config.server_auth_pubkey_hex.clone()
            } else {
                None
            },
            device_key: rt.config.device_key.clone(),
            cached_device_id: if same_endpoint && req.enabled {
                rt.config.cached_device_id.clone()
            } else {
                None
            },
        };
        let should_wipe = !same_endpoint || !next.is_configured();
        (next, should_wipe)
    };

    server_config::save_to_path(path, &next_cfg)?;
    let old_logout_ctx = if should_wipe_auth {
        Some(state.auth.take_logout_context_and_wipe())
    } else {
        None
    };
    state.auth.update_server_config(next_cfg.clone());

    let lan_addrs =
        if next_cfg.enabled && !next_cfg.server_host.trim().is_empty() && next_cfg.signal_port != 0
        {
            collect_local_lan_addrs(state)
        } else {
            Vec::new()
        };

    let authenticated = state
        .auth
        .current_session()
        .is_some_and(|s| !s.must_change_password)
        && next_cfg.is_configured();
    let options = build_signal_client_options(state);
    let (old, new_signal, snap) = {
        let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        rt.config = next_cfg.clone();
        rt.config_epoch = rt.config_epoch.wrapping_add(1);
        let old = rt.signal_client.take();
        let can_start = authenticated
            && !state.is_service_proxy()
            && !rt.shutting_down
            && next_cfg.enabled
            && !next_cfg.server_host.trim().is_empty()
            && next_cfg.signal_port != 0;
        let snap = if can_start {
            let handle = Arc::new(SignalClientHandle::spawn_with_options(
                next_cfg,
                lan_discovery::local_hostname(),
                lan_addrs,
                options,
                |_| {},
            ));
            let snap = handle.status();
            rt.signal_client = Some(handle);
            snap
        } else {
            rt.config.disabled_snapshot()
        };
        rt.last_status = snap.clone();
        (old, rt.signal_client.clone(), snap)
    };

    drop(_io_guard);
    sync_running_host_signal_client(state, new_signal);
    reap_signal_client_async(old);
    if let Some((old_cfg, prev_sess)) = old_logout_ctx {
        if prev_sess.is_some() {
            let _ = std::thread::Builder::new()
                .name("rc-server-switch-logout".to_string())
                .spawn(move || {
                    let _ = AuthSessionManager::notify_remote_logout(
                        &old_cfg,
                        prev_sess.as_ref(),
                        None,
                    );
                });
        }
    }
    notify_background_services_reload(state, true);
    Ok(snap)
}

/// IPC 命令：获取当前信令与中继服务器配置与实时连接状态快照。
#[tauri::command]
pub fn get_server_config(state: State<'_, AppState>) -> ServerStatusSnapshot {
    get_server_config_inner(&state)
}

/// IPC 命令：在后台线程池校验、持久化信令与中继服务器配置，重启信令客户端并广播 `"server://status"`（H2 + H8）。
#[tauri::command]
pub async fn save_server_config(
    cfg: SaveServerConfigRequest,
    app: AppHandle,
) -> Result<ServerStatusSnapshot, String> {
    let state_gate = app.state::<AppState>();
    state_gate.require_unlocked()?;
    state_gate.require_authenticated()?;
    let app_for_blocking = app.clone();
    let snap = tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        save_server_config_to_path(&state, cfg, &server_config::default_config_path())
    })
    .await
    .map_err(|e| format!("保存服务器配置任务异常：{e}"))??;
    {
        let state = app.state::<AppState>();
        if state.gate_state() != AppGateState::Authenticated {
            stop_auth_maintainer_async(&state);
            disconnect_active_session(&app);
            window::close_session_and_tool_windows(&app);
            let (host_snap, cancelled_gen) = stop_host_service_with_cancelled(&state, false);
            state
                .host
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .allow_owner_control = true;
            if let Some(generation) = cancelled_gen {
                let _ = app.emit(
                    "host://update",
                    &HostUpdate::ApprovalCancelled { generation },
                );
            }
            if !host_snap.enabled {
                let _ = app.emit("host://update", &HostUpdate::Stopped);
            }
        }
        let guard = state.lan.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lan) = guard.as_ref() {
            lan.set_local_device_id(snap.device_id.clone());
        }
        let _ = app.emit("gate://status", &state.gate_status_snapshot());
    }
    let _ = app.emit("server://status", &snap);
    Ok(snap)
}

/// 生成全新 `device_key`、清空 `cached_device_id` 并持久化，重启信令客户端换取新 9 位设备 ID。
fn refresh_device_id_to_path(
    state: &AppState,
    path: &Path,
) -> Result<ServerStatusSnapshot, String> {
    let _io_guard = state
        .server_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let next_cfg = {
        let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        let mut cloned = rt.config.clone();
        cloned.device_key = server_config::generate_device_key();
        cloned.cached_device_id = None;
        cloned
    };

    server_config::save_to_path(path, &next_cfg)?;
    let _ = state.auth.cert_manager().clear_device_cert();
    state.auth.update_server_config(next_cfg.clone());

    let lan_addrs =
        if next_cfg.enabled && !next_cfg.server_host.trim().is_empty() && next_cfg.signal_port != 0
        {
            collect_local_lan_addrs(state)
        } else {
            Vec::new()
        };

    let authenticated = state
        .auth
        .current_session()
        .is_some_and(|s| !s.must_change_password)
        && next_cfg.is_configured();
    let options = build_signal_client_options(state);
    let (old, new_signal, snap) = {
        let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        rt.config = next_cfg.clone();
        rt.config_epoch = rt.config_epoch.wrapping_add(1);
        let old = rt.signal_client.take();
        let can_start = authenticated
            && !state.is_service_proxy()
            && !rt.shutting_down
            && next_cfg.enabled
            && !next_cfg.server_host.trim().is_empty()
            && next_cfg.signal_port != 0;
        let snap = if can_start {
            let handle = Arc::new(SignalClientHandle::spawn_with_options(
                next_cfg,
                lan_discovery::local_hostname(),
                lan_addrs,
                options,
                |_| {},
            ));
            let snap = handle.status();
            rt.signal_client = Some(handle);
            snap
        } else if state.is_service_proxy() {
            let mut preserved = rt.last_status.clone();
            preserved.device_id = None;
            preserved.device_id_formatted = None;
            preserved.error_message = None;
            preserved.revision = server_config::next_status_revision();
            preserved
        } else {
            rt.config.disabled_snapshot()
        };
        rt.last_status = snap.clone();
        (old, rt.signal_client.clone(), snap)
    };

    drop(_io_guard);
    sync_running_host_signal_client(state, new_signal);
    reap_signal_client_async(old);
    refresh_host_security_runtime(state);
    Ok(snap)
}

/// IPC 命令：更换本机 `device_key` 并重新向信令服务器注册，刷新 9 位设备 ID。
#[tauri::command]
pub async fn refresh_device_id(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ServerStatusSnapshot, String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let app_for_blocking = app.clone();
    let snap = tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        let mut snap = refresh_device_id_to_path(&state, &server_config::default_config_path())?;
        match bind_device_and_ensure_cert(
            &state,
            &server_config::default_config_path(),
            Some(&app_for_blocking),
        ) {
            Ok(new_did) => {
                snap.device_id = Some(new_did.clone());
                snap.device_id_formatted = Some(server_config::format_device_id(&new_did));
                {
                    let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
                    rt.last_status = snap.clone();
                }
                refresh_host_security_runtime(&state);
                notify_background_services_reload_sync(&state, true);
                if state.is_service_proxy() {
                    if let Ok(ServiceResponse::ServerStatus(live)) =
                        call_agent_ipc(ServiceRequest::GetServerStatus, Duration::from_millis(400))
                    {
                        let mut live = *live;
                        if live.device_id.as_deref() != Some(new_did.as_str()) {
                            live.device_id = Some(new_did.clone());
                            live.device_id_formatted =
                                Some(server_config::format_device_id(&new_did));
                        }
                        live.revision = server_config::next_status_revision();
                        snap = live.clone();
                        let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
                        rt.apply_status_snapshot(&live);
                    }
                }
                Ok::<_, String>(snap)
            }
            Err(err) => {
                {
                    let guard = state.lan.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(lan) = guard.as_ref() {
                        lan.set_local_device_id(snap.device_id.clone());
                    }
                }
                refresh_host_security_runtime(&state);
                notify_background_services_reload(&state, true);
                let _ = app_for_blocking.emit("gate://status", &state.gate_status_snapshot());
                let _ = app_for_blocking.emit("server://status", &snap);
                Err(err)
            }
        }
    })
    .await
    .map_err(|e| format!("刷新设备 ID 任务异常：{e}"))??;
    {
        let state = app.state::<AppState>();
        let guard = state.lan.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lan) = guard.as_ref() {
            lan.set_local_device_id(snap.device_id.clone());
        }
        let _ = app.emit("gate://status", &state.gate_status_snapshot());
    }
    let _ = app.emit("server://status", &snap);
    Ok(snap)
}

/// 校验并持久化远控流与 HUD 偏好的纯状态实现（便于离线单测）。
fn save_stream_prefs_to_path(
    state: &AppState,
    patch: StreamPrefsPatch,
    path: &Path,
) -> Result<AppPrefs, String> {
    let _io = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _file_lock = config_paths::acquire_file_lock(path, Duration::from_secs(3));
    let next = {
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        if state.is_service_proxy() && path.exists() {
            *prefs = app_prefs::load_from_path(path);
        }
        prefs.apply_stream_patch(patch)?;
        prefs.clone()
    };
    app_prefs::save_to_path(path, &next)?;
    notify_background_services_reload(state, false);
    Ok(next)
}

fn save_terminal_prefs_to_path(
    state: &AppState,
    patch: TerminalPrefsPatch,
    path: &Path,
) -> Result<AppPrefs, String> {
    let _io = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _file_lock = config_paths::acquire_file_lock(path, Duration::from_secs(3));
    let next = {
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        if state.is_service_proxy() && path.exists() {
            *prefs = app_prefs::load_from_path(path);
        }
        prefs.apply_terminal_patch(patch)?;
        prefs.clone()
    };
    app_prefs::save_to_path(path, &next)?;
    notify_background_services_reload(state, false);
    Ok(next)
}

/// 脱敏面向前端返回的 `AppPrefs` 副本，确保代理密码绝不经通用偏好 IPC 泄露至渲染层。
fn sanitize_app_prefs_for_ui(mut prefs: AppPrefs) -> AppPrefs {
    prefs.proxy.password = None;
    prefs
}

/// IPC 命令：读取持久化应用偏好（异步在工作线程执行，避免文件锁与磁盘读阻塞主线程）。
#[tauri::command]
pub async fn get_app_prefs(app: AppHandle) -> Result<AppPrefs, String> {
    app.state::<AppState>().require_authenticated()?;
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        sanitize_app_prefs_for_ui(sync_prefs_from_disk_if_proxy(&state))
    })
    .await
    .map_err(|e| format!("读取应用偏好任务异常：{e}"))
}

/// IPC 命令：增量保存远控流画质、帧率、分辨率与 HUD 显隐偏好。
#[tauri::command]
pub async fn save_stream_prefs(
    patch: StreamPrefsPatch,
    app: AppHandle,
) -> Result<AppPrefs, String> {
    let state_gate = app.state::<AppState>();
    state_gate.require_unlocked()?;
    state_gate.require_authenticated()?;
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        save_stream_prefs_to_path(&state, patch, &app_prefs::default_prefs_path())
            .map(sanitize_app_prefs_for_ui)
    })
    .await
    .map_err(|e| format!("保存偏好设置任务异常：{e}"))?
}

/// IPC 命令：获取已连接设备的历史快照与壁纸 Data URL 视图列表。
#[tauri::command]
pub fn get_device_snapshots(state: State<'_, AppState>) -> Result<Vec<DeviceSnapshotView>, String> {
    state.require_authenticated()?;
    let wallpapers_dir = app_prefs::default_wallpapers_dir();
    let prefs = sync_prefs_from_disk_if_proxy(&state);
    Ok(prefs.snapshot_views(&wallpapers_dir))
}

/// IPC 命令：获取当前活动远控会话只读状态快照（供主窗显示控制状态）。
#[tauri::command]
pub fn get_active_session_info(state: State<'_, AppState>) -> ActiveSessionInfo {
    state.active_session_info()
}

/// IPC 命令：在后台工作线程探测信令端口与中继端口的 TCP 连通性，返回实测 RTT 文案。
#[tauri::command]
pub async fn test_server_connection(
    server_host: String,
    signal_port: u16,
    relay_port: u16,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let (clean_host, effective_signal_port) =
            server_config::parse_server_input(&server_host, signal_port)?;
        signal_client::probe_server_connectivity(
            &clean_host,
            effective_signal_port,
            relay_port,
            Duration::from_secs(5),
        )
    })
    .await
    .map_err(|e| format!("连通性测试任务异常：{e}"))?
}

// ─────────────────────────────────────────────────────────────────
// 首次启动双阶强门禁、账号认证、我的设备与安全偏好 IPC 命令
// ─────────────────────────────────────────────────────────────────

/// 探测并保存服务器配置请求负载。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeAndSaveServerRequest {
    /// 服务器域名、IP 或 `host:port`。
    pub server_host: String,
    /// HTTP/HTTPS API 端口（默认 `21118`）。
    #[serde(default)]
    pub api_port: Option<u16>,
    /// 信令端口（默认 `21116`）。
    #[serde(default)]
    pub signal_port: Option<u16>,
    /// 中继端口（默认 `21117`）。
    #[serde(default)]
    pub relay_port: Option<u16>,
    /// API 通信是否使用 HTTPS（默认 `true`）。
    #[serde(default = "default_true")]
    pub use_https: bool,
    /// 可选期望 Root CA SHA-256 指纹（用于 TOFU 二次确认锁定）。
    #[serde(default)]
    pub expected_ca_fingerprint_sha256: Option<String>,
    /// 是否将探测成功的配置与 CA 锚定保存至 `server.json`（默认 `true`）。
    #[serde(default = "default_true")]
    pub save: bool,
}

/// 探测并保存服务器响应负载。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeServerResponse {
    /// 规范化后的服务器主机名或 IP。
    pub server_host: String,
    /// API 端口。
    pub api_port: u16,
    /// 信令端口。
    pub signal_port: u16,
    /// 中继端口。
    pub relay_port: u16,
    /// 是否启用 HTTPS。
    pub use_https: bool,
    /// 服务端健康状态。
    pub status: String,
    /// 服务端版本号。
    pub version: String,
    /// 探测往返延迟（毫秒）。
    pub latency_ms: u64,
    /// 服务端是否启用了 TLS。
    pub tls_enabled: bool,
    /// Root CA 证书 SHA-256 指纹。
    pub ca_fingerprint_sha256: Option<String>,
    /// TLS 叶子证书 SHA-256 指纹。
    pub tls_leaf_fingerprint_sha256: Option<String>,
    /// 服务端是否开放自助注册。
    pub allow_registration: bool,
    /// 本次是否已持久化保存。
    pub saved: bool,
    /// 最新门禁状态快照。
    pub gate_status: GateStatusSnapshot,
}

/// 认证公共配置响应。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthConfigView {
    /// 是否开放自助注册。
    pub allow_registration: bool,
}

/// 邮箱登录请求负载。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthLoginRequest {
    /// 登录邮箱。
    pub email: String,
    /// 登录密码。
    pub password: String,
}

/// 邮箱注册请求负载。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthRegisterRequest {
    /// 注册邮箱。
    pub email: String,
    /// 登录密码。
    pub password: String,
    /// 可选显示名称。
    #[serde(default)]
    pub display_name: Option<String>,
}

/// 修改密码请求负载。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthChangePasswordRequest {
    /// 当前密码。
    pub current_password: String,
    /// 新密码。
    pub new_password: String,
}

/// 「我的设备」列表条目（面向前端的 `camelCase` DTO）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MyDeviceEntry {
    /// 9 位纯数字设备码。
    pub device_id: String,
    /// 格式化设备码（如 `"604 928 235"`）。
    pub formatted_id: String,
    /// 主机名。
    pub hostname: String,
    /// 操作系统标识（`"macos"` / `"windows"` / `"linux"`）。
    pub os: String,
    /// 客户端上报的语义版本号（旧版本客户端为空串）。
    pub app_version: String,
    /// 客户端上报的 CPU 架构（如 `"arm64"` / `"x86_64"`，旧版本客户端为空串）。
    pub arch: String,
    /// 用户自定义设备备注。
    pub alias: String,
    /// 展示名称（优先 `alias`，其次 `hostname`，兜底 `formatted_id`）。
    pub display_name: String,
    /// 是否允许他人远程协助。
    pub allow_remote_assist: bool,
    /// 是否允许被远程控制（含同账号本人自控）。
    pub allow_remote_control: bool,
    /// 是否已被禁用。
    pub disabled: bool,
    /// 是否在线。
    pub online: bool,
    /// 是否正处于会话中。
    pub in_session: bool,
    /// 是否当前本机。
    pub is_local: bool,
    /// 是否具备 Wake-on-LAN 远程开机能力（目标已开启且已上报合法单播 MAC）。
    pub wol_capable: bool,
    /// 绑定时间（RFC3339）。
    pub bound_at: Option<String>,
    /// 最近活跃时间（RFC3339）。
    pub last_seen_at: Option<String>,
}

/// 安全设置偏好快照（面向「设置 → 安全」与 S3 控制中心）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecurityPrefsSnapshot {
    /// 默认开启端到端加密连接 (mTLS)。
    pub default_encrypt_connection: bool,
    /// 是否开启允许他人远程协助。
    pub allow_remote_assist: bool,
    /// 他人远程协助本机时是否允许文件互传（默认 `true`；同账号本人自控始终豁免）。
    pub allow_assist_file_transfer: bool,
    /// 是否显示局域网设备（默认关闭）。
    pub show_lan_devices: bool,
    /// 本机设备证书状态。
    pub cert_status: GateCertStatus,
}

/// 桌面端安全设置偏好补丁（包含核心 `SecurityPrefsPatch` 与 Issue #6 文件互传门控）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UiSecurityPrefsPatch {
    /// 可选「允许其他设备远程协助本机」开关。
    pub allow_remote_assist: Option<bool>,
    /// 可选「被远程协助时允许文件互传」开关。
    pub allow_assist_file_transfer: Option<bool>,
    /// 可选「默认开启加密连接（mTLS）」开关。
    pub default_encrypt_connection: Option<bool>,
    /// 可选「显示局域网设备」开关。
    pub show_lan_devices: Option<bool>,
    /// 可选自定义访问验证码。
    pub custom_host_password: Option<String>,
    /// 为 `Some(true)` 时清除自定义密码并切回临时验证码模式。
    pub clear_custom_host_password: Option<bool>,
}

impl From<SecurityPrefsPatch> for UiSecurityPrefsPatch {
    fn from(p: SecurityPrefsPatch) -> Self {
        Self {
            allow_remote_assist: p.allow_remote_assist,
            allow_assist_file_transfer: None,
            default_encrypt_connection: p.default_encrypt_connection,
            show_lan_devices: p.show_lan_devices,
            custom_host_password: p.custom_host_password,
            clear_custom_host_password: p.clear_custom_host_password,
        }
    }
}

impl UiSecurityPrefsPatch {
    fn apply_to(self, prefs: &mut AppPrefs) -> Result<(), String> {
        let allow_ft = self.allow_assist_file_transfer;
        prefs.apply_security_patch(SecurityPrefsPatch {
            allow_remote_assist: self.allow_remote_assist,
            default_encrypt_connection: self.default_encrypt_connection,
            show_lan_devices: self.show_lan_devices,
            custom_host_password: self.custom_host_password,
            clear_custom_host_password: self.clear_custom_host_password,
        })?;
        if let Some(v) = allow_ft {
            prefs.allow_assist_file_transfer = v;
        }
        Ok(())
    }
}

fn format_auth_error(err: AuthClientError) -> String {
    match err {
        AuthClientError::Api { code, message, .. } => {
            if !message.trim().is_empty() {
                message
            } else {
                format!("请求失败（{code}）")
            }
        }
        AuthClientError::ReloginRequired(msg) => {
            let trimmed = msg.trim();
            if trimmed.is_empty() {
                "登录已过期，请重新登录".to_string()
            } else if trimmed.contains("登录") {
                msg
            } else {
                format!("登录状态已失效，请重新登录（{trimmed}）")
            }
        }
        other => other.to_string(),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn map_my_device_entry(info: MyDeviceInfo, local_device_id: Option<&str>) -> MyDeviceEntry {
    let wol_capable = info.wol_capable;
    map_my_device_entry_with_wol(info, local_device_id, wol_capable)
}

/// 解析设备最终的 `wolCapable` 状态：
/// 1. 优先采用信令连接实时缓存的 `device_wol_capable_snapshot`（最新变更）；
/// 2. 未命中信令缓存时回退至 REST `/api/v1/me/devices` 返回的 `info.wol_capable`（确保已离线设备首次启动即可展示远程开机）；
/// 3. 本机设备无信令缓存时额外兼容本机实时 `local_wol_fallback`。
fn resolve_device_wol_capable(
    info: &MyDeviceInfo,
    is_local: bool,
    wol_map: &HashMap<String, bool>,
    local_wol_fallback: bool,
) -> bool {
    let norm_id = normalize_device_id_for_match(&info.device_id);
    wol_map.get(&norm_id).copied().unwrap_or(if is_local {
        info.wol_capable || local_wol_fallback
    } else {
        info.wol_capable
    })
}

fn map_my_device_entry_with_wol(
    info: MyDeviceInfo,
    local_device_id: Option<&str>,
    wol_capable: bool,
) -> MyDeviceEntry {
    let formatted_id = if info.formatted_id.trim().is_empty() {
        server_config::format_device_id(&info.device_id)
    } else {
        info.formatted_id.clone()
    };
    let alias_trimmed = info.alias.trim();
    let host_trimmed = info.hostname.trim();
    let display_name = if !alias_trimmed.is_empty() {
        alias_trimmed.to_string()
    } else if !host_trimmed.is_empty() {
        host_trimmed.to_string()
    } else {
        formatted_id.clone()
    };
    let is_local = local_device_id.is_some_and(|local| local == info.device_id);
    MyDeviceEntry {
        device_id: info.device_id,
        formatted_id,
        hostname: info.hostname,
        os: info.os,
        app_version: info.app_version,
        arch: info.arch,
        alias: info.alias,
        display_name,
        allow_remote_assist: info.allow_remote_assist,
        allow_remote_control: info.allow_remote_control,
        disabled: info.disabled,
        online: info.online,
        in_session: info.in_session,
        is_local,
        wol_capable,
        bound_at: info.bound_at,
        last_seen_at: info.last_seen_at,
    }
}

fn query_wol_capable_snapshot(state: &AppState) -> HashMap<String, bool> {
    if state.is_service_proxy() {
        match call_agent_ipc(
            ServiceRequest::GetWolCapableSnapshot,
            Duration::from_millis(400),
        ) {
            Ok(ServiceResponse::WolCapableSnapshot { devices }) => devices,
            _ => HashMap::new(),
        }
    } else {
        let client = {
            let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
            rt.signal_client.as_ref().cloned()
        };
        client
            .map(|c| c.device_wol_capable_snapshot())
            .unwrap_or_default()
    }
}

/// 对「我的设备」列表排序：在线设备置顶、离线设备沉底，
/// 同在线状态下本机置前并保留服务端返回的 `last_seen_at DESC, device_id ASC` 相对顺序。
fn sort_my_device_entries(entries: &mut [MyDeviceEntry]) {
    entries.sort_by(|a, b| {
        b.online
            .cmp(&a.online)
            .then_with(|| b.is_local.cmp(&a.is_local))
    });
}

fn sync_access_token_via_daemon_inner(
    auth: &AuthSessionManager,
    failed_token: Option<&str>,
    allow_initial_login: bool,
) -> Result<Option<String>, AuthClientError> {
    let daemon_sock = config_paths::daemon_socket_path();
    let secret_path = config_paths::ipc_secret_path();
    if !daemon_sock.exists() {
        return Ok(None);
    }
    match IpcClient::call(
        &daemon_sock,
        &secret_path,
        ServiceRequest::EnsureAccessToken {
            failed_token: failed_token.map(str::to_string),
        },
        Duration::from_secs(5),
    ) {
        Ok(ServiceResponse::AuthSession(sess)) => {
            let token = sess.access_token.clone();
            if allow_initial_login {
                auth.apply_external_session(*sess);
                Ok(Some(token))
            } else if auth.apply_external_session_if_logged_in(*sess) {
                Ok(Some(token))
            } else {
                Ok(None)
            }
        }
        Ok(ServiceResponse::ReloginRequired { message }) => {
            let _ = auth.reload_session_from_disk_without_cert_check();
            Err(AuthClientError::ReloginRequired(message))
        }
        Ok(ServiceResponse::Error { message }) => Err(AuthClientError::Api {
            status: 500,
            code: "daemon_refresh_error".to_string(),
            message,
        }),
        Ok(_) | Err(_) => Ok(None),
    }
}

fn sync_access_token_via_daemon(
    auth: &AuthSessionManager,
    failed_token: Option<&str>,
) -> Result<Option<String>, AuthClientError> {
    sync_access_token_via_daemon_inner(auth, failed_token, false)
}

/// 代理模式下控制端启动时优先通过 `daemon.sock` 复用单点维护的会话令牌，避免并发刷新轮换 token。
pub(crate) fn restore_proxy_session_from_daemon(
    auth: &AuthSessionManager,
) -> Result<Option<rc_ui::auth_client::StoredAuthSession>, AuthClientError> {
    match sync_access_token_via_daemon_inner(auth, None, true) {
        Ok(Some(_)) => Ok(auth.current_session()),
        Ok(None) => auth.restore_on_startup(),
        Err(err) => Err(err),
    }
}

fn with_valid_token_in_mode_raw<T>(
    auth: &AuthSessionManager,
    use_daemon_ipc: bool,
    f: impl Fn(&AuthHttpClient, &str) -> Result<T, AuthClientError>,
) -> Result<T, AuthClientError> {
    let cfg = auth.server_config();
    let client = AuthHttpClient::from_server_config(&cfg)?;
    let mut token = if use_daemon_ipc {
        match sync_access_token_via_daemon(auth, None)? {
            Some(tok) => tok,
            None => auth.ensure_valid_access_token()?,
        }
    } else {
        auth.ensure_valid_access_token()?
    };
    match f(&client, &token) {
        Ok(res) => Ok(res),
        Err(err) if err.is_unauthorized() => {
            token = if use_daemon_ipc {
                match sync_access_token_via_daemon(auth, Some(&token))? {
                    Some(tok) => tok,
                    None => auth.refresh_after_unauthorized(&token)?,
                }
            } else {
                auth.refresh_after_unauthorized(&token)?
            };
            f(&client, &token)
        }
        Err(err) => Err(err),
    }
}

fn handle_relogin_required_error(app: &AppHandle, err: AuthClientError) -> String {
    match err {
        AuthClientError::ReloginRequired(msg) => {
            let ui_err =
                AuthClientError::ReloginRequired(msg.clone()).to_ui_error(ErrorPlatform::Macos);
            let zh = format_auth_error(AuthClientError::ReloginRequired(msg));
            force_logout_from_kick_with_ui_error(app, zh.clone(), ui_err);
            zh
        }
        other => format_auth_error(other),
    }
}

fn handle_relogin_required_ui_error(app: &AppHandle, err: AuthClientError) -> UiCommandError {
    match err {
        AuthClientError::ReloginRequired(msg) => {
            let ui_err =
                AuthClientError::ReloginRequired(msg.clone()).to_ui_error(ErrorPlatform::Macos);
            let zh = format_auth_error(AuthClientError::ReloginRequired(msg));
            force_logout_from_kick_with_ui_error(app, zh.clone(), ui_err.clone());
            UiCommandError::from_ui_error_with_message(zh, ui_err)
        }
        other => UiCommandError::from_auth_error(&other),
    }
}

fn with_valid_token_in_app<T>(
    app: &AppHandle,
    state: &AppState,
    f: impl Fn(&AuthHttpClient, &str) -> Result<T, AuthClientError>,
) -> Result<T, String> {
    with_valid_token_in_mode_raw(&state.auth, state.is_service_proxy(), f)
        .map_err(|err| handle_relogin_required_error(app, err))
}

/// 启动后台认证维护线程（周期性轮换临期 Token 与 `< 48h` 设备证书自动续签）。
pub(crate) fn start_auth_maintainer(app: &AppHandle, state: &AppState) {
    if state.is_service_proxy() {
        return;
    }
    let mut slot = state
        .auth_maintainer
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        return;
    }
    let app_for_dev = app.clone();
    let app_for_renew = app.clone();
    let app_for_relogin = app.clone();
    let handle = state.auth.spawn_background_maintainer_with_callbacks(
        Duration::from_secs(60),
        move || {
            let st = app_for_dev.state::<AppState>();
            let rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
            rt.current_status()
                .device_id
                .or_else(|| rt.config.cached_device_id.clone())
        },
        move || {
            let st = app_for_renew.state::<AppState>();
            if st.gate_state() == AppGateState::Authenticated {
                let signal_missing = st
                    .server
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .signal_client
                    .is_none();
                if signal_missing {
                    post_auth_ready(&app_for_renew, &st);
                    return;
                }
            }
            refresh_host_security_runtime(&st);
            let _ = app_for_renew.emit("gate://status", &st.gate_status_snapshot());
        },
        move |reason| {
            let app_kick = app_for_relogin.clone();
            let _ = std::thread::Builder::new()
                .name("rc-auth-maintainer-kick".to_string())
                .spawn(move || {
                    force_logout_from_kick(&app_kick, reason);
                });
        },
    );
    *slot = Some(handle);
}

/// 停止并回收后台认证维护线程（同步等待，供应用退出清理使用）。
pub(crate) fn stop_auth_maintainer(state: &AppState) {
    let old = state
        .auth_maintainer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(mut handle) = old {
        handle.stop();
    }
}

/// 在后台短线程异步通知并回收认证维护线程，避免同步 HTTP 刷新阻塞登出或被踢清理。
fn stop_auth_maintainer_async(state: &AppState) {
    let old = state
        .auth_maintainer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(handle) = old {
        let _ = std::thread::Builder::new()
            .name("rc-auth-maintainer-reaper".to_string())
            .spawn(move || drop(handle));
    }
}

fn persist_cached_device_id(state: &AppState, path: &Path, device_id: &str) -> Result<(), String> {
    let _io_guard = state
        .server_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let next_cfg = {
        let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        if rt.config.cached_device_id.as_deref() == Some(device_id) {
            return Ok(());
        }
        rt.config.cached_device_id = Some(device_id.to_string());
        rt.config.clone()
    };
    server_config::save_to_path(path, &next_cfg)?;
    state.auth.update_server_config(next_cfg);
    Ok(())
}

fn bind_device_and_ensure_cert(
    state: &AppState,
    server_cfg_path: &Path,
    kick_app: Option<&AppHandle>,
) -> Result<String, String> {
    let map_auth_err = |err: AuthClientError| match kick_app {
        Some(app) => handle_relogin_required_error(app, err),
        None => format_auth_error(err),
    };
    let cfg = state.auth.server_config();
    let pubkey_hex = state.auth.cert_manager().device_pubkey_hex();
    let allow_remote_assist = sync_prefs_from_disk_if_proxy(state).allow_remote_assist;
    let hostname = lan_discovery::local_hostname();
    let os_name = std::env::consts::OS.to_string();

    let bound =
        with_valid_token_in_mode_raw(&state.auth, state.is_service_proxy(), |client, token| {
            client.bind_device(
                token,
                &BindDeviceRequest {
                    device_key: cfg.device_key.clone(),
                    hostname: Some(hostname.clone()),
                    os: Some(os_name.clone()),
                    app_version: Some(rc_ui::updater_macos::resolve_current_app_version()),
                    arch: Some(rc_ui::updater_macos::current_macos_arch().to_string()),
                    alias: None,
                    device_pubkey: Some(pubkey_hex.clone()),
                    allow_remote_assist: Some(allow_remote_assist),
                },
            )
        })
        .map_err(map_auth_err)?;

    persist_cached_device_id(state, server_cfg_path, &bound.device_id)?;
    if state.is_service_proxy() {
        let _ = sync_access_token_via_daemon(&state.auth, None).map_err(map_auth_err)?;
    }
    state
        .auth
        .ensure_device_cert(&bound.device_id)
        .map_err(map_auth_err)?;
    Ok(bound.device_id)
}

/// 认证就绪后激活后台维护、信令长连接与本机常驻被控监听（支持仅本人自控通道或完整远程协助），并向前端推送最新门禁/服务器状态。
pub(crate) fn post_auth_ready(app: &AppHandle, state: &AppState) {
    post_auth_ready_with_reload(app, state, false);
}

fn post_auth_ready_with_reload(app: &AppHandle, state: &AppState, reload_proxy: bool) {
    if state.gate_state() != AppGateState::Authenticated {
        let _ = app.emit("gate://status", &state.gate_status_snapshot());
        return;
    }
    if state.is_service_proxy() {
        if reload_proxy {
            notify_background_services_reload_sync(state, true);
        }
        let snap = get_server_config_inner(state);
        if let Some(lan) = state.lan.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            lan.set_local_device_id(snap.device_id.clone());
        }
        let _ = app.emit("server://status", &snap);
        let _ = app.emit("gate://status", &state.gate_status_snapshot());
        return;
    }
    start_auth_maintainer(app, state);
    let snap = start_signal_client_if_enabled(state);
    if let Some(lan) = state.lan.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        lan.set_local_device_id(snap.device_id.clone());
    }
    auto_start_host_if_permitted(state);
    refresh_host_security_runtime(state);
    let host_snap = state
        .host
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .snapshot();
    if host_snap.enabled {
        if let (Some(listen_addr), Some(display_addr)) =
            (host_snap.listen_addr, host_snap.display_addr)
        {
            let _ = app.emit(
                "host://update",
                &HostUpdate::Listening {
                    listen_addr,
                    display_addr,
                    backend: host_snap.backend,
                },
            );
        }
    }
    let _ = app.emit("server://status", &snap);
    let _ = app.emit("gate://status", &state.gate_status_snapshot());
}

fn probe_and_save_server_to_path_ui(
    state: &AppState,
    req: ProbeAndSaveServerRequest,
    path: &Path,
) -> Result<ProbeServerResponse, UiCommandError> {
    let default_api_port = req.api_port.unwrap_or(server_config::DEFAULT_API_PORT);
    let signal_port = req
        .signal_port
        .unwrap_or(server_config::DEFAULT_SIGNAL_PORT);
    let relay_port = req.relay_port.unwrap_or(server_config::DEFAULT_RELAY_PORT);
    if default_api_port == 0 || signal_port == 0 || relay_port == 0 {
        return Err(UiCommandError::from_validation_error(
            "API 端口、信令端口与中继端口必须为 1–65535",
        ));
    }

    let (clean_host, effective_api_port) =
        server_config::parse_server_input(&req.server_host, default_api_port)
            .map_err(UiCommandError::from_validation_error)?;

    let client = AuthHttpClient::new(clean_host.clone(), effective_api_port, req.use_https, None)
        .map_err(|e| UiCommandError::from_auth_error(&e))?;
    let probe = client
        .probe_server(req.expected_ca_fingerprint_sha256.as_deref())
        .map_err(|e| UiCommandError::from_auth_error(&e))?;

    *state
        .allow_registration
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = probe.allow_registration;

    let ca_fp = probe
        .ca_bundle
        .as_ref()
        .map(|b| b.ca_fingerprint_sha256.clone())
        .or_else(|| probe.health.ca_fingerprint_sha256.clone());

    let mut endpoint_logout_task: Option<(
        ServerConfig,
        Option<rc_ui::auth_client::StoredAuthSession>,
        Option<String>,
    )> = None;

    if req.save {
        let _io_guard = state
            .server_io_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let (next_cfg, endpoint_changed, old_did) = {
            let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
            let same_endpoint = rt.config.server_host == clean_host
                && rt.config.api_port == effective_api_port
                && rt.config.use_https == req.use_https
                && rt.config.ca_fingerprint_sha256 == ca_fp;
            let old_did = rt
                .current_status()
                .device_id
                .or_else(|| rt.config.cached_device_id.clone());
            let next = ServerConfig {
                enabled: true,
                server_host: clean_host.clone(),
                signal_port,
                relay_port,
                api_port: effective_api_port,
                use_https: req.use_https,
                ca_fingerprint_sha256: ca_fp.clone(),
                ca_cert_pem: probe.ca_bundle.as_ref().map(|b| b.ca_cert_pem.clone()),
                server_auth_pubkey_hex: probe.ca_bundle.as_ref().map(|b| b.auth_pubkey_hex.clone()),
                device_key: rt.config.device_key.clone(),
                cached_device_id: if same_endpoint {
                    rt.config.cached_device_id.clone()
                } else {
                    None
                },
            };
            (next, !same_endpoint, old_did)
        };

        server_config::save_to_path(path, &next_cfg).map_err(|err| {
            UiCommandError::from_ui_error_with_message(
                err.clone(),
                ui_error::resolve_ui_error(
                    codes::VALIDATION_INVALID_CONFIG,
                    ErrorPlatform::Macos,
                    &[],
                    None,
                    Some(err),
                ),
            )
        })?;
        if endpoint_changed {
            let (old_cfg, prev_sess) = state.auth.take_logout_context_and_wipe();
            if prev_sess.is_some() {
                endpoint_logout_task = Some((old_cfg, prev_sess, old_did));
            }
        }
        state.auth.update_server_config(next_cfg.clone());

        let old = {
            let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
            rt.config = next_cfg;
            rt.config_epoch = rt.config_epoch.wrapping_add(1);
            rt.signal_client.take()
        };
        drop(_io_guard);
        sync_running_host_signal_client(state, None);
        reap_signal_client_async(old);
        notify_background_services_reload(state, true);
        if let Some((old_cfg, prev_sess, old_did)) = endpoint_logout_task {
            let _ = std::thread::Builder::new()
                .name("rc-probe-old-logout".to_string())
                .spawn(move || {
                    let _ = AuthSessionManager::notify_remote_logout(
                        &old_cfg,
                        prev_sess.as_ref(),
                        old_did.as_deref(),
                    );
                });
        }
    }

    Ok(ProbeServerResponse {
        server_host: clean_host,
        api_port: effective_api_port,
        signal_port,
        relay_port,
        use_https: req.use_https,
        status: probe.health.status,
        version: probe.health.version,
        latency_ms: probe.latency_ms,
        tls_enabled: probe.health.tls_enabled,
        ca_fingerprint_sha256: ca_fp,
        tls_leaf_fingerprint_sha256: probe.tls_leaf_fingerprint_sha256,
        allow_registration: probe.allow_registration,
        saved: req.save,
        gate_status: state.gate_status_snapshot(),
    })
}

#[cfg(test)]
fn probe_and_save_server_to_path(
    state: &AppState,
    req: ProbeAndSaveServerRequest,
    path: &Path,
) -> Result<ProbeServerResponse, String> {
    probe_and_save_server_to_path_ui(state, req, path).map_err(|e| e.message)
}

/// IPC 命令：查询首次启动双阶门禁状态、服务器配置、当前登录用户与设备证书状态。
#[tauri::command]
pub fn get_gate_status(state: State<'_, AppState>) -> GateStatusSnapshot {
    state.gate_status_snapshot()
}

/// IPC 命令：探测服务器 `/api/v1/health` 与 `/api/v1/pki/ca`，校验并保存 CA 指纹与服务器配置。
#[tauri::command]
pub async fn probe_and_save_server(
    req: ProbeAndSaveServerRequest,
    app: AppHandle,
) -> Result<ProbeServerResponse, UiCommandError> {
    app.state::<AppState>()
        .require_unlocked()
        .map_err(UiCommandError::from_auth_gate_error)?;
    let app_for_blocking = app.clone();
    let res = tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        probe_and_save_server_to_path_ui(&state, req, &server_config::default_config_path())
    })
    .await
    .map_err(|e| {
        let msg = format!("探测服务器任务异常：{e}");
        UiCommandError::from_ui_error_with_message(
            msg.clone(),
            ui_error::classify_signal_error(&msg, ErrorPlatform::Macos),
        )
    })??;
    if res.saved {
        let state = app.state::<AppState>();
        if state.gate_state() != AppGateState::Authenticated {
            stop_auth_maintainer_async(&state);
            disconnect_active_session(&app);
            window::close_session_and_tool_windows(&app);
            let (host_snap, cancelled_gen) = stop_host_service_with_cancelled(&state, false);
            state
                .host
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .allow_owner_control = true;
            if let Some(generation) = cancelled_gen {
                let _ = app.emit(
                    "host://update",
                    &HostUpdate::ApprovalCancelled { generation },
                );
            }
            if !host_snap.enabled {
                let _ = app.emit("host://update", &HostUpdate::Stopped);
            }
        }
        let _ = app.emit("gate://status", &state.gate_status_snapshot());
    }
    Ok(res)
}

/// IPC 命令：刷新并返回服务端 `allowRegistration` 公共配置。
#[tauri::command]
pub async fn fetch_auth_config(app: AppHandle) -> Result<AuthConfigView, UiCommandError> {
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        let cfg = state.auth.server_config();
        let client = AuthHttpClient::from_server_config(&cfg)
            .map_err(|e| UiCommandError::from_auth_error(&e))?;
        let auth_cfg = client
            .fetch_auth_config()
            .map_err(|e| UiCommandError::from_auth_error(&e))?;
        *state
            .allow_registration
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = auth_cfg.allow_registration;
        Ok(AuthConfigView {
            allow_registration: auth_cfg.allow_registration,
        })
    })
    .await
    .map_err(|e| {
        let msg = format!("拉取认证配置任务异常：{e}");
        UiCommandError::from_ui_error_with_message(
            msg.clone(),
            ui_error::classify_signal_error(&msg, ErrorPlatform::Macos),
        )
    })?
}

/// IPC 命令：注册新账号并在成功后签发设备证书、绑定设备及启动后台服务。
#[tauri::command]
pub async fn auth_register(
    req: AuthRegisterRequest,
    app: AppHandle,
) -> Result<GateStatusSnapshot, UiCommandError> {
    app.state::<AppState>()
        .require_unlocked()
        .map_err(UiCommandError::from_auth_gate_error)?;
    if req.email.trim().is_empty() {
        return Err(UiCommandError::from_validation_error("请输入邮箱地址"));
    }
    if req.password.is_empty() {
        return Err(UiCommandError::from_validation_error("请输入密码"));
    }
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        if state.gate_state() == AppGateState::Unconfigured {
            return Err(UiCommandError::from_auth_gate_error(
                "请先配置并连接 RabbitControl 服务器",
            ));
        }
        let hostname = lan_discovery::local_hostname();
        let os_name = std::env::consts::OS;
        let sess = state
            .auth
            .register(
                &req.email,
                &req.password,
                req.display_name.as_deref(),
                Some(&hostname),
                Some(os_name),
            )
            .map_err(|e| UiCommandError::from_auth_error(&e))?;
        if !sess.must_change_password {
            if let Err(err) =
                bind_device_and_ensure_cert(&state, &server_config::default_config_path(), None)
            {
                let _ = perform_logout_cleanup(&app_for_blocking, false);
                return Err(UiCommandError::from_ui_error_with_message(
                    err.clone(),
                    ui_error::classify_signal_error(&err, ErrorPlatform::Macos),
                ));
            }
            post_auth_ready_with_reload(&app_for_blocking, &state, true);
        } else {
            let _ = app_for_blocking.emit("gate://status", &state.gate_status_snapshot());
        }
        Ok(state.gate_status_snapshot())
    })
    .await
    .map_err(|e| {
        let msg = format!("注册任务异常：{e}");
        UiCommandError::from_ui_error_with_message(
            msg.clone(),
            ui_error::classify_signal_error(&msg, ErrorPlatform::Macos),
        )
    })?
}

/// IPC 命令：邮箱密码登录并在成功后签发设备证书、绑定设备及启动后台服务。
#[tauri::command]
pub async fn auth_login(
    req: AuthLoginRequest,
    app: AppHandle,
) -> Result<GateStatusSnapshot, UiCommandError> {
    app.state::<AppState>()
        .require_unlocked()
        .map_err(UiCommandError::from_auth_gate_error)?;
    if req.email.trim().is_empty() {
        return Err(UiCommandError::from_validation_error("请输入邮箱地址"));
    }
    if req.password.is_empty() {
        return Err(UiCommandError::from_validation_error("请输入密码"));
    }
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        if state.gate_state() == AppGateState::Unconfigured {
            return Err(UiCommandError::from_auth_gate_error(
                "请先配置并连接 RabbitControl 服务器",
            ));
        }
        let hostname = lan_discovery::local_hostname();
        let os_name = std::env::consts::OS;
        let sess = state
            .auth
            .login(&req.email, &req.password, Some(&hostname), Some(os_name))
            .map_err(|e| UiCommandError::from_auth_error(&e))?;
        if !sess.must_change_password {
            if let Err(err) =
                bind_device_and_ensure_cert(&state, &server_config::default_config_path(), None)
            {
                let _ = perform_logout_cleanup(&app_for_blocking, false);
                return Err(UiCommandError::from_ui_error_with_message(
                    err.clone(),
                    ui_error::classify_signal_error(&err, ErrorPlatform::Macos),
                ));
            }
            post_auth_ready_with_reload(&app_for_blocking, &state, true);
        } else {
            let _ = app_for_blocking.emit("gate://status", &state.gate_status_snapshot());
        }
        Ok(state.gate_status_snapshot())
    })
    .await
    .map_err(|e| {
        let msg = format!("登录任务异常：{e}");
        UiCommandError::from_ui_error_with_message(
            msg.clone(),
            ui_error::classify_signal_error(&msg, ErrorPlatform::Macos),
        )
    })?
}

/// IPC 命令：修改当前登录账号密码；若由首次强制改密解锁则立即完成设备绑定与证书签发。
#[tauri::command]
pub async fn auth_change_password(
    req: AuthChangePasswordRequest,
    app: AppHandle,
) -> Result<GateStatusSnapshot, UiCommandError> {
    app.state::<AppState>()
        .require_unlocked()
        .map_err(UiCommandError::from_auth_gate_error)?;
    if req.current_password.is_empty() || req.new_password.is_empty() {
        return Err(UiCommandError::from_validation_error(
            "请填写当前密码与新密码",
        ));
    }
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        if state.auth.current_session().is_none() {
            return Err(UiCommandError::from_auth_gate_error("当前未登录"));
        }
        let prev_must_change = state
            .auth
            .current_session()
            .is_some_and(|s| s.must_change_password);
        if state.is_service_proxy() {
            let _ = sync_access_token_via_daemon(&state.auth, None)
                .map_err(|err| handle_relogin_required_ui_error(&app_for_blocking, err))?;
        }
        let updated = state
            .auth
            .change_password(&req.current_password, &req.new_password)
            .map_err(|err| handle_relogin_required_ui_error(&app_for_blocking, err))?;
        if !updated.must_change_password {
            if !prev_must_change {
                let _ = state.auth.cert_manager().clear_device_cert();
            }
            if let Err(err) =
                bind_device_and_ensure_cert(&state, &server_config::default_config_path(), None)
            {
                let _ = perform_logout_cleanup(&app_for_blocking, false);
                return Err(UiCommandError::from_ui_error_with_message(
                    err.clone(),
                    ui_error::classify_signal_error(&err, ErrorPlatform::Macos),
                ));
            }
            post_auth_ready_with_reload(&app_for_blocking, &state, true);
        } else {
            let _ = app_for_blocking.emit("gate://status", &state.gate_status_snapshot());
        }
        Ok(state.gate_status_snapshot())
    })
    .await
    .map_err(|e| {
        let msg = format!("修改密码任务异常：{e}");
        UiCommandError::from_ui_error_with_message(
            msg.clone(),
            ui_error::classify_signal_error(&msg, ErrorPlatform::Macos),
        )
    })?
}

fn perform_logout_cleanup(app: &AppHandle, notify_remote: bool) -> GateStatusSnapshot {
    let state = app.state::<AppState>();
    let device_id = local_device_id_snapshot(&state);
    *state
        .last_session_params
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    let (old_cfg, prev_sess) = state.auth.take_logout_context_and_wipe();

    stop_auth_maintainer_async(&state);
    disconnect_active_session(app);
    window::close_session_and_tool_windows(app);
    let (host_snap, cancelled_gen) = stop_host_service_with_cancelled(&state, false);
    state
        .host
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .allow_owner_control = true;
    if let Some(generation) = cancelled_gen {
        let _ = app.emit(
            "host://update",
            &HostUpdate::ApprovalCancelled { generation },
        );
    }
    if !host_snap.enabled {
        let _ = app.emit("host://update", &HostUpdate::Stopped);
    }
    let srv_snap = stop_signal_client_for_logout(&state);
    // 先同步通知后台服务清空凭据与停机，避免随后阻塞式远程 /logout HTTP 调用期间被控端仍接收连接。
    notify_background_services_reload_sync(&state, true);
    if notify_remote && prev_sess.is_some() {
        let _ = AuthSessionManager::notify_remote_logout(
            &old_cfg,
            prev_sess.as_ref(),
            device_id.as_deref(),
        );
    }

    let gate_snap = state.gate_status_snapshot();
    sync_standalone_status_bar(&state);
    let _ = app.emit("server://status", &srv_snap);
    let _ = app.emit("gate://status", &gate_snap);
    gate_snap
}

/// 当账号被管理员禁用/删除/踢线时，强制清理本地凭据与证书并回退门禁至 `Unauthenticated`。
pub(crate) fn force_logout_from_kick(app: &AppHandle, reason: String) {
    let ui_error = ui_error::classify_signal_error(&reason, ErrorPlatform::Macos);
    force_logout_from_kick_with_ui_error(app, reason, ui_error);
}

fn force_logout_from_kick_with_ui_error(app: &AppHandle, reason: String, ui_error: UiError) {
    let _ = perform_logout_cleanup(app, false);
    let _ = app.emit(
        "auth://kicked",
        serde_json::json!({ "reason": reason, "uiError": ui_error }),
    );
}

/// IPC 命令：退出当前账号登录，清空凭据与设备证书、停信令与被控端并回退门禁至 `Unauthenticated`。
#[tauri::command]
pub async fn auth_logout(app: AppHandle) -> Result<GateStatusSnapshot, String> {
    app.state::<AppState>().require_unlocked()?;
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || perform_logout_cleanup(&app_for_blocking, true))
        .await
        .map_err(|e| format!("退出登录任务异常：{e}"))
}

/// IPC 命令：列出当前账号下绑定的所有设备（`/api/v1/me/devices`）。
#[tauri::command]
pub async fn list_my_devices(app: AppHandle) -> Result<Vec<MyDeviceEntry>, String> {
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        state.require_authenticated()?;
        let local_did = local_device_id_snapshot(&state);
        let list = with_valid_token_in_app(&app_for_blocking, &state, |client, token| {
            client.list_my_devices(token)
        })?;
        let wol_map = query_wol_capable_snapshot(&state);
        let local_wol_fallback = sync_prefs_from_disk_if_proxy(&state).allow_wol
            && rc_ui::platform::primary_mac_address().is_some();
        let mut entries: Vec<MyDeviceEntry> = list
            .into_iter()
            .map(|d| {
                let norm_id = normalize_device_id_for_match(&d.device_id);
                let is_local = local_did
                    .as_deref()
                    .is_some_and(|local| normalize_device_id_for_match(local) == norm_id);
                let wol_capable =
                    resolve_device_wol_capable(&d, is_local, &wol_map, local_wol_fallback);
                map_my_device_entry_with_wol(d, local_did.as_deref(), wol_capable)
            })
            .collect();
        sort_my_device_entries(&mut entries);
        Ok(entries)
    })
    .await
    .map_err(|e| format!("拉取我的设备列表任务异常：{e}"))?
}

/// IPC 命令：修改「我的设备」中指定设备的备注名称。
#[tauri::command]
pub async fn rename_my_device(
    device_id: String,
    alias: String,
    app: AppHandle,
) -> Result<MyDeviceEntry, String> {
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        state.require_unlocked()?;
        state.require_authenticated()?;
        let local_did = local_device_id_snapshot(&state);
        let updated = with_valid_token_in_app(&app_for_blocking, &state, |client, token| {
            client.rename_device(token, &device_id, &alias)
        })?;
        let wol_map = query_wol_capable_snapshot(&state);
        let norm_id = normalize_device_id_for_match(&updated.device_id);
        let is_local = local_did
            .as_deref()
            .is_some_and(|local| normalize_device_id_for_match(local) == norm_id);
        let local_wol_fallback = is_local
            && sync_prefs_from_disk_if_proxy(&state).allow_wol
            && rc_ui::platform::primary_mac_address().is_some();
        let wol_capable =
            resolve_device_wol_capable(&updated, is_local, &wol_map, local_wol_fallback);
        Ok(map_my_device_entry_with_wol(
            updated,
            local_did.as_deref(),
            wol_capable,
        ))
    })
    .await
    .map_err(|e| format!("修改设备备注任务异常：{e}"))?
}

/// IPC 命令：从「我的设备」解绑并移除指定设备。
#[tauri::command]
pub async fn remove_my_device(device_id: String, app: AppHandle) -> Result<(), String> {
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        state.require_unlocked()?;
        state.require_authenticated()?;
        let norm_target = server_config::parse_device_id_input(&device_id)
            .ok_or_else(|| format!("设备码格式无效：{device_id:?}"))?;
        let local_did = local_device_id_snapshot(&state);
        with_valid_token_in_app(&app_for_blocking, &state, |client, token| {
            client.remove_device(token, &norm_target)
        })?;
        if local_did.as_deref() == Some(norm_target.as_str()) {
            let _ = state.auth.cert_manager().clear_device_cert();
            refresh_host_security_runtime(&state);
            notify_background_services_reload(&state, true);
            let _ = app_for_blocking.emit("gate://status", &state.gate_status_snapshot());
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("移除设备任务异常：{e}"))?
}

fn security_prefs_snapshot(state: &AppState) -> SecurityPrefsSnapshot {
    let prefs = sync_prefs_from_disk_if_proxy(state);
    SecurityPrefsSnapshot {
        default_encrypt_connection: prefs.default_encrypt_connection,
        allow_remote_assist: prefs.allow_remote_assist,
        allow_assist_file_transfer: prefs.allow_assist_file_transfer,
        show_lan_devices: prefs.show_lan_devices,
        cert_status: state.cert_status(),
    }
}

#[cfg(test)]
fn save_security_prefs_to_path(
    state: &AppState,
    patch: impl Into<UiSecurityPrefsPatch>,
    path: &Path,
) -> Result<SecurityPrefsSnapshot, String> {
    let patch: UiSecurityPrefsPatch = patch.into();
    let _io = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let next = {
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        patch.apply_to(&mut prefs)?;
        prefs.clone()
    };
    app_prefs::save_to_path(path, &next)?;
    Ok(SecurityPrefsSnapshot {
        default_encrypt_connection: next.default_encrypt_connection,
        allow_remote_assist: next.allow_remote_assist,
        allow_assist_file_transfer: next.allow_assist_file_transfer,
        show_lan_devices: next.show_lan_devices,
        cert_status: state.cert_status(),
    })
}

/// IPC 命令：读取安全与 mTLS 加密偏好设置（异步在工作线程执行，避免文件锁与证书解析阻塞主线程）。
#[tauri::command]
pub async fn get_security_prefs(app: AppHandle) -> Result<SecurityPrefsSnapshot, String> {
    app.state::<AppState>().require_authenticated()?;
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        security_prefs_snapshot(&state)
    })
    .await
    .map_err(|e| format!("读取安全偏好任务异常：{e}"))
}

/// IPC 命令：保存安全与 mTLS 加密偏好设置，并实时同步被控端策略（始终保留同账号本人免密自控监听）。
#[tauri::command]
pub async fn save_security_prefs(
    patch: UiSecurityPrefsPatch,
    app: AppHandle,
) -> Result<SecurityPrefsSnapshot, String> {
    let state_gate = app.state::<AppState>();
    state_gate.require_unlocked()?;
    state_gate.require_authenticated()?;
    let assist_changed = patch.allow_remote_assist;
    let assist_ft_changed = patch.allow_assist_file_transfer;
    let app_for_blocking = app.clone();
    let snap = tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        let snap = {
            let prefs_path = app_prefs::default_prefs_path();
            let _io = state
                .prefs_io_lock
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let _file_lock = config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
            let (prev, next) = {
                let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
                let prev = prefs.clone();
                if state.is_service_proxy() && prefs_path.exists() {
                    *prefs = app_prefs::load_from_path(&prefs_path);
                }
                patch.apply_to(&mut prefs)?;
                (prev, prefs.clone())
            };
            if let Err(err) = app_prefs::save_to_path(&prefs_path, &next) {
                *state.prefs.lock().unwrap_or_else(|e| e.into_inner()) = prev;
                return Err(err);
            }
            if state.gate_state() == AppGateState::Authenticated && !state.is_service_proxy() {
                let host_missing = state
                    .host
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .handle
                    .is_none();
                if host_missing {
                    let perms = permissions::check_permissions();
                    if permissions::ensure_host_ready(&perms).is_ok() {
                        let default_addr =
                            SocketAddr::from(([0, 0, 0, 0], host_bridge::DEFAULT_HOST_PORT));
                        let _ = start_host_service_with_policy(
                            &state,
                            default_addr,
                            true,
                            next.allow_remote_assist,
                        );
                    }
                } else {
                    if !next.allow_remote_assist {
                        let cancelled_gen = {
                            let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
                            host.allow_remote_assist = false;
                            let cancelled = host.pending_approval.take().map(|p| p.generation);
                            if let Some(handle) = host.handle.as_ref() {
                                if let Some(gen) = cancelled {
                                    let _ = handle.respond_approval(gen, false);
                                }
                                handle.set_allow_remote_assist(false);
                            }
                            cancelled
                        };
                        if let Some(generation) = cancelled_gen {
                            let _ = app_for_blocking.emit(
                                "host://update",
                                &HostUpdate::ApprovalCancelled { generation },
                            );
                        }
                    }
                    refresh_host_security_runtime(&state);
                }
            }
            SecurityPrefsSnapshot {
                default_encrypt_connection: next.default_encrypt_connection,
                allow_remote_assist: next.allow_remote_assist,
                allow_assist_file_transfer: next.allow_assist_file_transfer,
                show_lan_devices: next.show_lan_devices,
                cert_status: state.cert_status(),
            }
        };
        if let Some(allow) = assist_changed {
            sync_assist_state_to_server(&app_for_blocking, &state, allow);
        }
        if state.is_service_proxy() {
            if let Some(allow_ft) = assist_ft_changed {
                match call_agent_ipc(
                    ServiceRequest::SetAssistFileTransfer { enabled: allow_ft },
                    Duration::from_secs(2),
                ) {
                    Ok(ServiceResponse::Snapshot(_)) | Ok(ServiceResponse::Ok) => {}
                    Ok(ServiceResponse::Error { message }) => {
                        eprintln!(
                            "[rc-ui] 后台服务同步远程协助文件互传策略被拒绝: {message}"
                        );
                        return Err(format!("后台服务同步文件互传策略失败：{message}"));
                    }
                    Ok(other) => {
                        eprintln!(
                            "[rc-ui] 后台服务同步远程协助文件互传策略返回异常响应: {other:?}"
                        );
                    }
                    Err(err) => {
                        eprintln!(
                            "[rc-ui] 通知后台服务热更新文件互传策略失败（已落盘并由 ReloadPrefs 兜底）: {err}"
                        );
                    }
                }
            }
        }
        notify_background_services_reload(&state, false);
        sync_standalone_status_bar(&state);
        Ok::<_, String>(snap)
    })
    .await
    .map_err(|e| format!("保存安全偏好任务异常：{e}"))??;
    Ok(snap)
}

/// IPC 命令：手动立即续签本机 mTLS 设备证书。
#[tauri::command]
pub async fn renew_device_cert(app: AppHandle) -> Result<GateCertStatus, String> {
    let app_for_blocking = app.clone();
    let status = tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        state.require_unlocked()?;
        state.require_authenticated()?;
        let _ = state.auth.cert_manager().clear_device_cert();
        if let Err(err) = bind_device_and_ensure_cert(
            &state,
            &server_config::default_config_path(),
            Some(&app_for_blocking),
        ) {
            refresh_host_security_runtime(&state);
            let _ = app_for_blocking.emit("gate://status", &state.gate_status_snapshot());
            return Err(err);
        }
        refresh_host_security_runtime(&state);
        notify_background_services_reload(&state, true);
        Ok::<_, String>(state.cert_status())
    })
    .await
    .map_err(|e| format!("续签设备证书任务异常：{e}"))??;
    let state = app.state::<AppState>();
    let _ = app.emit("gate://status", &state.gate_status_snapshot());
    Ok(status)
}

// ─────────────────────────────────────────────────────────────────
// W2 文件传输 / W3 远程终端 / W4 TCP 端口映射 IPC 命令
// ─────────────────────────────────────────────────────────────────

/// W2 本地目录浏览快照（IPC `camelCase`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalDirSnapshot {
    /// 规范化后的目录路径。
    pub path: String,
    /// 目录下的条目列表。
    pub entries: Vec<FileEntry>,
}

/// 在极短单次持锁窗口内取出当前活动会话的工具操作句柄及其归一化设备持久化键。
fn active_session_tools_with_portmap_key(
    state: &AppState,
) -> Result<(ToolClientHandle, Option<String>), String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = slot.as_ref() else {
        return Err("请先建立远控会话".to_string());
    };
    if active.disconnecting || !active.handle.is_running() {
        return Err("请先建立远控会话".to_string());
    }
    if active.watch_only {
        return Err("当前为只读观看模式，无法使用文件传输、终端或端口映射".to_string());
    }
    let raw_key = if !active.meta.device_key.trim().is_empty() {
        active.meta.device_key.trim()
    } else {
        active.meta.target.trim()
    };
    let key = (!raw_key.is_empty()).then(|| raw_key.to_string());
    Ok((active.handle.tools(), key))
}

/// 在极短持锁窗口内取出当前活动会话的工具操作句柄；无活动会话、未通过门禁、正在断开或处于只读观看模式时返回可读错误。
fn active_session_tools(state: &AppState) -> Result<ToolClientHandle, String> {
    active_session_tools_with_portmap_key(state).map(|(tools, _)| tools)
}

/// 列举本机目录的纯同步实现（在 `spawn_blocking` 中执行，避免阻塞 UI 主线程）。
///
/// 当 `path` 为空时优先使用已配置的文件传输存储目录：
/// - 若为默认目录且尚未创建，自动 `create_dir_all`；
/// - 若存在且为目录则列举；
/// - 若自定义目录已被外部删除或失效，优雅回退至 `rc_tools::default_home_dir()`。
fn file_list_local_inner(
    path: Option<String>,
    configured_dir: Option<(&Path, bool)>,
) -> Result<LocalDirSnapshot, String> {
    let target = match path {
        Some(p) if !p.trim().is_empty() => p,
        _ => {
            let mut chosen: Option<PathBuf> = None;
            if let Some((dir, is_default)) = configured_dir {
                if is_default && !dir.exists() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if dir.is_dir() {
                    chosen = Some(dir.to_path_buf());
                }
            }
            chosen
                .unwrap_or_else(rc_tools::default_home_dir)
                .to_string_lossy()
                .into_owned()
        }
    };
    let (resolved, entries) = rc_tools::list_local_dir(&target)?;
    Ok(LocalDirSnapshot {
        path: resolved,
        entries,
    })
}

/// 在本机创建目录的纯同步实现（在 `spawn_blocking` 中执行，避免阻塞 UI 主线程）。
fn file_mkdir_local_inner(parent: String, name: String) -> Result<String, String> {
    rc_tools::create_local_dir(&parent, &name)
}

/// IPC 命令：异步列举本机目录（`spawn_blocking`，无需活动远控会话即可工作）。
#[tauri::command]
pub async fn file_list_local(
    path: Option<String>,
    state: State<'_, AppState>,
) -> Result<LocalDirSnapshot, String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let prefs = sync_prefs_from_disk_if_proxy(&state);
    let transfer_dir = prefs.resolved_file_transfer_dir();
    let is_default = prefs.is_default_file_transfer_dir();
    tauri::async_runtime::spawn_blocking(move || {
        file_list_local_inner(path, Some((&transfer_dir, is_default)))
    })
    .await
    .map_err(|e| format!("列举本地目录任务异常：{e}"))?
}

/// IPC 命令：异步在本机指定父目录下创建新目录（`spawn_blocking`，无需活动远控会话即可工作）。
#[tauri::command]
pub async fn file_mkdir_local(
    parent: String,
    name: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    tauri::async_runtime::spawn_blocking(move || file_mkdir_local_inner(parent, name))
        .await
        .map_err(|e| format!("创建本地目录任务异常：{e}"))?
}

/// IPC 命令：异步删除本机文件或目录（`spawn_blocking`）。
#[tauri::command]
pub async fn file_delete_local(path: String, state: State<'_, AppState>) -> Result<(), String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    tauri::async_runtime::spawn_blocking(move || rc_tools::file_transfer::delete_local_path(&path))
        .await
        .map_err(|e| format!("删除本地文件任务异常：{e}"))?
}

/// IPC 命令：异步重命名本机文件或目录（`spawn_blocking`）。
#[tauri::command]
pub async fn file_rename_local(
    path: String,
    new_name: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    tauri::async_runtime::spawn_blocking(move || {
        rc_tools::file_transfer::rename_local_path(&path, &new_name)
    })
    .await
    .map_err(|e| format!("重命名本地文件任务异常：{e}"))?
}

/// 校验 W3 终端行列尺寸（M10：1–512）。
fn validate_terminal_size(cols: u16, rows: u16) -> Result<(), String> {
    if cols == 0 || rows == 0 || cols > 512 || rows > 512 {
        return Err("终端行列数必须在 1–512 之间".to_string());
    }
    Ok(())
}

/// 校验 W3 单次终端输入字节上限（M10：<= 64 KiB）。
fn validate_terminal_input_len(len: usize) -> Result<(), String> {
    if len > 64 * 1024 {
        return Err("单次终端输入不能超过 64 KiB".to_string());
    }
    Ok(())
}

/// 校验 W4 添加端口映射参数（M10：名称、非零端口与目标地址）。
fn validate_portmap_add(
    name: &str,
    local_port: u16,
    target_addr: &str,
    target_port: u16,
) -> Result<(), String> {
    model::validate_map_name(name)?;
    if local_port == 0 || target_port == 0 {
        return Err("端口不能为 0，范围必须为 1–65535".to_string());
    }
    model::validate_target_addr(target_addr)?;
    Ok(())
}

/// IPC 命令：请求列举远端目录（结果通过 `"file://event"` 推送，H12 经 `spawn_blocking` 投递专用文件工作线程）。
#[tauri::command]
pub async fn file_list_remote(
    path: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), UiCommandError> {
    let allow_ft = active_session_file_transfer_allowed(&state);
    let tools = active_session_tools(&state)
        .map_err(|e| UiCommandError::from_file_tool_error_with_policy(e, allow_ft))?;
    tauri::async_runtime::spawn_blocking(move || tools.file_list_remote(path))
        .await
        .map_err(|e| {
            UiCommandError::from_file_tool_error_with_policy(
                format!("列举远端目录任务异常：{e}"),
                allow_ft,
            )
        })?
        .map_err(|e| {
            let cur_allow_ft = active_session_file_transfer_allowed(&state);
            UiCommandError::from_file_tool_error_with_policy(e, cur_allow_ft)
        })
}

/// IPC 命令：请求在远端创建目录（结果通过 `"file://event"` 推送，H12 经 `spawn_blocking` 投递专用文件工作线程）。
#[tauri::command]
pub async fn file_mkdir_remote(
    parent: String,
    name: String,
    state: State<'_, AppState>,
) -> Result<(), UiCommandError> {
    let allow_ft = active_session_file_transfer_allowed(&state);
    let tools = active_session_tools(&state)
        .map_err(|e| UiCommandError::from_file_tool_error_with_policy(e, allow_ft))?;
    tauri::async_runtime::spawn_blocking(move || tools.file_mkdir_remote(parent, name))
        .await
        .map_err(|e| {
            UiCommandError::from_file_tool_error_with_policy(
                format!("创建远端目录任务异常：{e}"),
                allow_ft,
            )
        })?
        .map_err(|e| {
            let cur_allow_ft = active_session_file_transfer_allowed(&state);
            UiCommandError::from_file_tool_error_with_policy(e, cur_allow_ft)
        })
}

/// IPC 命令：请求删除远端文件或目录（结果通过 `"file://event"` 推送）。
#[tauri::command]
pub async fn file_delete_remote(
    path: String,
    state: State<'_, AppState>,
) -> Result<(), UiCommandError> {
    let allow_ft = active_session_file_transfer_allowed(&state);
    let tools = active_session_tools(&state)
        .map_err(|e| UiCommandError::from_file_tool_error_with_policy(e, allow_ft))?;
    tauri::async_runtime::spawn_blocking(move || tools.file_delete_remote(path))
        .await
        .map_err(|e| {
            UiCommandError::from_file_tool_error_with_policy(
                format!("删除远端文件任务异常：{e}"),
                allow_ft,
            )
        })?
        .map_err(|e| {
            let cur_allow_ft = active_session_file_transfer_allowed(&state);
            UiCommandError::from_file_tool_error_with_policy(e, cur_allow_ft)
        })
}

/// IPC 命令：请求重命名远端文件或目录（结果通过 `"file://event"` 推送）。
#[tauri::command]
pub async fn file_rename_remote(
    path: String,
    new_name: String,
    state: State<'_, AppState>,
) -> Result<(), UiCommandError> {
    let allow_ft = active_session_file_transfer_allowed(&state);
    let tools = active_session_tools(&state)
        .map_err(|e| UiCommandError::from_file_tool_error_with_policy(e, allow_ft))?;
    tauri::async_runtime::spawn_blocking(move || tools.file_rename_remote(path, new_name))
        .await
        .map_err(|e| {
            UiCommandError::from_file_tool_error_with_policy(
                format!("重命名远端文件任务异常：{e}"),
                allow_ft,
            )
        })?
        .map_err(|e| {
            let cur_allow_ft = active_session_file_transfer_allowed(&state);
            UiCommandError::from_file_tool_error_with_policy(e, cur_allow_ft)
        })
}

/// IPC 命令：上传本地文件至远端目录，返回传输任务 ID（H12：不阻塞 UI IPC 线程与 60fps 视频线程）。
#[tauri::command]
pub async fn file_upload(
    local_path: String,
    remote_dir: String,
    state: State<'_, AppState>,
) -> Result<u64, UiCommandError> {
    let allow_ft = active_session_file_transfer_allowed(&state);
    let tools = active_session_tools(&state)
        .map_err(|e| UiCommandError::from_file_tool_error_with_policy(e, allow_ft))?;
    tauri::async_runtime::spawn_blocking(move || tools.file_upload(&local_path, &remote_dir))
        .await
        .map_err(|e| {
            UiCommandError::from_file_tool_error_with_policy(
                format!("发起文件上传任务异常：{e}"),
                allow_ft,
            )
        })?
        .map_err(|e| {
            let cur_allow_ft = active_session_file_transfer_allowed(&state);
            UiCommandError::from_file_tool_error_with_policy(e, cur_allow_ft)
        })
}

/// IPC 命令：从远端下载文件至本地目录，返回传输任务 ID（H12：不阻塞 UI IPC 线程与 60fps 视频线程）。
#[tauri::command]
pub async fn file_download(
    remote_path: String,
    local_dir: String,
    state: State<'_, AppState>,
) -> Result<u64, UiCommandError> {
    let allow_ft = active_session_file_transfer_allowed(&state);
    let tools = active_session_tools(&state)
        .map_err(|e| UiCommandError::from_file_tool_error_with_policy(e, allow_ft))?;
    let fallback_dir = if local_dir.trim().is_empty() {
        let prefs = sync_prefs_from_disk_if_proxy(&state);
        Some((
            prefs.resolved_file_transfer_dir(),
            prefs.is_default_file_transfer_dir(),
        ))
    } else {
        None
    };
    tauri::async_runtime::spawn_blocking(move || {
        let effective_dir = match fallback_dir {
            Some((dir, is_default)) => {
                if is_macos_package_bundle(&dir) {
                    return Err(format!(
                        "不能使用应用程序包或系统软件包作为存储路径：{}",
                        dir.display()
                    ));
                }
                if !dir.exists() {
                    if is_default {
                        std::fs::create_dir_all(&dir).map_err(|e| {
                            format!("无法创建默认存储目录：{}（{e}）", dir.display())
                        })?;
                    } else {
                        return Err("存储目录不存在或尚未挂载，请重新选择路径".to_string());
                    }
                }
                let canonical = std::fs::canonicalize(&dir)
                    .map_err(|e| format!("无法访问传输文件存储目录：{}（{e}）", dir.display()))?;
                if is_macos_package_bundle(&canonical) {
                    return Err(format!(
                        "不能使用应用程序包或系统软件包作为存储路径：{}",
                        canonical.display()
                    ));
                }
                if !canonical.is_dir() {
                    return Err(format!(
                        "文件传输存储路径不是有效文件夹：{}",
                        canonical.display()
                    ));
                }
                canonical.to_string_lossy().into_owned()
            }
            None => local_dir,
        };
        tools.file_download(&remote_path, &effective_dir)
    })
    .await
    .map_err(|e| {
        UiCommandError::from_file_tool_error_with_policy(
            format!("发起文件下载任务异常：{e}"),
            allow_ft,
        )
    })?
    .map_err(|e| {
        let cur_allow_ft = active_session_file_transfer_allowed(&state);
        UiCommandError::from_file_tool_error_with_policy(e, cur_allow_ft)
    })
}

/// IPC 命令：取消进行中的文件传输任务（H12）。
#[tauri::command]
pub async fn file_cancel(id: u64, state: State<'_, AppState>) -> Result<(), String> {
    let tools = active_session_tools(&state)?;
    tauri::async_runtime::spawn_blocking(move || tools.file_cancel(id))
        .await
        .map_err(|e| format!("取消文件传输任务异常：{e}"))?
}

/// IPC 命令：获取当前文件传输队列快照（无活动会话时返回空表；读取轻量缓存副本，永不阻塞）。
#[tauri::command]
pub fn file_list_transfers(state: State<'_, AppState>) -> Vec<TransferItem> {
    match active_session_tools(&state) {
        Ok(tools) => tools.file_list_transfers(),
        Err(_) => Vec::new(),
    }
}

/// IPC 命令：清除已终态的文件传输任务记录（H12）。
#[tauri::command]
pub async fn file_clear_finished(state: State<'_, AppState>) -> Result<(), String> {
    let tools = active_session_tools(&state)?;
    tauri::async_runtime::spawn_blocking(move || tools.file_clear_finished())
        .await
        .map_err(|e| format!("清理已完成传输记录任务异常：{e}"))?
}

/// IPC 命令：移除单条文件传输任务记录（H12）。
#[tauri::command]
pub async fn file_remove_transfer(id: u64, state: State<'_, AppState>) -> Result<(), String> {
    let tools = active_session_tools(&state)?;
    tauri::async_runtime::spawn_blocking(move || tools.file_remove_transfer(id))
        .await
        .map_err(|e| format!("移除传输记录任务异常：{e}"))?
}

/// IPC 命令：在远端开启新的 PTY 终端会话（支持可选会话名称与仅后台创建模式），返回分配的 `termId`（后台创建时返回 `0`，真实 ID 通过 `"terminal://event"` 推送）。
#[tauri::command]
pub fn terminal_open(
    cols: u16,
    rows: u16,
    name: Option<String>,
    attach: Option<bool>,
    state: State<'_, AppState>,
) -> Result<u32, String> {
    validate_terminal_size(cols, rows)?;
    active_session_tools(&state)?.terminal_open_with_options(
        cols,
        rows,
        name,
        attach.unwrap_or(true),
    )
}

/// IPC 命令：请求远端主机返回当前后台运行的终端会话列表（结果通过 `"terminal://event"` 推送）。
#[tauri::command]
pub fn terminal_list(state: State<'_, AppState>) -> Result<(), String> {
    active_session_tools(&state)?.terminal_list()
}

/// IPC 命令：附着至远端指定后台 PTY 终端（回放历史输出缓冲并开启实时流）。
#[tauri::command]
pub fn terminal_attach(
    term_id: u32,
    cols: u16,
    rows: u16,
    state: State<'_, AppState>,
) -> Result<(), String> {
    validate_terminal_size(cols, rows)?;
    active_session_tools(&state)?.terminal_attach(term_id, cols, rows)
}

/// IPC 命令：脱离远端指定 PTY 终端视图（仅暂停实时流，保留远端 PTY 在后台运行）。
#[tauri::command]
pub fn terminal_detach(term_id: u32, state: State<'_, AppState>) -> Result<(), String> {
    active_session_tools(&state)?.terminal_detach(term_id)
}

/// IPC 命令：重命名远端指定 PTY 终端会话。
#[tauri::command]
pub fn terminal_rename(
    term_id: u32,
    name: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    active_session_tools(&state)?.terminal_rename(term_id, &name)
}

/// IPC 命令：向远端指定 PTY 终端发送键盘/粘贴文本输入（M10：限制单次 64 KiB 并核验终端活跃）。
#[tauri::command]
pub fn terminal_input(
    term_id: u32,
    data: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    validate_terminal_input_len(data.len())?;
    let tools = active_session_tools(&state)?;
    if !tools.has_active_terminal(term_id) {
        return Err("目标终端会话不存在或已关闭".to_string());
    }
    tools.terminal_input(term_id, data.as_bytes())
}

/// IPC 命令：调整远端指定 PTY 终端尺寸（M10：校验行列范围 1–512）。
#[tauri::command]
pub fn terminal_resize(
    term_id: u32,
    cols: u16,
    rows: u16,
    state: State<'_, AppState>,
) -> Result<(), String> {
    validate_terminal_size(cols, rows)?;
    active_session_tools(&state)?.terminal_resize(term_id, cols, rows)
}

/// IPC 命令：关闭并终止远端指定 PTY 终端会话。
#[tauri::command]
pub fn terminal_close(term_id: u32, state: State<'_, AppState>) -> Result<(), String> {
    active_session_tools(&state)?.terminal_close(term_id)
}

/// IPC 命令：读取当前会话 W3 远程终端锁屏验证状态（含本机钥匙串是否存有已记忆系统密码）。
#[tauri::command]
pub async fn terminal_get_lock_state(
    state: State<'_, AppState>,
) -> Result<crate::state::TerminalLockStateSnapshot, String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let (mut snap, device_key, keychain) = {
        let guard = state.session.lock().unwrap_or_else(|e| e.into_inner());
        let kc = state.keychain.clone();
        match guard.as_ref().filter(|s| !s.disconnecting) {
            Some(active) => {
                let snap = active.terminal_lock_snapshot_at(std::time::Instant::now());
                let dk = if !active.meta.device_key.trim().is_empty() {
                    active.meta.device_key.trim().to_string()
                } else {
                    active.meta.target.trim().to_string()
                };
                (snap, dk, kc)
            }
            None => (
                crate::state::TerminalLockStateSnapshot::default(),
                String::new(),
                kc,
            ),
        }
    };
    snap.has_saved_password = if !device_key.is_empty() {
        let dk_for_kc = device_key.clone();
        tauri::async_runtime::spawn_blocking(move || keychain.has_password(dk_for_kc))
            .await
            .map_err(|e| format!("查询钥匙串状态任务异常：{e}"))?
    } else {
        false
    };
    snap.device_key = (!device_key.is_empty()).then_some(device_key);
    Ok(snap)
}

/// IPC 命令：向被控端提交 W3 远程终端系统密码校验请求（可选记忆到本机登录钥匙串）。
#[tauri::command]
pub fn terminal_unlock(
    password: String,
    username: Option<String>,
    remember: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut password = zeroize::Zeroizing::new(password);
    state.require_unlocked()?;
    state.require_authenticated()?;
    if password.is_empty() || password.len() > 512 {
        return Err("系统密码不能为空且长度不能超过 512 字节".to_string());
    }
    let clean_username = username
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty());

    let mut guard = state.session.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = guard.as_mut() else {
        return Err("请先建立远控会话".to_string());
    };
    if active.disconnecting || !active.handle.is_running() {
        return Err("请先建立远控会话".to_string());
    }
    if active.watch_only {
        return Err("当前为只读观看模式，无法使用远程终端".to_string());
    }
    if !active.meta.tool_permissions.allow_terminal {
        return Err("远程终端仅限本人账号使用".to_string());
    }

    let device_key = if !active.meta.device_key.trim().is_empty() {
        active.meta.device_key.trim().to_string()
    } else {
        active.meta.target.trim().to_string()
    };
    if remember && !device_key.is_empty() {
        active.pending_terminal_remember = Some((
            zeroize::Zeroizing::new(device_key),
            zeroize::Zeroizing::new((*password).clone()),
        ));
    } else {
        active.clear_pending_terminal_remember();
    }
    active.terminal_auto_unlock_in_flight = false;
    active.terminal_unlock_seq = active.terminal_unlock_seq.wrapping_add(1);
    if let Err(err) = active
        .handle
        .submit_terminal_unlock(std::mem::take(&mut *password), clean_username)
    {
        active.clear_pending_terminal_remember();
        return Err(err);
    }
    Ok(())
}

/// IPC 命令：尝试从本机登录钥匙串读取已记忆密码并静默完成 W3 终端校验（有密码提交返回 `true`）。
#[tauri::command]
pub async fn terminal_try_auto_unlock(state: State<'_, AppState>) -> Result<bool, String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let (sid, seq, device_key, username, keychain) = {
        let mut guard = state.session.lock().unwrap_or_else(|e| e.into_inner());
        let Some(active) = guard.as_mut() else {
            return Ok(false);
        };
        if active.disconnecting
            || !active.handle.is_running()
            || active.watch_only
            || !active.meta.tool_permissions.allow_terminal
            || !active.terminal_lock_state.required
            || active.terminal_lock_state.unlocked
            || active.terminal_auto_unlock_in_flight
        {
            return Ok(false);
        }
        let dk = if !active.meta.device_key.trim().is_empty() {
            active.meta.device_key.trim().to_string()
        } else {
            active.meta.target.trim().to_string()
        };
        if dk.is_empty() {
            return Ok(false);
        }
        active.clear_pending_terminal_remember();
        active.terminal_unlock_seq = active.terminal_unlock_seq.wrapping_add(1);
        let seq = active.terminal_unlock_seq;
        active.terminal_auto_unlock_in_flight = true;
        (
            active.id,
            seq,
            dk,
            active.terminal_lock_state.username.clone(),
            state.keychain.clone(),
        )
    };

    let load_task =
        tauri::async_runtime::spawn_blocking(move || keychain.load_password(device_key)).await;
    let mut guard = state.session.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = guard.as_mut() else {
        return Ok(false);
    };
    if active.id != sid
        || active.terminal_unlock_seq != seq
        || !active.terminal_auto_unlock_in_flight
    {
        return Ok(false);
    }
    if active.disconnecting
        || !active.handle.is_running()
        || active.watch_only
        || !active.meta.tool_permissions.allow_terminal
        || !active.terminal_lock_state.required
        || active.terminal_lock_state.unlocked
    {
        active.terminal_auto_unlock_in_flight = false;
        return Ok(false);
    }
    let loaded = match load_task {
        Ok(res) => res,
        Err(e) => {
            active.terminal_auto_unlock_in_flight = false;
            return Err(format!("读取钥匙串密码任务异常：{e}"));
        }
    };
    match loaded {
        Ok(Some(pw)) => {
            if let Err(err) = active
                .handle
                .submit_terminal_unlock((*pw).clone(), username)
            {
                active.terminal_auto_unlock_in_flight = false;
                return Err(err);
            }
            Ok(true)
        }
        Ok(None) => {
            active.terminal_auto_unlock_in_flight = false;
            Ok(false)
        }
        Err(e) => {
            eprintln!("[rc-ui] 钥匙串自动填充读取失败: {e}");
            active.terminal_auto_unlock_in_flight = false;
            Ok(false)
        }
    }
}

/// IPC 命令：清除本机登录钥匙串中保存的设备终端系统密码。
#[tauri::command]
pub async fn terminal_clear_saved_password(
    device_key: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let (resolved, keychain) = {
        let kc = state.keychain.clone();
        let explicit = device_key
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let mut guard = state.session.lock().unwrap_or_else(|e| e.into_inner());
        let active_dk = guard.as_ref().and_then(|active| {
            let dk = if !active.meta.device_key.trim().is_empty() {
                active.meta.device_key.trim().to_string()
            } else {
                active.meta.target.trim().to_string()
            };
            (!dk.is_empty()).then_some(dk)
        });
        let res = explicit.or(active_dk.clone());
        if let (Some(ref target_dk), Some(active)) = (&res, guard.as_mut()) {
            if active
                .pending_terminal_remember
                .as_ref()
                .is_some_and(|(pending_dk, _)| pending_dk.as_str() == target_dk.as_str())
            {
                active.clear_pending_terminal_remember();
            }
        }
        (res, kc)
    };
    if let Some(dk) = resolved {
        tauri::async_runtime::spawn_blocking(move || keychain.delete_password(dk))
            .await
            .map_err(|e| format!("删除钥匙串密码任务异常：{e}"))??;
    }
    Ok(())
}

const MAX_DISK_CACHED_TERMINAL_FONTS: usize = 8;

fn sanitize_cached_font_id(font_id: &str) -> Result<&str, String> {
    let clean = font_id.trim();
    if clean.is_empty()
        || clean.len() > 64
        || !clean
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("非法的终端字体缓存标识".to_string());
    }
    Ok(clean)
}

fn terminal_font_cache_dir() -> PathBuf {
    config_paths::config_dir().join("terminal-fonts")
}

fn terminal_font_cache_path(dir: &Path, font_id: &str) -> Result<PathBuf, String> {
    let clean = sanitize_cached_font_id(font_id)?;
    Ok(dir.join(format!("{clean}.font")))
}

fn save_cached_remote_font_to_dir(dir: &Path, font_id: &str, bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() as u64 > rc_tools::fonts::MAX_REMOTE_FONT_BYTES {
        return Err("终端字体缓存体积非法".to_string());
    }
    let path = terminal_font_cache_path(dir, font_id)?;
    config_paths::write_private_file(&path, bytes)?;
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) != Some("font") {
                    return None;
                }
                let meta = e.metadata().ok()?;
                if !meta.is_file() {
                    return None;
                }
                let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                Some((mtime, p))
            })
            .collect();
        if files.len() > MAX_DISK_CACHED_TERMINAL_FONTS {
            files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            let excess = files.len() - MAX_DISK_CACHED_TERMINAL_FONTS;
            for (_, old_path) in files.into_iter().take(excess) {
                if old_path != path {
                    let _ = std::fs::remove_file(old_path);
                }
            }
        }
    }
    Ok(())
}

fn load_cached_remote_font_from_dir(dir: &Path, font_id: &str) -> Result<Vec<u8>, String> {
    let path = terminal_font_cache_path(dir, font_id)?;
    let meta = std::fs::metadata(&path).map_err(|_| "本地尚未缓存该远程字体".to_string())?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > rc_tools::fonts::MAX_REMOTE_FONT_BYTES {
        return Err("本地缓存的远程字体已损坏".to_string());
    }
    std::fs::read(&path).map_err(|e| format!("读取本地缓存字体失败：{e}"))
}

/// IPC 命令：扫描本机系统字体（用于 W3 终端本地字体选择）。
#[tauri::command]
pub async fn terminal_list_local_fonts(
    state: State<'_, AppState>,
) -> Result<Vec<TerminalFontInfo>, String> {
    state.require_authenticated()?;
    tauri::async_runtime::spawn_blocking(rc_tools::fonts::list_local_fonts)
        .await
        .map_err(|e| format!("扫描本机字体任务异常：{e}"))
}

/// IPC 命令：读取本机已安装的字体二进制流，用于 `WKWebView` 通过 `FontFace` 直接渲染用户安装字体。
#[tauri::command]
pub async fn terminal_get_local_font_bytes(
    font_id: String,
    family: Option<String>,
    state: State<'_, AppState>,
) -> Result<tauri::ipc::Response, String> {
    state.require_authenticated()?;
    tauri::async_runtime::spawn_blocking(move || {
        let cat = rc_tools::fonts::FontCatalog::scan_system();
        let trimmed_id = font_id.trim();
        if !trimmed_id.is_empty() {
            if let Ok((_info, _fmt, bytes)) = cat.load_font_bytes(trimmed_id) {
                return Ok(tauri::ipc::Response::new(bytes));
            }
        }
        if let Some(fam) = family.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(matched) = cat
                .list()
                .into_iter()
                .find(|info| info.family.eq_ignore_ascii_case(fam))
            {
                let (_info, _fmt, bytes) = cat.load_font_bytes(&matched.id)?;
                return Ok(tauri::ipc::Response::new(bytes));
            }
        }
        Err("未找到匹配的本机系统字体".to_string())
    })
    .await
    .map_err(|e| format!("读取本机字体数据任务异常：{e}"))?
}

/// IPC 命令：请求被控端列举其已安装的系统字体（结果经 `"terminal://event"` 推送）。
#[tauri::command]
pub fn terminal_list_remote_fonts(state: State<'_, AppState>) -> Result<(), String> {
    active_session_tools(&state)?.terminal_list_remote_fonts()
}

/// IPC 命令：请求从被控端拉取指定 `font_id` 的字体二进制流（就绪后经 `"terminal://event"` 推送 `fontReady`）。
#[tauri::command]
pub fn terminal_fetch_remote_font(
    font_id: String,
    state: State<'_, AppState>,
) -> Result<u32, String> {
    active_session_tools(&state)?.terminal_fetch_remote_font(font_id)
}

/// IPC 命令：读取已缓存的远程字体原始二进制（先查活动会话内存缓存并落盘，未命中时回退本机持久化缓存）。
#[tauri::command]
pub fn terminal_get_remote_font_bytes(
    font_id: String,
    state: State<'_, AppState>,
) -> Result<tauri::ipc::Response, String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    if let Ok(tools) = active_session_tools(&state) {
        if let Ok(bytes) = tools.terminal_get_cached_remote_font(&font_id) {
            let _ = save_cached_remote_font_to_dir(&terminal_font_cache_dir(), &font_id, &bytes);
            return Ok(tauri::ipc::Response::new(bytes));
        }
    }
    let bytes = load_cached_remote_font_from_dir(&terminal_font_cache_dir(), &font_id)?;
    Ok(tauri::ipc::Response::new(bytes))
}

/// IPC 命令：保存 W3 终端字体偏好设置。
#[tauri::command]
pub async fn terminal_set_font_prefs(
    patch: TerminalPrefsPatch,
    app: AppHandle,
) -> Result<AppPrefs, String> {
    let st = app.state::<AppState>();
    st.require_unlocked()?;
    st.require_authenticated()?;
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        save_terminal_prefs_to_path(&state, patch, &app_prefs::default_prefs_path())
            .map(sanitize_app_prefs_for_ui)
    })
    .await
    .map_err(|e| format!("保存终端字体偏好任务异常：{e}"))?
}

fn persist_portmaps_locked(
    state: &AppState,
    _io_guard: &std::sync::MutexGuard<'_, ()>,
    device_key: &str,
    rules: &[PortMapRuleSnapshot],
    path: &Path,
) {
    let clean_key = device_key.trim();
    if clean_key.is_empty() {
        return;
    }
    let _file_lock = config_paths::acquire_file_lock(path, Duration::from_secs(3));
    let saved: Vec<SavedPortMapRule> = rules
        .iter()
        .map(|r| SavedPortMapRule {
            name: r.name.clone(),
            local_port: r.local_port,
            target_addr: r.target_addr.clone(),
            target_port: r.target_port,
            enabled: r.enabled,
        })
        .collect();
    let next = {
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        if state.is_service_proxy() && path.exists() {
            *prefs = app_prefs::load_from_path(path);
        }
        prefs.set_device_portmaps(clean_key, saved);
        prefs.clone()
    };
    let _ = app_prefs::save_to_path(path, &next);
    notify_background_services_reload(state, false);
}

#[cfg_attr(not(test), allow(dead_code))]
fn persist_portmaps_to_path(
    state: &AppState,
    device_key: &str,
    rules: &[PortMapRuleSnapshot],
    path: &Path,
) {
    let io_guard = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    persist_portmaps_locked(state, &io_guard, device_key, rules, path);
}

/// 在远控会话首次协商完成后自动恢复该设备持久化的 W4 TCP 端口映射规则。
pub(crate) fn restore_saved_portmaps_for_active_session(
    state: &AppState,
    sid: u64,
    device_key: &str,
) {
    if state.require_authenticated().is_err() {
        return;
    }
    let clean_key = device_key.trim();
    if clean_key.is_empty() {
        return;
    }
    let saved = {
        let prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        prefs.saved_portmaps.get(clean_key).cloned()
    };
    let Some(saved) = saved else {
        return;
    };
    if saved.is_empty() {
        return;
    }
    let tools = {
        let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        let Some(active) = slot.as_ref() else {
            return;
        };
        if active.id != sid
            || active.watch_only
            || active.disconnecting
            || !active.handle.is_running()
        {
            return;
        }
        active.handle.tools()
    };
    let _io = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let existing = tools.portmap_list();
    for rule in saved {
        let dup = existing.iter().any(|cur| {
            cur.name == rule.name
                && cur.local_port == rule.local_port
                && cur.target_addr == rule.target_addr
                && cur.target_port == rule.target_port
        });
        if dup {
            continue;
        }
        let _ = tools.portmap_add_with_enabled(
            rule.name,
            rule.local_port,
            rule.target_addr,
            rule.target_port,
            rule.enabled,
        );
    }
}

/// IPC 命令：列出当前全部 TCP 端口映射规则（无活动会话或处于观看模式时返回空表）。
#[tauri::command]
pub fn portmap_list(state: State<'_, AppState>) -> Vec<PortMapRuleSnapshot> {
    match active_session_tools(&state) {
        Ok(tools) => tools.portmap_list(),
        Err(_) => Vec::new(),
    }
}

/// IPC 命令：添加一条 TCP 端口映射规则并持久化到设备偏好（M10：校验名称/端口，快照由 `portmap_latest` 经轮询器单点串行发射）。
#[tauri::command]
pub fn portmap_add(
    name: String,
    local_port: u16,
    target_addr: String,
    target_port: u16,
    state: State<'_, AppState>,
) -> Result<PortMapRuleSnapshot, String> {
    let name = name.trim().to_string();
    let target_addr = target_addr.trim().to_string();
    validate_portmap_add(&name, local_port, &target_addr, target_port)?;
    let (tools, device_key) = active_session_tools_with_portmap_key(&state)?;
    let io_guard = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (rule, all) = tools.portmap_add(name, local_port, target_addr, target_port)?;
    if let Some(ref key) = device_key {
        persist_portmaps_locked(
            &state,
            &io_guard,
            key,
            &all,
            &app_prefs::default_prefs_path(),
        );
    }
    Ok(rule)
}

/// IPC 命令：启用或暂停指定 TCP 端口映射规则并同步持久化（快照由 `portmap_latest` 经轮询器单点串行发射）。
#[tauri::command]
pub fn portmap_toggle(
    id: u64,
    enabled: bool,
    state: State<'_, AppState>,
) -> Result<PortMapRuleSnapshot, String> {
    let (tools, device_key) = active_session_tools_with_portmap_key(&state)?;
    let io_guard = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (rule, all) = tools.portmap_toggle(id, enabled)?;
    if let Some(ref key) = device_key {
        persist_portmaps_locked(
            &state,
            &io_guard,
            key,
            &all,
            &app_prefs::default_prefs_path(),
        );
    }
    Ok(rule)
}

/// IPC 命令：删除指定 TCP 端口映射规则并同步持久化（快照由 `portmap_latest` 经轮询器单点串行发射）。
#[tauri::command]
pub fn portmap_delete(id: u64, state: State<'_, AppState>) -> Result<(), String> {
    let (tools, device_key) = active_session_tools_with_portmap_key(&state)?;
    let io_guard = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let all = tools.portmap_delete(id)?;
    if let Some(ref key) = device_key {
        persist_portmaps_locked(
            &state,
            &io_guard,
            key,
            &all,
            &app_prefs::default_prefs_path(),
        );
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────
// 常规偏好、系统级后台服务托管与菜单栏整体退出 IPC 命令
// ─────────────────────────────────────────────────────────────────

/// 常规窗口行为、文件存储路径与远程开机偏好快照（IPC `camelCase`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneralPrefsSnapshot {
    /// 关闭主窗口时是否隐藏到顶部菜单栏而非退出程序。
    pub close_to_tray: bool,
    /// 当前生效的 W2 文件传输存储目录路径。
    pub file_transfer_dir: String,
    /// 当前文件传输存储路径是否为平台默认目录。
    pub file_transfer_dir_is_default: bool,
    /// 是否允许通过 Wake-on-LAN 远程开机启动本机。
    pub allow_wol: bool,
}

/// 常规偏好增量更新补丁（IPC `camelCase`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GeneralPrefsPatch {
    /// 关闭主窗口时是否隐藏到顶部菜单栏。
    pub close_to_tray: Option<bool>,
    /// W2 文件传输存储目录（`Some("")` 表示恢复默认目录）。
    pub file_transfer_dir: Option<String>,
    /// 是否允许通过 Wake-on-LAN 远程开机启动本机。
    pub allow_wol: Option<bool>,
}

fn general_prefs_snapshot_from_prefs(prefs: &AppPrefs) -> GeneralPrefsSnapshot {
    GeneralPrefsSnapshot {
        close_to_tray: prefs.close_to_tray,
        file_transfer_dir: prefs
            .resolved_file_transfer_dir()
            .to_string_lossy()
            .into_owned(),
        file_transfer_dir_is_default: prefs.is_default_file_transfer_dir(),
        allow_wol: prefs.allow_wol,
    }
}

/// macOS 软件包/应用包扩展名黑名单：禁止作为传输文件存储路径或被 `/usr/bin/open` 意外执行。
const MACOS_PACKAGE_BUNDLE_EXTS: &[&str] = &[
    "app",
    "bundle",
    "pkg",
    "mpkg",
    "scptd",
    "workflow",
    "action",
    "prefpane",
    "saver",
    "framework",
    "plugin",
    "appex",
    "xpc",
    "kext",
];

/// 判断给定路径是否具有 macOS 应用程序包或系统软件包扩展名（大小写不敏感）。
fn is_macos_package_bundle(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            MACOS_PACKAGE_BUNDLE_EXTS
                .iter()
                .any(|blocked| ext.eq_ignore_ascii_case(blocked))
        })
}

/// 校验文件传输存储目录存在、为普通目录（非 macOS 软件包/符号链接指向包）且具备实际写入权限
/// （不覆盖既有文件，自动清理探针文件）。
fn validate_transfer_dir_writable(dir: &Path, create_if_missing: bool) -> Result<(), String> {
    if is_macos_package_bundle(dir) {
        return Err(format!(
            "不能选择应用程序包或系统软件包作为存储路径：{}",
            dir.display()
        ));
    }
    if !dir.exists() {
        if create_if_missing {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("无法创建默认存储目录：{}（{e}）", dir.display()))?;
        } else {
            return Err(format!("所选目录不存在：{}", dir.display()));
        }
    }
    let canonical = std::fs::canonicalize(dir)
        .map_err(|e| format!("无法访问所选目录：{}（{e}）", dir.display()))?;
    if is_macos_package_bundle(&canonical) {
        return Err(format!(
            "不能选择应用程序包或系统软件包作为存储路径：{}",
            canonical.display()
        ));
    }
    let meta = std::fs::metadata(&canonical)
        .map_err(|e| format!("无法访问所选目录：{}（{e}）", canonical.display()))?;
    if !meta.is_dir() {
        return Err(format!("所选路径不是有效文件夹：{}", canonical.display()));
    }

    static PROBE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = PROBE_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let probe_path = canonical.join(format!(
        ".rc-write-probe-{}-{nanos}-{seq}",
        std::process::id()
    ));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe_path)
    {
        Ok(file) => {
            drop(file);
            let _ = std::fs::remove_file(&probe_path);
            Ok(())
        }
        Err(e) => Err(format!(
            "所选文件夹不可写，请检查目录权限：{}（{e}）",
            canonical.display()
        )),
    }
}

/// 判断 `/usr/bin/osascript` 的失败输出是否属于用户主动取消对话框。
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn is_osascript_user_canceled(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    stderr.contains("(-128)") || lower.contains("user canceled") || lower.contains("user cancelled")
}

/// 规范化文件夹选择器返回的 POSIX 路径（去除首尾换行及除根 `/` 外的多余末尾斜杠）。
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn normalize_picked_dir_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim_end_matches(&['\r', '\n'][..]).trim();
    if trimmed.is_empty() {
        return None;
    }
    let stripped = trimmed.trim_end_matches('/');
    if stripped.is_empty() {
        Some("/".to_string())
    } else {
        Some(stripped.to_string())
    }
}

fn save_general_prefs_to_path(
    state: &AppState,
    patch: GeneralPrefsPatch,
    prefs_path: &Path,
) -> Result<GeneralPrefsSnapshot, String> {
    let _io = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _file_lock = config_paths::acquire_file_lock(prefs_path, Duration::from_secs(3));
    let mut next = {
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        if state.is_service_proxy() && prefs_path.exists() {
            *prefs = app_prefs::load_from_path(prefs_path);
        }
        prefs.clone()
    };

    if let Some(close_to_tray) = patch.close_to_tray {
        next.close_to_tray = close_to_tray;
    }
    if let Some(allow_wol) = patch.allow_wol {
        next.allow_wol = allow_wol;
    }

    let mut updated_transfer_dir: Option<PathBuf> = None;
    if let Some(raw_dir) = patch.file_transfer_dir {
        let resolved = next.apply_file_transfer_dir(Some(raw_dir))?;
        let is_default = next.is_default_file_transfer_dir();
        validate_transfer_dir_writable(&resolved, is_default)?;
        updated_transfer_dir = Some(resolved);
    }

    app_prefs::save_to_path(prefs_path, &next)?;
    *state.prefs.lock().unwrap_or_else(|e| e.into_inner()) = next.clone();

    if let Some(resolved) = updated_transfer_dir {
        let host = state.host.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = host.handle.as_ref() {
            handle.set_file_transfer_dir(Some(resolved));
        }
    }

    if let Some(allow_wol) = patch.allow_wol {
        if !state.is_service_proxy() {
            let client = {
                let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
                rt.signal_client.as_ref().cloned()
            };
            if let Some(sig) = client {
                let local_mac = if allow_wol {
                    rc_ui::platform::primary_mac_address()
                } else {
                    None
                };
                apply_wol_state_to_signal(&sig, allow_wol, local_mac);
            }
        }
    }

    Ok(general_prefs_snapshot_from_prefs(&next))
}

/// IPC 命令：读取常规偏好（关闭窗口行为、文件传输存储路径等）。
#[tauri::command]
pub fn get_general_prefs(state: State<'_, AppState>) -> GeneralPrefsSnapshot {
    let prefs = sync_prefs_from_disk_if_proxy(&state);
    general_prefs_snapshot_from_prefs(&prefs)
}

/// IPC 命令：保存常规偏好至 `prefs.json`。
#[tauri::command]
pub fn save_general_prefs(
    patch: GeneralPrefsPatch,
    state: State<'_, AppState>,
) -> Result<GeneralPrefsSnapshot, String> {
    state.require_unlocked()?;
    let snap = save_general_prefs_to_path(&state, patch, &app_prefs::default_prefs_path())?;
    notify_background_services_reload(&state, false);
    Ok(snap)
}

fn pick_transfer_storage_dir_blocking(current_dir: &Path) -> Result<Option<String>, String> {
    #[cfg(target_os = "macos")]
    {
        let script = r#"on run argv
set defaultLoc to missing value
if (count of argv) >= 1 then
    try
        set defaultLoc to (POSIX file (item 1 of argv)) as alias
    on error
        set defaultLoc to missing value
    end try
end if
if defaultLoc is not missing value then
    set chosenFolder to choose folder with prompt "选择传输文件存储路径" default location defaultLoc
else
    set chosenFolder to choose folder with prompt "选择传输文件存储路径"
end if
return POSIX path of chosenFolder
end run"#;
        let mut cmd = std::process::Command::new("/usr/bin/osascript");
        cmd.arg("-e").arg(script);
        if current_dir.is_dir() {
            cmd.arg("--").arg(current_dir);
        }
        let output = cmd
            .output()
            .map_err(|e| format!("启动系统文件夹选择器失败：{e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if is_osascript_user_canceled(&stderr) {
                return Ok(None);
            }
            let msg = stderr.trim();
            return Err(if msg.is_empty() {
                "打开系统文件夹选择器失败".to_string()
            } else {
                format!("打开系统文件夹选择器失败：{msg}")
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(normalize_picked_dir_path(&stdout))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = current_dir;
        Err("当前平台暂不支持原生文件夹选择器".to_string())
    }
}

/// IPC 命令：弹出系统原生文件夹选择框选择 W2 传输文件存储路径；取消时返回 `Ok(None)`。
#[tauri::command]
pub async fn pick_transfer_storage_dir(
    state: State<'_, AppState>,
) -> Result<Option<String>, String> {
    state.require_unlocked()?;
    let current_dir = sync_prefs_from_disk_if_proxy(&state).resolved_file_transfer_dir();
    tauri::async_runtime::spawn_blocking(move || pick_transfer_storage_dir_blocking(&current_dir))
        .await
        .map_err(|e| format!("打开文件夹选择器任务异常：{e}"))?
}

fn open_transfer_storage_dir_blocking(dir: &Path, is_default: bool) -> Result<(), String> {
    if is_macos_package_bundle(dir) {
        return Err(format!(
            "不能打开应用程序包或系统软件包路径：{}",
            dir.display()
        ));
    }
    if !dir.exists() {
        if is_default {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("无法创建默认存储目录：{}（{e}）", dir.display()))?;
        } else {
            return Err("存储目录不存在或尚未挂载，请重新选择路径".to_string());
        }
    }
    let canonical = std::fs::canonicalize(dir)
        .map_err(|e| format!("无法访问存储目录：{}（{e}）", dir.display()))?;
    if is_macos_package_bundle(&canonical) {
        return Err(format!(
            "不能打开应用程序包或系统软件包路径：{}",
            canonical.display()
        ));
    }
    if !canonical.is_dir() {
        return Err(format!("存储路径不是有效文件夹：{}", canonical.display()));
    }
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("/usr/bin/open")
            .args(["-a", "Finder", "--"])
            .arg(&canonical)
            .status()
            .map_err(|e| format!("在访达中打开文件夹失败：{e}"))?;
        if !status.success() {
            return Err(format!(
                "在访达中打开文件夹失败（退出码 {:?}）",
                status.code()
            ));
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = canonical;
        Err("当前平台暂不支持打开系统文件夹".to_string())
    }
}

/// IPC 命令：在访达中打开当前生效的 W2 传输文件存储目录（默认目录不存在时自动创建，自定义目录缺失时返回可读错误）。
#[tauri::command]
pub async fn open_transfer_storage_dir(state: State<'_, AppState>) -> Result<(), String> {
    state.require_unlocked()?;
    let prefs = sync_prefs_from_disk_if_proxy(&state);
    let dir = prefs.resolved_file_transfer_dir();
    let is_default = prefs.is_default_file_transfer_dir();
    tauri::async_runtime::spawn_blocking(move || {
        open_transfer_storage_dir_blocking(&dir, is_default)
    })
    .await
    .map_err(|e| format!("打开存储目录任务异常：{e}"))?
}

/// IPC 命令：查询后台系统服务（LaunchDaemon / LaunchAgent / FileVault）安装与运行状态。
#[tauri::command]
pub async fn get_service_status() -> Result<ServiceStatusReport, String> {
    tauri::async_runtime::spawn_blocking(service_manager::query_service_status)
        .await
        .map_err(|e| format!("查询后台服务状态任务异常：{e}"))
}

/// 解析后的应用版本与构建元信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppVersionInfo {
    /// 展示用语义版本号（优先取自 `.app/Contents/Info.plist` 的 `CFBundleShortVersionString`）。
    pub version: String,
    /// 包内构建号（优先取自 `CFBundleVersion`，未配置时回退至 `version`）。
    pub bundle_version: String,
    /// 构建标识（优先取自编译期 `RC_GIT_SHA`，空时在 `bundle_version != version` 场景回退至 `bundle_version`）。
    pub build: String,
}

/// Core 统一构建与协议版本元数据 DTO（面向前端 IPC，owned 字段便于序列化与反序列化）。
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreBuildInfoDto {
    /// Core 语义化版本号（来自 `rc_client_core::version::core_version()`）。
    pub core_version: String,
    /// Core 构建时 Git 短提交哈希。
    pub git_commit: String,
    /// Core 构建时间戳。
    pub build_timestamp: String,
    /// 会话层控制协议版本号。
    pub session_protocol_version: u8,
    /// UDP P2P 打洞协议版本号。
    pub punch_version: u8,
    /// 局域网发现信标协议版本号。
    pub beacon_version: u8,
    /// Core 对外 ABI 版本号。
    pub abi_version: u32,
}

impl From<rc_client_core::version::CoreBuildInfo> for CoreBuildInfoDto {
    fn from(info: rc_client_core::version::CoreBuildInfo) -> Self {
        Self {
            core_version: info.core_version.to_string(),
            git_commit: info.git_commit.to_string(),
            build_timestamp: info.build_timestamp.to_string(),
            session_protocol_version: info.session_protocol_version,
            punch_version: info.punch_version,
            beacon_version: info.beacon_version,
            abi_version: info.abi_version,
        }
    }
}

/// 返回给前端「关于」弹窗的应用元信息（IPC `camelCase`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    /// 应用名称（如 `"RabbitControl"`）。
    pub name: String,
    /// 应用版本号（如 `"0.1.7"`）。
    pub version: String,
    /// 构建号或 Git 短 SHA（如 `"dbc3df3"`）。
    pub build: String,
    /// 应用 Bundle Identifier（如 `"com.rabbitcontrol.dev"`）。
    pub bundle_id: String,
    /// 操作系统名称（macOS 上展示 `"macOS"`）。
    pub os: String,
    /// CPU 架构（如 `"aarch64"` / `"x86_64"`）。
    pub arch: String,
    /// 开源许可证（如 `"AGPL-3.0-only"`）。
    pub license: String,
    /// 应用简介。
    pub description: String,
    /// Core 语义化版本号（来自 `rc_client_core::version::core_version()`）。
    #[serde(default)]
    pub core_version: String,
    /// Core 完整构建与协议版本元数据（来自 `rc_client_core::version::core_build_info()`）。
    #[serde(default)]
    pub core_build_info: Option<CoreBuildInfoDto>,
}

fn normalize_version_candidate(raw: &str) -> String {
    raw.trim().trim_start_matches('v').trim().to_string()
}

/// 从 `Info.plist` XML 文本中提取指定 `<key>` 对应的 `<string>` 值。
fn parse_plist_string_value(plist_xml: &str, key: &str) -> Option<String> {
    let key_tag = format!("<key>{key}</key>");
    let key_idx = plist_xml.find(&key_tag)?;
    let after_key = &plist_xml[key_idx + key_tag.len()..];
    let trimmed = after_key.trim_start();
    let after_open = trimmed.strip_prefix("<string>")?;
    let end_idx = after_open.find("</string>")?;
    let val = after_open[..end_idx].trim();
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    }
}

/// 按 `Info.plist` → 编译期 `RC_APP_VERSION` → `CARGO_PKG_VERSION` 优先级解析版本信息。
fn resolve_app_version_from_sources(
    plist_xml: Option<&str>,
    compile_version: &str,
    pkg_version: &str,
    git_sha: &str,
) -> AppVersionInfo {
    let plist_short = plist_xml
        .and_then(|xml| parse_plist_string_value(xml, "CFBundleShortVersionString"))
        .map(|s| normalize_version_candidate(&s))
        .filter(|s| !s.is_empty());
    let plist_bundle = plist_xml
        .and_then(|xml| parse_plist_string_value(xml, "CFBundleVersion"))
        .map(|s| normalize_version_candidate(&s))
        .filter(|s| !s.is_empty());
    let compile_ver = {
        let v = normalize_version_candidate(compile_version);
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    };
    let pkg_ver = normalize_version_candidate(pkg_version);

    let version = plist_short
        .or(compile_ver)
        .unwrap_or_else(|| pkg_ver.clone());
    let bundle_version = plist_bundle.unwrap_or_else(|| version.clone());
    let clean_sha = git_sha.trim().to_string();
    let build = if !clean_sha.is_empty() {
        clean_sha
    } else if bundle_version != version {
        bundle_version.clone()
    } else {
        String::new()
    };

    AppVersionInfo {
        version,
        bundle_version,
        build,
    }
}

/// 解析当前运行进程所属应用包的真实版本与构建号。
pub fn resolve_app_version_info() -> AppVersionInfo {
    let plist_xml = std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.parent()?
                .parent()
                .map(|contents| contents.join("Info.plist"))
        })
        .and_then(|plist_path| std::fs::read_to_string(plist_path).ok());

    resolve_app_version_from_sources(
        plist_xml.as_deref(),
        env!("RC_APP_VERSION"),
        env!("CARGO_PKG_VERSION"),
        env!("RC_GIT_SHA"),
    )
}

/// IPC 命令：获取当前应用名称、真实版本号、构建信息与运行环境元数据。
#[tauri::command]
pub fn get_app_info(app: AppHandle) -> AppInfo {
    let ver = resolve_app_version_info();
    let pkg_info = app.package_info();
    let config = app.config();
    let os = if cfg!(target_os = "macos") {
        "macOS".to_string()
    } else {
        std::env::consts::OS.to_string()
    };
    let description = config
        .bundle
        .short_description
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "低延迟远程桌面控制软件".to_string());

    let core_info = rc_client_core::version::core_build_info();
    AppInfo {
        name: pkg_info.name.clone(),
        version: ver.version,
        build: ver.build,
        bundle_id: config.identifier.clone(),
        os,
        arch: std::env::consts::ARCH.to_string(),
        license: env!("CARGO_PKG_LICENSE").to_string(),
        description,
        core_version: core_info.core_version.to_string(),
        core_build_info: Some(CoreBuildInfoDto::from(core_info)),
    }
}

/// 构造 `UpdateCheckResult`（计算 `has_update` 与当前架构匹配安装包）。
fn build_update_check_result(
    release: rc_ui::updater::ReleaseInfo,
) -> rc_ui::updater::UpdateCheckResult {
    let current_version = rc_ui::updater_macos::resolve_current_app_version();
    let arch = rc_ui::updater_macos::current_macos_arch().to_string();
    let latest_version = release.version.clone();
    let has_update = rc_ui::updater::is_newer_version(&latest_version, &current_version);
    let selected_asset = rc_ui::updater::select_asset_for_target(&release, "macos", &arch);
    rc_ui::updater::UpdateCheckResult {
        current_version,
        latest_version,
        has_update,
        arch,
        release,
        selected_asset,
    }
}

/// IPC 命令：检查最新版本（从 GitHub Releases 拉取元数据与 SHA-256 校验值并缓存在 `AppState`）。
#[tauri::command]
pub async fn check_for_updates(
    app: AppHandle,
) -> Result<rc_ui::updater::UpdateCheckResult, String> {
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        let mirror_url = sync_prefs_from_disk_if_proxy(&state).update_mirror_url;
        let release = rc_ui::updater_macos::fetch_latest_release(&mirror_url)?;
        *state
            .latest_release
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(release.clone());
        Ok(build_update_check_result(release))
    })
    .await
    .map_err(|e| format!("检查更新任务异常：{e}"))?
}

/// IPC 命令：读取最近一次缓存的 Release 元数据（若尚未检查则返回 `None`）。
#[tauri::command]
pub fn get_cached_release(state: State<'_, AppState>) -> Option<rc_ui::updater::ReleaseInfo> {
    state
        .latest_release
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// IPC 命令：读取「设置 → 网络」配置的更新镜像加速前缀。
#[tauri::command]
pub fn get_update_mirror_url(state: State<'_, AppState>) -> String {
    sync_prefs_from_disk_if_proxy(&state).update_mirror_url
}

/// IPC 命令：保存「设置 → 网络」更新镜像加速前缀至 `prefs.json` 并同步通知后台服务。
#[tauri::command]
pub fn save_update_mirror_url(
    mirror_url: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    state.require_unlocked()?;
    let sanitized = rc_ui::updater::sanitize_update_mirror_url(&mirror_url)?;
    let prefs_path = app_prefs::default_prefs_path();
    let _io = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _file_lock = config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
    let (prev, next) = {
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        let prev = prefs.clone();
        if state.is_service_proxy() && prefs_path.exists() {
            *prefs = app_prefs::load_from_path(&prefs_path);
        }
        prefs.apply_update_mirror_url(&sanitized)?;
        (prev, prefs.clone())
    };
    if let Err(err) = app_prefs::save_to_path(&prefs_path, &next) {
        *state.prefs.lock().unwrap_or_else(|e| e.into_inner()) = prev;
        return Err(err);
    }
    notify_background_services_reload(&state, false);
    Ok(next.update_mirror_url)
}

/// IPC 命令：启动本机自更新（下载 → SHA-256 校验 → codesign 证书钉扎 → 原子替换 → 重启）。
#[tauri::command]
pub async fn start_self_update(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.require_unlocked()?;
    if !cfg!(target_os = "macos") {
        return Err("当前平台暂不支持自动安装更新".to_string());
    }
    if state
        .self_update_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err("正在执行本机更新，请勿重复操作".to_string());
    }
    state.self_update_cancel.store(false, Ordering::Release);

    let running_flag = Arc::clone(&state.self_update_running);
    let cancel_flag = Arc::clone(&state.self_update_cancel);
    let app_bg = app.clone();

    std::thread::Builder::new()
        .name("rc-self-update".to_string())
        .spawn(move || {
            struct RunningGuard(Arc<std::sync::atomic::AtomicBool>);
            impl Drop for RunningGuard {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::Release);
                }
            }
            let _guard = RunningGuard(running_flag);

            let emit_progress = |p: rc_ui::updater::UpdateProgress| {
                let _ = app_bg.emit("updater://progress", &p);
            };

            let st = app_bg.state::<AppState>();
            let mirror_url = sync_prefs_from_disk_if_proxy(&st).update_mirror_url;
            let cached = st
                .latest_release
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let release = match cached {
                Some(r) => r,
                None => match rc_ui::updater_macos::fetch_latest_release(&mirror_url) {
                    Ok(r) => {
                        *st.latest_release.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(r.clone());
                        r
                    }
                    Err(err) => {
                        emit_progress(rc_ui::updater::UpdateProgress {
                            stage: rc_ui::updater::UpdateStage::Failed,
                            progress_pct: 0,
                            downloaded_bytes: 0,
                            total_bytes: 0,
                            target_version: String::new(),
                            message: err.clone(),
                            need_controller_relay: false,
                            error: Some(err),
                        });
                        return;
                    }
                },
            };

            let current_ver = rc_ui::updater_macos::resolve_current_app_version();
            if !rc_ui::updater::is_newer_version(&release.version, &current_ver) {
                let err = format!("当前已是最新版本（v{current_ver}），无需更新");
                emit_progress(rc_ui::updater::UpdateProgress {
                    stage: rc_ui::updater::UpdateStage::Failed,
                    progress_pct: 0,
                    downloaded_bytes: 0,
                    total_bytes: 0,
                    target_version: release.version,
                    message: err.clone(),
                    need_controller_relay: false,
                    error: Some(err),
                });
                return;
            }

            let arch = rc_ui::updater_macos::current_macos_arch();
            let Some(asset) = rc_ui::updater::select_asset_for_target(&release, "macos", arch)
            else {
                let err = format!("未找到适用于当前架构（{arch}）的 macOS 安装包");
                emit_progress(rc_ui::updater::UpdateProgress {
                    stage: rc_ui::updater::UpdateStage::Failed,
                    progress_pct: 0,
                    downloaded_bytes: 0,
                    total_bytes: 0,
                    target_version: release.version,
                    message: err.clone(),
                    need_controller_relay: false,
                    error: Some(err),
                });
                return;
            };
            let Some(expected_sha) = asset
                .sha256
                .clone()
                .filter(|s| rc_ui::updater::is_valid_sha256_hex(s))
            else {
                let err = "当前 Release 缺少有效的 SHA-256 校验摘要，已中止更新".to_string();
                emit_progress(rc_ui::updater::UpdateProgress {
                    stage: rc_ui::updater::UpdateStage::Failed,
                    progress_pct: 0,
                    downloaded_bytes: 0,
                    total_bytes: 0,
                    target_version: release.version,
                    message: err.clone(),
                    need_controller_relay: false,
                    error: Some(err),
                });
                return;
            };

            let target_ver = release.version.clone();
            emit_progress(rc_ui::updater::UpdateProgress {
                stage: rc_ui::updater::UpdateStage::Downloading,
                progress_pct: 0,
                downloaded_bytes: 0,
                total_bytes: asset.size,
                target_version: target_ver.clone(),
                message: format!("正在下载 v{target_ver}..."),
                need_controller_relay: false,
                error: None,
            });

            let mut rand_bytes = [0u8; 8];
            rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut rand_bytes);
            let suffix: String = rand_bytes.iter().map(|b| format!("{b:02x}")).collect();
            let dest_path = std::env::temp_dir().join(format!(
                "rc-self-update-{}-{suffix}.dmg",
                std::process::id()
            ));
            struct CleanupTmp(std::path::PathBuf);
            impl Drop for CleanupTmp {
                fn drop(&mut self) {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
            let _cleanup = CleanupTmp(dest_path.clone());

            let effective_url = if !mirror_url.is_empty() {
                rc_ui::updater::apply_mirror_prefix(&mirror_url, &asset.download_url)
                    .unwrap_or_else(|_| asset.download_url.clone())
            } else {
                asset.download_url.clone()
            };

            let mut progress_cb = |done: u64, total: u64| {
                let effective_total = if total > 0 { total } else { asset.size };
                let pct = if effective_total > 0 {
                    done.saturating_mul(100)
                        .checked_div(effective_total)
                        .unwrap_or(0)
                        .min(100) as u8
                } else {
                    0
                };
                emit_progress(rc_ui::updater::UpdateProgress {
                    stage: rc_ui::updater::UpdateStage::Downloading,
                    progress_pct: pct,
                    downloaded_bytes: done,
                    total_bytes: effective_total,
                    target_version: target_ver.clone(),
                    message: format!("正在下载 v{target_ver}..."),
                    need_controller_relay: false,
                    error: None,
                });
            };

            let dl_res = match rc_ui::updater_macos::download_update_package(
                &effective_url,
                &expected_sha,
                &dest_path,
                &cancel_flag,
                &mut progress_cb,
            ) {
                Ok(()) => Ok(()),
                Err(first_err)
                    if effective_url != asset.download_url
                        && !cancel_flag.load(Ordering::Acquire) =>
                {
                    // 镜像下载失败时自动回退 GitHub 直连重试一次。
                    rc_ui::updater_macos::download_update_package(
                        &asset.download_url,
                        &expected_sha,
                        &dest_path,
                        &cancel_flag,
                        &mut progress_cb,
                    )
                    .map_err(|direct_err| {
                        format!("镜像与直连下载均失败（镜像: {first_err}；直连: {direct_err}）")
                    })
                }
                Err(e) => Err(e),
            };

            if let Err(err) = dl_res {
                emit_progress(rc_ui::updater::UpdateProgress {
                    stage: rc_ui::updater::UpdateStage::Failed,
                    progress_pct: 0,
                    downloaded_bytes: 0,
                    total_bytes: asset.size,
                    target_version: target_ver,
                    message: err.clone(),
                    need_controller_relay: false,
                    error: Some(err),
                });
                return;
            }

            if cancel_flag.load(Ordering::Acquire) {
                let err = "已取消更新下载".to_string();
                emit_progress(rc_ui::updater::UpdateProgress {
                    stage: rc_ui::updater::UpdateStage::Failed,
                    progress_pct: 0,
                    downloaded_bytes: 0,
                    total_bytes: asset.size,
                    target_version: target_ver,
                    message: err.clone(),
                    need_controller_relay: false,
                    error: Some(err),
                });
                return;
            }

            emit_progress(rc_ui::updater::UpdateProgress {
                stage: rc_ui::updater::UpdateStage::Installing,
                progress_pct: 100,
                downloaded_bytes: asset.size,
                total_bytes: asset.size,
                target_version: target_ver.clone(),
                message: "校验签名通过，正在原子替换应用...".to_string(),
                need_controller_relay: false,
                error: None,
            });

            if let Err(err) = rc_ui::updater_macos::install_verified_update(
                &dest_path,
                &expected_sha,
                true,
                false,
            ) {
                emit_progress(rc_ui::updater::UpdateProgress {
                    stage: rc_ui::updater::UpdateStage::Failed,
                    progress_pct: 0,
                    downloaded_bytes: asset.size,
                    total_bytes: asset.size,
                    target_version: target_ver,
                    message: err.clone(),
                    need_controller_relay: false,
                    error: Some(err),
                });
                return;
            }

            emit_progress(rc_ui::updater::UpdateProgress {
                stage: rc_ui::updater::UpdateStage::Restarting,
                progress_pct: 100,
                downloaded_bytes: asset.size,
                total_bytes: asset.size,
                target_version: target_ver.clone(),
                message: format!("v{target_ver} 安装完成，正在重启应用..."),
                need_controller_relay: false,
                error: None,
            });
        })
        .map_err(|e| {
            state.self_update_running.store(false, Ordering::Release);
            format!("启动更新线程失败：{e}")
        })?;

    Ok(())
}

/// IPC 命令：取消正在进行的本机自更新下载。
#[tauri::command]
pub fn cancel_self_update(state: State<'_, AppState>) -> Result<(), String> {
    state.require_unlocked()?;
    state.self_update_cancel.store(true, Ordering::Release);
    Ok(())
}

/// IPC 命令：读取当前活动会话中对端被控端上报的版本与更新能力元数据。
#[tauri::command]
pub fn get_session_host_version(
    state: State<'_, AppState>,
) -> Option<rc_ui::updater::RemoteHostVersion> {
    let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
    let active = slot.as_ref()?;
    if active.disconnecting {
        return None;
    }
    let hv = state.host_version.lock().unwrap_or_else(|e| e.into_inner());
    hv.as_ref()
        .filter(|(sid, _)| *sid == active.id)
        .map(|(_, info)| info.clone())
}

/// IPC 命令：向当前活动会话的对端被控端发起经用户明确确认的远程更新。
///
/// 安全红线（A3）：仅当被控端经 `OwnerTicket` Ed25519 PoP 确认 `is_owner_session == true`
/// 时才允许放行；非本人自控的临时协助会话一律 fail-closed 拒绝。
#[tauri::command]
pub async fn trigger_remote_update(
    target_version: Option<String>,
    app: AppHandle,
) -> Result<rc_ui::updater::RemoteHostVersion, String> {
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        state.require_unlocked()?;

        let (sid, host_ver) = {
            let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
            let Some(active) = slot.as_ref().filter(|a| !a.disconnecting) else {
                return Err("当前没有活动的远控会话".to_string());
            };
            let hv_guard = state.host_version.lock().unwrap_or_else(|e| e.into_inner());
            let Some((_, hv)) = hv_guard.as_ref().filter(|(s, _)| *s == active.id) else {
                return Err(
                    "尚未收到被控端版本信息（被控端可能为旧版本，不支持远程自动更新）".to_string(),
                );
            };
            (active.id, hv.clone())
        };

        if !host_ver.is_owner_session {
            return Err("安全限制：仅同账号本人自控会话允许远程更新被控端".to_string());
        }
        if !host_ver.update_capable {
            return Err("当前被控端运行环境暂不支持自动安装更新".to_string());
        }

        let mirror_url = sync_prefs_from_disk_if_proxy(&state).update_mirror_url;
        let cached = state
            .latest_release
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let wanted = target_version
            .as_deref()
            .map(|s| s.trim().trim_start_matches(['v', 'V']))
            .filter(|s| !s.is_empty());
        let release = match cached {
            Some(r) if wanted.map_or(true, |w| w.eq_ignore_ascii_case(&r.version)) => r,
            _ => {
                let fresh = rc_ui::updater_macos::fetch_latest_release(&mirror_url)?;
                *state
                    .latest_release
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(fresh.clone());
                fresh
            }
        };

        if !rc_ui::updater::is_newer_version(&release.version, &host_ver.version) {
            return Err(format!("被控端当前已是最新版本（v{}）", host_ver.version));
        }

        let Some(asset) =
            rc_ui::updater::select_asset_for_target(&release, &host_ver.os, &host_ver.arch)
        else {
            return Err(format!(
                "未找到适用于被控端架构（{}）的安装包",
                host_ver.arch
            ));
        };
        let Some(expected_sha256) = asset
            .sha256
            .clone()
            .filter(|s| rc_ui::updater::is_valid_sha256_hex(s))
        else {
            return Err("目标 Release 缺少有效 SHA-256 校验摘要，已拒绝远程推送".to_string());
        };

        let req = rc_ui::updater::RemoteUpdateRequest {
            target_version: release.version.clone(),
            download_url: asset.download_url.clone(),
            expected_sha256,
            asset_name: asset.name.clone(),
            mirror_url,
            relay_fallback_allowed: true,
        };

        let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        let Some(active) = slot.as_ref().filter(|a| !a.disconnecting && a.id == sid) else {
            return Err("远控会话已断开或已切换，请重试".to_string());
        };
        active.handle.trigger_remote_update(req);
        Ok(host_ver)
    })
    .await
    .map_err(|e| format!("触发远程更新任务异常：{e}"))?
}

/// IPC 命令：取消当前活动会话中正在进行的远程被控端更新。
#[tauri::command]
pub fn cancel_remote_update(state: State<'_, AppState>) -> Result<(), String> {
    state.require_unlocked()?;
    let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = slot.as_ref().filter(|a| !a.disconnecting) else {
        return Ok(());
    };
    active.handle.cancel_remote_update();
    Ok(())
}

/// 显示并前置 `"main"` 控制端主窗口（macOS 上同步取消应用隐藏并激活至最前）。
pub(crate) fn show_and_focus_main_window(app: &AppHandle) {
    metal_attach::activate_app_and_unhide();
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

/// 在独立模式下于 macOS 主线程安装顶部菜单栏图标与交互动作。
pub(crate) fn install_standalone_status_bar(app: &AppHandle) {
    let st = app.state::<AppState>();
    if st.is_service_proxy() {
        return;
    }
    let server_snap = {
        let rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
        rt.current_status()
    };
    let connected_peer = {
        let host = st.host.lock().unwrap_or_else(|e| e.into_inner());
        host.connected_peer.clone()
    };
    let perms = permissions::check_permissions();
    let initial = StatusBarDisplay {
        device_id: server_snap.device_id.or_else(|| {
            st.server
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .config
                .cached_device_id
                .clone()
        }),
        signal_online: server_snap.state == ServerConnectionState::Connected,
        connected_peer,
        missing_permissions: perms.platform_supported && !perms.host_ready,
        session_label: "独立托管模式".to_string(),
    };
    let app_cb = app.clone();
    let _ = metal_attach::install_status_bar(
        initial,
        Arc::new(move |action| match action {
            StatusBarAction::OpenMainGui => {
                show_and_focus_main_window(&app_cb);
            }
            StatusBarAction::QuitAll => {
                quit_app_completely_inner(&app_cb);
            }
        }),
    );
}

/// 执行「整体退出」：设置退出标志、停止本地及后台守护服务并退出主程序。
pub(crate) fn quit_app_completely_inner(app: &AppHandle) {
    let st = app.state::<AppState>();
    if st.quitting.swap(true, Ordering::AcqRel) {
        return;
    }
    st.mark_quitting();
    let is_proxy = st.is_service_proxy();
    let app_worker = app.clone();
    let _ = std::thread::Builder::new()
        .name("rc-quit-all".to_string())
        .spawn(move || {
            if is_proxy {
                let secret_path = config_paths::ipc_secret_path();
                let _ = IpcClient::call(
                    &config_paths::agent_socket_path(),
                    &secret_path,
                    ServiceRequest::QuitAll,
                    Duration::from_millis(800),
                );
                let _ = IpcClient::call(
                    &config_paths::daemon_socket_path(),
                    &secret_path,
                    ServiceRequest::QuitAll,
                    Duration::from_millis(800),
                );
            }
            shutdown_host_and_lan(&app_worker);
            let app_ui = app_worker.clone();
            let _ = app_worker.run_on_main_thread(move || {
                metal_attach::remove_status_bar();
                app_ui.exit(0);
            });
        });
}

/// IPC 命令：一键安装并启动 macOS 系统级后台服务（LaunchDaemon + 双会话 LaunchAgent）。
#[tauri::command]
pub async fn install_system_service(app: AppHandle) -> Result<ServiceStatusReport, String> {
    app.state::<AppState>().require_unlocked()?;
    let app_for_blocking = app.clone();
    let report = tauri::async_runtime::spawn_blocking(move || {
        let st = app_for_blocking.state::<AppState>();
        let _transition_guard = st
            .service_transition_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 安装前先释放独立模式占用的 9901 端口与信令/认证线程，避免与后台 Agent 冲突。
        stop_auth_maintainer(&st);
        let _ = stop_host_service(&st, true);
        let _ = stop_signal_client_for_logout(&st);

        let install_res = service_manager::install_system_service();
        match install_res {
            Ok(mut report) => {
                if report.installed && !report.manual_stopped && !report.proxy_active {
                    for _ in 0..20 {
                        std::thread::sleep(Duration::from_millis(200));
                        let latest = service_manager::query_service_status();
                        report = latest;
                        if report.proxy_active {
                            break;
                        }
                    }
                }
                let use_proxy = report.proxy_active || (report.installed && !report.manual_stopped);
                st.set_service_proxy(use_proxy);
                if use_proxy {
                    st.rebind_storage_to_active_paths();
                    rebind_gui_ipc(&app_for_blocking);
                    let _ = app_for_blocking.run_on_main_thread(move || {
                        metal_attach::remove_status_bar();
                    });
                    notify_background_services_reload_sync(&st, true);
                    let _ = app_for_blocking.emit("gate://status", &st.gate_status_snapshot());
                } else {
                    let app_main = app_for_blocking.clone();
                    let _ = app_for_blocking.run_on_main_thread(move || {
                        install_standalone_status_bar(&app_main);
                    });
                    post_auth_ready(&app_for_blocking, &st);
                }
                Ok(report)
            }
            Err(err) => {
                // 若用户取消管理员授权，先核查当前系统服务是否仍在托管：若仍在运行则保持代理模式，
                // 避免误切回独立模式产生双托盘图标与 9901 端口/设备连接冲突。
                let current = service_manager::query_service_status();
                let keep_proxy =
                    current.proxy_active || (current.installed && !current.manual_stopped);
                st.set_service_proxy(keep_proxy);
                if keep_proxy {
                    st.rebind_storage_to_active_paths();
                    rebind_gui_ipc(&app_for_blocking);
                    let _ = app_for_blocking.run_on_main_thread(move || {
                        metal_attach::remove_status_bar();
                    });
                    notify_background_services_reload_sync(&st, true);
                    let _ = app_for_blocking.emit("gate://status", &st.gate_status_snapshot());
                } else {
                    let app_main = app_for_blocking.clone();
                    let _ = app_for_blocking.run_on_main_thread(move || {
                        install_standalone_status_bar(&app_main);
                    });
                    post_auth_ready(&app_for_blocking, &st);
                }
                Err(err)
            }
        }
    })
    .await
    .map_err(|e| format!("安装系统后台服务任务异常：{e}"))??;
    let _ = app.emit("service://status", &report);
    Ok(report)
}

/// IPC 命令：卸载 macOS 系统级后台服务并无缝切回前台独立托管模式。
#[tauri::command]
pub async fn uninstall_system_service(app: AppHandle) -> Result<ServiceStatusReport, String> {
    app.state::<AppState>().require_unlocked()?;
    let app_for_blocking = app.clone();
    let report = tauri::async_runtime::spawn_blocking(move || {
        let st = app_for_blocking.state::<AppState>();
        let _transition_guard = st
            .service_transition_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let report = service_manager::uninstall_system_service()?;
        st.set_service_proxy(false);
        st.rebind_storage_to_active_paths();
        rebind_gui_ipc(&app_for_blocking);
        {
            let mut rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
            rt.shutting_down = false;
        }
        let app_main = app_for_blocking.clone();
        let _ = app_for_blocking.run_on_main_thread(move || {
            install_standalone_status_bar(&app_main);
        });
        post_auth_ready(&app_for_blocking, &st);
        Ok::<_, String>(report)
    })
    .await
    .map_err(|e| format!("卸载系统后台服务任务异常：{e}"))??;
    let _ = app.emit("service://status", &report);
    Ok(report)
}

static PENDING_TRAY_PERM_CHECK: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 记录一次尚未被前端确认的「从后台菜单栏发起的权限检查」唤醒信号。
pub(crate) fn mark_tray_perm_check_pending() {
    PENDING_TRAY_PERM_CHECK.store(true, Ordering::Release);
}

/// IPC 命令：消费一次后台菜单栏点击「检查系统权限…」留下的待处理唤醒标记。
#[tauri::command]
pub fn consume_tray_perm_check() -> bool {
    let marker = config_paths::run_dir().join("open-perm-check");
    let removed_file = std::fs::remove_file(&marker).is_ok();
    let had_pending = PENDING_TRAY_PERM_CHECK.swap(false, Ordering::AcqRel);
    removed_file || had_pending
}

/// 前端「设置 → 网络」UPnP / NAT-PMP 自动端口映射开关与实时状态快照。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpnpStatusPayload {
    /// 是否已开启 UPnP / NAT-PMP 自动端口映射。
    pub enabled: bool,
    /// 当前实时端口映射状态。
    pub status: UpnpStatus,
}

fn query_upnp_status_payload(state: &AppState) -> UpnpStatusPayload {
    let enabled = sync_prefs_from_disk_if_proxy(state).upnp_enabled;
    if !enabled {
        return UpnpStatusPayload {
            enabled: false,
            status: UpnpStatus::Disabled,
        };
    }
    let status = if state.is_service_proxy() {
        match call_agent_ipc(ServiceRequest::GetSnapshot, Duration::from_millis(500)) {
            Ok(ServiceResponse::Snapshot(snap)) => {
                snap.upnp_status.unwrap_or(UpnpStatus::Discovering)
            }
            _ => UpnpStatus::Unavailable {
                reason: "后台服务暂未响应".to_string(),
            },
        }
    } else {
        let client = {
            let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
            rt.signal_client.as_ref().cloned()
        };
        match client {
            Some(c) => c.upnp_status(),
            None => UpnpStatus::Unavailable {
                reason: "信令服务器未连接".to_string(),
            },
        }
    };
    UpnpStatusPayload { enabled, status }
}

fn save_upnp_enabled_to_path(state: &AppState, enabled: bool, path: &Path) -> Result<(), String> {
    persist_prefs_mutation(state, path, |prefs| {
        prefs.upnp_enabled = enabled;
    })
}

/// IPC 命令：查询 UPnP / NAT-PMP 自动端口映射开关与当前状态。
#[tauri::command]
pub async fn get_upnp_status(app: AppHandle) -> Result<UpnpStatusPayload, String> {
    app.state::<AppState>().require_authenticated()?;
    let app_bg = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let st = app_bg.state::<AppState>();
        query_upnp_status_payload(&st)
    })
    .await
    .map_err(|e| format!("查询 UPnP 状态任务异常：{e}"))
}

/// IPC 命令：持久化开关 UPnP / NAT-PMP 自动端口映射并立即生效。
#[tauri::command]
pub async fn save_upnp_enabled(enabled: bool, app: AppHandle) -> Result<UpnpStatusPayload, String> {
    {
        let st = app.state::<AppState>();
        st.require_unlocked()?;
        st.require_authenticated()?;
    }
    let app_bg = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let st = app_bg.state::<AppState>();
        save_upnp_enabled_to_path(&st, enabled, &app_prefs::default_prefs_path())?;
        if st.is_service_proxy() {
            notify_background_services_reload_sync(&st, false);
        } else {
            let client = {
                let rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
                rt.signal_client.as_ref().cloned()
            };
            if let Some(c) = client {
                c.set_upnp_enabled(enabled);
            }
        }
        Ok(query_upnp_status_payload(&st))
    })
    .await
    .map_err(|e| format!("保存 UPnP 设置任务异常：{e}"))?
}

// ─────────────────────────────────────────────────────────────────
// 出站网络代理（Issue #5）与 Wake-on-LAN 远程开机（Issue #7）IPC 命令
// ─────────────────────────────────────────────────────────────────

/// 面向前端渲染层的脱敏网络代理配置视图（绝不回传代理密码明文）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyPrefsView {
    /// 代理模式（`"none"` / `"system"` / `"manual"`）。
    pub mode: ProxyMode,
    /// 手动代理协议（`"socks5"` / `"http"`）。
    pub protocol: Option<ProxyProtocol>,
    /// 代理服务器主机名或 IP。
    pub host: String,
    /// 代理服务器端口。
    pub port: u16,
    /// 可选代理认证用户名。
    pub username: Option<String>,
    /// 是否已在本地保存了代理认证密码（用于 UI 展示脱敏占位，绝不回传明文）。
    pub has_password: bool,
}

/// 保存出站网络代理配置请求（自定义 `Debug` 对密码字段脱敏，防日志泄露）。
#[derive(Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SaveProxyPrefsRequest {
    /// 代理模式（`"none"` / `"system"` / `"manual"`）。
    pub mode: ProxyMode,
    /// 手动代理协议（`"socks5"` / `"http"`）。
    pub protocol: Option<ProxyProtocol>,
    /// 代理服务器主机名或 IP。
    pub host: String,
    /// 代理服务器端口。
    pub port: u16,
    /// 可选代理认证用户名。
    pub username: Option<String>,
    /// 可选新代理认证密码（留空且 `clear_password == false` 时保留已保存密码）。
    pub password: Option<String>,
    /// 为 `true` 时显式清空已保存密码。
    pub clear_password: bool,
}

impl fmt::Debug for SaveProxyPrefsRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted = self.password.as_ref().map(|_| "<redacted>");
        f.debug_struct("SaveProxyPrefsRequest")
            .field("mode", &self.mode)
            .field("protocol", &self.protocol)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &redacted)
            .field("clear_password", &self.clear_password)
            .finish()
    }
}

fn proxy_prefs_view_from_config(cfg: &ProxyConfig) -> ProxyPrefsView {
    ProxyPrefsView {
        mode: cfg.mode,
        protocol: cfg.protocol,
        host: cfg.host.clone(),
        port: cfg.port,
        username: cfg.username.clone(),
        has_password: cfg.password.as_deref().is_some_and(|p| !p.is_empty()),
    }
}

fn save_proxy_prefs_to_path(
    state: &AppState,
    req: SaveProxyPrefsRequest,
    prefs_path: &Path,
) -> Result<(ProxyPrefsView, ProxyConfig, bool), UiCommandError> {
    let _io = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _file_lock = config_paths::acquire_file_lock(prefs_path, Duration::from_secs(3));

    let mut next = {
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        if state.is_service_proxy() && prefs_path.exists() {
            *prefs = app_prefs::load_from_path(prefs_path);
        }
        prefs.clone()
    };

    let prev_proxy = next.proxy.clone();
    let username = req.username.filter(|u| !u.is_empty());
    let password = if req.clear_password {
        None
    } else if let Some(pw) = req.password.filter(|p| !p.is_empty()) {
        Some(pw)
    } else if username.is_none() && prev_proxy.username.is_some() {
        // 用户清空了先前保存的用户名且未提供新密码时，一并清空遗留密码避免孤立凭据。
        None
    } else {
        prev_proxy.password.clone()
    };

    let candidate = ProxyConfig {
        mode: req.mode,
        protocol: req.protocol,
        host: req.host.trim().to_string(),
        port: req.port,
        username,
        password,
    };

    if let Err(err) = candidate.validate() {
        return Err(UiCommandError::from_ui_error(
            err.to_ui_error(ErrorPlatform::Macos),
        ));
    }

    let changed = prev_proxy != candidate;
    next.proxy = candidate.clone();

    app_prefs::save_to_path(prefs_path, &next).map_err(|e| {
        UiCommandError::from_ui_error_with_message(
            e.clone(),
            ui_error::resolve_ui_error(
                codes::VALIDATION_INVALID_CONFIG,
                ErrorPlatform::Macos,
                &[],
                None,
                Some(e),
            ),
        )
    })?;
    *state.prefs.lock().unwrap_or_else(|e| e.into_inner()) = next;

    Ok((proxy_prefs_view_from_config(&candidate), candidate, changed))
}

/// IPC 命令：读取当前出站网络代理配置（脱敏视图，不返回明文密码）。
#[tauri::command]
pub fn get_proxy_prefs(state: State<'_, AppState>) -> ProxyPrefsView {
    let prefs = sync_prefs_from_disk_if_proxy(&state);
    proxy_prefs_view_from_config(&prefs.proxy)
}

/// IPC 命令：校验并保存出站网络代理配置，立即更新全局代理并在变更时触发信令重连及后台服务重载。
#[tauri::command]
pub async fn save_proxy_prefs(
    req: SaveProxyPrefsRequest,
    app: AppHandle,
) -> Result<ProxyPrefsView, UiCommandError> {
    app.state::<AppState>()
        .require_unlocked()
        .map_err(UiCommandError::from_auth_gate_error)?;
    let app_bg = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let st = app_bg.state::<AppState>();
        let (view, active_cfg, changed) =
            save_proxy_prefs_to_path(&st, req, &app_prefs::default_prefs_path())?;
        proxy::set_global_proxy_config(active_cfg);
        if changed {
            if st.is_service_proxy() {
                notify_background_services_reload_sync(&st, false);
            } else {
                let client = {
                    let rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
                    rt.signal_client.as_ref().cloned()
                };
                if let Some(sig) = client {
                    sig.trigger_reconnect("proxy_config_changed");
                }
            }
        }
        Ok(view)
    })
    .await
    .map_err(|e| {
        UiCommandError::from_ui_error(ui_error::resolve_ui_error(
            codes::COMMON_UNKNOWN,
            ErrorPlatform::Macos,
            &[],
            None,
            Some(format!("保存网络代理设置任务异常：{e}")),
        ))
    })?
}

/// 发起 Wake-on-LAN 远程开机请求的纯状态执行实现（兼容独立模式与系统服务代理模式）。
fn request_device_wol_inner(
    state: &AppState,
    target_device_id: &str,
) -> Result<WolAckResult, UiCommandError> {
    state
        .require_unlocked()
        .map_err(UiCommandError::from_auth_gate_error)?;
    state
        .require_authenticated()
        .map_err(UiCommandError::from_auth_gate_error)?;

    let norm_id = server_config::parse_device_id_input(target_device_id).ok_or_else(|| {
        UiCommandError::from_validation_error("目标设备码格式无效，请输入 9 位数字设备码")
    })?;

    if state.is_service_proxy() {
        match call_agent_ipc(
            ServiceRequest::RequestWol {
                target_device_id: norm_id,
                timeout_ms: 8_000,
            },
            Duration::from_secs(10),
        ) {
            Ok(ServiceResponse::WolAck(ack)) => Ok(*ack),
            Ok(ServiceResponse::WolError(ui_err)) => Err(UiCommandError::from_ui_error(*ui_err)),
            Ok(ServiceResponse::Error { message }) => {
                let ui_err = ui_error::classify_signal_error(&message, ErrorPlatform::Macos);
                Err(UiCommandError::from_ui_error_with_message(message, ui_err))
            }
            Ok(other) => Err(UiCommandError::from_ui_error(ui_error::resolve_ui_error(
                codes::COMMON_UNKNOWN,
                ErrorPlatform::Macos,
                &[],
                None,
                Some(format!("后台服务返回了意外的远程开机响应：{other:?}")),
            ))),
            Err(err) => {
                let ui_err = ui_error::classify_signal_error(&err, ErrorPlatform::Macos);
                Err(UiCommandError::from_ui_error_with_message(err, ui_err))
            }
        }
    } else {
        let client = {
            let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
            rt.signal_client.as_ref().cloned()
        };
        let Some(sig) = client else {
            return Err(UiCommandError::from_ui_error(ui_error::resolve_ui_error(
                codes::NETWORK_UNREACHABLE,
                ErrorPlatform::Macos,
                &[],
                None,
                Some("请先在「设置 → 网络」配置并连接服务器".to_string()),
            )));
        };
        sig.request_wol(&norm_id, Duration::from_secs(8))
            .map_err(UiCommandError::from_ui_error)
    }
}

/// IPC 命令：向同账号离线设备发起 Wake-on-LAN 远程开机请求。
#[tauri::command]
pub async fn request_device_wol(
    target_device_id: String,
    app: AppHandle,
) -> Result<WolAckResult, UiCommandError> {
    let app_bg = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let st = app_bg.state::<AppState>();
        request_device_wol_inner(&st, &target_device_id)
    })
    .await
    .map_err(|e| {
        UiCommandError::from_ui_error(ui_error::resolve_ui_error(
            codes::COMMON_UNKNOWN,
            ErrorPlatform::Macos,
            &[],
            None,
            Some(format!("发送远程开机请求任务异常：{e}")),
        ))
    })?
}

/// 判断当前是否仍有 S1 远控窗口或任一 W2/W3/W4 工具窗口打开。
pub(crate) fn has_any_session_or_tool_window(app: &AppHandle) -> bool {
    app.get_webview_window("session").is_some()
        || app.get_webview_window("terminal").is_some()
        || app.get_webview_window("transfer").is_some()
        || app.get_webview_window("portmap").is_some()
}

fn is_signal_online_or_connecting(state: &AppState) -> bool {
    let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
    let snap = rt.current_status();
    matches!(
        snap.state,
        ServerConnectionState::Connected | ServerConnectionState::Connecting
    )
}

fn resolve_reconnect_target(
    state: &AppState,
    meta: &mut ActiveSessionMeta,
    prefer_device_id_upgrade: bool,
) -> Result<ConnectTarget, String> {
    let parsed = model::parse_connect_input(&meta.target)?;
    if meta.relay_target_id.is_none() {
        let (_, _, found_relay_id) =
            resolve_session_target_meta(state, &meta.target, &parsed, Some(&meta.device_name));
        meta.relay_target_id = found_relay_id;
    }
    if meta.connection_mode == ConnectionMode::TcpRelay {
        return effective_target_for_mode(
            parsed,
            meta.relay_target_id.as_deref(),
            ConnectionMode::TcpRelay,
        );
    }
    if prefer_device_id_upgrade {
        if let (ConnectTarget::Direct(_), Some(ref did)) = (&parsed, &meta.relay_target_id) {
            if is_signal_online_or_connecting(state) {
                return Ok(ConnectTarget::DeviceId(did.clone()));
            }
        }
    }
    Ok(parsed)
}

async fn reconnect_with_saved_params(
    app: &AppHandle,
    state: &AppState,
    my_epoch: u64,
    watch_only: bool,
    mut meta: ActiveSessionMeta,
    prefer_device_id_upgrade: bool,
) -> Result<(), String> {
    if state.session_open_epoch.load(Ordering::Acquire) != my_epoch {
        return Ok(());
    }
    let effective_target = resolve_reconnect_target(state, &mut meta, prefer_device_id_upgrade)?;
    let force_relay = meta.connection_mode == ConnectionMode::TcpRelay;
    meta.force_relay = force_relay;

    let (connect_addr, relay_grant) =
        resolve_endpoint_and_grant(state, effective_target, force_relay, Some(my_epoch)).await?;
    if state.session_open_epoch.load(Ordering::Acquire) != my_epoch {
        return Ok(());
    }

    let relay_target_for_sec = meta.relay_target_id.clone();
    let grant_for_sec = relay_grant.clone();
    let encrypt_conn = meta.encrypt_connection;
    let app_sec = app.clone();
    let security = tauri::async_runtime::spawn_blocking(move || {
        let st_sec = app_sec.state::<AppState>();
        prepare_session_security(
            &st_sec,
            relay_target_for_sec.as_deref(),
            grant_for_sec.as_ref(),
            force_relay,
            encrypt_conn,
        )
    })
    .await
    .map_err(|e| format!("准备重连安全配置异常：{e}"))??;

    if state.session_open_epoch.load(Ordering::Acquire) != my_epoch {
        return Ok(());
    }

    if meta.tools_only && app.get_webview_window("session").is_some() {
        meta.tools_only = false;
        meta.media_active = true;
    }

    meta.connected = false;
    meta.transport = None;
    meta.mtls_active = false;
    meta.peer_cert_fingerprint = None;
    meta.phase = "connecting".to_string();
    meta.tool_permissions = state::ToolPermissionsSnapshot::default();

    let signal_client = state
        .server
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .signal_client
        .clone();
    let ui_cfg = UiSessionConfig {
        connect_addr,
        password: meta.password.clone(),
        watch_only,
        width: meta.width,
        height: meta.height,
        fps: meta.fps,
        bitrate_bps: meta.bitrate_bps,
        prefer_hardware: meta.prefer_hardware,
        clipboard_enabled: meta.clipboard_enabled,
        audio_enabled: if meta.tools_only {
            false
        } else {
            meta.audio_enabled
        },
        relay_grant,
        force_relay,
        connection_mode: meta.connection_mode,
        tools_only: meta.tools_only,
        encrypt_connection: meta.encrypt_connection,
        mtls_config: security.mtls_config,
        owner_grant: security.owner_grant,
        owner_signer: security.owner_signer,
        lan_direct_addr: security.lan_direct_addr,
        signal_client,
        target_device_id: meta.relay_target_id.clone(),
    };

    activate_session_with_cfg(app, state, my_epoch, ui_cfg, watch_only, meta, None)
}

/// 自动重连退避序列（8 次 / 约 49 秒）。
const AUTO_RECONNECT_BACKOFFS_SECS: [u64; 8] = [1, 2, 3, 5, 8, 10, 10, 10];
/// 自动重连最大重试次数。
pub(crate) const AUTO_RECONNECT_MAX_ATTEMPTS: u32 = AUTO_RECONNECT_BACKOFFS_SECS.len() as u32;
/// 切网或用户点击「立即重试」唤醒后的短防抖延迟（500ms）。
const AUTO_RECONNECT_WAKE_DEBOUNCE: Duration = Duration::from_millis(500);

/// 启动后台无感自动重连任务（退避 `[1s, 2s, 3s, 5s, 8s, 10s, 10s, 10s]`，支持切网与「立即重试」唤醒；
/// 8 次耗尽后仅推送不可恢复 `Ended` 与 `inactive` 并记录终态错误，绝不自动关闭 S1 窗口）。
pub(crate) fn spawn_auto_reconnect_task(
    app: AppHandle,
    expected_epoch: u64,
    initial_reason: String,
) {
    let st = app.state::<AppState>();
    let start_idx = st.auto_reconnect_attempts.load(Ordering::Acquire) as usize;
    let initial_attempt = ((start_idx + 1) as u32).min(AUTO_RECONNECT_MAX_ATTEMPTS);
    let Some(guard) =
        st.try_acquire_auto_reconnect(expected_epoch, initial_attempt, initial_reason.clone())
    else {
        return;
    };
    let app_bg = app.clone();
    tauri::async_runtime::spawn(async move {
        let guard = guard;
        let st = app_bg.state::<AppState>();
        let max_attempts = AUTO_RECONNECT_MAX_ATTEMPTS;
        let start_idx = st.auto_reconnect_attempts.load(Ordering::Acquire) as usize;
        let mut last_err = initial_reason;

        for (idx, &delay_secs) in AUTO_RECONNECT_BACKOFFS_SECS
            .iter()
            .enumerate()
            .skip(start_idx)
        {
            if st.session_open_epoch.load(Ordering::Acquire) != expected_epoch
                || !has_any_session_or_tool_window(&app_bg)
                || st.is_quitting()
                || st.gate_state() != AppGateState::Authenticated
            {
                return;
            }
            let Some((watch_only, mut meta)) = st
                .last_session_params
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            else {
                last_err = "暂无可重连的历史远控会话参数".to_string();
                break;
            };

            let has_session_window = app_bg.get_webview_window("session").is_some();
            if has_session_window {
                meta.media_active = true;
                meta.tools_only = false;
            } else {
                meta.media_active = false;
                meta.tools_only = true;
            }
            meta.phase = "reconnecting".to_string();
            meta.connected = false;
            meta.tool_permissions = state::ToolPermissionsSnapshot::default();
            save_last_session_params(&st, watch_only, meta.clone());

            let attempt = (idx + 1) as u32;
            st.update_auto_reconnect_status(expected_epoch, attempt, last_err.clone());
            let _ = app_bg.emit(
                "session://update",
                &SessionUpdate::Reconnecting {
                    attempt,
                    max_attempts,
                    reason: last_err.clone(),
                },
            );
            let _ = app_bg.emit(
                "session://active-state",
                &ActiveSessionInfo::reconnecting_from_meta(watch_only, &meta),
            );

            // 使用条件变量等待本轮退避：被切网或用户「立即重试」唤醒时缩短至 500ms 立刻尝试。
            let wake_pair = Arc::clone(&st.auto_reconnect_wake);
            let wait_dur = Duration::from_secs(delay_secs);
            let woken_early = tauri::async_runtime::spawn_blocking(move || {
                let (lock, cv) = &*wake_pair;
                let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                let start_seq = *guard;
                let (after_guard, _wait_res) = cv
                    .wait_timeout_while(guard, wait_dur, |seq| *seq == start_seq)
                    .unwrap_or_else(|e| e.into_inner());
                *after_guard != start_seq
            })
            .await
            .unwrap_or(false);

            if st.session_open_epoch.load(Ordering::Acquire) != expected_epoch
                || !has_any_session_or_tool_window(&app_bg)
                || st.is_quitting()
            {
                return;
            }
            if woken_early {
                let _ = tauri::async_runtime::spawn_blocking(|| {
                    std::thread::sleep(AUTO_RECONNECT_WAKE_DEBOUNCE);
                })
                .await;
                if st.session_open_epoch.load(Ordering::Acquire) != expected_epoch
                    || !has_any_session_or_tool_window(&app_bg)
                    || st.is_quitting()
                {
                    return;
                }
            }

            st.auto_reconnect_attempts.store(attempt, Ordering::Release);
            match reconnect_with_saved_params(&app_bg, &st, expected_epoch, watch_only, meta, true)
                .await
            {
                Ok(()) => {
                    return;
                }
                Err(err) => {
                    last_err = err;
                }
            }
        }

        // 8 次全部耗尽：仅在代次未变更时记录终态错误、释放活跃令牌并通知终态失败与非活动态，绝不自动关闭 S1 窗口。
        if st.session_open_epoch.load(Ordering::Acquire) == expected_epoch {
            let reason = format!("自动重连失败（已重试 {max_attempts} 次）：{last_err}");
            let ended = session_ended_with_error(reason.clone(), false);
            let ui_err = match &ended {
                SessionUpdate::Ended { ui_error, .. } => ui_error.clone(),
                _ => None,
            };
            st.auto_reconnect_attempts.store(0, Ordering::Release);
            st.record_terminal_error_with_ui_if_epoch(expected_epoch, reason, ui_err);
            drop(guard);
            let _ = app_bg.emit("session://active-state", &ActiveSessionInfo::inactive());
            let _ = app_bg.emit("session://update", &ended);
        }
    });
}

/// IPC 命令：在远端触发 `ByeReason::SessionSwitching`（macOS 登录界面切换桌面）或连接失败后复用上次参数重新连接。
#[tauri::command]
pub async fn reconnect_last_session(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let (watch_only, mut meta) = {
        let guard = state
            .last_session_params
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard
            .clone()
            .ok_or_else(|| "暂无可重连的历史远控会话参数".to_string())?
    };

    let my_epoch = state.session_open_epoch.fetch_add(1, Ordering::AcqRel) + 1;
    cancel_auto_reconnect(&state);
    state.clear_terminal_error();

    meta.phase = "connecting".to_string();
    meta.connected = false;
    meta.transport = None;
    meta.mtls_active = false;
    meta.peer_cert_fingerprint = None;
    meta.tool_permissions = state::ToolPermissionsSnapshot::default();
    save_last_session_params(&state, watch_only, meta.clone());
    let _connecting_guard = ConnectingEpochGuard::begin(&state, my_epoch);

    let previous = {
        let mut slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        state::clear_session_snapshot_locked(&state.last_session_update);
        *state.host_version.lock().unwrap_or_else(|e| e.into_inner()) = None;
        slot.take()
    };
    if let Some(old) = previous {
        let _ = old.handle.tools().detach_all_terminals();
        let _ = app.emit("portmap://update", &Vec::<PortMapRuleSnapshot>::new());
        teardown_session_and_wait(&app, old).await;
    }

    let _ = app.emit(
        "session://active-state",
        &ActiveSessionInfo::connecting_from_meta(watch_only, &meta),
    );
    let _ = app.emit(
        "session://update",
        &SessionUpdate::Connecting {
            phase: rc_ui::session_bridge::ConnectPhase::Connecting,
            watch_only,
        },
    );

    if let Err(err) =
        reconnect_with_saved_params(&app, &state, my_epoch, watch_only, meta, true).await
    {
        state.clear_connecting_epoch(my_epoch);
        if state.session_open_epoch.load(Ordering::Acquire) == my_epoch {
            let ended = session_ended_with_error(err.clone(), false);
            let ui_err = match &ended {
                SessionUpdate::Ended { ui_error, .. } => ui_error.clone(),
                _ => None,
            };
            state.record_terminal_error_with_ui_if_epoch(my_epoch, err.clone(), ui_err);
            let _ = app.emit("session://active-state", &ActiveSessionInfo::inactive());
            let _ = app.emit("session://update", &ended);
        }
        return Err(err);
    }
    Ok(())
}

/// IPC 命令：立即重试连接（若处于自动重连退避期则立刻唤醒；否则直接复用上次连接参数立即重连）。
#[tauri::command]
pub async fn trigger_reconnect_now(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    if state.is_auto_reconnect_active() {
        state.wake_auto_reconnect();
        return Ok(());
    }
    reconnect_last_session(app, state).await
}

/// IPC 命令：彻底「整体退出」RabbitControl（同时终止系统后台服务与控制端界面，不触发自动重启）。
#[tauri::command]
pub fn quit_app_completely(app: AppHandle) -> Result<(), String> {
    quit_app_completely_inner(&app);
    Ok(())
}

/// 在当前生效运行目录重新绑定前台 GUI 单实例唤醒用 `gui.sock`。
pub(crate) fn rebind_gui_ipc(app: &AppHandle) {
    let st = app.state::<AppState>();
    if let Some(mut old) = st.gui_ipc.lock().unwrap_or_else(|e| e.into_inner()).take() {
        old.stop();
    }
    if let Ok(secret) =
        rc_ui::service_ipc::read_or_create_ipc_secret(&config_paths::ipc_secret_path())
    {
        let app_wake = app.clone();
        if let Ok(gui_srv) = rc_ui::service_ipc::IpcServer::bind(
            config_paths::gui_socket_path(),
            secret,
            move |req| match req {
                ServiceRequest::Ping => ServiceResponse::Pong {
                    pid: std::process::id(),
                    session_type: rc_ui::service_ipc::ServiceSessionType::Standalone,
                    binary_sha256: None,
                },
                ServiceRequest::OpenMainGui => {
                    let app_ui = app_wake.clone();
                    let _ = app_wake.run_on_main_thread(move || {
                        show_and_focus_main_window(&app_ui);
                    });
                    ServiceResponse::Ok
                }
                _ => ServiceResponse::Error {
                    message: "gui.sock 仅支持 Ping 与 OpenMainGui".to_string(),
                },
            },
        ) {
            *st.gui_ipc.lock().unwrap_or_else(|e| e.into_inner()) = Some(gui_srv);
        }
    }
}

/// 客户端「安全锁定」状态快照（供主窗口锁屏遮罩与设置页读取）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientLockStatus {
    /// 当前主界面是否处于安全锁定状态。
    pub locked: bool,
    /// 是否已设置 8–16 位安全密码。
    pub has_password: bool,
    /// 脱敏展示的当前登录账号（未登录时为空）。
    pub account_masked: Option<String>,
}

fn build_client_lock_status(state: &AppState) -> ClientLockStatus {
    let (locked, has_password) = {
        let prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        (prefs.client_lock_enabled, prefs.has_client_lock_password())
    };
    let account_masked = state
        .auth
        .current_session()
        .map(|s| app_prefs::mask_account_display(&s.email))
        .filter(|s| !s.is_empty());
    ClientLockStatus {
        locked,
        has_password,
        account_masked,
    }
}

/// IPC 命令：查询客户端「安全锁定」当前状态。
#[tauri::command]
pub fn get_client_lock_status(state: State<'_, AppState>) -> ClientLockStatus {
    build_client_lock_status(&state)
}

/// IPC 命令：开启客户端「安全锁定」（首次锁定或修改密码时传入 8–16 位安全密码，已设密码时可传 `None` 直接锁定）。
#[tauri::command]
pub fn lock_client(
    password: Option<String>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ClientLockStatus, String> {
    let prefs_path = app_prefs::default_prefs_path();
    {
        let _io = state
            .prefs_io_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _file_lock = config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
        let next = {
            let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
            if state.is_service_proxy() && prefs_path.exists() {
                *prefs = app_prefs::load_from_path(&prefs_path);
            }
            let prev = prefs.clone();
            if prefs.client_lock_enabled {
                if password.is_some() {
                    return Err("客户端已处于锁定状态，请先解锁后再修改安全密码".to_string());
                }
                drop(prefs);
                drop(_file_lock);
                drop(_io);
                return Ok(build_client_lock_status(&state));
            }
            if let Some(pw) = password {
                if prefs.has_client_lock_password() {
                    return Err("已设置安全密码，修改密码需先校验当前安全密码".to_string());
                }
                if let Err(err) = prefs.set_client_lock_password(&pw) {
                    *prefs = prev;
                    return Err(err);
                }
            } else if !prefs.has_client_lock_password() {
                return Err("请先设置 8-16 位安全密码".to_string());
            }
            prefs.client_lock_enabled = true;
            let cloned = prefs.clone();
            if let Err(err) = app_prefs::save_to_path(&prefs_path, &cloned) {
                *prefs = prev;
                return Err(err);
            }
            cloned
        };
        let _ = next;
    }
    // 锁定时关闭已打开的设置、远控与工具窗口并停止快捷键拦截器，防止通过已开窗口绕过锁定。
    stop_shortcut_tap_runtime(&state);
    disconnect_active_session(&app);
    window::close_session_and_tool_windows(&app);
    let app_main = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(win) = app_main.get_webview_window("settings") {
            let _ = win.destroy();
        }
    });
    let status = build_client_lock_status(&state);
    let _ = app.emit("client://lock-changed", &status);
    Ok(status)
}

/// IPC 命令：校验安全密码并解除客户端「安全锁定」。
#[tauri::command]
pub fn unlock_client(
    password: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ClientLockStatus, String> {
    let prefs_path = app_prefs::default_prefs_path();
    {
        let _io = state
            .prefs_io_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _file_lock = config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        if state.is_service_proxy() && prefs_path.exists() {
            *prefs = app_prefs::load_from_path(&prefs_path);
        }
        if !prefs.client_lock_enabled {
            return Ok(ClientLockStatus {
                locked: false,
                has_password: prefs.has_client_lock_password(),
                account_masked: state
                    .auth
                    .current_session()
                    .map(|s| app_prefs::mask_account_display(&s.email))
                    .filter(|s| !s.is_empty()),
            });
        }
        if !prefs.verify_client_lock_password(&password) {
            return Err("安全密码错误，请重新输入".to_string());
        }
        let prev = prefs.clone();
        prefs.client_lock_enabled = false;
        if let Err(err) = app_prefs::save_to_path(&prefs_path, &prefs) {
            *prefs = prev;
            return Err(err);
        }
    }
    let status = build_client_lock_status(&state);
    let _ = app.emit("client://lock-changed", &status);
    Ok(status)
}

/// IPC 命令：修改客户端「安全锁定」密码（需先处于已解锁态；若已有旧密码须提供当前密码校验）。
#[tauri::command]
pub fn change_client_lock_password(
    current_password: Option<String>,
    new_password: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ClientLockStatus, String> {
    state.require_unlocked()?;
    let prefs_path = app_prefs::default_prefs_path();
    {
        let _io = state
            .prefs_io_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _file_lock = config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        if state.is_service_proxy() && prefs_path.exists() {
            *prefs = app_prefs::load_from_path(&prefs_path);
        }
        if prefs.has_client_lock_password() {
            let cur = current_password.as_deref().unwrap_or("");
            if !prefs.verify_client_lock_password(cur) {
                return Err("当前安全密码不正确".to_string());
            }
        }
        let prev = prefs.clone();
        if let Err(err) = prefs.set_client_lock_password(&new_password) {
            *prefs = prev;
            return Err(err);
        }
        if let Err(err) = app_prefs::save_to_path(&prefs_path, &prefs) {
            *prefs = prev;
            return Err(err);
        }
    }
    let status = build_client_lock_status(&state);
    let _ = app.emit("client://lock-changed", &status);
    Ok(status)
}

// ─────────────────────────────────────────────────────────────────
// 防窥隐私幕布 / 被控端静音 / 外设快捷键与鼠标模式 / 虚拟显示器 IPC 命令
// ─────────────────────────────────────────────────────────────────

/// 将 `data:image/<mime>;base64,<payload>` 解析为 `(ext, raw_bytes)` 并限制大小在 320 KiB 内。
fn decode_privacy_data_url(data_url: &str) -> Result<(String, Vec<u8>), String> {
    let trimmed = data_url.trim();
    let Some(rest) = trimmed.strip_prefix("data:") else {
        return Err("图片数据格式无效，缺少 data: 前缀".to_string());
    };
    let Some((header, b64)) = rest.split_once(',') else {
        return Err("图片数据格式无效，缺少 base64 分隔符".to_string());
    };
    if !header.ends_with(";base64") {
        return Err("仅支持 Base64 编码的 JPEG/PNG/WebP 图片".to_string());
    }
    let mime = header
        .trim_end_matches(";base64")
        .trim()
        .to_ascii_lowercase();
    let ext = match mime.as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        _ => return Err("仅支持 JPEG、PNG 或 WebP 图片".to_string()),
    };
    let raw = rc_client_core::display_ctrl::base64_decode(b64.trim())
        .ok_or_else(|| "图片 Base64 解码失败".to_string())?;
    if raw.is_empty() {
        return Err("图片内容不能为空".to_string());
    }
    if raw.len() > 320 * 1024 {
        return Err("自定义屏保图片压缩后不能超过 320 KiB".to_string());
    }
    Ok((ext.to_string(), raw))
}

fn privacy_file_to_data_url(filename: &str) -> Option<String> {
    let dir = app_prefs::default_privacy_dir();
    let bytes = app_prefs::load_privacy_custom_image(&dir, filename)?;
    let mime = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/jpeg"
    };
    let b64 = rc_client_core::display_ctrl::base64_encode(&bytes);
    Some(format!("data:{mime};base64,{b64}"))
}

/// IPC 命令：读取本机已保存的自定义隐私屏保图片（返回 Data URL）。
#[tauri::command]
pub fn get_privacy_custom_image(state: State<'_, AppState>) -> Option<String> {
    let fname = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .privacy_custom_image
        .clone()?;
    privacy_file_to_data_url(&fname)
}

/// IPC 命令：保存并持久化用户选择的自定义隐私屏保图片（返回 Data URL）。
#[tauri::command]
pub fn save_privacy_custom_image(
    data_url: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let (_ext, raw) = decode_privacy_data_url(&data_url)?;
    let dir = app_prefs::default_privacy_dir();
    let filename = app_prefs::save_privacy_custom_image(&dir, &raw)?;
    let prefs_path = app_prefs::default_prefs_path();
    {
        let _io = state
            .prefs_io_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _file_lock = config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        if state.is_service_proxy() && prefs_path.exists() {
            *prefs = app_prefs::load_from_path(&prefs_path);
        }
        let prev = prefs.clone();
        prefs.privacy_custom_image = Some(filename.clone());
        if let Err(err) = app_prefs::save_to_path(&prefs_path, &prefs) {
            *prefs = prev;
            return Err(err);
        }
    }
    privacy_file_to_data_url(&filename).ok_or_else(|| "保存隐私屏保图片后读取失败".to_string())
}

/// IPC 命令：设置当前远控会话的防窥模式并持久化偏好。
#[tauri::command]
pub fn set_session_privacy_mode(
    mode: String,
    preset: Option<String>,
    custom_image_data_url: Option<String>,
    persist: Option<bool>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let norm_mode = match mode.trim() {
        "privacy" => "privacy",
        "black" => "black",
        _ => "off",
    };
    let mut resolved_preset = preset
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            state
                .prefs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .privacy_preset
                .clone()
        });
    if resolved_preset.is_empty() {
        resolved_preset = "preset-1".to_string();
    }

    let custom_b64: Option<String> = if norm_mode == "privacy" && resolved_preset == "custom" {
        if let Some(ref du) = custom_image_data_url {
            let (_ext, raw) = decode_privacy_data_url(du)?;
            if persist.unwrap_or(true) {
                let dir = app_prefs::default_privacy_dir();
                if let Ok(fname) = app_prefs::save_privacy_custom_image(&dir, &raw) {
                    let prefs_path = app_prefs::default_prefs_path();
                    let _io = state
                        .prefs_io_lock
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let _fl = config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
                    let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
                    if state.is_service_proxy() && prefs_path.exists() {
                        *prefs = app_prefs::load_from_path(&prefs_path);
                    }
                    prefs.privacy_custom_image = Some(fname);
                    let _ = app_prefs::save_to_path(&prefs_path, &prefs);
                }
            }
            Some(rc_client_core::display_ctrl::base64_encode(&raw))
        } else {
            let fname = state
                .prefs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .privacy_custom_image
                .clone();
            let dir = app_prefs::default_privacy_dir();
            fname
                .and_then(|f| app_prefs::load_privacy_custom_image(&dir, &f))
                .map(|raw| rc_client_core::display_ctrl::base64_encode(&raw))
        }
    } else {
        None
    };

    if persist.unwrap_or(true) {
        let prefs_path = app_prefs::default_prefs_path();
        let _io = state
            .prefs_io_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _fl = config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        if state.is_service_proxy() && prefs_path.exists() {
            *prefs = app_prefs::load_from_path(&prefs_path);
        }
        let prev = prefs.clone();
        if let Err(err) = prefs.apply_privacy_patch(app_prefs::PrivacyPrefsPatch {
            privacy_mode: Some(norm_mode.to_string()),
            privacy_preset: Some(resolved_preset.clone()),
            host_mute_on_connect: None,
            host_restore_audio_on_exit: None,
        }) {
            *prefs = prev;
            return Err(err);
        }
        if let Err(err) = app_prefs::save_to_path(&prefs_path, &prefs) {
            *prefs = prev;
            return Err(err);
        }
    }

    let guard = state.session.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(active) = guard.as_ref() {
        if !active.disconnecting && !active.watch_only {
            active.handle.send_display_ctrl(
                rc_client_core::display_ctrl::DisplayControlMsg::SetPrivacyMode {
                    mode: norm_mode.to_string(),
                    preset: resolved_preset,
                    custom_image_base64: custom_b64,
                },
            );
        }
    }
    Ok(())
}

/// IPC 命令：设置被控端本机静音运行及远控结束后是否恢复声音设置。
#[tauri::command]
pub fn set_session_host_mute(
    muted: bool,
    restore_on_exit: Option<bool>,
    persist: Option<bool>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let eff_restore = restore_on_exit.unwrap_or_else(|| {
        state
            .prefs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .host_restore_audio_on_exit
    });

    if persist.unwrap_or(true) {
        let prefs_path = app_prefs::default_prefs_path();
        let _io = state
            .prefs_io_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _fl = config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        if state.is_service_proxy() && prefs_path.exists() {
            *prefs = app_prefs::load_from_path(&prefs_path);
        }
        let prev = prefs.clone();
        if let Err(err) = prefs.apply_privacy_patch(app_prefs::PrivacyPrefsPatch {
            privacy_mode: None,
            privacy_preset: None,
            host_mute_on_connect: Some(muted),
            host_restore_audio_on_exit: Some(eff_restore),
        }) {
            *prefs = prev;
            return Err(err);
        }
        if let Err(err) = app_prefs::save_to_path(&prefs_path, &prefs) {
            *prefs = prev;
            return Err(err);
        }
    }

    let guard = state.session.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(active) = guard.as_ref() {
        if !active.disconnecting && !active.watch_only {
            active.handle.send_display_ctrl(
                rc_client_core::display_ctrl::DisplayControlMsg::SetHostMute {
                    muted,
                    restore_on_exit: eff_restore,
                },
            );
        }
    }
    Ok(())
}

/// 卸载并释放当前运行的 macOS 系统快捷键拦截器（关闭 S1 / 断线 / 锁定 / 停机时调用，不留常驻全局钩子）。
pub(crate) fn stop_shortcut_tap_runtime(state: &AppState) {
    #[cfg(target_os = "macos")]
    {
        let old = state
            .shortcut_tap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(tap) = old {
            tap.set_enabled(false);
            drop(tap);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = state;
    }
}

/// 按显式 `(enabled, exceptions)` 同步 macOS 主动系统快捷键拦截器；关闭时直接卸载释放钩子线程。
#[cfg(target_os = "macos")]
pub(crate) fn sync_shortcut_tap_with_params(
    app: &AppHandle,
    state: &AppState,
    enabled: bool,
    exceptions: &[String],
) -> Result<(), String> {
    let has_controllable_session = {
        let guard = state.session.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_ref()
            .is_some_and(|active| !active.disconnecting && !active.watch_only)
    };
    if !enabled || !has_controllable_session {
        stop_shortcut_tap_runtime(state);
        return Ok(());
    }
    let session_focused = app
        .get_webview_window("session")
        .and_then(|w| w.is_focused().ok())
        .unwrap_or(false);

    let mut slot = state.shortcut_tap.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tap) = slot.as_ref() {
        tap.set_enabled(true);
        tap.set_exceptions(exceptions);
        tap.set_window_focused(session_focused);
        return Ok(());
    }

    let app_cb = app.clone();
    let on_event = Arc::new(move |ev: rc_input_macos::InterceptedShortcutEvent| {
        let st = app_cb.state::<AppState>();
        let guard = st.session.lock().unwrap_or_else(|e| e.into_inner());
        let Some(active) = guard.as_ref() else {
            return;
        };
        if active.disconnecting || active.watch_only {
            return;
        }
        // `SystemShortcutTap` 已通过 `FlagsChanged` 精确配对修饰键按下/抬起，此处直接按原序发送每个扫描码。
        active.handle.send_input(rc_input::InputEvent::Key {
            scancode: ev.scancode,
            pressed: ev.pressed,
            modifiers: ev.modifiers,
        });
    });

    let tap = rc_input_macos::SystemShortcutTap::spawn(true, exceptions, on_event)?;
    tap.set_window_focused(session_focused);
    *slot = Some(tap);
    Ok(())
}

/// 按当前持久化偏好同步 macOS 主动系统快捷键拦截器。
#[cfg(target_os = "macos")]
pub(crate) fn sync_shortcut_tap_runtime(app: &AppHandle, state: &AppState) -> Result<(), String> {
    let (enabled, exceptions) = {
        let p = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        (p.send_system_shortcuts, p.shortcut_exceptions.clone())
    };
    sync_shortcut_tap_with_params(app, state, enabled, &exceptions)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn sync_shortcut_tap_with_params(
    _app: &AppHandle,
    _state: &AppState,
    _enabled: bool,
    _exceptions: &[String],
) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn sync_shortcut_tap_runtime(_app: &AppHandle, _state: &AppState) -> Result<(), String> {
    Ok(())
}

/// 更新 S1 远控窗口焦点变化至系统快捷键拦截器。
pub(crate) fn notify_session_window_focus(state: &AppState, focused: bool) {
    #[cfg(target_os = "macos")]
    {
        if let Some(tap) = state
            .shortcut_tap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            tap.set_window_focused(focused);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (state, focused);
    }
}

/// IPC 命令：保存并实时应用外设偏好（发送系统快捷键、例外快捷键列表、鼠标模式）。
#[tauri::command]
pub fn set_session_peripherals(
    send_system_shortcuts: Option<bool>,
    shortcut_exceptions: Option<Vec<String>>,
    mouse_mode: Option<String>,
    persist: Option<bool>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<AppPrefs, String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let do_persist = persist.unwrap_or(true);
    let (prev_prefs_snapshot, next_prefs) = if do_persist {
        let prefs_path = app_prefs::default_prefs_path();
        let _io = state
            .prefs_io_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _fl = config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        if state.is_service_proxy() && prefs_path.exists() {
            *prefs = app_prefs::load_from_path(&prefs_path);
        }
        let prev = prefs.clone();
        if let Err(err) = prefs.apply_peripherals_patch(app_prefs::PeripheralsPrefsPatch {
            send_system_shortcuts,
            shortcut_exceptions: shortcut_exceptions.clone(),
            mouse_mode: mouse_mode.clone(),
        }) {
            *prefs = prev;
            return Err(err);
        }
        if let Err(err) = app_prefs::save_to_path(&prefs_path, &prefs) {
            *prefs = prev;
            return Err(err);
        }
        (Some(prev), prefs.clone())
    } else {
        let mut effective = state
            .prefs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        effective.apply_peripherals_patch(app_prefs::PeripheralsPrefsPatch {
            send_system_shortcuts,
            shortcut_exceptions,
            mouse_mode: mouse_mode.clone(),
        })?;
        (None, effective)
    };

    if send_system_shortcuts.is_some()
        || next_prefs.send_system_shortcuts
        || !next_prefs.shortcut_exceptions.is_empty()
    {
        if let Err(err) = sync_shortcut_tap_with_params(
            &app,
            &state,
            next_prefs.send_system_shortcuts,
            &next_prefs.shortcut_exceptions,
        ) {
            if send_system_shortcuts == Some(true) {
                // 拦截器初始化失败（如缺少辅助功能权限）时回滚持久化状态，避免磁盘与 UI 脱节。
                if let Some(prev) = prev_prefs_snapshot {
                    let prefs_path = app_prefs::default_prefs_path();
                    let _io = state
                        .prefs_io_lock
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let _fl = config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
                    let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
                    *prefs = prev;
                    let _ = app_prefs::save_to_path(&prefs_path, &prefs);
                }
                return Err(err);
            }
        }
    } else {
        let _ = sync_shortcut_tap_with_params(
            &app,
            &state,
            next_prefs.send_system_shortcuts,
            &next_prefs.shortcut_exceptions,
        );
    }

    if let Some(ref mode) = mouse_mode {
        let norm = match mode.trim() {
            "remote" => "remote",
            "local" => "local",
            _ => "smart",
        };
        let guard = state.session.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = guard.as_ref() {
            if !active.disconnecting && !active.watch_only {
                active.handle.send_display_ctrl(
                    rc_client_core::display_ctrl::DisplayControlMsg::SetMouseMode {
                        mode: norm.to_string(),
                    },
                );
            }
        }
    }

    Ok(sanitize_app_prefs_for_ui(next_prefs))
}

/// IPC 命令：请求被控端刷新全部可用物理与虚拟显示器列表。
#[tauri::command]
pub fn list_session_displays(state: State<'_, AppState>) -> Result<(), String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let guard = state.session.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = guard.as_ref() else {
        return Err("当前无活动远控会话".to_string());
    };
    if active.disconnecting {
        return Err("远控会话正在断开".to_string());
    }
    active
        .handle
        .send_display_ctrl(rc_client_core::display_ctrl::DisplayControlMsg::ListDisplays);
    Ok(())
}

/// IPC 命令：切换当前远控采集与控制的目标显示器。
#[tauri::command]
pub fn switch_session_display(display_id: u32, state: State<'_, AppState>) -> Result<(), String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let guard = state.session.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = guard.as_ref() else {
        return Err("当前无活动远控会话".to_string());
    };
    if active.disconnecting {
        return Err("远控会话正在断开".to_string());
    }
    active.handle.send_display_ctrl(
        rc_client_core::display_ctrl::DisplayControlMsg::SwitchDisplay { display_id },
    );
    Ok(())
}

/// IPC 命令：在被控端创建一个新的虚拟显示器并切至该屏。
#[tauri::command]
pub fn create_session_virtual_display(
    width: Option<u32>,
    height: Option<u32>,
    hidpi: Option<bool>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let guard = state.session.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = guard.as_ref() else {
        return Err("当前无活动远控会话".to_string());
    };
    if active.disconnecting {
        return Err("远控会话正在断开".to_string());
    }
    if active.watch_only {
        return Err("观看模式下无法创建虚拟屏".to_string());
    }
    active.handle.send_display_ctrl(
        rc_client_core::display_ctrl::DisplayControlMsg::CreateVirtualDisplay {
            width: width.unwrap_or(1920),
            height: height.unwrap_or(1080),
            hidpi: hidpi.unwrap_or(true),
        },
    );
    Ok(())
}

/// IPC 命令：销毁被控端指定的虚拟显示器。
#[tauri::command]
pub fn remove_session_virtual_display(
    display_id: u32,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.require_unlocked()?;
    state.require_authenticated()?;
    let guard = state.session.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = guard.as_ref() else {
        return Err("当前无活动远控会话".to_string());
    };
    if active.disconnecting {
        return Err("远控会话正在断开".to_string());
    }
    if active.watch_only {
        return Err("观看模式下无法移除虚拟屏".to_string());
    }
    active.handle.send_display_ctrl(
        rc_client_core::display_ctrl::DisplayControlMsg::RemoveVirtualDisplay { display_id },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_input::batch::modifier_bits as rc_mod;

    #[test]
    fn parse_connect_target_command_delegates_to_model() {
        assert_eq!(
            parse_connect_target("127.0.0.1:9901".to_string()).unwrap(),
            "127.0.0.1:9901"
        );
        let err = parse_connect_target("example.com:9901".to_string()).unwrap_err();
        assert!(err.contains("暂仅支持 IP:端口直连"));
    }

    #[test]
    fn validate_map_form_command_reports_field_errors() {
        let bad = validate_map_form(MapForm {
            name: String::new(),
            local_port: "0".to_string(),
            target_addr: "  ".to_string(),
            target_port: "70000".to_string(),
        });
        assert!(!bad.valid);
        assert!(bad.name.is_some());
        assert!(bad.local_port.is_some());
        assert!(bad.target_addr.is_some());
        assert!(bad.target_port.is_some());

        let good = validate_map_form(MapForm {
            name: "ssh-tunnel".to_string(),
            local_port: "2222".to_string(),
            target_addr: "127.0.0.1".to_string(),
            target_port: "22".to_string(),
        });
        assert!(good.valid);
        assert_eq!(good.name, None);
        assert_eq!(good.local_port, None);
        assert_eq!(good.target_addr, None);
        assert_eq!(good.target_port, None);
    }

    #[test]
    fn map_web_input_event_handles_all_kinds_and_unmapped_keys() {
        let (evs, unmapped) = map_web_input_event(&WebInputEvent::MouseMove {
            x: 500.0,
            y: 250.0,
            stage_w: 1000.0,
            stage_h: 500.0,
        });
        assert!(!unmapped);
        assert_eq!(evs, vec![InputEvent::MouseMove { x: 32768, y: 32768 }]);

        // 等比居中视频区对齐：14" MBP (1512x982) 渲染进 1280x752 stage 时，
        // metal_attach 的 Frame::fit_aspect 左右边缘精确对应 0 与 65535，无偏移。
        let stage_frame = Frame {
            x: 0.0,
            y: 48.0,
            width: 1280.0,
            height: 752.0,
        };
        let fitted = stage_frame.fit_aspect(1512, 982);
        let (left_evs, _) = map_web_input_event_for_video(
            &WebInputEvent::MouseMove {
                x: fitted.x,
                y: 0.0,
                stage_w: 1280.0,
                stage_h: 752.0,
            },
            Some((1512, 982)),
        );
        assert_eq!(left_evs, vec![InputEvent::MouseMove { x: 0, y: 0 }]);
        let (right_evs, _) = map_web_input_event_for_video(
            &WebInputEvent::MouseMove {
                x: fitted.x + fitted.width,
                y: fitted.height,
                stage_w: 1280.0,
                stage_h: 752.0,
            },
            Some((1512, 982)),
        );
        assert_eq!(
            right_evs,
            vec![InputEvent::MouseMove { x: 65535, y: 65535 }]
        );

        // DOM button: 0=Left, 1=Middle, 2=Right
        let (evs, _) = map_web_input_event(&WebInputEvent::MouseButton {
            button: 1,
            pressed: true,
        });
        assert_eq!(
            evs,
            vec![InputEvent::MouseButton {
                button: MouseButton::Middle,
                pressed: true
            }]
        );
        let (evs, _) = map_web_input_event(&WebInputEvent::MouseButton {
            button: 2,
            pressed: false,
        });
        assert_eq!(
            evs,
            vec![InputEvent::MouseButton {
                button: MouseButton::Right,
                pressed: false
            }]
        );

        let (evs, _) = map_web_input_event(&WebInputEvent::MouseWheel {
            delta_y_px: 32.0,
            line_height_px: 16.0,
            delta_x_px: 0.0,
        });
        assert_eq!(
            evs,
            vec![InputEvent::MouseWheel {
                delta: -2,
                horizontal: false
            }]
        );

        let (evs, unmapped) = map_web_input_event(&WebInputEvent::Key {
            code: "KeyA".to_string(),
            pressed: true,
            ctrl: true,
            shift: false,
            alt: false,
            meta: false,
        });
        assert!(!unmapped);
        assert_eq!(
            evs,
            vec![InputEvent::Key {
                scancode: 0x1E,
                pressed: true,
                modifiers: rc_mod::LEFT_CTRL,
            }]
        );

        // 未收录键码：静默丢弃并标记 unmapped=true
        let (evs, unmapped) = map_web_input_event(&WebInputEvent::Key {
            code: "UnknownVendorKey".to_string(),
            pressed: true,
            ctrl: false,
            shift: false,
            alt: false,
            meta: false,
        });
        assert!(unmapped);
        assert!(evs.is_empty());
    }

    #[test]
    fn css_rect_scales_to_physical_pixels() {
        let frame = css_rect_to_physical_frame(
            StageRectMsg {
                x: 10.0,
                y: 48.0,
                width: 1280.0,
                height: 752.0,
            },
            2.0,
        );
        assert_eq!(
            frame,
            Frame {
                x: 20.0,
                y: 96.0,
                width: 2560.0,
                height: 1504.0,
            }
        );
    }

    #[test]
    fn host_service_lifecycle_and_password_updates_use_ephemeral_port() {
        let state = AppState::new_for_test();
        assert!(!state.host.lock().unwrap().snapshot().enabled);
        assert!(disconnect_host_client_inner(&state).is_err());
        assert!(respond_approval_inner(&state, 1, true).is_err());

        let started = start_host_service_on(&state, "127.0.0.1:0".parse().unwrap(), false).unwrap();
        assert!(started.enabled);
        assert!(started.listen_addr.is_some());
        assert!(started.display_addr.is_some());
        assert!(!started.custom_password);
        assert_eq!(started.password.len(), host_bridge::TEMP_CODE_LEN);

        // 设置自定义密码
        let custom =
            set_host_password_inner(&state, Some("Secret#2026".to_string()), false).unwrap();
        assert!(custom.custom_password);
        assert_eq!(custom.password, "Secret#2026");

        // 清空密码（无密码模式）
        let cleared = set_host_password_inner(&state, Some(String::new()), false).unwrap();
        assert!(cleared.custom_password);
        assert!(cleared.password.is_empty());

        // 重新生成临时验证码
        let regen = set_host_password_inner(&state, None, true).unwrap();
        assert!(!regen.custom_password);
        assert_eq!(regen.password.len(), host_bridge::TEMP_CODE_LEN);

        // 运行期间 disconnect_host_client 返回 Ok；无待审批时 respond_approval 返回 Err。
        assert!(disconnect_host_client_inner(&state).is_ok());
        assert!(respond_approval_inner(&state, 42, true).is_err());

        // 停止服务并回收线程
        let stopped = stop_host_service(&state, true);
        assert!(!stopped.enabled);
        assert!(stopped.listen_addr.is_none());
        assert!(stopped.display_addr.is_none());
        assert!(disconnect_host_client_inner(&state).is_err());
    }

    /// 回归测试（U-M5）：关闭被控端服务时若有在途审批请求，`stop_host_service_with_cancelled`
    /// 必须取出并返回该审批代次供 `set_host_enabled(false)` 发射 `ApprovalCancelled`，且清空快照中的 `pending_approval`。
    #[test]
    fn stop_host_service_captures_pending_approval_generation_for_cancellation() {
        let state = AppState::new_for_test();
        let _ = start_host_service_on(&state, "127.0.0.1:0".parse().unwrap(), false).unwrap();

        {
            let mut host = state.host.lock().unwrap();
            host.pending_approval = Some(rc_ui::host_bridge::HostApprovalPrompt {
                generation: 17,
                session_id: 42,
                remote_addr: "192.168.1.99:55555".to_string(),
                requested_res: "1920x1080".to_string(),
                requested_fps: 60,
                password_verified: true,
            });
        }

        let (stopped, cancelled_gen) = stop_host_service_with_cancelled(&state, true);
        assert!(!stopped.enabled);
        assert!(stopped.pending_approval.is_none());
        assert_eq!(cancelled_gen, Some(17));

        // 再次停止（无在途审批）应返回 None。
        let (_, second_cancelled) = stop_host_service_with_cancelled(&state, true);
        assert_eq!(second_cancelled, None);
    }

    #[test]
    fn save_server_config_validates_preserves_device_key_and_persists() {
        let state = AppState::new_for_test();
        let original_key = state.server.lock().unwrap().config.device_key.clone();
        let dir =
            std::env::temp_dir().join(format!("rc-ui-cmd-server-cfg-test-{}", std::process::id()));
        let path = dir.join("server.json");

        // 启用但地址非法：必须拒绝，不落盘。
        let bad = save_server_config_to_path(
            &state,
            SaveServerConfigRequest {
                enabled: true,
                server_host: "   ".to_string(),
                signal_port: 21116,
                relay_port: 21117,
            },
            &path,
        );
        assert!(bad.is_err());

        // 保存禁用态合法配置：保留 device_key 并写入磁盘，递增 config_epoch。
        let disabled = save_server_config_to_path(
            &state,
            SaveServerConfigRequest {
                enabled: false,
                server_host: "http://172.16.1.2:21116/".to_string(),
                signal_port: 21116,
                relay_port: 21117,
            },
            &path,
        )
        .expect("save disabled config");
        assert!(!disabled.enabled);
        assert_eq!(disabled.server_host, "172.16.1.2");
        assert_eq!(disabled.signal_port, 21116);
        assert_eq!(disabled.relay_port, 21117);
        assert_eq!(disabled.state, ServerConnectionState::Disabled);
        assert_eq!(state.server.lock().unwrap().config_epoch, 1);

        let loaded = server_config::load_from_path(&path);
        assert_eq!(loaded.device_key, original_key);
        assert_eq!(loaded.server_host, "172.16.1.2");

        // 读取快照与当前状态一致。
        assert_eq!(get_server_config_inner(&state), disabled);

        // H8：停机后调用 save_server_config 不得派生孤儿 SignalClientHandle。
        shutdown_signal_client(&state);
        let after_shutdown = save_server_config_to_path(
            &state,
            SaveServerConfigRequest {
                enabled: true,
                server_host: "172.16.1.2".to_string(),
                signal_port: 21116,
                relay_port: 21117,
            },
            &path,
        )
        .expect("save during shutdown succeeds without spawning client");
        assert_eq!(after_shutdown.state, ServerConnectionState::Disabled);
        assert!(state.server.lock().unwrap().signal_client.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_command_arg_accepts_type_and_kind_tags() {
        let a: SessionCommandArg =
            serde_json::from_str(r#"{"type":"set_audio_muted","muted":true}"#).unwrap();
        assert_eq!(a.0, SessionCommand::SetAudioMuted { muted: true });

        let b: SessionCommandArg =
            serde_json::from_str(r#"{"kind":"set_audio_muted","muted":false}"#).unwrap();
        assert_eq!(b.0, SessionCommand::SetAudioMuted { muted: false });

        let c: SessionCommandArg =
            serde_json::from_str(r#"{"type":"set_clipboard_enabled","enabled":false}"#).unwrap();
        assert_eq!(c.0, SessionCommand::SetClipboardEnabled { enabled: false });

        let d: SessionCommandArg =
            serde_json::from_str(r#"{"kind":"set_clipboard_enabled","enabled":true}"#).unwrap();
        assert_eq!(d.0, SessionCommand::SetClipboardEnabled { enabled: true });

        let e: SessionCommandArg = serde_json::from_str(r#"{"type":"disconnect"}"#).unwrap();
        assert_eq!(e.0, SessionCommand::Disconnect);

        // L5：`type: null` 时回退使用 `kind`。
        let f: SessionCommandArg =
            serde_json::from_str(r#"{"type":null,"kind":"disconnect"}"#).unwrap();
        assert_eq!(f.0, SessionCommand::Disconnect);

        let g: SessionCommandArg =
            serde_json::from_str(r#"{"type":null,"kind":"set_audio_muted","muted":true}"#).unwrap();
        assert_eq!(g.0, SessionCommand::SetAudioMuted { muted: true });

        let h: SessionCommandArg =
            serde_json::from_str(r#"{"type":"set_codec","codec":"hevc"}"#).unwrap();
        assert_eq!(
            h.0,
            SessionCommand::SetCodec {
                codec: rc_ui::display_ctrl::StreamCodec::Hevc
            }
        );

        let i: SessionCommandArg =
            serde_json::from_str(r#"{"kind":"set_codec","codec":"h264"}"#).unwrap();
        assert_eq!(
            i.0,
            SessionCommand::SetCodec {
                codec: rc_ui::display_ctrl::StreamCodec::H264
            }
        );

        let j: SessionCommandArg =
            serde_json::from_str(r#"{"type":"set_codec","codec":"av1"}"#).unwrap();
        assert_eq!(
            j.0,
            SessionCommand::SetCodec {
                codec: rc_ui::display_ctrl::StreamCodec::Av1
            }
        );

        let k: SessionCommandArg =
            serde_json::from_str(r#"{"type":"set_still_refinement_enabled","enabled":false}"#)
                .unwrap();
        assert_eq!(
            k.0,
            SessionCommand::SetStillRefinementEnabled { enabled: false }
        );

        let l: SessionCommandArg =
            serde_json::from_str(r#"{"kind":"set_still_refinement_enabled","enabled":true}"#)
                .unwrap();
        assert_eq!(
            l.0,
            SessionCommand::SetStillRefinementEnabled { enabled: true }
        );
    }

    #[test]
    fn still_refinement_command_respects_app_prefs_and_tools_only_mode() {
        let state = AppState::new_for_test();
        // 默认偏好为 true：开启桌面会话时不下发关闭指令。
        assert!(state.prefs.lock().unwrap().still_refinement_enabled);
        assert_eq!(
            still_refinement_command_for_desktop_open(&state, false),
            None
        );
        assert_eq!(
            still_refinement_command_for_desktop_open(&state, true),
            None
        );

        // 用户偏好关闭时：开启/升级桌面会话下发 SetStillRefinementEnabled { enabled: false }；
        // 纯工具无头会话保持 None（待后续升级桌面时再下发）。
        state.prefs.lock().unwrap().still_refinement_enabled = false;
        assert_eq!(
            still_refinement_command_for_desktop_open(&state, false),
            Some(SessionCommand::SetStillRefinementEnabled { enabled: false })
        );
        assert_eq!(
            still_refinement_command_for_desktop_open(&state, true),
            None
        );
    }

    #[test]
    fn terminal_and_portmap_command_validators_enforce_bounds() {
        assert_eq!(
            validate_terminal_size(0, 24).unwrap_err(),
            "终端行列数必须在 1–512 之间"
        );
        assert_eq!(
            validate_terminal_size(80, 0).unwrap_err(),
            "终端行列数必须在 1–512 之间"
        );
        assert_eq!(
            validate_terminal_size(513, 24).unwrap_err(),
            "终端行列数必须在 1–512 之间"
        );
        assert_eq!(
            validate_terminal_size(80, 513).unwrap_err(),
            "终端行列数必须在 1–512 之间"
        );
        assert!(validate_terminal_size(1, 1).is_ok());
        assert!(validate_terminal_size(512, 512).is_ok());

        assert!(validate_terminal_input_len(64 * 1024).is_ok());
        assert_eq!(
            validate_terminal_input_len(64 * 1024 + 1).unwrap_err(),
            "单次终端输入不能超过 64 KiB"
        );

        assert!(validate_portmap_add("", 8080, "127.0.0.1", 80).is_err());
        assert!(validate_portmap_add("web", 0, "127.0.0.1", 80).is_err());
        assert!(validate_portmap_add("web", 8080, "127.0.0.1", 0).is_err());
        assert!(validate_portmap_add("web", 8080, "   ", 80).is_err());
        assert!(validate_portmap_add("web", 8080, "127.0.0.1", 80).is_ok());
    }

    #[test]
    fn local_file_commands_work_without_session_and_remote_tools_require_session() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-local-file-cmd-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp test dir");
        let parent = dir.to_string_lossy().into_owned();

        let created = file_mkdir_local_inner(parent.clone(), "sub_folder".to_string())
            .expect("file_mkdir_local should succeed without active session");
        assert!(created.ends_with("sub_folder"));

        let listed = file_list_local_inner(Some(parent), None)
            .expect("file_list_local should succeed without active session");
        assert!(listed
            .entries
            .iter()
            .any(|e| e.name == "sub_folder" && e.is_dir));

        let state = AppState::new_for_test();
        let err = active_session_tools(&state)
            .err()
            .expect("remote tools must fail when unauthenticated or no session is active");
        assert_eq!(err, "请先配置并连接 RabbitControl 服务器");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn app_state_and_host_runtime_default_disabled_on_init() {
        let rt = HostRuntime::new();
        assert!(rt.handle.is_none());
        let rt_snap = rt.snapshot();
        assert!(!rt_snap.enabled);
        assert!(rt_snap.listen_addr.is_none());
        assert!(rt_snap.display_addr.is_none());
        assert!(rt_snap.connected_peer.is_none());
        assert!(rt_snap.pending_approval.is_none());
        assert!(!rt_snap.custom_password);
        assert_eq!(rt_snap.password.len(), host_bridge::TEMP_CODE_LEN);

        let state = AppState::new_for_test();
        let snap = state.host.lock().unwrap().snapshot();
        assert!(!snap.enabled, "应用初始化时默认不得启用被控端监听");
        assert!(snap.listen_addr.is_none());
        assert!(snap.display_addr.is_none());
    }

    #[test]
    fn open_permission_settings_command_rejects_invalid_kind() {
        let err = open_permission_settings("unknown_perm".to_string()).unwrap_err();
        assert!(err.message.contains("unknown_perm"));
        assert_eq!(err.ui_error.code, codes::VALIDATION_INVALID_INPUT);
    }

    #[test]
    fn open_session_request_allows_omitted_optional_stream_fields() {
        let raw = r#"{"target":"192.168.1.10:9901","watchOnly":true,"deviceName":"Studio"}"#;
        let req: OpenSessionRequest =
            serde_json::from_str(raw).expect("OpenSessionRequest 应支持省略可选宽高帧率码率字段");
        assert_eq!(req.target, "192.168.1.10:9901");
        assert!(req.watch_only);
        assert_eq!(req.device_name.as_deref(), Some("Studio"));
        assert!(!req.force_relay);
        assert_eq!(req.width, None);
        assert_eq!(req.height, None);
        assert_eq!(req.fps, None);
        assert_eq!(req.bitrate_bps, None);
    }

    #[test]
    fn persist_allow_remote_assist_and_stream_prefs_roundtrip_in_temp_dir() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-prefs-cmd-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let prefs_path = dir.join("prefs.json");

        let state = AppState::new_for_test();
        persist_allow_remote_assist_to_path(&state, true, &prefs_path)
            .expect("持久化 allow_remote_assist=true 应成功");
        let loaded = app_prefs::load_from_path(&prefs_path);
        assert!(loaded.allow_remote_assist);

        let updated = save_stream_prefs_to_path(
            &state,
            StreamPrefsPatch {
                bitrate_preset: Some("30m".to_string()),
                bitrate_bps: Some(30_000_000),
                fps: Some(120),
                resolution_width: Some(1280),
                resolution_height: Some(800),
                clear_resolution: None,
                show_hud: Some(false),
                lock_on_exit: Some(true),
            },
            &prefs_path,
        )
        .expect("增量保存流偏好应成功");
        assert_eq!(updated.bitrate_preset, "30m");
        assert_eq!(updated.bitrate_bps, 30_000_000);
        assert_eq!(updated.fps, 120);
        assert_eq!(updated.resolution_width, Some(1280));
        assert_eq!(updated.resolution_height, Some(800));
        assert!(!updated.show_hud);
        assert!(updated.lock_on_exit);

        let reloaded = app_prefs::load_from_path(&prefs_path);
        assert_eq!(reloaded, updated);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_device_id_to_path_rotates_device_key_and_clears_cached_id() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-refresh-id-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let cfg_path = dir.join("server.json");

        let state = AppState::new_for_test();
        let old_key = {
            let mut rt = state.server.lock().unwrap();
            rt.config.enabled = false;
            rt.config.cached_device_id = Some("604928235".to_string());
            rt.config.device_key.clone()
        };

        let snap = refresh_device_id_to_path(&state, &cfg_path)
            .expect("refresh_device_id_to_path 应成功持久化");
        assert!(snap.device_id.is_none());

        let persisted = server_config::load_from_path(&cfg_path);
        assert_ne!(persisted.device_key, old_key);
        assert!(persisted.cached_device_id.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_custom_host_password_roundtrips_and_clears_in_temp_dir() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-custom-pw-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let prefs_path = dir.join("prefs.json");

        let state = AppState::new_for_test();
        persist_custom_host_password_to_path(&state, Some("ABC123".to_string()), &prefs_path)
            .expect("持久化自定义密码应成功");
        let loaded = app_prefs::load_from_path(&prefs_path);
        assert_eq!(loaded.custom_host_password.as_deref(), Some("ABC123"));

        persist_custom_host_password_to_path(&state, Some(String::new()), &prefs_path)
            .expect("持久化无密码自定义模式应成功");
        let loaded_empty = app_prefs::load_from_path(&prefs_path);
        assert_eq!(loaded_empty.custom_host_password.as_deref(), Some(""));

        persist_custom_host_password_to_path(&state, None, &prefs_path)
            .expect("清空自定义密码回退临时码应成功");
        let cleared = app_prefs::load_from_path(&prefs_path);
        assert_eq!(cleared.custom_host_password, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gate_state_transitions_and_require_authenticated_enforces_stages() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-gate-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let auth_path = dir.join("auth.json");
        let certs_dir = dir.join("certs");

        // 1) Unconfigured
        let state = AppState::with_initial_configs_and_paths(
            ServerConfig::default(),
            AppPrefs::default(),
            auth_path.clone(),
            certs_dir.clone(),
            None,
        );
        assert_eq!(state.gate_state(), AppGateState::Unconfigured);
        assert!(state
            .require_authenticated()
            .unwrap_err()
            .contains("请先配置"));

        // 2) Configured -> Unauthenticated
        let configured = ServerConfig {
            enabled: true,
            server_host: "127.0.0.1".to_string(),
            signal_port: 21116,
            relay_port: 21117,
            api_port: 21118,
            use_https: false,
            ..ServerConfig::default()
        };
        {
            let mut rt = state.server.lock().unwrap();
            rt.config = configured.clone();
        }
        state.auth.update_server_config(configured.clone());
        assert_eq!(state.gate_state(), AppGateState::Unauthenticated);
        assert!(state
            .require_authenticated()
            .unwrap_err()
            .contains("请先登录"));

        // 3) PasswordChangeRequired
        let future_exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let must_change_sess = rc_ui::auth_client::StoredAuthSession {
            user_id: "00000000-0000-0000-0000-000000000001".to_string(),
            email: "admin@example.com".to_string(),
            display_name: "Admin".to_string(),
            role: "admin".to_string(),
            must_change_password: true,
            access_token: "tok-1".to_string(),
            refresh_token: "ref-1".to_string(),
            access_expires_at_unix_sec: future_exp,
            server_host: "127.0.0.1".to_string(),
            api_port: 21118,
        };
        rc_ui::auth_client::save_auth_session(&auth_path, &must_change_sess).unwrap();
        let state2 = AppState::with_initial_configs_and_paths(
            configured.clone(),
            AppPrefs::default(),
            auth_path.clone(),
            certs_dir.clone(),
            None,
        );
        let _ = state2.auth.restore_on_startup();
        assert_eq!(state2.gate_state(), AppGateState::PasswordChangeRequired);
        assert!(state2
            .require_authenticated()
            .unwrap_err()
            .contains("修改密码"));

        // 4) Authenticated
        let ready_sess = rc_ui::auth_client::StoredAuthSession {
            must_change_password: false,
            ..must_change_sess
        };
        rc_ui::auth_client::save_auth_session(&auth_path, &ready_sess).unwrap();
        let state3 = AppState::with_initial_configs_and_paths(
            configured,
            AppPrefs::default(),
            auth_path,
            certs_dir,
            None,
        );
        let _ = state3.auth.restore_on_startup();
        assert_eq!(state3.gate_state(), AppGateState::Authenticated);
        assert!(state3.require_authenticated().is_ok());

        let snap = state3.gate_status_snapshot();
        assert_eq!(snap.gate, AppGateState::Authenticated);
        assert!(snap.server_configured);
        assert_eq!(
            snap.user.as_ref().map(|u| u.email.as_str()),
            Some("admin@example.com")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_and_save_server_to_path_probes_mock_http_and_advances_gate() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock http");
        let port = listener.local_addr().unwrap().port();
        let server_thread = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let (status, body) = if req.starts_with("GET /api/v1/pki/ca") {
                    (
                        "404 Not Found",
                        r#"{"error":"not_found","message":"no ca"}"#,
                    )
                } else if req.starts_with("GET /api/v1/health") {
                    (
                        "200 OK",
                        r#"{"status":"ok","version":"0.1.0","tls_enabled":false,"allow_registration":false}"#,
                    )
                } else {
                    ("200 OK", r#"{"allow_registration":false}"#)
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });

        let dir = std::env::temp_dir().join(format!(
            "rc-ui-probe-gate-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let cfg_path = dir.join("server.json");

        let state = AppState::new_for_test();
        assert_eq!(state.gate_state(), AppGateState::Unconfigured);

        let res = probe_and_save_server_to_path(
            &state,
            ProbeAndSaveServerRequest {
                server_host: "127.0.0.1".to_string(),
                api_port: Some(port),
                signal_port: Some(21116),
                relay_port: Some(21117),
                use_https: false,
                expected_ca_fingerprint_sha256: None,
                save: true,
            },
            &cfg_path,
        )
        .expect("probe_and_save_server_to_path must succeed");

        assert!(res.saved);
        assert!(!res.allow_registration);
        assert_eq!(res.gate_status.gate, AppGateState::Unauthenticated);
        assert_eq!(state.gate_state(), AppGateState::Unauthenticated);
        assert!(server_config::load_from_path(&cfg_path).is_configured());

        let _ = server_thread.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_security_prefs_and_my_device_entry_mapping_work() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-sec-prefs-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let prefs_path = dir.join("prefs.json");

        let state = AppState::new_for_test();
        let initial = security_prefs_snapshot(&state);
        assert!(initial.default_encrypt_connection);
        assert!(!initial.show_lan_devices);

        let updated = save_security_prefs_to_path(
            &state,
            SecurityPrefsPatch {
                default_encrypt_connection: Some(false),
                allow_remote_assist: Some(true),
                ..Default::default()
            },
            &prefs_path,
        )
        .expect("save_security_prefs_to_path must succeed");
        assert!(!updated.default_encrypt_connection);
        assert!(updated.allow_remote_assist);
        let snap_json = serde_json::to_value(&updated).expect("serialize SecurityPrefsSnapshot");
        assert!(
            snap_json.get("allowOwnerSelfControl").is_none(),
            "SecurityPrefsSnapshot must not emit removed allowOwnerSelfControl field"
        );

        let legacy_path = dir.join("legacy-prefs.json");
        std::fs::write(
            &legacy_path,
            r#"{"defaultEncryptConnection":false,"allowOwnerSelfControl":false}"#,
        )
        .expect("write legacy prefs json");
        let legacy_loaded = app_prefs::load_from_path(&legacy_path);
        assert!(!legacy_loaded.default_encrypt_connection);

        let entry = map_my_device_entry(
            MyDeviceInfo {
                device_id: "604928235".to_string(),
                formatted_id: String::new(),
                hostname: "Studio-Mac".to_string(),
                os: "macos".to_string(),
                app_version: "0.1.10".to_string(),
                arch: "arm64".to_string(),
                alias: "办公室 Mac".to_string(),
                device_pubkey: String::new(),
                allow_remote_assist: true,
                allow_remote_control: true,
                disabled: false,
                online: true,
                in_session: false,
                wol_capable: false,
                bound_at: None,
                last_seen_at: None,
            },
            Some("604928235"),
        );
        assert_eq!(entry.formatted_id, "604 928 235");
        assert_eq!(entry.display_name, "办公室 Mac");
        assert!(entry.is_local);
        assert!(entry.allow_remote_control);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn my_device_entry_allow_remote_control_serialization_and_android_filtering() {
        let state = AppState::new_for_test();
        let sig_opts = build_signal_client_options(&state);
        assert_eq!(
            sig_opts.allow_remote_control,
            Some(true),
            "macOS 桌面端信令注册选项应显式声明 allow_remote_control = Some(true)"
        );

        let mac_entry = map_my_device_entry(
            MyDeviceInfo {
                device_id: "604928235".to_string(),
                formatted_id: "604 928 235".to_string(),
                hostname: "Studio-Mac".to_string(),
                os: "macos".to_string(),
                app_version: "0.1.11".to_string(),
                arch: "arm64".to_string(),
                alias: String::new(),
                device_pubkey: String::new(),
                allow_remote_assist: false,
                allow_remote_control: true,
                disabled: false,
                online: true,
                in_session: false,
                wol_capable: false,
                bound_at: None,
                last_seen_at: None,
            },
            Some("999000111"),
        );
        let mac_json = serde_json::to_value(&mac_entry).expect("serialize mac MyDeviceEntry");
        assert_eq!(
            mac_json.get("allowRemoteControl"),
            Some(&serde_json::Value::Bool(true))
        );

        // 老服务端省略 allow_remote_control 字段时，Android 设备应默认反序列化为 false。
        let legacy_android: MyDeviceInfo = serde_json::from_str(
            r#"{
                "device_id": "512345678",
                "formatted_id": "512 345 678",
                "hostname": "Pixel-9",
                "os": "android",
                "alias": "我的手机",
                "allow_remote_assist": false,
                "disabled": false,
                "online": true,
                "in_session": false
            }"#,
        )
        .expect("deserialize legacy android MyDeviceInfo");
        assert!(!legacy_android.allow_remote_control);
        let android_off_entry = map_my_device_entry(legacy_android, Some("999000111"));
        assert!(!android_off_entry.allow_remote_control);
        let android_off_json =
            serde_json::to_value(&android_off_entry).expect("serialize android MyDeviceEntry");
        assert_eq!(
            android_off_json.get("allowRemoteControl"),
            Some(&serde_json::Value::Bool(false))
        );

        let android_on_entry = map_my_device_entry(
            MyDeviceInfo {
                device_id: "512345679".to_string(),
                formatted_id: "512 345 679".to_string(),
                hostname: "OnePlus".to_string(),
                os: "android".to_string(),
                app_version: "0.1.11".to_string(),
                arch: "arm64".to_string(),
                alias: "开启远控的手机".to_string(),
                device_pubkey: String::new(),
                allow_remote_assist: false,
                allow_remote_control: true,
                disabled: false,
                online: true,
                in_session: false,
                wol_capable: false,
                bound_at: None,
                last_seen_at: None,
            },
            Some("999000111"),
        );
        let local_entry = map_my_device_entry(
            MyDeviceInfo {
                device_id: "999000111".to_string(),
                formatted_id: "999 000 111".to_string(),
                hostname: "Local-Mac".to_string(),
                os: "macos".to_string(),
                app_version: "0.1.11".to_string(),
                arch: "arm64".to_string(),
                alias: String::new(),
                device_pubkey: String::new(),
                allow_remote_assist: false,
                allow_remote_control: true,
                disabled: false,
                online: true,
                in_session: false,
                wol_capable: false,
                bound_at: None,
                last_seen_at: None,
            },
            Some("999000111"),
        );

        let entries = [mac_entry, android_off_entry, android_on_entry, local_entry];
        let controllable_ids: Vec<&str> = entries
            .iter()
            .filter(|d| !d.is_local && d.allow_remote_control)
            .map(|d| d.device_id.as_str())
            .collect();
        assert_eq!(controllable_ids, vec!["604928235", "512345679"]);
    }

    #[test]
    fn apply_local_device_status_policy_hot_syncs_host_allow_owner_control() {
        let state = AppState::new_for_test();
        {
            let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
            rt.config.cached_device_id = Some("604928235".to_string());
        }
        let _ = start_host_service_on(&state, SocketAddr::from(([127, 0, 0, 1], 0)), false)
            .expect("spawn fake host");
        assert!(state.host.lock().unwrap().allow_owner_control);
        assert_eq!(
            build_signal_client_options(&state).allow_remote_control,
            Some(true)
        );

        let other_ev = rc_ui::signal_client::DeviceStatusEvent {
            device_id: "111222333".to_string(),
            online: true,
            hostname: "Other-Android".to_string(),
            alias: String::new(),
            os: "android".to_string(),
            app_version: String::new(),
            arch: String::new(),
            allow_remote_assist: false,
            allow_remote_control: false,
        };
        assert!(!apply_local_device_status_policy(&state, &other_ev));
        assert!(state.host.lock().unwrap().allow_owner_control);

        // 带空格的格式化设备码也应匹配本机并热同步为 false。
        let self_disable_ev = rc_ui::signal_client::DeviceStatusEvent {
            device_id: "604 928 235".to_string(),
            online: true,
            hostname: "Local-Mac".to_string(),
            alias: String::new(),
            os: "macos".to_string(),
            app_version: "0.1.11".to_string(),
            arch: "arm64".to_string(),
            allow_remote_assist: false,
            allow_remote_control: false,
        };
        assert!(apply_local_device_status_policy(&state, &self_disable_ev));
        assert!(!state.host.lock().unwrap().allow_owner_control);
        assert_eq!(
            build_signal_client_options(&state).allow_remote_control,
            Some(false)
        );

        // 停机后重建 HostHandle 时应保留远端同步下来的 allow_owner_control = false。
        let _ = stop_host_service_with_cancelled(&state, true);
        let _ = start_host_service_on(&state, SocketAddr::from(([127, 0, 0, 1], 0)), false)
            .expect("respawn fake host");
        assert!(!state.host.lock().unwrap().allow_owner_control);

        let self_enable_ev = rc_ui::signal_client::DeviceStatusEvent {
            device_id: "604928235".to_string(),
            allow_remote_control: true,
            ..self_disable_ev
        };
        assert!(apply_local_device_status_policy(&state, &self_enable_ev));
        assert!(state.host.lock().unwrap().allow_owner_control);
        assert_eq!(
            build_signal_client_options(&state).allow_remote_control,
            Some(true)
        );

        let _ = stop_host_service_with_cancelled(&state, true);
    }

    #[test]
    fn sort_my_device_entries_places_online_above_offline() {
        let make = |id: &str, online: bool, is_local: bool| MyDeviceEntry {
            device_id: id.to_string(),
            formatted_id: id.to_string(),
            hostname: id.to_string(),
            os: "macos".to_string(),
            app_version: "0.1.11".to_string(),
            arch: "arm64".to_string(),
            alias: String::new(),
            display_name: id.to_string(),
            allow_remote_assist: false,
            allow_remote_control: true,
            disabled: false,
            online,
            in_session: false,
            is_local,
            wol_capable: false,
            bound_at: None,
            last_seen_at: None,
        };

        let mut entries = vec![
            make("offline-recent", false, false),
            make("online-remote-1", true, false),
            make("offline-local", false, true),
            make("online-local", true, true),
            make("offline-older", false, false),
            make("online-remote-2", true, false),
        ];

        sort_my_device_entries(&mut entries);

        let ids: Vec<&str> = entries.iter().map(|d| d.device_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "online-local",
                "online-remote-1",
                "online-remote-2",
                "offline-local",
                "offline-recent",
                "offline-older",
            ]
        );
    }

    #[test]
    fn active_session_tools_requires_auth_and_persists_portmaps() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-tools-persist-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let prefs_path = dir.join("prefs.json");

        let state = AppState::new_for_test();
        let err = active_session_tools(&state)
            .err()
            .expect("unauthenticated must fail");
        assert!(err.contains("配置并连接") || err.contains("登录"));

        let rules = vec![PortMapRuleSnapshot {
            id: 1,
            name: "Web".to_string(),
            local_port: 18080,
            target_addr: "127.0.0.1".to_string(),
            target_port: 80,
            enabled: true,
            status: rc_tools::PortMapStatus::Ok,
            active_conns: 0,
            bytes_tx: 0,
            bytes_rx: 0,
            error: None,
        }];
        persist_portmaps_to_path(&state, "dev-key-1", &rules, &prefs_path);
        let reloaded = app_prefs::load_from_path(&prefs_path);
        let saved = reloaded.saved_portmaps.get("dev-key-1").unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "Web");
        assert_eq!(saved[0].local_port, 18080);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_local_lan_addrs_filters_loopback_and_unusable_display_addrs() {
        assert_eq!(filter_usable_display_lan_addr(""), None);
        assert_eq!(filter_usable_display_lan_addr("not-an-addr"), None);
        assert_eq!(filter_usable_display_lan_addr("127.0.0.1:9901"), None);
        assert_eq!(filter_usable_display_lan_addr("[::1]:9901"), None);
        assert_eq!(
            filter_usable_display_lan_addr("[::ffff:127.0.0.1]:9901"),
            None
        );
        assert_eq!(filter_usable_display_lan_addr("0.0.0.0:9901"), None);
        assert_eq!(filter_usable_display_lan_addr("[::]:9901"), None);
        assert_eq!(filter_usable_display_lan_addr("224.0.0.1:9901"), None);
        assert_eq!(filter_usable_display_lan_addr("[ff02::1]:9901"), None);
        assert_eq!(filter_usable_display_lan_addr("169.254.1.2:9901"), None);
        assert_eq!(filter_usable_display_lan_addr("[fe80::1]:9901"), None);
        assert_eq!(
            filter_usable_display_lan_addr("192.168.1.50:9901").as_deref(),
            Some("192.168.1.50:9901")
        );
        assert_eq!(
            filter_usable_display_lan_addr("[::ffff:192.168.1.50]:9901").as_deref(),
            Some("192.168.1.50:9901")
        );
        assert_eq!(
            filter_usable_display_lan_addr("[fd00::10]:9901").as_deref(),
            Some("[fd00::10]:9901")
        );

        let state = AppState::new_for_test();
        assert!(build_signal_client_options(&state).dynamic_lan_discovery);

        {
            let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
            host.display_addr = Some("192.168.1.50:9901".to_string());
        }
        assert_eq!(
            collect_local_lan_addrs(&state),
            vec!["192.168.1.50:9901".to_string()]
        );

        for rejected in ["127.0.0.1:9901", "[::1]:9901"] {
            {
                let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
                host.display_addr = Some(rejected.to_string());
            }
            let addrs = collect_local_lan_addrs(&state);
            assert!(
                !addrs.iter().any(|a| a == rejected),
                "must not keep rejected display_addr {rejected}, got {addrs:?}"
            );
            assert!(
                addrs
                    .iter()
                    .all(|a| filter_usable_display_lan_addr(a).is_some()),
                "all fallback addresses must be usable LAN addresses: {addrs:?}"
            );
        }
    }

    #[test]
    fn local_device_id_snapshot_falls_back_to_cached_device_id() {
        let state = AppState::new_for_test();
        assert_eq!(local_device_id_snapshot(&state), None);

        {
            let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
            rt.config.cached_device_id = Some("604928235".to_string());
        }
        assert_eq!(
            local_device_id_snapshot(&state).as_deref(),
            Some("604928235")
        );
    }

    #[test]
    fn parse_plist_string_value_extracts_versions() {
        let plist = r#"
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>RabbitControl</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.7</string>
    <key>CFBundleVersion</key>
    <string> 245 </string>
    <key>EmptyKey</key>
    <string>   </string>
</dict>
</plist>
"#;
        assert_eq!(
            parse_plist_string_value(plist, "CFBundleShortVersionString").as_deref(),
            Some("0.1.7")
        );
        assert_eq!(
            parse_plist_string_value(plist, "CFBundleVersion").as_deref(),
            Some("245")
        );
        assert_eq!(parse_plist_string_value(plist, "EmptyKey"), None);
        assert_eq!(parse_plist_string_value(plist, "MissingKey"), None);
    }

    #[test]
    fn resolve_app_version_prefers_plist_then_compile_env_then_pkg() {
        let plist = r#"
<dict>
    <key>CFBundleShortVersionString</key>
    <string>v0.1.9</string>
    <key>CFBundleVersion</key>
    <string>900</string>
</dict>
"#;
        // 1) Info.plist 优先，且 git_sha 非空时作为 build。
        let from_plist = resolve_app_version_from_sources(Some(plist), "0.1.7", "0.1.0", "abc1234");
        assert_eq!(
            from_plist,
            AppVersionInfo {
                version: "0.1.9".to_string(),
                bundle_version: "900".to_string(),
                build: "abc1234".to_string(),
            }
        );

        // 2) Info.plist 优先且 git_sha 为空时，若 bundle_version != version 则回退至 bundle_version。
        let from_plist_no_sha = resolve_app_version_from_sources(Some(plist), "0.1.7", "0.1.0", "");
        assert_eq!(
            from_plist_no_sha,
            AppVersionInfo {
                version: "0.1.9".to_string(),
                bundle_version: "900".to_string(),
                build: "900".to_string(),
            }
        );

        // 3) 无 Info.plist 时回退到编译期 RC_APP_VERSION（自动去 v 前缀）。
        let from_compile = resolve_app_version_from_sources(None, "v0.1.8", "0.1.0", "def5678");
        assert_eq!(
            from_compile,
            AppVersionInfo {
                version: "0.1.8".to_string(),
                bundle_version: "0.1.8".to_string(),
                build: "def5678".to_string(),
            }
        );

        // 4) 编译期 RC_APP_VERSION 与 git_sha 均为空时回退 CARGO_PKG_VERSION，build 置空。
        let from_pkg = resolve_app_version_from_sources(None, "", "v0.1.7", "");
        assert_eq!(
            from_pkg,
            AppVersionInfo {
                version: "0.1.7".to_string(),
                bundle_version: "0.1.7".to_string(),
                build: String::new(),
            }
        );
    }

    #[test]
    fn app_info_exposes_core_version_and_build_info_without_breaking_legacy_json() {
        let core_info = rc_client_core::version::core_build_info();
        let app_info = AppInfo {
            name: "RabbitControl".to_string(),
            version: "0.1.13".to_string(),
            build: "abc1234".to_string(),
            bundle_id: "com.rabbitcontrol.dev".to_string(),
            os: "macOS".to_string(),
            arch: "aarch64".to_string(),
            license: "AGPL-3.0-only".to_string(),
            description: "低延迟远程桌面控制软件".to_string(),
            core_version: core_info.core_version.to_string(),
            core_build_info: Some(CoreBuildInfoDto::from(core_info.clone())),
        };

        let val = serde_json::to_value(&app_info).expect("serialize AppInfo");
        let expected_core_version = rc_client_core::version::core_version();
        assert!(
            !expected_core_version.is_empty(),
            "rc_client_core::version::core_version() 不得为空"
        );
        assert_eq!(
            val.get("coreVersion").and_then(|v| v.as_str()),
            Some(expected_core_version)
        );
        let cbi = val
            .get("coreBuildInfo")
            .and_then(|v| v.as_object())
            .expect("coreBuildInfo object");
        assert_eq!(
            cbi.get("coreVersion").and_then(|v| v.as_str()),
            Some(core_info.core_version)
        );
        assert_eq!(
            cbi.get("gitCommit").and_then(|v| v.as_str()),
            Some(core_info.git_commit)
        );
        assert_eq!(
            cbi.get("buildTimestamp").and_then(|v| v.as_str()),
            Some(core_info.build_timestamp)
        );
        assert_eq!(
            cbi.get("sessionProtocolVersion").and_then(|v| v.as_u64()),
            Some(u64::from(core_info.session_protocol_version))
        );
        assert_eq!(
            cbi.get("punchVersion").and_then(|v| v.as_u64()),
            Some(u64::from(core_info.punch_version))
        );
        assert_eq!(
            cbi.get("beaconVersion").and_then(|v| v.as_u64()),
            Some(u64::from(core_info.beacon_version))
        );
        assert_eq!(
            cbi.get("abiVersion").and_then(|v| v.as_u64()),
            Some(u64::from(core_info.abi_version))
        );

        // 旧版 JSON 不含 coreVersion / coreBuildInfo 时仍可零破坏反序列化。
        let legacy = serde_json::json!({
            "name": "RabbitControl",
            "version": "0.1.12",
            "build": "",
            "bundleId": "com.rabbitcontrol.dev",
            "os": "macOS",
            "arch": "aarch64",
            "license": "AGPL-3.0-only",
            "description": "低延迟远程桌面控制软件"
        });
        let parsed: AppInfo = serde_json::from_value(legacy).expect("deserialize legacy AppInfo");
        assert_eq!(parsed.core_version, "");
        assert_eq!(parsed.core_build_info, None);
    }

    #[test]
    fn general_prefs_file_transfer_dir_save_validate_and_fallback() {
        let base = std::env::temp_dir().join(format!(
            "rc-ui-general-transfer-dir-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let custom_dir = base.join("custom_downloads");
        let missing_dir = base.join("does_not_exist");
        let regular_file = base.join("not_a_dir.txt");
        let prefs_path = base.join("prefs.json");
        std::fs::create_dir_all(&custom_dir).expect("create custom_dir");
        std::fs::write(&regular_file, b"hello").expect("create regular_file");

        let state = AppState::new_for_test();
        let initial = general_prefs_snapshot_from_prefs(&state.prefs.lock().unwrap());
        assert!(initial.file_transfer_dir_is_default);
        assert!(!initial.file_transfer_dir.is_empty());

        // 1) Custom writable directory persists cleanly.
        let saved = save_general_prefs_to_path(
            &state,
            GeneralPrefsPatch {
                close_to_tray: Some(false),
                file_transfer_dir: Some(format!("{}/", custom_dir.display())),
                allow_wol: Some(true),
            },
            &prefs_path,
        )
        .expect("saving writable custom dir should succeed");
        assert!(!saved.close_to_tray);
        assert!(saved.allow_wol);
        assert!(!saved.file_transfer_dir_is_default);
        assert_eq!(saved.file_transfer_dir, custom_dir.to_string_lossy());
        let on_disk = app_prefs::load_from_path(&prefs_path);
        assert_eq!(on_disk.resolved_file_transfer_dir(), custom_dir);
        assert!(!on_disk.close_to_tray);
        assert!(on_disk.allow_wol);

        // 2) Missing custom directory is rejected without modifying prefs.json or state.prefs.
        let err_missing = save_general_prefs_to_path(
            &state,
            GeneralPrefsPatch {
                close_to_tray: Some(true),
                file_transfer_dir: Some(missing_dir.to_string_lossy().into_owned()),
                allow_wol: Some(false),
            },
            &prefs_path,
        )
        .unwrap_err();
        assert!(
            err_missing.contains("不存在"),
            "unexpected error: {err_missing}"
        );
        assert_eq!(
            state.prefs.lock().unwrap().resolved_file_transfer_dir(),
            custom_dir
        );
        assert!(!app_prefs::load_from_path(&prefs_path).close_to_tray);
        assert!(app_prefs::load_from_path(&prefs_path).allow_wol);

        // 3) Regular file path is rejected without modifying prefs.json.
        let err_file = save_general_prefs_to_path(
            &state,
            GeneralPrefsPatch {
                close_to_tray: None,
                file_transfer_dir: Some(regular_file.to_string_lossy().into_owned()),
                allow_wol: None,
            },
            &prefs_path,
        )
        .unwrap_err();
        assert!(
            err_file.contains("不是有效文件夹"),
            "unexpected error: {err_file}"
        );
        assert_eq!(
            app_prefs::load_from_path(&prefs_path).resolved_file_transfer_dir(),
            custom_dir
        );

        // 4) Empty string resets back to default directory and allow_wol can be toggled off.
        let reset = save_general_prefs_to_path(
            &state,
            GeneralPrefsPatch {
                close_to_tray: None,
                file_transfer_dir: Some(String::new()),
                allow_wol: Some(false),
            },
            &prefs_path,
        )
        .expect("resetting file_transfer_dir to default should succeed");
        assert!(reset.file_transfer_dir_is_default);
        assert!(!reset.allow_wol);
        assert!(!app_prefs::load_from_path(&prefs_path).allow_wol);
        assert_eq!(
            reset.file_transfer_dir,
            app_prefs::default_file_transfer_dir().to_string_lossy()
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn validate_transfer_dir_writable_creates_default_and_cleans_probe() {
        let base = std::env::temp_dir().join(format!(
            "rc-ui-validate-dir-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let auto_dir = base.join("default_nested/Download");
        assert!(!auto_dir.exists());

        // Missing non-default must fail without creating.
        assert!(validate_transfer_dir_writable(&auto_dir, false).is_err());
        assert!(!auto_dir.exists());

        // Missing default auto-creates, verifies writability, and leaves no probe file behind.
        validate_transfer_dir_writable(&auto_dir, true).expect("default dir should auto-create");
        assert!(auto_dir.is_dir());
        let leftover: Vec<_> = std::fs::read_dir(&auto_dir)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(
            leftover.is_empty(),
            "probe file must be cleaned up, found: {leftover:?}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn file_list_local_inner_resolves_configured_default_and_falls_back_when_deleted() {
        let base = std::env::temp_dir().join(format!(
            "rc-ui-list-local-default-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let default_dir = base.join("auto_default_download");
        let custom_dir = base.join("custom_folder");
        std::fs::create_dir_all(&custom_dir).expect("create custom_dir");
        std::fs::write(custom_dir.join("sample.txt"), b"ok").expect("create sample file");

        // 1) Default missing dir is auto-created and listed when path is None / whitespace.
        let listed_default = file_list_local_inner(None, Some((&default_dir, true)))
            .expect("missing default dir should be auto-created and listed");
        assert!(default_dir.is_dir());
        assert_eq!(
            std::fs::canonicalize(&listed_default.path).unwrap(),
            std::fs::canonicalize(&default_dir).unwrap()
        );

        // 2) Existing custom dir is listed when path is empty/whitespace.
        let listed_custom =
            file_list_local_inner(Some("   ".to_string()), Some((&custom_dir, false)))
                .expect("existing custom dir should be listed");
        assert!(listed_custom.entries.iter().any(|e| e.name == "sample.txt"));

        // 3) Externally deleted custom dir gracefully falls back to default_home_dir().
        let deleted_custom = base.join("deleted_custom");
        let listed_fallback = file_list_local_inner(None, Some((&deleted_custom, false)))
            .expect("deleted custom dir should fall back to home dir");
        assert!(!deleted_custom.exists());
        assert_eq!(
            std::fs::canonicalize(&listed_fallback.path).unwrap(),
            std::fs::canonicalize(rc_tools::default_home_dir()).unwrap()
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn osascript_cancel_detection_and_path_normalization_helpers() {
        assert!(is_osascript_user_canceled(
            "execution error: User canceled. (-128)"
        ));
        assert!(is_osascript_user_canceled("22:55: execution error: (-128)"));
        assert!(is_osascript_user_canceled("User Cancelled by operator"));
        assert!(!is_osascript_user_canceled(
            "execution error: /Volumes/disk-128/Downloads wasn't found. (-43)"
        ));
        assert!(!is_osascript_user_canceled(
            "execution error: File wasn't found. (-43)"
        ));
        assert!(!is_osascript_user_canceled(""));

        assert_eq!(
            normalize_picked_dir_path("/Users/demo/Downloads/\n"),
            Some("/Users/demo/Downloads".to_string())
        );
        assert_eq!(
            normalize_picked_dir_path("/Users/demo/My Folder///\r\n"),
            Some("/Users/demo/My Folder".to_string())
        );
        assert_eq!(normalize_picked_dir_path("/\n"), Some("/".to_string()));
        assert_eq!(normalize_picked_dir_path("///\n"), Some("/".to_string()));
        assert_eq!(normalize_picked_dir_path("   \n"), None);
    }

    #[test]
    fn macos_package_bundle_rejection_and_open_missing_custom_dir() {
        assert!(is_macos_package_bundle(Path::new(
            "/Applications/Safari.app"
        )));
        assert!(is_macos_package_bundle(Path::new("/Applications/Demo.APP")));
        assert!(is_macos_package_bundle(Path::new("/tmp/Installer.Pkg")));
        assert!(is_macos_package_bundle(Path::new("/tmp/My.workflow")));
        assert!(is_macos_package_bundle(Path::new("/tmp/Test.scptd")));
        assert!(is_macos_package_bundle(Path::new("/tmp/Ext.appex")));
        assert!(!is_macos_package_bundle(Path::new("/Users/demo/Downloads")));
        assert!(!is_macos_package_bundle(Path::new("/Users/demo/app")));

        let base = std::env::temp_dir().join(format!(
            "rc-ui-bundle-guard-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let fake_app = base.join("Fake.app");
        let missing_custom = base.join("unmounted_volume");
        std::fs::create_dir_all(&fake_app).expect("create fake .app dir");

        let val_err = validate_transfer_dir_writable(&fake_app, false).unwrap_err();
        assert!(val_err.contains("应用程序包"), "unexpected err: {val_err}");

        let open_bundle_err = open_transfer_storage_dir_blocking(&fake_app, false).unwrap_err();
        assert!(
            open_bundle_err.contains("应用程序包"),
            "unexpected err: {open_bundle_err}"
        );

        #[cfg(unix)]
        {
            let link_to_app = base.join("DownloadsLink");
            std::os::unix::fs::symlink(&fake_app, &link_to_app).expect("create symlink to .app");
            let sym_val_err = validate_transfer_dir_writable(&link_to_app, false).unwrap_err();
            assert!(
                sym_val_err.contains("应用程序包"),
                "symlink to .app must be rejected: {sym_val_err}"
            );
            let sym_open_err = open_transfer_storage_dir_blocking(&link_to_app, false).unwrap_err();
            assert!(
                sym_open_err.contains("应用程序包"),
                "symlink to .app must not be opened: {sym_open_err}"
            );
        }

        // Missing custom directory must return error without creating it.
        let open_missing_err =
            open_transfer_storage_dir_blocking(&missing_custom, false).unwrap_err();
        assert!(
            open_missing_err.contains("不存在"),
            "unexpected err: {open_missing_err}"
        );
        assert!(!missing_custom.exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_connection_mode_respects_force_relay_then_request_then_prefs() {
        assert_eq!(
            resolve_connection_mode(true, Some("p2p_only"), ConnectionMode::Auto),
            ConnectionMode::TcpRelay
        );
        assert_eq!(
            resolve_connection_mode(false, Some("p2p_only"), ConnectionMode::Auto),
            ConnectionMode::P2pOnly
        );
        assert_eq!(
            resolve_connection_mode(false, Some("invalid"), ConnectionMode::TcpRelay),
            ConnectionMode::TcpRelay
        );
        assert_eq!(
            resolve_connection_mode(false, None, ConnectionMode::P2pOnly),
            ConnectionMode::P2pOnly
        );
    }

    #[test]
    fn headless_reuse_matches_same_target_across_phases_without_teardown() {
        let mut active = ActiveSession::new(42, (), false);
        active.set_meta(ActiveSessionMeta {
            target: "604 928 235".to_string(),
            device_key: "604928235".to_string(),
            device_name: "Studio Mac".to_string(),
            relay_target_id: Some("604928235".to_string()),
            tools_only: true,
            media_active: false,
            phase: "connecting".to_string(),
            connection_mode: ConnectionMode::Auto,
            ..Default::default()
        });
        let parsed_same = model::parse_connect_input("604928235").unwrap();
        let parsed_other = model::parse_connect_input("999888777").unwrap();

        // connecting / connected / reconnecting 同目标纯工具会话均直接复用，不拆旧会话。
        for phase in ["connecting", "connected", "reconnecting"] {
            active.meta.phase = phase.to_string();
            assert!(
                can_reuse_headless_session(
                    &active,
                    "604928235",
                    &parsed_same,
                    "604928235",
                    Some("604928235"),
                ),
                "phase={phase} should reuse same-target session"
            );
        }

        // 不同目标不可复用。
        assert!(!can_reuse_headless_session(
            &active,
            "999888777",
            &parsed_other,
            "999888777",
            Some("999888777"),
        ));

        // 观看模式不可复用于可写工具会话。
        active.watch_only = true;
        assert!(!can_reuse_headless_session(
            &active,
            "604928235",
            &parsed_same,
            "604928235",
            Some("604928235"),
        ));

        // disconnecting 中不可复用。
        active.watch_only = false;
        active.disconnecting = true;
        assert!(!can_reuse_headless_session(
            &active,
            "604928235",
            &parsed_same,
            "604928235",
            Some("604928235"),
        ));
    }

    #[test]
    fn failed_session_preserves_last_session_params_and_persists_upnp_pref() {
        let state = AppState::new_for_test();
        let meta = ActiveSessionMeta {
            target: "604928235".to_string(),
            device_key: "604928235".to_string(),
            device_name: "Studio Mac".to_string(),
            relay_target_id: Some("604928235".to_string()),
            connection_mode: ConnectionMode::P2pOnly,
            media_active: true,
            tools_only: false,
            phase: "connecting".to_string(),
            ..Default::default()
        };
        save_last_session_params(&state, false, meta.clone());

        // 即使活动会话槽为空（模拟连接失败态），last_session_params 仍完整保留本次目标参数。
        assert!(state.session.lock().unwrap().is_none());
        let saved = state
            .last_session_params
            .lock()
            .unwrap()
            .clone()
            .expect("last_session_params must be preserved on failure");
        assert!(!saved.0);
        assert_eq!(saved.1.target, "604928235");
        assert_eq!(saved.1.connection_mode, ConnectionMode::P2pOnly);

        // UPnP 开关持久化验证。
        let base = std::env::temp_dir().join(format!(
            "rc-ui-upnp-pref-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let prefs_path = base.join("prefs.json");
        save_upnp_enabled_to_path(&state, false, &prefs_path).expect("save upnp_enabled=false");
        assert!(!state.prefs.lock().unwrap().upnp_enabled);
        assert!(!app_prefs::load_from_path(&prefs_path).upnp_enabled);

        save_upnp_enabled_to_path(&state, true, &prefs_path).expect("save upnp_enabled=true");
        assert!(state.prefs.lock().unwrap().upnp_enabled);
        assert!(app_prefs::load_from_path(&prefs_path).upnp_enabled);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn session_snapshot_returns_reconnecting_connecting_and_terminal_error_for_current_epoch() {
        let state = AppState::new_for_test();
        let epoch = state.session_open_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        save_last_session_params(
            &state,
            true,
            ActiveSessionMeta {
                target: "604928235".to_string(),
                device_key: "604928235".to_string(),
                device_name: "Studio Mac".to_string(),
                ..Default::default()
            },
        );

        // 1) 信令协商期返回 Connecting（携带 watch_only）。
        state.connecting_epoch.store(epoch, Ordering::Release);
        assert_eq!(
            resolve_session_snapshot(&state).0,
            Some(SessionUpdate::Connecting {
                phase: rc_ui::session_bridge::ConnectPhase::Connecting,
                watch_only: true,
            })
        );
        state.clear_connecting_epoch(epoch);

        // 2) 自动重连期返回 Reconnecting（携带最新 attempt/max_attempts/reason）。
        let reconn_guard = state
            .try_acquire_auto_reconnect(epoch, 2, "对端短暂失联".to_string())
            .expect("acquire reconnect");
        assert_eq!(
            resolve_session_snapshot(&state).0,
            Some(SessionUpdate::Reconnecting {
                attempt: 2,
                max_attempts: AUTO_RECONNECT_MAX_ATTEMPTS,
                reason: "对端短暂失联".to_string(),
            })
        );
        drop(reconn_guard);

        // 3) 快速/早期失败写入当前 epoch 终态错误后，晚加载 S1 通过 session_snapshot 可 100% 补领 Ended（附带结构化 UiError）。
        state.record_terminal_error_if_epoch(epoch, "对端设备离线".to_string());
        assert_eq!(
            resolve_session_snapshot(&state).0,
            Some(SessionUpdate::Ended {
                reason: "对端设备离线".to_string(),
                recoverable: false,
                ui_error: Some(classify_macos_session_or_signal_error("对端设备离线")),
            })
        );

        // 4) 进入新一代次后，旧 epoch 终态错误绝不会串台返回给新窗口。
        let _next_epoch = state.session_open_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        assert_eq!(resolve_session_snapshot(&state).0, None);
    }

    #[test]
    fn disconnect_command_on_empty_slot_cancels_auto_reconnect_and_connecting_epoch() {
        let state = AppState::new_for_test();
        let epoch1 = state.session_open_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let reconn_guard = state
            .try_acquire_auto_reconnect(epoch1, 3, "网络波动".to_string())
            .expect("acquire auto reconnect");
        state.auto_reconnect_attempts.store(3, Ordering::Release);
        state.connecting_epoch.store(epoch1, Ordering::Release);
        state.record_terminal_error_if_epoch(epoch1, "旧失败".to_string());
        let wake_before = *state.auto_reconnect_wake.0.lock().unwrap();

        let outcome = prepare_disconnect_command(&state, false);
        assert!(matches!(outcome, DisconnectCommandOutcome::EmptySlot));
        assert_eq!(state.session_open_epoch.load(Ordering::Acquire), epoch1 + 1);
        assert_eq!(state.connecting_epoch.load(Ordering::Acquire), 0);
        assert_eq!(state.auto_reconnect_attempts.load(Ordering::Acquire), 0);
        assert!(state.last_terminal_error.lock().unwrap().is_none());
        assert!(
            !state.is_auto_reconnect_active(),
            "Disconnect 后即使旧任务尚未 drop 守卫，is_auto_reconnect_active 也必须立即为 false"
        );
        assert!(
            *state.auto_reconnect_wake.0.lock().unwrap() > wake_before,
            "Disconnect 必须唤醒退避睡眠中的自动重连任务"
        );
        assert!(!state.active_session_info().active);

        drop(reconn_guard);
        assert_eq!(state.auto_reconnect_active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn persisted_tcp_relay_falls_back_to_auto_for_pure_direct_ip_without_overwriting_prefs() {
        let direct = model::parse_connect_input("192.168.1.50:9901").unwrap();
        let device_id = model::parse_connect_input("604928235").unwrap();

        // 1) 全局持久化偏好为 TcpRelay，未显式传 force_relay / connection_mode，纯 IP 直连（无 relay_target_id）自动回退 Auto。
        let fallback_mode = resolve_connection_mode_for_target(
            false,
            None,
            ConnectionMode::TcpRelay,
            &direct,
            None,
        );
        assert_eq!(fallback_mode, ConnectionMode::Auto);
        assert_eq!(
            effective_target_for_mode(direct.clone(), None, fallback_mode).unwrap(),
            direct
        );

        // 2) 若显式传 force_relay=true 或 connection_mode="tcp_relay"，仍保持 TcpRelay 并由 effective_target_for_mode 报错。
        let explicit_force =
            resolve_connection_mode_for_target(true, None, ConnectionMode::Auto, &direct, None);
        assert_eq!(explicit_force, ConnectionMode::TcpRelay);
        assert!(effective_target_for_mode(direct.clone(), None, explicit_force).is_err());

        let explicit_mode = resolve_connection_mode_for_target(
            false,
            Some("tcp_relay"),
            ConnectionMode::Auto,
            &direct,
            None,
        );
        assert_eq!(explicit_mode, ConnectionMode::TcpRelay);
        assert!(effective_target_for_mode(direct.clone(), None, explicit_mode).is_err());

        // 3) 9 位设备码或绑定了 relay_target_id 的局域网直连仍尊重持久化 TcpRelay。
        assert_eq!(
            resolve_connection_mode_for_target(
                false,
                None,
                ConnectionMode::TcpRelay,
                &device_id,
                Some("604928235"),
            ),
            ConnectionMode::TcpRelay
        );
        assert_eq!(
            resolve_connection_mode_for_target(
                false,
                None,
                ConnectionMode::TcpRelay,
                &direct,
                Some("604928235"),
            ),
            ConnectionMode::TcpRelay
        );
    }

    #[test]
    fn allow_assist_file_transfer_prefs_persist_and_json_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-assist-ft-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let prefs_path = dir.join("prefs.json");

        let state = AppState::new_for_test();
        let initial = security_prefs_snapshot(&state);
        assert!(
            initial.allow_assist_file_transfer,
            "默认应允许远程协助文件互传以兼容存量配置"
        );

        let updated = save_security_prefs_to_path(
            &state,
            UiSecurityPrefsPatch {
                allow_assist_file_transfer: Some(false),
                ..Default::default()
            },
            &prefs_path,
        )
        .expect("save allow_assist_file_transfer=false must succeed");
        assert!(!updated.allow_assist_file_transfer);
        let snap_json = serde_json::to_value(&updated).expect("serialize SecurityPrefsSnapshot");
        assert_eq!(snap_json["allowAssistFileTransfer"], false);

        let reloaded = app_prefs::load_from_path(&prefs_path);
        assert!(!reloaded.allow_assist_file_transfer);

        let reenabled = save_security_prefs_to_path(
            &state,
            UiSecurityPrefsPatch {
                allow_assist_file_transfer: Some(true),
                ..Default::default()
            },
            &prefs_path,
        )
        .expect("save allow_assist_file_transfer=true must succeed");
        assert!(reenabled.allow_assist_file_transfer);
        assert!(app_prefs::load_from_path(&prefs_path).allow_assist_file_transfer);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ui_command_error_maps_auth_signal_session_tcc_and_tool_forbidden() {
        let auth_err = AuthClientError::Api {
            status: 401,
            code: "INVALID_CREDENTIALS".to_string(),
            message: "邮箱或密码错误".to_string(),
        };
        let cmd_auth = UiCommandError::from_auth_error(&auth_err);
        assert_eq!(cmd_auth.ui_error.code, codes::AUTH_INVALID_CREDENTIALS);
        assert_eq!(cmd_auth.ui_error.status, Some(401));
        let auth_json = serde_json::to_value(&cmd_auth).expect("serialize UiCommandError");
        assert_eq!(
            auth_json["uiError"]["code"],
            codes::AUTH_INVALID_CREDENTIALS
        );
        assert!(!auth_json["message"].as_str().unwrap_or("").is_empty());

        let reuse_err = AuthClientError::ReloginRequired("TOKEN_REUSE_DETECTED".to_string());
        let cmd_reuse = UiCommandError::from_auth_error(&reuse_err);
        assert_eq!(cmd_reuse.ui_error.code, codes::AUTH_TOKEN_REUSE_DETECTED);

        let offline_ui = classify_macos_session_or_signal_error("对端设备离线");
        assert_eq!(offline_ui.code, codes::SIGNAL_PEER_OFFLINE);

        let auth_rej_ui = classify_macos_session_or_signal_error("访问密码校验失败");
        assert_eq!(auth_rej_ui.code, codes::SESSION_AUTH_REJECTED);

        let host_tcc = UiCommandError::from_host_error("缺少屏幕录制权限，无法采集画面");
        assert_eq!(
            host_tcc.ui_error.code,
            codes::PLATFORM_MACOS_SCREEN_RECORDING_DENIED
        );

        // 本地磁盘 I/O Permission denied（会话允许文件互传）绝不得误判为 tool.forbidden。
        let os_perm_denied = UiCommandError::from_file_tool_error_with_policy(
            "打开本地文件失败: Permission denied (os error 13)",
            true,
        );
        assert_eq!(
            os_perm_denied.ui_error.code,
            codes::TOOL_FILE_TRANSFER_FAILED
        );

        let zh_perm_denied = UiCommandError::from_file_tool_error_with_policy(
            "写入目标目录失败：目录权限不足",
            true,
        );
        assert_eq!(
            zh_perm_denied.ui_error.code,
            codes::TOOL_FILE_TRANSFER_FAILED
        );

        // 会话策略明确关闭文件互传，或核心层返回规范 tool.forbidden 文案时才映射为 tool.forbidden。
        let policy_denied =
            UiCommandError::from_file_tool_error_with_policy("任意文件调用失败", false);
        assert_eq!(policy_denied.ui_error.code, codes::TOOL_FORBIDDEN);
        assert!(policy_denied.ui_error.message.contains("文件传输"));

        let core_forbidden_msg = ui_error::tool_forbidden_ui_error(
            rc_session::TOOL_CHANNEL_FILE,
            ErrorPlatform::Macos,
            None,
        )
        .message;
        let core_denied =
            UiCommandError::from_file_tool_error_with_policy(core_forbidden_msg, true);
        assert_eq!(core_denied.ui_error.code, codes::TOOL_FORBIDDEN);

        let file_generic = UiCommandError::from_file_tool_error_with_policy("磁盘空间不足", true);
        assert_eq!(file_generic.ui_error.code, codes::TOOL_FILE_TRANSFER_FAILED);
    }

    #[test]
    fn proxy_prefs_roundtrip_validation_and_password_redaction() {
        let base = std::env::temp_dir().join(format!(
            "rc-ui-proxy-prefs-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).expect("create temp dir");
        let prefs_path = base.join("prefs.json");
        let state = AppState::new_for_test();

        let secret_pw = "SuperSecretProxyPass!987";
        let req_manual = SaveProxyPrefsRequest {
            mode: ProxyMode::Manual,
            protocol: Some(ProxyProtocol::Socks5),
            host: "  proxy.corp.local  ".to_string(),
            port: 1080,
            username: Some("alice".to_string()),
            password: Some(secret_pw.to_string()),
            clear_password: false,
        };
        let dbg_req = format!("{req_manual:?}");
        assert!(
            !dbg_req.contains(secret_pw) && dbg_req.contains("<redacted>"),
            "SaveProxyPrefsRequest Debug must redact password: {dbg_req}"
        );

        let (view, cfg, changed) =
            save_proxy_prefs_to_path(&state, req_manual, &prefs_path).expect("save manual proxy");
        assert!(changed);
        assert_eq!(view.mode, ProxyMode::Manual);
        assert_eq!(view.protocol, Some(ProxyProtocol::Socks5));
        assert_eq!(view.host, "proxy.corp.local");
        assert_eq!(view.port, 1080);
        assert_eq!(view.username.as_deref(), Some("alice"));
        assert!(view.has_password);
        assert_eq!(cfg.password.as_deref(), Some(secret_pw));

        // 响应序列化与 sanitize_app_prefs_for_ui 绝不泄露密码明文。
        let view_json = serde_json::to_string(&view).expect("serialize ProxyPrefsView");
        assert!(!view_json.contains(secret_pw));
        assert!(view_json.contains("\"hasPassword\":true"));

        let sanitized_prefs = sanitize_app_prefs_for_ui(state.prefs.lock().unwrap().clone());
        assert_eq!(sanitized_prefs.proxy.password, None);
        let prefs_json = serde_json::to_string(&sanitized_prefs).expect("serialize sanitized");
        assert!(!prefs_json.contains(secret_pw));

        // 再次保存端口变更且密码留空时，应保留已保存密码。
        let (view2, cfg2, changed2) = save_proxy_prefs_to_path(
            &state,
            SaveProxyPrefsRequest {
                mode: ProxyMode::Manual,
                protocol: Some(ProxyProtocol::Socks5),
                host: "proxy.corp.local".to_string(),
                port: 1081,
                username: Some("alice".to_string()),
                password: None,
                clear_password: false,
            },
            &prefs_path,
        )
        .expect("update port preserving password");
        assert!(changed2);
        assert!(view2.has_password);
        assert_eq!(cfg2.password.as_deref(), Some(secret_pw));

        // 显式 clear_password 清空密码。
        let (view3, cfg3, changed3) = save_proxy_prefs_to_path(
            &state,
            SaveProxyPrefsRequest {
                mode: ProxyMode::Manual,
                protocol: Some(ProxyProtocol::Http),
                host: "proxy.corp.local".to_string(),
                port: 8080,
                username: Some("alice".to_string()),
                password: None,
                clear_password: true,
            },
            &prefs_path,
        )
        .expect("clear proxy password");
        assert!(changed3);
        assert!(!view3.has_password);
        assert_eq!(cfg3.password, None);

        // 非法配置（端口 0）必须被拒绝且不破坏已有持久化配置。
        let err = save_proxy_prefs_to_path(
            &state,
            SaveProxyPrefsRequest {
                mode: ProxyMode::Manual,
                protocol: Some(ProxyProtocol::Http),
                host: "proxy.corp.local".to_string(),
                port: 0,
                username: None,
                password: Some("ShouldNotLeakInError!".to_string()),
                clear_password: false,
            },
            &prefs_path,
        )
        .unwrap_err();
        assert_eq!(err.ui_error.code, codes::PROXY_CONFIG_CONFLICT);
        assert!(!err.message.contains("ShouldNotLeakInError!"));
        assert_eq!(app_prefs::load_from_path(&prefs_path).proxy.port, 8080);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn wol_entry_mapping_and_request_device_wol_validation() {
        // 1) REST /api/v1/me/devices 返回 wol_capable=true 时，即使信令快照缓存为空也必须透传 wolCapable=true。
        let rest_offline: MyDeviceInfo = serde_json::from_str(
            r#"{
                "device_id": "604928235",
                "formatted_id": "604 928 235",
                "hostname": "studio-mac",
                "os": "macos",
                "app_version": "0.1.12",
                "arch": "arm64",
                "alias": "工作室 Mac",
                "allow_remote_assist": false,
                "allow_remote_control": true,
                "disabled": false,
                "online": false,
                "in_session": false,
                "wol_capable": true
            }"#,
        )
        .expect("deserialize REST MyDeviceInfo with wol_capable=true");
        assert!(rest_offline.wol_capable);

        let empty_signal_cache: HashMap<String, bool> = HashMap::new();
        assert!(
            resolve_device_wol_capable(&rest_offline, false, &empty_signal_cache, false),
            "无信令缓存时必须回退 REST wol_capable=true"
        );
        let entry_from_rest = map_my_device_entry(rest_offline.clone(), Some("111222333"));
        assert!(entry_from_rest.wol_capable);
        let json = serde_json::to_value(&entry_from_rest).expect("serialize MyDeviceEntry");
        assert_eq!(json["wolCapable"], serde_json::Value::Bool(true));

        // 2) 信令实时缓存更新（例如目标关闭 WoL）优先覆盖旧 REST 值。
        let mut live_signal_cache: HashMap<String, bool> = HashMap::new();
        live_signal_cache.insert("604928235".to_string(), false);
        assert!(
            !resolve_device_wol_capable(&rest_offline, false, &live_signal_cache, false),
            "信令实时快照应优先覆盖 REST 旧值"
        );

        // 3) 未登录态调用 request_device_wol_inner 应返回结构化认证门禁错误。
        let state = AppState::new_for_test();
        let err_unauth = request_device_wol_inner(&state, "604928235").unwrap_err();
        assert!(
            matches!(
                err_unauth.ui_error.code.as_str(),
                codes::AUTH_UNAUTHORIZED | codes::VALIDATION_INVALID_CONFIG
            ),
            "unexpected code: {}",
            err_unauth.ui_error.code
        );
    }

    #[test]
    fn save_terminal_prefs_to_path_persists_local_remote_size_and_reset() {
        let base = std::env::temp_dir().join(format!(
            "rc-ui-term-font-prefs-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).expect("create temp dir");
        let prefs_path = base.join("prefs.json");
        let state = AppState::new_for_test();

        let local_saved = save_terminal_prefs_to_path(
            &state,
            TerminalPrefsPatch {
                terminal_font_source: Some("local".to_string()),
                terminal_font_family: Some("MesloLGS NF".to_string()),
                terminal_font_id: Some("0123456789abcdef".to_string()),
                clear_font: Some(false),
                terminal_font_size: Some(16),
            },
            &prefs_path,
        )
        .expect("save local terminal font prefs");
        assert_eq!(local_saved.terminal_font_source, "local");
        assert_eq!(
            local_saved.terminal_font_family.as_deref(),
            Some("MesloLGS NF")
        );
        assert_eq!(
            local_saved.terminal_font_id.as_deref(),
            Some("0123456789abcdef")
        );
        assert_eq!(local_saved.terminal_font_size, 16);

        let reloaded_local = app_prefs::load_from_path(&prefs_path);
        assert_eq!(reloaded_local.terminal_font_source, "local");
        assert_eq!(
            reloaded_local.terminal_font_family.as_deref(),
            Some("MesloLGS NF")
        );
        assert_eq!(
            reloaded_local.terminal_font_id.as_deref(),
            Some("0123456789abcdef")
        );
        assert_eq!(reloaded_local.terminal_font_size, 16);

        let remote_saved = save_terminal_prefs_to_path(
            &state,
            TerminalPrefsPatch {
                terminal_font_source: Some("remote".to_string()),
                terminal_font_family: Some("JetBrains Mono".to_string()),
                terminal_font_id: Some("fedcba9876543210".to_string()),
                clear_font: Some(false),
                terminal_font_size: Some(15),
            },
            &prefs_path,
        )
        .expect("save remote terminal font prefs");
        assert_eq!(remote_saved.terminal_font_source, "remote");
        let reloaded_remote = app_prefs::load_from_path(&prefs_path);
        assert_eq!(reloaded_remote.terminal_font_source, "remote");
        assert_eq!(
            reloaded_remote.terminal_font_family.as_deref(),
            Some("JetBrains Mono")
        );
        assert_eq!(
            reloaded_remote.terminal_font_id.as_deref(),
            Some("fedcba9876543210")
        );
        assert_eq!(reloaded_remote.terminal_font_size, 15);

        let reset_saved = save_terminal_prefs_to_path(
            &state,
            TerminalPrefsPatch {
                terminal_font_source: Some("default".to_string()),
                terminal_font_family: Some(String::new()),
                terminal_font_id: Some(String::new()),
                clear_font: Some(true),
                terminal_font_size: Some(13),
            },
            &prefs_path,
        )
        .expect("reset terminal font prefs");
        assert_eq!(reset_saved.terminal_font_source, "default");
        assert_eq!(reset_saved.terminal_font_family, None);
        assert_eq!(reset_saved.terminal_font_id, None);
        assert_eq!(reset_saved.terminal_font_size, 13);

        let reloaded_reset = app_prefs::load_from_path(&prefs_path);
        assert_eq!(reloaded_reset.terminal_font_source, "default");
        assert_eq!(reloaded_reset.terminal_font_family, None);
        assert_eq!(reloaded_reset.terminal_font_id, None);
        assert_eq!(reloaded_reset.terminal_font_size, 13);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn terminal_font_disk_cache_roundtrip_eviction_and_rejects_traversal() {
        let base = std::env::temp_dir().join(format!(
            "rc-ui-term-font-cache-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).expect("create temp dir");

        assert!(terminal_font_cache_path(&base, "../evil").is_err());
        assert!(terminal_font_cache_path(&base, "bad/id").is_err());
        assert!(terminal_font_cache_path(&base, "").is_err());

        for i in 0..10 {
            let id = format!("font_id_{i:02}");
            let data = vec![b'A' + (i as u8); 32];
            save_cached_remote_font_to_dir(&base, &id, &data).expect("save cached font");
            std::thread::sleep(Duration::from_millis(5));
        }

        let loaded_latest =
            load_cached_remote_font_from_dir(&base, "font_id_09").expect("latest must exist");
        assert_eq!(loaded_latest, vec![b'A' + 9; 32]);

        let cached_count = std::fs::read_dir(&base)
            .expect("read cache dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("font"))
            .count();
        assert_eq!(
            cached_count, MAX_DISK_CACHED_TERMINAL_FONTS,
            "cache must evict oldest files down to MAX_DISK_CACHED_TERMINAL_FONTS"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn standalone_signal_client_lifecycle_syncs_running_host_handle() {
        let base = std::env::temp_dir().join(format!(
            "rc-ui-standalone-signal-sync-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let certs_dir = base.join("certs");
        let auth_path = base.join("auth.json");
        let server_path = base.join("server.json");
        std::fs::create_dir_all(&certs_dir).expect("create temp dirs");

        let configured = ServerConfig {
            enabled: true,
            server_host: "127.0.0.1".to_string(),
            signal_port: 1,
            relay_port: 21117,
            use_https: false,
            ..ServerConfig::default()
        };
        let future_exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + 3600;
        rc_ui::auth_client::save_auth_session(
            &auth_path,
            &rc_ui::auth_client::StoredAuthSession {
                user_id: "00000000-0000-0000-0000-000000000002".to_string(),
                email: "standalone@example.com".to_string(),
                display_name: "Standalone".to_string(),
                role: "user".to_string(),
                must_change_password: false,
                access_token: "tok-standalone".to_string(),
                refresh_token: "ref-standalone".to_string(),
                access_expires_at_unix_sec: future_exp,
                server_host: "127.0.0.1".to_string(),
                api_port: 21118,
            },
        )
        .expect("save auth session");

        let state = AppState::with_initial_configs_and_paths(
            configured,
            AppPrefs::default(),
            auth_path,
            certs_dir,
            None,
        );
        let _ = state.auth.restore_on_startup();
        assert_eq!(state.gate_state(), AppGateState::Authenticated);

        TEST_HOST_SIGNAL_SYNC_LOG.with(|slot| {
            *slot.borrow_mut() = Some(Vec::new());
        });

        // 1) 先启动 HostHandle（此时 signal_client 为 None），再后启动信令并同步进 HostHandle。
        let host_snap =
            start_host_service_on(&state, "127.0.0.1:0".parse().unwrap(), false).unwrap();
        assert!(host_snap.enabled);
        assert!(state.server.lock().unwrap().signal_client.is_none());

        let started_snap = start_signal_client_if_enabled(&state);
        assert!(started_snap.enabled);
        let ptr1 = Arc::as_ptr(
            state
                .server
                .lock()
                .unwrap()
                .signal_client
                .as_ref()
                .expect("signal_client spawned"),
        ) as usize;

        // 2) 再次调用 start_host_service_on 命中已有 HostHandle 刷新分支，不应死锁且同步同一句柄。
        let refreshed_snap =
            start_host_service_on(&state, "127.0.0.1:0".parse().unwrap(), false).unwrap();
        assert!(refreshed_snap.enabled);

        // 3) 登出清空 signal_client 并同步 None 至 HostHandle。
        let logout_snap = stop_signal_client_for_logout(&state);
        assert_eq!(logout_snap.state, ServerConnectionState::Disabled);
        assert!(state.server.lock().unwrap().signal_client.is_none());

        // 4) 重新拉起后调用 save_server_config_to_path 禁用与 shutdown_signal_client，均正常同步 None。
        let _ = start_signal_client_if_enabled(&state);
        let ptr2 = Arc::as_ptr(
            state
                .server
                .lock()
                .unwrap()
                .signal_client
                .as_ref()
                .expect("signal_client respawned"),
        ) as usize;
        assert_ne!(ptr1, ptr2);
        let disabled_snap = save_server_config_to_path(
            &state,
            SaveServerConfigRequest {
                enabled: false,
                server_host: "127.0.0.1".to_string(),
                signal_port: 1,
                relay_port: 21117,
            },
            &server_path,
        )
        .expect("disable server config");
        assert!(!disabled_snap.enabled);
        assert_eq!(disabled_snap.state, ServerConnectionState::Disabled);
        assert!(state.server.lock().unwrap().signal_client.is_none());

        shutdown_signal_client(&state);
        assert!(state.server.lock().unwrap().shutting_down);
        assert!(state.server.lock().unwrap().signal_client.is_none());

        let recorded = TEST_HOST_SIGNAL_SYNC_LOG
            .with(|slot| slot.borrow_mut().take())
            .expect("sync log present");
        assert_eq!(
            recorded,
            vec![Some(ptr1), Some(ptr1), None, Some(ptr2), None, None]
        );

        let stopped = stop_host_service(&state, true);
        assert!(!stopped.enabled);
        let _ = std::fs::remove_dir_all(&base);
    }
}

//! RabbitControl UI —— Tauri v2 应用入口（T21 M2-6 控制端界面）。
//!
//! 唯一依据：`docs/reference/design/08-ui-design.md` 与 `docs/reference/tickets/T21-controller-ui.md`。
//! - `window`：多窗口按需创建（W1–W4 / S1）
//! - `state`：当前唯一活动会话与 §6.0 mock 设备表
//! - `commands`：全部 Tauri IPC 命令（连接校验、会话启停、输入桥接、stage 尺寸同步、剪贴板）
//! - 后台轮询线程：非阻塞排空 `SessionHandle::try_recv_update()`，向前端推送
//!   `session://update` 事件，并在主线程完成 `CAMetalLayer` 挂载与摘除（T21 坑 2）。

// Windows 上以 GUI 子系统启动，避免附带控制台窗口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(unsafe_code)]
#![deny(missing_docs)]

mod commands;
mod state;
mod window;

use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::rngs::OsRng;
use rand::RngCore;
use rc_tools::{FileEvent, PortMapRuleSnapshot, TerminalEvent};
use rc_ui::app_prefs;
use rc_ui::config_paths;
use rc_ui::display_ctrl;
use rc_ui::host_bridge::HostUpdate;
use rc_ui::lan_discovery::{self, LanDiscoveryHandle};
use rc_ui::metal_attach::{self, DockMenuAction, Frame};
use rc_ui::server_config::{self, ServerStatusSnapshot};
use rc_ui::service_ipc::{
    read_or_create_ipc_secret, AgentSnapshot, IpcClient, IpcServer, ServiceRequest,
    ServiceResponse, ServiceSessionType,
};
use rc_ui::service_manager;
use rc_ui::session_bridge::{LayerLease, SessionUpdate};
use tauri::{AppHandle, Emitter, Manager, RunEvent, WindowEvent};

use crate::state::{AppGateState, AppState};

/// 后台状态更新轮询粒度（低开销且保证 `Connecting`/`Negotiated`/`Ended` 与审批弹窗实时送达；
/// HUD 本身在会话线程内已做 1s 节流）。
const POLLER_INTERVAL: Duration = Duration::from_millis(20);

fn sync_proxy_state_and_emit(
    app: &AppHandle,
    prev_agent: &mut Option<AgentSnapshot>,
    prev_server: &mut Option<ServerStatusSnapshot>,
) {
    let st = app.state::<AppState>();
    if let Ok(_transition_guard) = st.service_transition_lock.try_lock() {
        if st.is_service_proxy() {
            if st.auth.sync_external_wipe_from_disk() {
                let app_for_kick = app.clone();
                let _ = thread::Builder::new()
                    .name("rc-auth-kicked".to_string())
                    .spawn(move || {
                        commands::force_logout_from_kick(
                            &app_for_kick,
                            "登录状态已失效或账号已下线，请重新登录".to_string(),
                        );
                    });
            } else if st.auth.sync_external_login_from_disk() {
                let app_rec = app.clone();
                let _ = thread::Builder::new()
                    .name("rc-proxy-auth-recover".to_string())
                    .spawn(move || {
                        let st_rec = app_rec.state::<AppState>();
                        let latest_cfg =
                            server_config::load_from_path(&server_config::default_config_path());
                        st_rec.auth.update_server_config(latest_cfg.clone());
                        {
                            let mut rt = st_rec.server.lock().unwrap_or_else(|e| e.into_inner());
                            rt.config = latest_cfg;
                        }
                        commands::post_auth_ready(&app_rec, &st_rec);
                    });
            }
        }
    }
    if let Ok(ServiceResponse::Snapshot(cur)) =
        commands::call_agent_ipc(ServiceRequest::GetSnapshot, Duration::from_millis(350))
    {
        let cur = *cur;
        let prev = prev_agent.as_ref();
        if prev.map_or(true, |p| p.enabled != cur.enabled) {
            if cur.enabled {
                if let (Some(listen_addr), Some(display_addr)) =
                    (cur.listen_addr.clone(), cur.display_addr.clone())
                {
                    let _ = app.emit(
                        "host://update",
                        &HostUpdate::Listening {
                            listen_addr,
                            display_addr,
                            backend: cur.backend.clone(),
                        },
                    );
                }
            } else if prev.is_some() {
                let _ = app.emit("host://update", &HostUpdate::Stopped);
            }
        }
        if prev.is_some_and(|p| !p.password.is_empty() && p.password != cur.password)
            && !cur.custom_password
        {
            let _ = app.emit(
                "host://update",
                &HostUpdate::PasswordRotated {
                    password: cur.password.clone(),
                },
            );
        }
        if let Some(old_ap) = prev.and_then(|p| p.pending_approval.as_ref()) {
            let same = cur
                .pending_approval
                .as_ref()
                .is_some_and(|n| n.generation == old_ap.generation);
            if !same {
                let _ = app.emit(
                    "host://update",
                    &HostUpdate::ApprovalCancelled {
                        generation: old_ap.generation,
                    },
                );
            }
        }
        if let Some(ref new_ap) = cur.pending_approval {
            let is_new = prev
                .and_then(|p| p.pending_approval.as_ref())
                .map_or(true, |old| old.generation != new_ap.generation);
            if is_new {
                let _ = app.emit(
                    "host://update",
                    &HostUpdate::ApprovalRequested(new_ap.clone()),
                );
            }
        }
        if prev.and_then(|p| p.connected_peer.as_ref()) != cur.connected_peer.as_ref() {
            match cur.connected_peer.as_ref() {
                Some(peer) => {
                    let _ = app.emit(
                        "host://update",
                        &HostUpdate::ClientConnected {
                            remote_addr: peer.clone(),
                        },
                    );
                }
                None if prev.is_some() => {
                    let _ = app.emit(
                        "host://update",
                        &HostUpdate::ClientDisconnected {
                            reason: "远端会话已结束".to_string(),
                        },
                    );
                }
                None => {}
            }
        }
        *prev_agent = Some(cur);
    }

    if let Ok(ServiceResponse::ServerStatus(srv)) =
        commands::call_agent_ipc(ServiceRequest::GetServerStatus, Duration::from_millis(350))
    {
        let raw = *srv;
        let changed = prev_server.as_ref().map_or(true, |prev| {
            prev.enabled != raw.enabled
                || prev.server_host != raw.server_host
                || prev.signal_port != raw.signal_port
                || prev.relay_port != raw.relay_port
                || prev.state != raw.state
                || prev.device_id != raw.device_id
                || prev.device_id_formatted != raw.device_id_formatted
                || prev.error_message != raw.error_message
                || prev.ui_error != raw.ui_error
                || prev.signal_addr != raw.signal_addr
                || prev.relay_addr != raw.relay_addr
        });
        if changed {
            let mut emitted = raw.clone();
            emitted.revision = server_config::next_status_revision();
            let st = app.state::<AppState>();
            {
                let mut rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
                rt.apply_status_snapshot(&emitted);
                if let Some(ref did) = emitted.device_id {
                    if rt.config.cached_device_id.is_none() {
                        rt.config.cached_device_id = Some(did.clone());
                    }
                }
            }
            if let Some(lan) = st.lan.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                lan.set_local_device_id(emitted.device_id.clone());
            }
            let _ = app.emit("server://status", &emitted);
            *prev_server = Some(raw);
        }
    }

    if let Ok(ServiceResponse::DeviceStatusEvents(events)) = commands::call_agent_ipc(
        ServiceRequest::DrainDeviceStatusEvents,
        Duration::from_millis(350),
    ) {
        for dev_ev in events {
            let _ = app.emit("device://status-changed", &dev_ev);
        }
    }

    if let Ok(ServiceResponse::DeviceWolEvents { events }) = commands::call_agent_ipc(
        ServiceRequest::DrainDeviceWolEvents,
        Duration::from_millis(350),
    ) {
        for wol_ev in events {
            let _ = app.emit("device://wol-changed", &wol_ev);
        }
    }
}

/// 查询当前进程已注册平台是否具备 AV1 硬件解码能力（M3/M4 为 `true`，M1/M2 为 `false`）。
fn local_client_supports_hardware_av1_decode() -> bool {
    rc_pipeline::platform_decode_capabilities()
        .iter()
        .any(|c| c.codec == rc_encode::Codec::Av1 && c.hardware)
}

/// 将 `SessionUpdate` 序列化为发往前端 `"session://update"` 的 JSON 载荷。
///
/// 对 `SessionUpdate::DisplayConfig` 在上游未显式携带 `client_supports_av1` 时注入本机硬件 AV1
/// 解码能力（不覆盖上游未来新增的同名字段），确保 M1/M2 与 M3/M4 严格按本机硬解能力 fail-closed 门控 AV1 菜单项。
fn serialize_session_update_for_ui(
    upd: &SessionUpdate,
    client_supports_av1: bool,
) -> Option<serde_json::Value> {
    let mut val = serde_json::to_value(upd).ok()?;
    if matches!(upd, SessionUpdate::DisplayConfig { .. }) {
        inject_client_supports_av1_if_missing(&mut val, client_supports_av1);
    }
    Some(val)
}

fn inject_client_supports_av1_if_missing(val: &mut serde_json::Value, client_supports_av1: bool) {
    if let Some(data) = val
        .get_mut("data")
        .and_then(serde_json::Value::as_object_mut)
    {
        data.entry("client_supports_av1".to_string())
            .or_insert(serde_json::Value::Bool(client_supports_av1));
    }
}

/// 启动后台状态轮询线程：
/// 1) 排空 `SessionHandle` 更新队列 → 缓存快照（M8）→ emit `"session://update"`，
///    并协调主线程上的 `CAMetalLayer` 挂载 / 摘除；
/// 2) 排空 `HostHandle` 更新队列 → 折算 `HostRuntime` 状态（含断开后刷新临时验证码）
///    → emit `"host://update"`；
/// 3) 排空信令状态与入站中继分发（代理模式下改为定时同步 `agent.sock` 快照）。
fn start_background_poller(app: AppHandle) {
    let _ = thread::Builder::new()
        .name("rc-ui-poller".to_string())
        .spawn(move || {
            let mut poller_tick: u64 = 0;
            let mut prev_proxy_agent: Option<AgentSnapshot> = None;
            let mut prev_proxy_server: Option<ServerStatusSnapshot> = None;
            loop {
                thread::sleep(POLLER_INTERVAL);
                poller_tick = poller_tick.wrapping_add(1);

                // 每 ~500ms 检查后台菜单栏发来的「打开权限检查向导」信号，以及代理/独立托盘状态同步。
                if poller_tick % 10 == 0 {
                    let marker = config_paths::run_dir().join("open-perm-check");
                    if std::fs::remove_file(&marker).is_ok() {
                        commands::mark_tray_perm_check_pending();
                        let app_perm = app.clone();
                        let _ = app.run_on_main_thread(move || {
                            commands::show_and_focus_main_window(&app_perm);
                            let _ = app_perm.emit("tray://open-permissions", ());
                        });
                    }
                    let st = app.state::<AppState>();
                    if st.is_service_proxy() {
                        sync_proxy_state_and_emit(
                            &app,
                            &mut prev_proxy_agent,
                            &mut prev_proxy_server,
                        );
                    } else {
                        prev_proxy_agent = None;
                        prev_proxy_server = None;
                        if poller_tick % 20 == 0 {
                            commands::sync_standalone_status_bar(&st);
                        }
                    }
                }

                // ── 1) 控制端会话更新 + 外围工具事件 + Metal 挂载/摘除 ──
                let mut polled_sid: Option<u64> = None;
                let mut polled_saw_ended = false;
                let mut updates: Vec<SessionUpdate> = Vec::new();
                let mut file_events: Vec<FileEvent> = Vec::new();
                let mut terminal_events: Vec<TerminalEvent> = Vec::new();
                let mut portmap_update: Option<Vec<PortMapRuleSnapshot>> = None;
                let mut attach_req: Option<(u64, LayerLease, Frame)> = None;
                let mut sync_layer_req: Option<(u64, LayerLease, Frame)> = None;
                let mut auto_fit_req: Option<(u32, u32)> = None;
                let mut emit_active_state = false;
                let mut negotiated_record: Option<(String, String)> = None;
                let mut restore_portmaps_req: Option<(u64, String)> = None;
                let mut wallpaper_updates: Vec<(String, String, String)> = Vec::new();
                let mut keychain_sync_op: Option<rc_ui::keychain::KeychainSyncOp> = None;
                let mut ended_session = None;
                let mut auto_reconnect_req: Option<(u64, String)> = None;

                {
                    let st = app.state::<AppState>();
                    let mut slot = st.session.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(active) = slot.as_mut() {
                        let sid = active.id;
                        polled_sid = Some(sid);
                        let mut saw_ended = false;
                        while let Some(mut upd) = active.handle.try_recv_update() {
                            match upd {
                                SessionUpdate::Connecting {
                                    ref mut watch_only, ..
                                } => {
                                    *watch_only = active.watch_only;
                                    active.meta.phase = "connecting".to_string();
                                }
                                SessionUpdate::Negotiated {
                                    width,
                                    height,
                                    ref transport,
                                    mtls_active,
                                    ref peer_cert_fingerprint,
                                    ref mut watch_only,
                                    ..
                                } => {
                                    *watch_only = active.watch_only;
                                    active.set_video_size(width, height);
                                    active.meta.width = width;
                                    active.meta.height = height;
                                    active.meta.connected = true;
                                    active.meta.phase = "connected".to_string();
                                    active.meta.transport = Some(transport.clone());
                                    active.meta.mtls_active = mtls_active;
                                    active.meta.peer_cert_fingerprint =
                                        peer_cert_fingerprint.clone();
                                    st.auto_reconnect_attempts
                                        .store(0, std::sync::atomic::Ordering::Release);
                                    *st.auto_reconnect_status
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner()) = None;
                                    st.clear_terminal_error();
                                    emit_active_state = true;
                                    if !active.meta.device_key.is_empty() {
                                        negotiated_record = Some((
                                            active.meta.device_key.clone(),
                                            active.meta.device_name.clone(),
                                        ));
                                    }
                                    if !active.watch_only && !active.portmaps_restored {
                                        active.portmaps_restored = true;
                                        let restore_key =
                                            if !active.meta.device_key.trim().is_empty() {
                                                active.meta.device_key.trim().to_string()
                                            } else {
                                                active.meta.target.trim().to_string()
                                            };
                                        if !restore_key.is_empty() {
                                            restore_portmaps_req = Some((sid, restore_key));
                                        }
                                    }
                                    if !active.meta.tools_only {
                                        auto_fit_req = Some((width, height));
                                        let fallback = Frame {
                                            x: 0.0,
                                            y: 0.0,
                                            width: f64::from(width),
                                            height: f64::from(height),
                                        };
                                        let eff = active.effective_layer_frame(fallback);
                                        let eff_w = eff.width.round() as u32;
                                        let eff_h = eff.height.round() as u32;
                                        if eff_w > 0 && eff_h > 0 {
                                            active.handle.send(
                                                rc_ui::session_bridge::SessionCommand::StageRect {
                                                    width: eff_w,
                                                    height: eff_h,
                                                },
                                            );
                                        }
                                        if let Some(lease) = active.handle.layer_lease() {
                                            if let Some(req) = active.begin_attach(lease, fallback)
                                            {
                                                attach_req = Some(req);
                                            }
                                        }
                                    }
                                }
                                SessionUpdate::MediaResumed { width, height, .. } => {
                                    active.meta.media_active = true;
                                    active.meta.tools_only = false;
                                    if width > 0 && height > 0 {
                                        active.set_video_size(width, height);
                                        active.meta.width = width;
                                        active.meta.height = height;
                                        auto_fit_req = Some((width, height));
                                    }
                                    *st.last_session_params
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner()) =
                                        Some((active.watch_only, active.meta.clone()));
                                    emit_active_state = true;
                                    let fallback_w = if width > 0 {
                                        width
                                    } else {
                                        active.meta.width.max(1)
                                    };
                                    let fallback_h = if height > 0 {
                                        height
                                    } else {
                                        active.meta.height.max(1)
                                    };
                                    let fallback = Frame {
                                        x: 0.0,
                                        y: 0.0,
                                        width: f64::from(fallback_w),
                                        height: f64::from(fallback_h),
                                    };
                                    let eff = active.effective_layer_frame(fallback);
                                    let eff_w = eff.width.round() as u32;
                                    let eff_h = eff.height.round() as u32;
                                    if eff_w > 0 && eff_h > 0 {
                                        active.handle.send(
                                            rc_ui::session_bridge::SessionCommand::StageRect {
                                                width: eff_w,
                                                height: eff_h,
                                            },
                                        );
                                    }
                                    if let Some(lease) = active.handle.layer_lease() {
                                        if let Some(req) = active.begin_attach(lease, fallback) {
                                            attach_req = Some(req);
                                        }
                                    }
                                }
                                SessionUpdate::MediaState { paused } => {
                                    active.meta.media_active = !paused;
                                    active.meta.tools_only = paused;
                                    emit_active_state = true;
                                }
                                SessionUpdate::Reconnecting { .. } => {
                                    active.meta.phase = "reconnecting".to_string();
                                    active.meta.connected = false;
                                    active.terminal_auto_unlock_in_flight = false;
                                    active.terminal_unlock_seq =
                                        active.terminal_unlock_seq.wrapping_add(1);
                                    active.clear_pending_terminal_remember();
                                    active.terminal_lock_state.required = false;
                                    active.terminal_lock_state.unlocked = false;
                                    active.terminal_lock_state.username = None;
                                    active.set_terminal_cooldown(0, std::time::Instant::now());
                                    emit_active_state = true;
                                }
                                SessionUpdate::DisplayConfig { width, height, .. }
                                    if width > 0 && height > 0 =>
                                {
                                    let prev_size = active.video_size;
                                    active.set_video_size(width, height);
                                    active.meta.width = width;
                                    active.meta.height = height;
                                    if let Some((pw, ph)) = prev_size {
                                        if pw > 0 && ph > 0 {
                                            let prev_ratio = f64::from(pw) / f64::from(ph);
                                            let next_ratio = f64::from(width) / f64::from(height);
                                            if (prev_ratio - next_ratio).abs() > 0.01 {
                                                auto_fit_req = Some((width, height));
                                            }
                                        }
                                    }
                                    if let Some(stage_frame) = active.last_stage_frame {
                                        let eff = active.effective_layer_frame(stage_frame);
                                        let eff_w = eff.width.round() as u32;
                                        let eff_h = eff.height.round() as u32;
                                        if eff_w > 0 && eff_h > 0 {
                                            active.handle.send(
                                                rc_ui::session_bridge::SessionCommand::StageRect {
                                                    width: eff_w,
                                                    height: eff_h,
                                                },
                                            );
                                        }
                                        if let Some(lease) = active.layer_state.attached_lease() {
                                            sync_layer_req = Some((sid, lease, eff));
                                        }
                                    }
                                }
                                SessionUpdate::Wallpaper { data_url } => {
                                    if !active.meta.device_key.is_empty() {
                                        wallpaper_updates.push((
                                            active.meta.device_key.clone(),
                                            active.meta.device_name.clone(),
                                            data_url,
                                        ));
                                    }
                                    // 不并入 `"session://update"`，亦不覆写 `last_session_update`。
                                    continue;
                                }
                                SessionUpdate::HostVersion(ref hv) => {
                                    *st.host_version.lock().unwrap_or_else(|e| e.into_inner()) =
                                        Some((sid, hv.clone()));
                                }
                                SessionUpdate::HostScreenState {
                                    locked,
                                    capture_suspended,
                                } => {
                                    active.host_screen_state = Some((locked, capture_suspended));
                                }
                                SessionUpdate::ToolPermissionsUpdated(perms) => {
                                    active.meta.tool_permissions = perms.into();
                                    active.terminal_lock_state.allow_terminal =
                                        perms.allow_terminal;
                                    emit_active_state = true;
                                }
                                SessionUpdate::TerminalLockState {
                                    required,
                                    unlocked,
                                    ref username,
                                    retry_after_secs,
                                } => {
                                    active.terminal_lock_state.required = required;
                                    active.terminal_lock_state.unlocked = unlocked;
                                    active.terminal_lock_state.username = username.clone();
                                    active.set_terminal_cooldown(
                                        retry_after_secs,
                                        std::time::Instant::now(),
                                    );
                                    if required && !unlocked {
                                        active.clear_pending_terminal_remember();
                                    }
                                    if !required || unlocked {
                                        active.terminal_cooldown_until = None;
                                        active.terminal_auto_unlock_in_flight = false;
                                        active.terminal_unlock_seq =
                                            active.terminal_unlock_seq.wrapping_add(1);
                                    }
                                }
                                SessionUpdate::TerminalUnlockResult {
                                    ok,
                                    ref error,
                                    retry_after_secs,
                                } => {
                                    let _ = error;
                                    let was_auto = std::mem::replace(
                                        &mut active.terminal_auto_unlock_in_flight,
                                        false,
                                    );
                                    let dk = if !active.meta.device_key.trim().is_empty() {
                                        active.meta.device_key.trim().to_string()
                                    } else {
                                        active.meta.target.trim().to_string()
                                    };
                                    if ok {
                                        active.terminal_lock_state.required = false;
                                        active.terminal_lock_state.unlocked = true;
                                        active.set_terminal_cooldown(0, std::time::Instant::now());
                                        if let Some((save_dk, pw)) =
                                            active.pending_terminal_remember.take()
                                        {
                                            rc_ui::keychain::KeychainSyncOp::set_latest(
                                                &mut keychain_sync_op,
                                                rc_ui::keychain::KeychainSyncOp::Save(
                                                    save_dk.as_str().to_string(),
                                                    pw,
                                                ),
                                            );
                                        }
                                    } else {
                                        active.set_terminal_cooldown(
                                            retry_after_secs,
                                            std::time::Instant::now(),
                                        );
                                        active.clear_pending_terminal_remember();
                                        if was_auto && !dk.is_empty() {
                                            rc_ui::keychain::KeychainSyncOp::set_latest(
                                                &mut keychain_sync_op,
                                                rc_ui::keychain::KeychainSyncOp::Delete(dk),
                                            );
                                        }
                                    }
                                }
                                SessionUpdate::Ended {
                                    ref reason,
                                    recoverable,
                                    ref ui_error,
                                } => {
                                    saw_ended = true;
                                    let should_auto_reconnect = recoverable
                                        && !active.disconnecting
                                        && !st.is_quitting()
                                        && st.gate_state() == AppGateState::Authenticated
                                        && commands::has_any_session_or_tool_window(&app);
                                    if should_auto_reconnect {
                                        let cur_epoch = st
                                            .session_open_epoch
                                            .load(std::sync::atomic::Ordering::Acquire);
                                        auto_reconnect_req = Some((cur_epoch, reason.clone()));
                                        continue;
                                    }
                                    if !active.disconnecting && !recoverable {
                                        let cur_epoch = st
                                            .session_open_epoch
                                            .load(std::sync::atomic::Ordering::Acquire);
                                        st.record_terminal_error_with_ui_if_epoch(
                                            cur_epoch,
                                            reason.clone(),
                                            ui_error.clone(),
                                        );
                                    }
                                    emit_active_state = true;
                                }
                                _ => {}
                            }
                            updates.push(upd);
                        }
                        while let Some(ev) = active.handle.try_recv_file_event() {
                            file_events.push(ev);
                        }
                        while let Some(ev) = active.handle.try_recv_terminal_event() {
                            terminal_events.push(ev);
                        }
                        portmap_update = active.handle.take_portmap_update();
                        polled_saw_ended = saw_ended;

                        if saw_ended {
                            // U-H2 / U-H3：会话结束时在 `session` 锁内原子摘除并清空快照，
                            // 防止新窗口拉到过期 `Ended` 并清空 W4 映射列表；远端后台 PTY 继续由被控端保活。
                            ended_session = slot.take();
                            state::clear_session_snapshot_locked(&st.last_session_update);
                            *st.host_version.lock().unwrap_or_else(|e| e.into_inner()) = None;
                            portmap_update = Some(Vec::new());
                        } else if let Some(latest) = updates.iter().rev().find(|u| {
                            !matches!(
                                u,
                                SessionUpdate::DisplayConfig { .. }
                                    | SessionUpdate::PrivacyState { .. }
                                    | SessionUpdate::HostMuteState { .. }
                                    | SessionUpdate::MouseModeState { .. }
                                    | SessionUpdate::DisplaysState { .. }
                                    | SessionUpdate::VirtualDisplayError { .. }
                                    | SessionUpdate::HostVersion(_)
                                    | SessionUpdate::RemoteUpdateStatus(_)
                                    | SessionUpdate::HostScreenState { .. }
                                    | SessionUpdate::MediaState { .. }
                                    | SessionUpdate::ToolPermissionsUpdated(_)
                                    | SessionUpdate::ToolPermissionDenied { .. }
                                    | SessionUpdate::TerminalLockState { .. }
                                    | SessionUpdate::TerminalUnlockResult { .. }
                            )
                        }) {
                            // Review M8 + U-H3：在 `session` 锁内绑定 `sid` 缓存最新快照，跳过纯控制元数据帧。
                            state::record_session_update_locked(
                                &st.last_session_update,
                                sid,
                                latest.clone(),
                            );
                        }
                    }
                }

                // 若同批次内会话尚未结束：
                // 1) 首次 Negotiated 自动将 S1 窗口适配到远端画面宽高比（消除左右/上下黑条，仍保留自由缩放能力）；
                // 2) 硬件 layer 就绪时在主线程挂载到 S1 窗口 NSView；
                // 3) 分辨率切换后若已挂载，同步更新 `CAMetalLayer` 等比视口。
                if ended_session.is_none() {
                    if let Some((pm_sid, pm_key)) = restore_portmaps_req {
                        let app_for_pm = app.clone();
                        let _ = thread::Builder::new()
                            .name("rc-portmap-restore".to_string())
                            .spawn(move || {
                                let st = app_for_pm.state::<AppState>();
                                commands::restore_saved_portmaps_for_active_session(
                                    &st, pm_sid, &pm_key,
                                );
                            });
                    }
                    if let Some((fw, fh)) = auto_fit_req {
                        let app_for_fit = app.clone();
                        let _ = app.run_on_main_thread(move || {
                            let _ =
                                window::fit_session_window_to_video(&app_for_fit, fw, fh, false);
                        });
                    }
                    if let Some((sid, lease, frame)) = attach_req {
                        attach_layer_on_main_thread(&app, sid, lease, frame);
                    } else if let Some((sid, lease, eff_frame)) = sync_layer_req {
                        let app_for_sync = app.clone();
                        let _ = app.run_on_main_thread(move || {
                            let st = app_for_sync.state::<AppState>();
                            if state::should_sync_layer_on_main_thread(&st.session, sid, &lease) {
                                let _ =
                                    rc_ui::metal_attach::sync_leased_layer_frame(&lease, eff_frame);
                            }
                            drop(lease);
                        });
                    }
                }

                if let Some(op) = keychain_sync_op.take() {
                    let st = app.state::<AppState>();
                    st.keychain.enqueue_sync(op);
                }

                // U-H3 + H11：发射 `"session://update"` 与 W2/W3/W4 工具事件前复核 `sid` 未被替换，
                // 且若槽位已被 `disconnect_active_session` 清空则屏蔽陈旧非终态批次。
                if let Some(sid) = polled_sid {
                    let st = app.state::<AppState>();
                    let (should_emit, latest_active_state) = state::resolve_session_update_emit(
                        &st.session,
                        sid,
                        polled_saw_ended,
                        emit_active_state,
                    );
                    if should_emit {
                        let client_supports_av1 = local_client_supports_hardware_av1_decode();
                        for upd in updates {
                            if let Some(payload) =
                                serialize_session_update_for_ui(&upd, client_supports_av1)
                            {
                                let _ = app.emit("session://update", &payload);
                            } else {
                                let _ = app.emit("session://update", &upd);
                            }
                        }
                        for ev in file_events {
                            let _ = app.emit("file://event", &ev);
                        }
                        for ev in terminal_events {
                            let _ = app.emit("terminal://event", &ev);
                        }
                        if let Some(rules) = portmap_update {
                            let _ = app.emit("portmap://update", &rules);
                        }
                        if let Some(info) = latest_active_state {
                            let _ = app.emit("session://active-state", &info);
                        }
                    }
                }

                if negotiated_record.is_some() || !wallpaper_updates.is_empty() {
                    let app_wp = app.clone();
                    let _ = thread::Builder::new()
                        .name("rc-snapshot-persist".to_string())
                        .spawn(move || {
                            let st = app_wp.state::<AppState>();
                            let wallpapers_dir = app_prefs::default_wallpapers_dir();
                            let prefs_path = app_prefs::default_prefs_path();
                            let _io = st.prefs_io_lock.lock().unwrap_or_else(|e| e.into_inner());
                            let _file_lock = config_paths::acquire_file_lock(
                                &prefs_path,
                                Duration::from_secs(2),
                            );
                            let now_ts = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();

                            let mut saved_wallpapers: Vec<(String, String, String)> = Vec::new();
                            for (key, name, data_url) in wallpaper_updates {
                                let b64 = data_url
                                    .split_once("base64,")
                                    .map(|(_, rest)| rest)
                                    .unwrap_or(data_url.as_str());
                                if let Some(bytes) = display_ctrl::base64_decode(b64) {
                                    if let Ok(filename) = app_prefs::save_device_wallpaper(
                                        &wallpapers_dir,
                                        &key,
                                        &bytes,
                                    ) {
                                        saved_wallpapers.push((key, name, filename));
                                    }
                                }
                            }

                            let (next_prefs, views) = {
                                let mut prefs = st.prefs.lock().unwrap_or_else(|e| e.into_inner());
                                if st.is_service_proxy() && prefs_path.exists() {
                                    *prefs = app_prefs::load_from_path(&prefs_path);
                                }
                                if let Some((key, name)) = negotiated_record {
                                    let rec = prefs.device_snapshots.entry(key).or_default();
                                    if !name.is_empty() {
                                        rec.name = name;
                                    }
                                    rec.last_connected_ts = now_ts;
                                }
                                for (key, name, filename) in saved_wallpapers {
                                    let rec = prefs.device_snapshots.entry(key).or_default();
                                    if !name.is_empty() {
                                        rec.name = name;
                                    }
                                    rec.last_connected_ts = rec.last_connected_ts.max(now_ts);
                                    rec.wallpaper_file = Some(filename);
                                }
                                let cloned = prefs.clone();
                                let views = cloned.snapshot_views(&wallpapers_dir);
                                (cloned, views)
                            };
                            let _ = app_prefs::save_to_path(&prefs_path, &next_prefs);
                            commands::notify_background_services_reload(&st, false);
                            let _ = app_wp.emit("device://snapshots", &views);
                        });
                }

                if let Some(old) = ended_session {
                    commands::teardown_session(&app, old);
                }
                if let Some((epoch, reason)) = auto_reconnect_req {
                    commands::spawn_auto_reconnect_task(app.clone(), epoch, reason);
                }

                // ── 2) 本机被控端状态更新 ──
                let mut host_updates: Vec<HostUpdate> = Vec::new();
                let mut stopped_host_handle = None;

                {
                    let st = app.state::<AppState>();
                    let mut host = st.host.lock().unwrap_or_else(|e| e.into_inner());
                    let mut host_stopped = false;
                    while let Some(upd) = host.handle.as_ref().and_then(|h| h.try_recv_update()) {
                        if !host.allow_remote_assist {
                            if let HostUpdate::ApprovalRequested(ref prompt) = upd {
                                if let Some(ref handle) = host.handle {
                                    let _ = handle.respond_approval(prompt.generation, false);
                                }
                                continue;
                            }
                        }
                        if host.apply_update(&upd) {
                            host_stopped = true;
                        }
                        host_updates.push(upd);
                    }
                    if host_stopped {
                        // M8：收到 HostUpdate::Stopped 时无条件回收句柄（工作线程已先清除 running 再推送 Stopped）。
                        stopped_host_handle = host.handle.take();
                        // U-M6：在持有 `st.host` 锁期间按 `host -> lan` 锁序同步撤回广播。
                        commands::sync_lan_advertising_locked(&st, &host);
                    }
                }

                drop(stopped_host_handle);

                let host_changed = !host_updates.is_empty();
                for upd in host_updates {
                    let _ = app.emit("host://update", &upd);
                }
                if host_changed {
                    let st = app.state::<AppState>();
                    commands::sync_standalone_status_bar(&st);
                }

                // ── 3) 信令服务器状态更新与入站中继会话分发（H2 + H8 + H10） ──
                let signal_client = {
                    let st = app.state::<AppState>();
                    let rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
                    rt.signal_client.as_ref().cloned()
                };
                if let Some(client) = signal_client {
                    let st = app.state::<AppState>();
                    while let Some(snap) = client.try_recv_status() {
                        let (accepted, persist_target) = {
                            let mut rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
                            let is_current = rt
                                .signal_client
                                .as_ref()
                                .is_some_and(|cur| Arc::ptr_eq(cur, &client));
                            if !is_current {
                                break;
                            }
                            if !rt.apply_status_snapshot(&snap) {
                                (false, None)
                            } else {
                                let persist = snap.device_id.as_ref().and_then(|new_id| {
                                    if rt.config.cached_device_id.as_deref()
                                        != Some(new_id.as_str())
                                    {
                                        rt.config.cached_device_id = Some(new_id.clone());
                                        Some((rt.config_epoch, rt.config.clone()))
                                    } else {
                                        None
                                    }
                                });
                                (true, persist)
                            }
                        };

                        if !accepted {
                            continue;
                        }

                        let updated_device_id = persist_target.is_some();
                        if let Some((epoch, cfg_to_save)) = persist_target {
                            let _io_guard =
                                st.server_io_lock.lock().unwrap_or_else(|e| e.into_inner());
                            let still_same_epoch = st
                                .server
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .config_epoch
                                == epoch;
                            if still_same_epoch {
                                let _ = server_config::save_to_path(
                                    &server_config::default_config_path(),
                                    &cfg_to_save,
                                );
                                st.auth.update_server_config(cfg_to_save);
                            }
                        }

                        if let Some(lan) = st.lan.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
                        {
                            lan.set_local_device_id(snap.device_id.clone());
                        }
                        if updated_device_id {
                            if let Some(new_did) = snap.device_id.clone() {
                                let cert_matches = st
                                    .auth
                                    .cert_manager()
                                    .load_device_cert()
                                    .is_some_and(|(_, m)| m.device_id == new_did);
                                if !cert_matches && st.gate_state() == AppGateState::Authenticated {
                                    let _ = st.auth.cert_manager().clear_device_cert();
                                    let _ = app.emit("gate://status", &st.gate_status_snapshot());
                                    let app_cert = app.clone();
                                    let _ = thread::Builder::new()
                                        .name("rc-cert-sync-did".to_string())
                                        .spawn(move || {
                                            let s = app_cert.state::<AppState>();
                                            if s.auth.ensure_device_cert(&new_did).is_ok() {
                                                commands::auto_start_host_if_permitted(&s);
                                                commands::refresh_host_security_runtime(&s);
                                            }
                                            let _ = app_cert
                                                .emit("gate://status", &s.gate_status_snapshot());
                                        });
                                }
                            }
                            commands::auto_start_host_if_permitted(&st);
                            commands::refresh_host_security_runtime(&st);
                        }
                        let _ = app.emit("server://status", &snap);
                    }

                    while let Some(dev_ev) = client.try_recv_device_status() {
                        let is_current = {
                            let rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
                            rt.signal_client
                                .as_ref()
                                .is_some_and(|cur| Arc::ptr_eq(cur, &client))
                        };
                        if !is_current {
                            break;
                        }
                        commands::apply_local_device_status_policy(&st, &dev_ev);
                        let _ = app.emit("device://status-changed", &dev_ev);
                    }

                    while let Some(wol_ev) = client.try_recv_device_wol_status() {
                        let is_current = {
                            let rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
                            rt.signal_client
                                .as_ref()
                                .is_some_and(|cur| Arc::ptr_eq(cur, &client))
                        };
                        if !is_current {
                            break;
                        }
                        let _ = app.emit("device://wol-changed", &wol_ev);
                    }

                    while let Some(raw_reason) = client.try_recv_kicked_reason() {
                        let Some(zh_reason) =
                            rc_ui::signal_client::classify_auth_revocation_kick(&raw_reason)
                        else {
                            continue;
                        };
                        let is_current = {
                            let rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
                            rt.signal_client
                                .as_ref()
                                .is_some_and(|cur| Arc::ptr_eq(cur, &client))
                        };
                        if is_current {
                            let app_for_kick = app.clone();
                            let _ = thread::Builder::new()
                                .name("rc-auth-kicked".to_string())
                                .spawn(move || {
                                    commands::force_logout_from_kick(&app_for_kick, zh_reason);
                                });
                            break;
                        }
                    }

                    while let Some(incoming) = client.try_recv_incoming() {
                        let is_current = {
                            let rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
                            rt.signal_client
                                .as_ref()
                                .is_some_and(|cur| Arc::ptr_eq(cur, &client))
                        };
                        if !is_current {
                            break;
                        }
                        let rejected = {
                            let host = st.host.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(host_handle) = host.handle.as_ref() {
                                host_handle.offer_relay_incoming(incoming);
                                None
                            } else {
                                Some(incoming)
                            }
                        };
                        if let Some(incoming) = rejected {
                            let mtls_cfg = commands::build_relay_reject_mtls(&st);
                            rc_ui::relay_transport::reject_relay_incoming_fast(incoming, mtls_cfg);
                        }
                    }

                    // H2：绝不在轮询线程上 drop/join 最后一个旧 SignalClientHandle 引用。
                    if let Some(owned) = Arc::into_inner(client) {
                        let _ = thread::Builder::new()
                            .name("rc-signal-reaper".to_string())
                            .spawn(move || drop(owned));
                    }
                }
            }
        });
}

/// 在主线程将离屏 `CAMetalLayer` 挂载到 `"session"` 窗口的 `NSView`（T21 坑 2）。
///
/// 闭包持有 [`LayerLease`] 保活底层渲染器，并在挂载前后按会话代际号 `sid` 复核槽位：
/// - 挂载前会话已结束/替换：直接跳过；
/// - 挂载失败（窗口缺失/`ns_view` 失败/`attach_layer` 报错）：回滚状态机为 `None`；
/// - 挂载期间会话被并发替换/断开：立刻 `detach_layer` 避免残留子层。
#[cfg(target_os = "macos")]
pub(crate) fn attach_layer_on_main_thread(
    app: &AppHandle,
    sid: u64,
    lease: LayerLease,
    fallback_frame: Frame,
) {
    let app_for_attach = app.clone();
    let _ = app.run_on_main_thread(move || {
        let st = app_for_attach.state::<AppState>();
        let Some(effective_frame) =
            state::should_attach_on_main_thread(&st.session, sid, &lease, fallback_frame)
        else {
            return;
        };

        let attach_ok = match app_for_attach.get_webview_window("session") {
            Some(win) => {
                match rc_ui::metal_attach::attach_layer_to_window(&win, &lease, effective_frame) {
                    Ok(()) => true,
                    Err(e) => {
                        eprintln!("rc-ui: attach_layer_to_window 失败: {e}");
                        false
                    }
                }
            }
            None => false,
        };

        match state::complete_attach_on_main_thread(
            &st.session,
            sid,
            lease.clone(),
            effective_frame,
            attach_ok,
        ) {
            state::CompleteAttachOutcome::Attached {
                updated_frame: Some(latest),
            } => {
                let _ = rc_ui::metal_attach::sync_leased_layer_frame(&lease, latest);
            }
            state::CompleteAttachOutcome::Attached {
                updated_frame: None,
            }
            | state::CompleteAttachOutcome::Failed => {}
            state::CompleteAttachOutcome::MustDetachNow => {
                rc_ui::metal_attach::detach_leased_layer(&lease);
            }
        }
    });
}

/// 非 macOS 平台占位实现：回滚待挂载状态。
#[cfg(not(target_os = "macos"))]
pub(crate) fn attach_layer_on_main_thread(
    app: &AppHandle,
    sid: u64,
    lease: LayerLease,
    fallback_frame: Frame,
) {
    let st = app.state::<AppState>();
    let _ = state::complete_attach_on_main_thread(&st.session, sid, lease, fallback_frame, false);
}

/// 构建应用原生菜单栏，确保 macOS「关于 RabbitControl」面板展示真实版本号与构建号。
fn build_app_menu(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{
        AboutMetadata, Menu, PredefinedMenuItem, Submenu, HELP_SUBMENU_ID, WINDOW_SUBMENU_ID,
    };

    let pkg_info = app.package_info();
    let config = app.config();
    let ver = commands::resolve_app_version_info();
    let short_version = if ver.build.is_empty() {
        ver.bundle_version.clone()
    } else {
        ver.build.clone()
    };
    let about_metadata = AboutMetadata {
        name: Some(pkg_info.name.clone()),
        version: Some(ver.version),
        short_version: Some(short_version),
        copyright: config.bundle.copyright.clone(),
        authors: config.bundle.publisher.clone().map(|p| vec![p]),
        ..Default::default()
    };

    let window_menu = Submenu::with_id_and_items(
        app,
        WINDOW_SUBMENU_ID,
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(app, None)?,
            &PredefinedMenuItem::maximize(app, None)?,
            #[cfg(target_os = "macos")]
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::close_window(app, None)?,
        ],
    )?;

    let help_menu = Submenu::with_id_and_items(
        app,
        HELP_SUBMENU_ID,
        "Help",
        true,
        &[
            #[cfg(not(target_os = "macos"))]
            &PredefinedMenuItem::about(app, None, Some(about_metadata.clone()))?,
        ],
    )?;

    Menu::with_items(
        app,
        &[
            #[cfg(target_os = "macos")]
            &Submenu::with_items(
                app,
                pkg_info.name.clone(),
                true,
                &[
                    &PredefinedMenuItem::about(app, None, Some(about_metadata))?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::services(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::hide(app, None)?,
                    &PredefinedMenuItem::hide_others(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::quit(app, None)?,
                ],
            )?,
            #[cfg(not(any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            )))]
            &Submenu::with_items(
                app,
                "File",
                true,
                &[
                    &PredefinedMenuItem::close_window(app, None)?,
                    #[cfg(not(target_os = "macos"))]
                    &PredefinedMenuItem::quit(app, None)?,
                ],
            )?,
            &Submenu::with_items(
                app,
                "Edit",
                true,
                &[
                    &PredefinedMenuItem::undo(app, None)?,
                    &PredefinedMenuItem::redo(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::cut(app, None)?,
                    &PredefinedMenuItem::copy(app, None)?,
                    &PredefinedMenuItem::paste(app, None)?,
                    &PredefinedMenuItem::select_all(app, None)?,
                ],
            )?,
            #[cfg(target_os = "macos")]
            &Submenu::with_items(
                app,
                "View",
                true,
                &[&PredefinedMenuItem::fullscreen(app, None)?],
            )?,
            &window_menu,
            &help_menu,
        ],
    )
}

/// 应用入口：构建 Tauri 应用、启动局域网发现与信令客户端、注册全部 IPC 命令，然后进入事件循环。
///
/// 本机被控端服务默认关闭，仅在用户手动开启「允许他人远程协助」并通过系统权限预检后启动。
fn main() {
    rc_platform_macos::register_all();
    rc_ui::updater_macos::register_macos_update_hooks();

    // 单实例守护：若已有控制端 GUI 实例正在监听 `gui.sock`，直接将其主窗口唤起到前台并退出本进程。
    let gui_sock = config_paths::gui_socket_path();
    let secret_path = config_paths::ipc_secret_path();
    if gui_sock.exists()
        && matches!(
            IpcClient::call(
                &gui_sock,
                &secret_path,
                ServiceRequest::OpenMainGui,
                Duration::from_millis(300),
            ),
            Ok(ServiceResponse::Ok)
        )
    {
        return;
    }

    // 平台提供者注册完成后非阻塞预热本机硬件编解码能力缓存（绝不阻塞启动或首帧建连热路径）。
    let _ = rc_client_core::codec_selector::trigger_default_hardware_caps_probe();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .menu(build_app_menu)
        .manage(AppState::new())
        .setup(|app| {
            let state = app.state::<AppState>();
            let app_for_standalone_upd = app.handle().clone();
            rc_ui::updater_macos::register_standalone_pre_restart_hook(Arc::new(move || {
                let st = app_for_standalone_upd.state::<AppState>();
                let handle_opt = {
                    let mut host = st.host.lock().unwrap_or_else(|e| e.into_inner());
                    host.handle.take()
                };
                if let Some(mut handle) = handle_opt {
                    handle.stop_with_reason(rc_session::proto::ByeReason::SessionSwitching);
                }
            }));

            // 0) 按当前显示器尺寸自适应主窗口大小并居中（兼容 13"/14" 笔记本与 2K/4K 大屏），
            //    并启动 `gui.sock` 单实例唤醒通道供后台菜单栏与重复启动实例直接前置本窗口。
            window::adapt_main_window_on_startup(app.handle());
            if let Ok(secret) = read_or_create_ipc_secret(&config_paths::ipc_secret_path()) {
                let app_wake = app.handle().clone();
                if let Ok(gui_srv) = IpcServer::bind(
                    config_paths::gui_socket_path(),
                    secret,
                    move |req| match req {
                        ServiceRequest::Ping => ServiceResponse::Pong {
                            pid: std::process::id(),
                            session_type: ServiceSessionType::Standalone,
                            binary_sha256: None,
                        },
                        ServiceRequest::OpenMainGui => {
                            let app_ui = app_wake.clone();
                            let _ = app_wake.run_on_main_thread(move || {
                                commands::show_and_focus_main_window(&app_ui);
                            });
                            ServiceResponse::Ok
                        }
                        _ => ServiceResponse::Error {
                            message: "gui.sock 仅支持 Ping 与 OpenMainGui".to_string(),
                        },
                    },
                ) {
                    *state.gui_ipc.lock().unwrap_or_else(|e| e.into_inner()) = Some(gui_srv);
                }
            }

            // 0.5) 检测是否已安装并运行 macOS 系统后台服务：若已退出过则先唤醒，并探测 agent.sock 进入代理模式。
            if config_paths::is_service_managed() {
                service_manager::wake_services_on_launch();
                let mut proxy_ok = false;
                for _ in 0..15 {
                    if matches!(
                        IpcClient::call(
                            &config_paths::agent_socket_path(),
                            &config_paths::ipc_secret_path(),
                            ServiceRequest::Ping,
                            Duration::from_millis(250),
                        ),
                        Ok(ServiceResponse::Pong { .. })
                    ) {
                        proxy_ok = true;
                        break;
                    }
                    thread::sleep(Duration::from_millis(150));
                }
                let manual_stopped = config_paths::manual_stop_path().exists();
                state.set_service_proxy(proxy_ok || !manual_stopped);
            } else if matches!(
                IpcClient::call(
                    &config_paths::agent_socket_path(),
                    &config_paths::ipc_secret_path(),
                    ServiceRequest::Ping,
                    Duration::from_millis(200),
                ),
                Ok(ServiceResponse::Pong { .. })
            ) {
                state.set_service_proxy(true);
            }

            // 1) 启动局域网多播发现后台线程（随机非零实例 ID + 本机设备名 + 已缓存 9 位设备码）。
            let mut instance_id = OsRng.next_u64();
            if instance_id == 0 {
                instance_id = 1;
            }
            let lan_handle =
                LanDiscoveryHandle::spawn(instance_id, lan_discovery::local_hostname());
            let initial_device_id = state
                .server
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .config
                .cached_device_id
                .clone();
            lan_handle.set_local_device_id(initial_device_id);
            *state.lan.lock().unwrap_or_else(|e| e.into_inner()) = Some(lan_handle);

            // 2) 安装 macOS Dock 右键菜单处理器与独立模式顶部菜单栏状态图标（需在主线程调用）。
            let app_dock = app.handle().clone();
            let is_proxy_mode = state.is_service_proxy();
            let _ = app.handle().run_on_main_thread(move || {
                if !is_proxy_mode {
                    commands::install_standalone_status_bar(&app_dock);
                }
                let app_cb = app_dock.clone();
                let _ =
                    metal_attach::install_dock_menu_handler(Arc::new(move |action| match action {
                        DockMenuAction::OpenMain => {
                            if let Some(win) = app_cb.get_webview_window("main") {
                                let _ = win.unminimize();
                                let _ = win.show();
                                let _ = win.set_focus();
                            }
                        }
                        DockMenuAction::ConnectDevice {
                            target,
                            name,
                            requires_password,
                        } => {
                            let st = app_cb.state::<AppState>();
                            if st.gate_state() != AppGateState::Authenticated {
                                if let Some(win) = app_cb.get_webview_window("main") {
                                    let _ = win.unminimize();
                                    let _ = win.show();
                                    let _ = win.set_focus();
                                }
                                return;
                            }
                            let info = st.active_session_info();
                            if info.active && info.target.as_deref() == Some(target.as_str()) {
                                let _ = window::focus_session_window(app_cb.clone());
                                return;
                            }
                            if let Some(win) = app_cb.get_webview_window("main") {
                                let _ = win.unminimize();
                                let _ = win.show();
                                let _ = win.set_focus();
                            }
                            let _ = app_cb.emit(
                                "dock://connect-device",
                                serde_json::json!({
                                    "target": target,
                                    "name": name,
                                    "requiresPassword": requires_password,
                                }),
                            );
                        }
                    }));
            });

            // 3) 后台恢复登录态（`auth.json` + Token 静默刷新 + 设备证书续签），仅在门禁放行后启动信令、被控端与轮换维护线程。
            let app_for_restore = app.handle().clone();
            let _ = thread::Builder::new()
                .name("rc-auth-restore".to_string())
                .spawn(move || {
                    let st = app_for_restore.state::<AppState>();
                    let restored = if st.is_service_proxy() {
                        commands::restore_proxy_session_from_daemon(&st.auth)
                    } else {
                        st.auth.restore_on_startup()
                    };
                    match restored {
                        Ok(Some(sess)) if !sess.must_change_password => {
                            commands::post_auth_ready(&app_for_restore, &st);
                        }
                        Ok(_) => {
                            let _ =
                                app_for_restore.emit("gate://status", &st.gate_status_snapshot());
                        }
                        Err(_) => {
                            if st.gate_state() == AppGateState::Authenticated {
                                commands::post_auth_ready(&app_for_restore, &st);
                            } else {
                                if !st.is_service_proxy()
                                    && rc_ui::auth_client::load_auth_session(&st.auth.auth_path())
                                        .is_some()
                                    && st.auth.server_config().is_configured()
                                {
                                    commands::start_auth_maintainer(&app_for_restore, &st);
                                }
                                let _ = app_for_restore
                                    .emit("gate://status", &st.gate_status_snapshot());
                            }
                        }
                    }
                });

            // 4) 启动网络路由切换监测器：切网时在独立模式触发信令快重连与局域网刷新，并在全模式下触发活动会话快检与唤醒自动重连。
            let app_net = app.handle().clone();
            let net_monitor = rc_ui::network_monitor::NetworkMonitorHandle::spawn(move |_ev| {
                let st = app_net.state::<AppState>();
                if !st.is_service_proxy() {
                    let sig = st
                        .server
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .signal_client
                        .as_ref()
                        .cloned();
                    if let Some(client) = sig {
                        client.trigger_reconnect("network_route_changed");
                    }
                    if let Some(lan) = st.lan.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                        lan.clear_and_refresh();
                    }
                }
                {
                    let slot = st.session.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(active) = slot.as_ref().filter(|a| !a.disconnecting) {
                        active
                            .handle
                            .send(rc_ui::session_bridge::SessionCommand::ProbeNow);
                    }
                }
                st.wake_auto_reconnect();
            });
            *state
                .network_monitor
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(net_monitor);

            // 5) 启动后台轮询线程（会话更新 + 被控端更新 + 信令状态/中继分发）。
            start_background_poller(app.handle().clone());
            Ok(())
        })
        .on_window_event(|win, event| match event {
            WindowEvent::CloseRequested { api, .. } if win.label() == "main" => {
                let app_handle = win.app_handle();
                let st = app_handle.state::<AppState>();
                let close_to_tray = st.is_service_proxy()
                    || st
                        .prefs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .close_to_tray;
                if cfg!(target_os = "macos") && !st.is_quitting() && close_to_tray {
                    api.prevent_close();
                    let _ = win.hide();
                }
            }
            WindowEvent::Focused(focused) if win.label() == "session" => {
                let st = win.app_handle().state::<AppState>();
                commands::notify_session_window_focus(&st, *focused);
            }
            WindowEvent::Destroyed => match win.label() {
                "session" => {
                    let ah = win.app_handle();
                    let st = ah.state::<AppState>();
                    commands::stop_shortcut_tap_runtime(&st);
                    st.clear_terminal_error();
                    let has_open_tools = ah.get_webview_window("terminal").is_some()
                        || ah.get_webview_window("transfer").is_some()
                        || ah.get_webview_window("portmap").is_some();
                    if has_open_tools {
                        // 仍有终端/文件传输/端口映射窗口在使用同一会话：降级回纯工具无头模式（摘除 Metal 图层、静音并暂停远端视频推流），不断开底层连接。
                        let (detach_lease, emitted_info) = {
                            let mut slot = st.session.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(active) = slot.as_mut() {
                                active.handle.send(
                                    rc_client_core::session_bridge::SessionCommand::SetAudioMuted {
                                        muted: true,
                                    },
                                );
                                active.handle.send(
                                    rc_client_core::session_bridge::SessionCommand::SetMediaPaused {
                                        paused: true,
                                    },
                                );
                                active.meta.media_active = false;
                                active.meta.tools_only = true;
                                *st.last_session_params
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner()) =
                                    Some((active.watch_only, active.meta.clone()));
                                let info = active.info();
                                (active.reset_layer_for_headless(), Some(info))
                            } else {
                                drop(slot);
                                let mut saved = st
                                    .last_session_params
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner());
                                let info = if let Some((watch_only, ref mut meta)) = *saved {
                                    meta.media_active = false;
                                    meta.tools_only = true;
                                    if st.is_auto_reconnect_active() {
                                        Some(state::ActiveSessionInfo::reconnecting_from_meta(
                                            watch_only, meta,
                                        ))
                                    } else if st.is_connecting_current_epoch() {
                                        Some(state::ActiveSessionInfo::connecting_from_meta(
                                            watch_only, meta,
                                        ))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                };
                                (None, info)
                            }
                        };
                        if let Some(info) = emitted_info {
                            let _ = ah.emit("session://active-state", &info);
                        }
                        if let Some(lease) = detach_lease {
                            let _ = ah.run_on_main_thread(move || {
                                metal_attach::detach_leased_layer(&lease);
                                std::thread::spawn(move || drop(lease));
                            });
                        }
                    } else {
                        let cur_epoch = st
                            .session_open_epoch
                            .load(std::sync::atomic::Ordering::Acquire);
                        let prog_epoch = st
                            .programmatic_close_epoch
                            .load(std::sync::atomic::Ordering::Acquire);
                        let has_session = st
                            .session
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .is_some();
                        let reconnecting = st.is_auto_reconnect_active();
                        let connecting = st.is_connecting_current_epoch();
                        if has_session || reconnecting || connecting || cur_epoch != prog_epoch {
                            commands::disconnect_active_session(ah);
                        }
                    }
                }
                "terminal" => {
                    commands::detach_active_terminals(win.app_handle());
                    let ah = win.app_handle();
                    let no_session_or_tools = ah.get_webview_window("session").is_none()
                        && ah.get_webview_window("transfer").is_none()
                        && ah.get_webview_window("portmap").is_none();
                    if no_session_or_tools {
                        commands::disconnect_active_session(ah);
                    }
                }
                "transfer" | "portmap" => {
                    let ah = win.app_handle();
                    let label = win.label();
                    let no_session_or_tools = ah.get_webview_window("session").is_none()
                        && ah.get_webview_window("terminal").is_none()
                        && (label == "transfer" || ah.get_webview_window("transfer").is_none())
                        && (label == "portmap" || ah.get_webview_window("portmap").is_none());
                    if no_session_or_tools {
                        commands::disconnect_active_session(ah);
                    }
                }
                "main" => {
                    let st = win.app_handle().state::<AppState>();
                    let keep_bg = !st.is_quitting() && st.is_service_proxy();
                    if !keep_bg {
                        commands::shutdown_host_and_lan(win.app_handle());
                        metal_attach::remove_status_bar();
                    }
                }
                _ => {}
            },
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. }
                if win.label() == "session" =>
            {
                let is_fs = win.is_fullscreen().unwrap_or(false);
                let _ = win.app_handle().emit_to(
                    "session",
                    "session://fullscreen-changed",
                    serde_json::json!({ "fullscreen": is_fs }),
                );
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            window::open_tool_window,
            window::open_session_window,
            window::fit_session_window,
            window::is_session_fullscreen,
            window::set_session_fullscreen,
            window::toggle_session_fullscreen,
            window::focus_session_window,
            window::close_session_window,
            commands::list_devices,
            commands::parse_connect_target,
            commands::open_session,
            commands::set_session_relay_mode,
            commands::set_session_connection_mode,
            commands::set_session_watch_only,
            commands::set_session_mtls_mode,
            commands::get_upnp_status,
            commands::save_upnp_enabled,
            commands::get_proxy_prefs,
            commands::save_proxy_prefs,
            commands::request_device_wol,
            commands::trigger_reconnect_now,
            commands::session_command,
            commands::session_snapshot,
            commands::get_app_prefs,
            commands::get_app_info,
            commands::check_for_updates,
            commands::get_cached_release,
            commands::start_self_update,
            commands::cancel_self_update,
            commands::get_update_mirror_url,
            commands::save_update_mirror_url,
            commands::get_session_host_version,
            commands::trigger_remote_update,
            commands::cancel_remote_update,
            commands::save_stream_prefs,
            commands::get_device_snapshots,
            commands::get_active_session_info,
            commands::refresh_device_id,
            commands::get_gate_status,
            commands::probe_and_save_server,
            commands::fetch_auth_config,
            commands::auth_register,
            commands::auth_login,
            commands::auth_logout,
            commands::auth_change_password,
            commands::list_my_devices,
            commands::rename_my_device,
            commands::remove_my_device,
            commands::get_security_prefs,
            commands::save_security_prefs,
            commands::renew_device_cert,
            commands::get_general_prefs,
            commands::save_general_prefs,
            commands::pick_transfer_storage_dir,
            commands::open_transfer_storage_dir,
            commands::get_service_status,
            commands::install_system_service,
            commands::uninstall_system_service,
            commands::consume_tray_perm_check,
            commands::quit_app_completely,
            commands::get_client_lock_status,
            commands::lock_client,
            commands::unlock_client,
            commands::change_client_lock_password,
            commands::get_privacy_custom_image,
            commands::save_privacy_custom_image,
            commands::set_session_privacy_mode,
            commands::set_session_host_mute,
            commands::set_session_peripherals,
            commands::list_session_displays,
            commands::switch_session_display,
            commands::create_session_virtual_display,
            commands::remove_session_virtual_display,
            commands::reconnect_last_session,
            commands::send_input_event,
            commands::stage_rect,
            commands::validate_map_form,
            commands::copy_text,
            commands::check_permissions,
            commands::request_screen_recording_permission,
            commands::open_permission_settings,
            commands::get_host_status,
            commands::set_host_enabled,
            commands::set_host_password,
            commands::respond_approval,
            commands::disconnect_host_client,
            commands::list_lan_devices,
            commands::get_server_config,
            commands::save_server_config,
            commands::test_server_connection,
            commands::file_list_local,
            commands::file_mkdir_local,
            commands::file_delete_local,
            commands::file_rename_local,
            commands::file_list_remote,
            commands::file_mkdir_remote,
            commands::file_delete_remote,
            commands::file_rename_remote,
            commands::file_upload,
            commands::file_download,
            commands::file_cancel,
            commands::file_list_transfers,
            commands::file_clear_finished,
            commands::file_remove_transfer,
            commands::terminal_open,
            commands::terminal_list,
            commands::terminal_attach,
            commands::terminal_detach,
            commands::terminal_rename,
            commands::terminal_input,
            commands::terminal_resize,
            commands::terminal_close,
            commands::terminal_get_lock_state,
            commands::terminal_unlock,
            commands::terminal_try_auto_unlock,
            commands::terminal_clear_saved_password,
            commands::terminal_list_local_fonts,
            commands::terminal_list_remote_fonts,
            commands::terminal_fetch_remote_font,
            commands::terminal_get_remote_font_bytes,
            commands::terminal_set_font_prefs,
            commands::portmap_list,
            commands::portmap_add,
            commands::portmap_toggle,
            commands::portmap_delete,
        ])
        .build(tauri::generate_context!())
        .expect("RabbitControl 主窗口启动失败");

    app.run(|app_handle, event| match event {
        RunEvent::ExitRequested { .. } => {
            let st = app_handle.state::<AppState>();
            st.mark_quitting();
        }
        #[cfg(target_os = "macos")]
        RunEvent::Reopen {
            has_visible_windows,
            ..
        } => {
            let main_not_visible = app_handle.get_webview_window("main").is_some_and(|w| {
                !w.is_visible().unwrap_or(true) || w.is_minimized().unwrap_or(false)
            });
            if !has_visible_windows || main_not_visible {
                commands::show_and_focus_main_window(app_handle);
            }
        }
        RunEvent::Exit => {
            let st = app_handle.state::<AppState>();
            if let Some(mut srv) = st.gui_ipc.lock().unwrap_or_else(|e| e.into_inner()).take() {
                srv.stop();
            }
            commands::shutdown_host_and_lan(app_handle);
            metal_attach::remove_status_bar();
        }
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_session_update_injects_client_supports_av1_on_display_config_without_overwriting()
    {
        let upd = SessionUpdate::DisplayConfig {
            width: 1920,
            height: 1080,
            fps: 60,
            native_width: 1920,
            native_height: 1080,
            max_fps: 120,
            presets: Vec::new(),
            codec: Some("hevc".to_string()),
            supported_codecs: vec!["av1".to_string(), "hevc".to_string(), "h264".to_string()],
            client_supports_hevc: true,
            advisory: rc_ui::display_ctrl::HighRefreshAdvisory::default(),
        };

        let val_false = serialize_session_update_for_ui(&upd, false).expect("应成功序列化");
        assert_eq!(val_false["kind"], "display_config");
        assert_eq!(val_false["data"]["codec"], "hevc");
        assert_eq!(val_false["data"]["client_supports_hevc"], true);
        assert_eq!(val_false["data"]["client_supports_av1"], false);

        let val_true = serialize_session_update_for_ui(&upd, true).expect("应成功序列化");
        assert_eq!(val_true["data"]["client_supports_av1"], true);

        // 若上游未来已显式提供 `client_supports_av1`，不得覆盖原值。
        let mut upstream = serde_json::json!({
            "kind": "display_config",
            "data": {
                "codec": "hevc",
                "client_supports_av1": false
            }
        });
        inject_client_supports_av1_if_missing(&mut upstream, true);
        assert_eq!(upstream["data"]["client_supports_av1"], false);

        // 非 DisplayConfig 更新不注入该字段。
        let media = SessionUpdate::MediaState { paused: true };
        let media_val = serialize_session_update_for_ui(&media, true).expect("应成功序列化");
        assert!(media_val["data"].get("client_supports_av1").is_none());
    }
}

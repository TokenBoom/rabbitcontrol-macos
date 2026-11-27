//! RabbitControl 双会话被控端宿主 Agent（`com.rabbitcontrol.agent`）。
//!
//! 根据所在 macOS 会话自动切换行为：
//! - **LoginWindow（登录密码界面前）**：只启用本人自控 `OwnerTicket` PoP 或已显式配置固定
//!   密码的无人值守连接；不创建 Menu Bar 图标，不弹出交互式审批。
//! - **Aqua（已登录桌面）**：检测屏幕录制与辅助功能权限；权限齐备时挂载硬件采集与注入被控
//!   服务，缺权限时进入安全降级模式避免无限重启；安装顶部 Menu Bar `NSStatusItem` 图标，支持
//!   打开控制端 GUI、复制设备码、打开权限向导、切换远程协助以及无密码「整体退出」。

#![deny(unsafe_code)]

use std::collections::VecDeque;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use rand::rngs::OsRng;
use rand::RngCore;
use rc_session::auth::OwnerTicketVerifier;
use rc_session::proto::ByeReason;
use rc_ui::app_prefs::{self, AppPrefs};
use rc_ui::auth_client::{self, AuthSessionManager};
use rc_ui::cert_manager::{self, CertManager, RcOwnerTicketVerifier};
use rc_ui::config_paths;
use rc_ui::host_bridge::{
    self, HostApprovalPrompt, HostHandle, HostUpdate, SessionMtlsBundle, UiHostConfig,
};
use rc_ui::lan_discovery::{self, LanDiscoveryHandle};
use rc_ui::metal_attach::{self, StatusBarAction, StatusBarDisplay};
use rc_ui::network_monitor::NetworkMonitorHandle;
use rc_ui::permissions::{self, PermissionSnapshot};
use rc_ui::server_config::{self, ServerConfig, ServerConnectionState};
use rc_ui::service_ipc::{
    current_exe_sha256, read_or_create_ipc_secret, AgentSnapshot, IpcClient, IpcServer,
    ServiceRequest, ServiceResponse, ServiceSessionType,
};
#[cfg(target_os = "macos")]
use rc_ui::service_manager;
use rc_ui::signal_client::{
    DeviceStatusEvent, DeviceWolCapabilityEvent, SignalClientHandle, SignalClientOptions,
};
use rc_ui::ui_error::{self, codes, ErrorPlatform};

const MAX_PENDING_DEVICE_EVENTS: usize = 64;
const DEFAULT_REQUEST_CONNECT_TIMEOUT_MS: u64 = 5_000;

fn resolve_request_connect_timeout(timeout_ms: u64) -> Duration {
    let effective_ms = if timeout_ms == 0 {
        DEFAULT_REQUEST_CONNECT_TIMEOUT_MS
    } else {
        timeout_ms.clamp(1_000, 30_000)
    };
    Duration::from_millis(effective_ms)
}

fn normalize_device_id_digits(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect()
}

fn current_os_boot_id() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "kern.boottime"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
    #[cfg(target_os = "linux")]
    {
        fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

fn should_run_at_startup() -> bool {
    let stop_path = config_paths::manual_stop_path();
    if !stop_path.exists() {
        return true;
    }

    // 系统刚开机时，daemon 可能晚几十毫秒清理上一会话遗留的 manual-stop，稍作等待避免竞态误退。
    if let Some(cur_boot) = current_os_boot_id() {
        let boot_path = config_paths::boot_marker_path();
        let prev_boot = fs::read_to_string(&boot_path)
            .ok()
            .map(|s| s.trim().to_string());
        if prev_boot.as_deref() != Some(cur_boot.as_str()) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if !stop_path.exists() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    !stop_path.exists()
}

fn detect_session_type() -> ServiceSessionType {
    if let Ok(forced) = std::env::var("RC_FORCE_SESSION_TYPE") {
        match forced.trim().to_ascii_lowercase().as_str() {
            "login_window" | "loginwindow" => return ServiceSessionType::LoginWindow,
            "aqua" => return ServiceSessionType::Aqua,
            "standalone" => return ServiceSessionType::Standalone,
            _ => {}
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("id").arg("-u").output() {
            if out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "0" {
                return ServiceSessionType::LoginWindow;
            }
        }
        ServiceSessionType::Aqua
    }
    #[cfg(not(target_os = "macos"))]
    {
        ServiceSessionType::Standalone
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SignalIdentityKey {
    authenticated: bool,
    enabled: bool,
    server_host: String,
    signal_port: u16,
    relay_port: u16,
    use_https: bool,
    ca_cert_pem: Option<String>,
    ca_fingerprint_sha256: Option<String>,
    device_key: String,
    user_id: Option<String>,
    device_pubkey_hex: String,
}

fn fetch_access_token(
    auth_slot: &RwLock<Arc<AuthSessionManager>>,
    failed_token: Option<&str>,
) -> Result<String, String> {
    let auth = auth_slot.read().unwrap_or_else(|e| e.into_inner()).clone();
    let daemon_sock = config_paths::daemon_socket_path();
    let secret_path = config_paths::ipc_secret_path();
    if daemon_sock.exists() {
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
                auth.apply_external_session(*sess);
                return Ok(token);
            }
            Ok(ServiceResponse::ReloginRequired { message }) => {
                let _ = auth.reload_from_disk();
                return Err(message);
            }
            Ok(ServiceResponse::Error { message }) => {
                return Err(message);
            }
            Ok(_) | Err(_) => {}
        }
    }
    match failed_token {
        Some(stale) => auth
            .refresh_after_unauthorized(stale)
            .map_err(|e| format!("{e:?}")),
        None => auth
            .ensure_valid_access_token()
            .map_err(|e| format!("{e:?}")),
    }
}

fn should_restart_signal(
    authenticated: bool,
    prev_key: &SignalIdentityKey,
    next_key: &SignalIdentityKey,
    signal_state: Option<ServerConnectionState>,
) -> bool {
    !authenticated
        || prev_key != next_key
        || matches!(signal_state, None | Some(ServerConnectionState::Error))
}

#[derive(Default)]
struct RetiredHandles {
    host: Option<HostHandle>,
    signal: Option<Arc<SignalClientHandle>>,
    lan: Option<LanDiscoveryHandle>,
}

impl RetiredHandles {
    fn is_empty(&self) -> bool {
        self.host.is_none() && self.signal.is_none() && self.lan.is_none()
    }

    fn stop_all(mut self, host_bye_reason: ByeReason) {
        if let Some(mut h) = self.host.take() {
            h.stop_with_reason(host_bye_reason);
        }
        if let Some(s) = self.signal.take() {
            s.request_stop();
            if let Some(mut owned) = Arc::into_inner(s) {
                owned.stop();
            }
        }
        if let Some(mut l) = self.lan.take() {
            l.stop();
        }
    }
}

struct ReloadedContext {
    server_cfg: ServerConfig,
    prefs: AppPrefs,
    auth: Option<Arc<AuthSessionManager>>,
    local_wol_mac: Option<String>,
}

fn probe_local_wol_mac_if_enabled(allow_wol: bool) -> Option<String> {
    if allow_wol {
        rc_ui::platform::primary_mac_address()
    } else {
        None
    }
}

fn load_reloaded_context() -> ReloadedContext {
    let server_cfg = server_config::load_from_path(&server_config::default_config_path());
    let prefs = app_prefs::load_from_path(&app_prefs::default_prefs_path());
    let local_wol_mac = probe_local_wol_mac_if_enabled(prefs.allow_wol);
    let certs_dir = cert_manager::default_certs_dir();
    let auth = CertManager::load_or_init(certs_dir).ok().map(|cert_mgr| {
        let mgr = Arc::new(AuthSessionManager::new(
            server_cfg.clone(),
            auth_client::default_auth_path(),
            Arc::new(cert_mgr),
        ));
        let _ = mgr.restore_on_startup();
        mgr
    });
    ReloadedContext {
        server_cfg,
        prefs,
        auth,
        local_wol_mac,
    }
}

fn format_display_addr(actual: SocketAddr) -> String {
    if actual.ip().is_unspecified() {
        let ip = lan_discovery::detect_local_lan_ipv4().unwrap_or(Ipv4Addr::LOCALHOST);
        SocketAddr::new(IpAddr::V4(ip), actual.port()).to_string()
    } else {
        actual.to_string()
    }
}

fn launch_main_gui() {
    #[cfg(target_os = "macos")]
    {
        use std::sync::OnceLock;

        // 防抖：短时间内连续点击菜单或 Dock Reopen 时避免重复触发唤醒/拉起线程。
        static LAST_SPAWN: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
        let guard_slot = LAST_SPAWN.get_or_init(|| Mutex::new(None));
        {
            let mut last = guard_slot.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|t| t.elapsed() < Duration::from_millis(600)) {
                return;
            }
            *last = Some(Instant::now());
        }

        // 卸载到短生命周期后台线程，避免 350ms 同步 IPC 阻塞 AppKit 主线程并确保 `open` 子进程被 `.status()` 回收防僵尸进程。
        let _ = std::thread::Builder::new()
            .name("rc-agent-open-gui".to_string())
            .spawn(|| {
                let gui_sock = config_paths::gui_socket_path();
                let secret_path = config_paths::ipc_secret_path();
                if gui_sock.exists()
                    && matches!(
                        IpcClient::call(
                            &gui_sock,
                            &secret_path,
                            ServiceRequest::OpenMainGui,
                            Duration::from_millis(350),
                        ),
                        Ok(ServiceResponse::Ok)
                    )
                {
                    return;
                }

                if let Some(bundle) = service_manager::locate_app_bundle() {
                    // 必须带 `-n`：否则 LaunchServices 会将同 Bundle 下的本 `rc-host-agent` 视为已运行实例，
                    // 只发 Reopen 而不真正启动 `Contents/MacOS/RabbitControl` 主界面进程。
                    let _ = std::process::Command::new("open")
                        .arg("-n")
                        .arg("-a")
                        .arg(&bundle)
                        .status();
                    return;
                }
                if let Ok(exe) = std::env::current_exe() {
                    if let Some(dir) = exe.parent() {
                        for candidate in ["RabbitControl", "rc-ui"] {
                            let bin = dir.join(candidate);
                            if bin.is_file() {
                                let _ = std::process::Command::new(bin).spawn();
                                return;
                            }
                        }
                    }
                }
            });
    }
}

fn resolve_host_password_mode(
    session_type: ServiceSessionType,
    custom_host_password: Option<String>,
    prev_custom: bool,
    current_pw: &str,
) -> (String, bool) {
    match custom_host_password {
        // 空串明确表示「无密码（仅机主交互审批）」；LoginWindow 会在 effective_allow_remote_assist 中拒绝无密码协助。
        Some(pw) if pw.trim().is_empty() => (String::new(), true),
        Some(pw) => (pw, true),
        None => {
            if session_type == ServiceSessionType::LoginWindow {
                (String::new(), false)
            } else if !prev_custom && !current_pw.is_empty() {
                (current_pw.to_string(), false)
            } else {
                (host_bridge::generate_temp_code(), false)
            }
        }
    }
}

struct AgentInner {
    session_type: ServiceSessionType,
    ipc_secret: String,
    server_cfg: ServerConfig,
    prefs: AppPrefs,
    /// 使用 RwLock 包装以便保活信令连接时，token_provider 闭包能动态读取刷新后的最新 AuthSessionManager。
    auth: Arc<RwLock<Arc<AuthSessionManager>>>,
    signal: Option<Arc<SignalClientHandle>>,
    lan: Option<LanDiscoveryHandle>,
    host: Option<HostHandle>,
    allow_owner_control: bool,
    pending_device_events: VecDeque<DeviceStatusEvent>,
    pending_device_wol_events: VecDeque<DeviceWolCapabilityEvent>,
    password: String,
    custom_password: bool,
    listen_addr: Option<String>,
    display_addr: Option<String>,
    backend: String,
    connected_peer: Option<String>,
    pending_approval: Option<HostApprovalPrompt>,
    device_id: Option<String>,
    signal_online: bool,
    permissions: PermissionSnapshot,
    degraded_permissions: bool,
    last_offline_disk_check: std::time::Instant,
    #[cfg(test)]
    signal_sync_log: Option<Arc<Mutex<Vec<Option<usize>>>>>,
}

impl AgentInner {
    fn new(session_type: ServiceSessionType, ipc_secret: String) -> Result<Self, String> {
        let cfg = server_config::load_from_path(&server_config::default_config_path());
        let prefs = app_prefs::load_from_path(&app_prefs::default_prefs_path());
        rc_ui::proxy::set_global_proxy_config(prefs.proxy.clone());
        let certs_dir = cert_manager::default_certs_dir();
        let cert_mgr = Arc::new(CertManager::load_or_init(certs_dir)?);
        let auth = Arc::new(AuthSessionManager::new(
            cfg.clone(),
            auth_client::default_auth_path(),
            cert_mgr,
        ));
        let _ = auth.restore_on_startup();

        let (password, custom_password) =
            resolve_host_password_mode(session_type, prefs.custom_host_password.clone(), false, "");
        let device_id = cfg.cached_device_id.clone();
        let permissions = permissions::check_permissions();

        Ok(Self {
            session_type,
            ipc_secret,
            server_cfg: cfg,
            prefs,
            auth: Arc::new(RwLock::new(auth)),
            signal: None,
            lan: None,
            host: None,
            allow_owner_control: true,
            pending_device_events: VecDeque::new(),
            pending_device_wol_events: VecDeque::new(),
            password,
            custom_password,
            listen_addr: None,
            display_addr: None,
            backend: String::new(),
            connected_peer: None,
            pending_approval: None,
            device_id,
            signal_online: false,
            permissions,
            degraded_permissions: false,
            last_offline_disk_check: std::time::Instant::now(),
            #[cfg(test)]
            signal_sync_log: None,
        })
    }

    fn effective_allow_remote_assist(&self) -> bool {
        if self.session_type == ServiceSessionType::LoginWindow {
            // 登录窗前严格禁止无固定密码或交互弹窗的外来远程协助。
            self.prefs.allow_remote_assist && self.custom_password && !self.password.is_empty()
        } else {
            self.prefs.allow_remote_assist
        }
    }

    fn current_auth(&self) -> Arc<AuthSessionManager> {
        self.auth.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn replace_auth(&self, next: Arc<AuthSessionManager>) {
        *self.auth.write().unwrap_or_else(|e| e.into_inner()) = next;
    }

    fn is_authenticated(&self) -> bool {
        self.server_cfg.is_configured()
            && self
                .current_auth()
                .current_session()
                .is_some_and(|s| !s.must_change_password)
    }

    fn signal_allow_remote_assist(&self) -> bool {
        self.host.is_some() && self.effective_allow_remote_assist()
    }

    fn sync_signal_allow_assist(&self) {
        if let Some(ref sig) = self.signal {
            let _ = sig.set_allow_remote_assist(self.signal_allow_remote_assist());
        }
    }

    fn sync_signal_wol(&self, local_mac: Option<String>) {
        if let Some(ref sig) = self.signal {
            if self.prefs.allow_wol {
                if let Some(mac) = local_mac {
                    let _ = sig.set_mac_addr(Some(mac));
                }
                let _ = sig.set_wol_enabled(true);
            } else {
                let _ = sig.set_mac_addr(None);
                let _ = sig.set_wol_enabled(false);
            }
        }
    }

    fn build_owner_verifier(&self) -> Option<Arc<dyn OwnerTicketVerifier>> {
        if !self.is_authenticated() {
            return None;
        }
        let auth = self.current_auth();
        let pubkey_hex = self
            .server_cfg
            .server_auth_pubkey_hex
            .clone()
            .or_else(|| auth.server_config().server_auth_pubkey_hex)?;
        let user_id = auth.current_session()?.user_id;
        let did = self
            .device_id
            .clone()
            .or_else(|| self.server_cfg.cached_device_id.clone())?;
        RcOwnerTicketVerifier::new(&pubkey_hex, user_id, did)
            .ok()
            .map(|v| Arc::new(v) as Arc<dyn OwnerTicketVerifier>)
    }

    fn build_mtls_bundle(&self) -> Option<SessionMtlsBundle> {
        if !self.is_authenticated() {
            return None;
        }
        let auth = self.current_auth();
        let ca_cert_pem = self
            .server_cfg
            .ca_cert_pem
            .clone()
            .or_else(|| auth.server_config().ca_cert_pem)?;
        let user_id = auth.current_session()?.user_id;
        let cert_mgr = auth.cert_manager();
        let (cert_pem, meta) = cert_mgr.load_device_cert()?;
        if !meta.user_id.eq_ignore_ascii_case(user_id.trim()) {
            return None;
        }
        if let Some(ref did) = self.device_id {
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
            expected_peer_device_id: None,
            expected_peer_user_id: None,
        })
    }

    fn start_lan(&mut self) {
        if let Some(mut old) = self.lan.take() {
            old.stop();
        }
        let mut instance_id = OsRng.next_u64();
        if instance_id == 0 {
            instance_id = 1;
        }
        let handle = LanDiscoveryHandle::spawn(instance_id, lan_discovery::local_hostname());
        handle.set_local_device_id(self.device_id.clone());
        self.lan = Some(handle);
    }

    fn sync_lan_advertising(&self) {
        let Some(ref lan) = self.lan else {
            return;
        };
        let target = if let Some(ref host) = self.host {
            let allow = self.effective_allow_remote_assist();
            let owner_ready = self.build_owner_verifier().is_some();
            if allow || owner_ready {
                let requires_password = allow && !self.password.is_empty();
                Some((host.listen_addr().port(), requires_password))
            } else {
                None
            }
        } else {
            None
        };
        lan.set_advertising(target);
    }

    /// 将当前 `self.signal` 实时同步至运行中的 `HostHandle`（支持后启动、替换或清空信令句柄）。
    fn sync_host_signal_client(&self) {
        if let Some(ref host) = self.host {
            host.set_signal_client(self.signal.clone());
            #[cfg(test)]
            if let Some(ref log) = self.signal_sync_log {
                log.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(self.signal.as_ref().map(|s| Arc::as_ptr(s) as usize));
            }
        }
    }

    #[must_use]
    fn start_signal(&mut self, local_mac: Option<String>) -> Option<Arc<SignalClientHandle>> {
        let old = self.signal.take();
        if let Some(ref o) = old {
            o.request_stop();
        }
        let authenticated = self.is_authenticated();
        if !authenticated
            || !self.server_cfg.enabled
            || self.server_cfg.server_host.trim().is_empty()
            || self.server_cfg.signal_port == 0
        {
            self.signal_online = false;
            self.sync_host_signal_client();
            return old;
        }

        let auth_slot_provider = Arc::clone(&self.auth);
        let token_provider: rc_ui::signal_client::TokenProvider =
            Arc::new(move || fetch_access_token(&auth_slot_provider, None));
        let auth_slot_refresher = Arc::clone(&self.auth);
        let token_refresher: rc_ui::signal_client::TokenRefresher =
            Arc::new(move |stale| fetch_access_token(&auth_slot_refresher, Some(stale)));
        let options = SignalClientOptions {
            token_provider: Some(token_provider),
            token_refresher: Some(token_refresher),
            device_pubkey: Some(self.current_auth().cert_manager().device_pubkey_hex()),
            allow_remote_assist: self.signal_allow_remote_assist(),
            allow_remote_control: Some(self.allow_owner_control),
            dynamic_lan_discovery: true,
            upnp_enabled: self.prefs.upnp_enabled,
        };
        let mut lan_addrs = Vec::new();
        if let Some(ip) = lan_discovery::detect_local_lan_ipv4() {
            lan_addrs.push(ip.to_string());
        }
        let handle = SignalClientHandle::spawn_with_options(
            self.server_cfg.clone(),
            lan_discovery::local_hostname(),
            lan_addrs,
            options,
            |_| {},
        );
        if self.prefs.allow_wol {
            if let Some(mac) = local_mac {
                let _ = handle.set_mac_addr(Some(mac));
            }
            let _ = handle.set_wol_enabled(true);
        } else {
            let _ = handle.set_mac_addr(None);
            let _ = handle.set_wol_enabled(false);
        }
        let snap = handle.status();
        self.signal_online = snap.state == ServerConnectionState::Connected;
        if let Some(did) = snap.device_id {
            self.device_id = Some(did.clone());
            if let Some(ref lan) = self.lan {
                lan.set_local_device_id(Some(did));
            }
        }
        self.signal = Some(Arc::new(handle));
        self.sync_host_signal_client();
        old
    }

    #[must_use]
    fn ensure_host_state(&mut self) -> Option<HostHandle> {
        self.try_ensure_host_state().ok().flatten()
    }

    fn try_ensure_host_state(&mut self) -> Result<Option<HostHandle>, String> {
        self.permissions = permissions::check_permissions();
        let owner_verifier = self.build_owner_verifier();
        let allow_remote_assist = self.effective_allow_remote_assist();
        let _ = (owner_verifier.is_some(), allow_remote_assist);
        let want_host = self.is_authenticated();

        if !want_host {
            let old = self.host.take();
            self.listen_addr = None;
            self.display_addr = None;
            self.backend.clear();
            self.connected_peer = None;
            self.pending_approval = None;
            self.degraded_permissions = false;
            self.sync_lan_advertising();
            return Ok(old);
        }

        if self.session_type == ServiceSessionType::Aqua
            && permissions::ensure_host_ready(&self.permissions).is_err()
        {
            let old = self.host.take();
            self.listen_addr = None;
            self.display_addr = None;
            self.backend = "degraded".to_string();
            self.connected_peer = None;
            self.pending_approval = None;
            self.degraded_permissions = true;
            self.sync_lan_advertising();
            return Ok(old);
        }

        self.degraded_permissions = false;
        let mtls_bundle = self.build_mtls_bundle();
        let accept_mtls_lan = self.prefs.default_encrypt_connection;
        let file_transfer_dir = Some(self.prefs.resolved_file_transfer_dir());

        if let Some(ref host) = self.host {
            if !allow_remote_assist {
                if let Some(prompt) = self.pending_approval.take() {
                    let _ = host.respond_approval(prompt.generation, false);
                }
            }
            host.set_allow_owner_control(self.allow_owner_control);
            host.set_allow_remote_assist(allow_remote_assist);
            host.set_allow_assist_file_transfer(self.prefs.allow_assist_file_transfer);
            host.set_owner_verifier(owner_verifier);
            host.set_mtls_config(mtls_bundle, accept_mtls_lan);
            host.set_file_transfer_dir(file_transfer_dir);
            host.set_signal_client(self.signal.clone());
            self.sync_lan_advertising();
            return Ok(None);
        }

        let listen_addr = SocketAddr::from(([0, 0, 0, 0], host_bridge::DEFAULT_HOST_PORT));
        let require_approval = self.session_type != ServiceSessionType::LoginWindow;
        let pw_opt = if self.password.is_empty() {
            None
        } else {
            Some(self.password.clone())
        };
        let cfg = UiHostConfig {
            listen_addr,
            password: pw_opt,
            approval_timeout_us: rc_session::auth::DEFAULT_AUTH_TIMEOUT_US,
            prefer_hardware: true,
            require_approval,
            auto_accept_with_password: true,
            auto_rotate_password: self.session_type != ServiceSessionType::LoginWindow
                && !self.custom_password,
            clipboard_enabled: self.session_type != ServiceSessionType::LoginWindow,
            audio_enabled: self.session_type != ServiceSessionType::LoginWindow,
            allow_owner_control: self.allow_owner_control,
            allow_remote_assist,
            owner_verifier,
            mtls_config: mtls_bundle,
            accept_mtls_lan,
            forbid_passwordless_auto_approve: self.session_type == ServiceSessionType::LoginWindow,
            file_transfer_dir,
            allow_assist_file_transfer: self.prefs.allow_assist_file_transfer,
            signal_client: self.signal.clone(),
        };

        match HostHandle::spawn(cfg) {
            Ok(handle) => {
                let actual = handle.listen_addr();
                self.listen_addr = Some(actual.to_string());
                self.display_addr = Some(format_display_addr(actual));
                self.backend = if self.session_type == ServiceSessionType::LoginWindow {
                    "loginwindow".to_string()
                } else {
                    "sck+videotoolbox".to_string()
                };
                self.host = Some(handle);
                self.sync_lan_advertising();
                Ok(None)
            }
            Err(err) => {
                eprintln!("[rc-host-agent] 启动 HostHandle 失败: {err}");
                self.backend = "error".to_string();
                self.sync_lan_advertising();
                Err(err)
            }
        }
    }

    fn signal_identity_key(&self) -> SignalIdentityKey {
        let auth = self.current_auth();
        SignalIdentityKey {
            authenticated: self.is_authenticated(),
            enabled: self.server_cfg.enabled,
            server_host: self.server_cfg.server_host.clone(),
            signal_port: self.server_cfg.signal_port,
            relay_port: self.server_cfg.relay_port,
            use_https: self.server_cfg.use_https,
            ca_cert_pem: self.server_cfg.ca_cert_pem.clone(),
            ca_fingerprint_sha256: self.server_cfg.ca_fingerprint_sha256.clone(),
            device_key: self.server_cfg.device_key.clone(),
            user_id: auth.current_session().map(|s| s.user_id),
            device_pubkey_hex: auth.cert_manager().device_pubkey_hex(),
        }
    }

    #[must_use]
    fn apply_reloaded_context(&mut self, ctx: ReloadedContext) -> RetiredHandles {
        let prev_sig_key = self.signal_identity_key();
        let prev_proxy = self.prefs.proxy.clone();
        let prev_allow_wol = self.prefs.allow_wol;
        self.server_cfg = ctx.server_cfg;
        self.prefs = ctx.prefs;
        rc_ui::proxy::set_global_proxy_config(self.prefs.proxy.clone());
        if let Some(auth) = ctx.auth {
            self.replace_auth(auth);
        } else {
            self.current_auth()
                .update_server_config(self.server_cfg.clone());
        }
        let (pw, custom) = resolve_host_password_mode(
            self.session_type,
            self.prefs.custom_host_password.clone(),
            self.custom_password,
            &self.password,
        );
        self.password = pw;
        self.custom_password = custom;
        self.device_id = self.server_cfg.cached_device_id.clone();

        let lan = if !self.is_authenticated() {
            self.lan.take()
        } else {
            if self.lan.is_none() {
                self.start_lan();
            } else if let Some(ref l) = self.lan {
                l.set_local_device_id(self.device_id.clone());
            }
            None
        };

        // 先收敛 host 状态与 allow_remote_assist（若 LoginWindow 下不再具备固定密码则先停机或禁用协助），
        // 再同步 password_mode 与 SignalClient 广播，消除暴露空密码或权限降级期间误宣告可协助的窗口。
        let host = self.ensure_host_state();
        if let Some(ref running_host) = self.host {
            let pw_opt = if self.password.is_empty() {
                None
            } else {
                Some(self.password.clone())
            };
            running_host.set_password_mode(
                pw_opt,
                self.session_type != ServiceSessionType::LoginWindow && !self.custom_password,
            );
        }
        let next_sig_key = self.signal_identity_key();
        let signal_state = self.signal.as_ref().map(|s| s.status().state);
        let signal = if should_restart_signal(
            self.is_authenticated(),
            &prev_sig_key,
            &next_sig_key,
            signal_state,
        ) {
            self.start_signal(ctx.local_wol_mac)
        } else {
            self.sync_signal_allow_assist();
            if prev_allow_wol != self.prefs.allow_wol {
                self.sync_signal_wol(ctx.local_wol_mac);
            }
            if let Some(ref sig) = self.signal {
                sig.set_upnp_enabled(self.prefs.upnp_enabled);
                if prev_proxy != self.prefs.proxy {
                    sig.trigger_reconnect("proxy_config_changed");
                }
            }
            None
        };
        self.sync_lan_advertising();
        RetiredHandles { host, signal, lan }
    }

    #[must_use]
    fn reload_prefs(
        &mut self,
        next_prefs: AppPrefs,
        local_wol_mac: Option<String>,
    ) -> Option<HostHandle> {
        let prev_proxy = self.prefs.proxy.clone();
        let prev_allow_wol = self.prefs.allow_wol;
        self.prefs = next_prefs;
        rc_ui::proxy::set_global_proxy_config(self.prefs.proxy.clone());
        let (pw, custom) = resolve_host_password_mode(
            self.session_type,
            self.prefs.custom_host_password.clone(),
            self.custom_password,
            &self.password,
        );
        self.password = pw;
        self.custom_password = custom;
        let retired = self.ensure_host_state();
        self.sync_signal_allow_assist();
        if prev_allow_wol != self.prefs.allow_wol {
            self.sync_signal_wol(local_wol_mac);
        }
        if let Some(ref sig) = self.signal {
            sig.set_upnp_enabled(self.prefs.upnp_enabled);
            if prev_proxy != self.prefs.proxy {
                sig.trigger_reconnect("proxy_config_changed");
            }
        }
        if let Some(ref running_host) = self.host {
            let pw_opt = if self.password.is_empty() {
                None
            } else {
                Some(self.password.clone())
            };
            running_host.set_password_mode(
                pw_opt,
                self.session_type != ServiceSessionType::LoginWindow && !self.custom_password,
            );
            running_host.set_file_transfer_dir(Some(self.prefs.resolved_file_transfer_dir()));
            running_host.set_allow_assist_file_transfer(self.prefs.allow_assist_file_transfer);
        }
        retired
    }

    fn snapshot(&self) -> AgentSnapshot {
        AgentSnapshot {
            enabled: self.host.is_some() && self.effective_allow_remote_assist(),
            allow_assist_file_transfer: self.prefs.allow_assist_file_transfer,
            host_running: self.host.is_some(),
            listen_addr: self.listen_addr.clone(),
            display_addr: self.display_addr.clone(),
            password: self.password.clone(),
            custom_password: self.custom_password,
            backend: self.backend.clone(),
            connected_peer: self.connected_peer.clone(),
            pending_approval: self.pending_approval.clone(),
            device_id: self.device_id.clone(),
            signal_online: self.signal_online,
            session_type: self.session_type,
            degraded_permissions: self.degraded_permissions,
            permissions: self.permissions.clone(),
            upnp_status: self.signal.as_ref().map(|s| s.upnp_status()),
        }
    }

    fn status_bar_display(&self) -> StatusBarDisplay {
        let session_label = match self.session_type {
            ServiceSessionType::LoginWindow => "登录界面托管".to_string(),
            ServiceSessionType::Aqua => "后台服务运行中".to_string(),
            ServiceSessionType::Standalone => "独立后台模式".to_string(),
        };
        StatusBarDisplay {
            device_id: self.device_id.clone(),
            signal_online: self.signal_online,
            connected_peer: self.connected_peer.clone(),
            missing_permissions: self.degraded_permissions
                || (self.permissions.platform_supported && !self.permissions.host_ready),
            session_label,
        }
    }

    fn apply_device_status_event(&mut self, ev: &DeviceStatusEvent) -> bool {
        let Some(ref local_did) = self.device_id else {
            return false;
        };
        let norm_local = normalize_device_id_digits(local_did);
        let norm_ev = normalize_device_id_digits(&ev.device_id);
        if norm_local.is_empty() || norm_local != norm_ev {
            return false;
        }
        let changed = self.allow_owner_control != ev.allow_remote_control;
        self.allow_owner_control = ev.allow_remote_control;
        if let Some(ref host) = self.host {
            host.set_allow_owner_control(ev.allow_remote_control);
        }
        changed
    }

    fn enqueue_device_status_event(&mut self, ev: DeviceStatusEvent) {
        if self.pending_device_events.len() >= MAX_PENDING_DEVICE_EVENTS {
            self.pending_device_events.pop_front();
        }
        self.pending_device_events.push_back(ev);
    }

    fn enqueue_device_wol_event(&mut self, ev: DeviceWolCapabilityEvent) {
        if self.pending_device_wol_events.len() >= MAX_PENDING_DEVICE_EVENTS {
            self.pending_device_wol_events.pop_front();
        }
        self.pending_device_wol_events.push_back(ev);
    }

    fn pump_events(&mut self) -> (bool, RetiredHandles) {
        let mut changed = false;
        let mut retired_host = None;
        let mut revoked_reason: Option<String> = None;

        if let Some(sig) = self.signal.clone() {
            while let Some(raw_reason) = sig.try_recv_kicked_reason() {
                if let Some(zh) = rc_ui::signal_client::classify_auth_revocation_kick(&raw_reason) {
                    revoked_reason = Some(zh);
                    break;
                }
            }

            while let Some(status) = sig.try_recv_status() {
                if revoked_reason.is_some() {
                    break;
                }
                let next_online = status.state == ServerConnectionState::Connected;
                if next_online != self.signal_online {
                    self.signal_online = next_online;
                    changed = true;
                }
                if let Some(did) = status.device_id {
                    if self.device_id.as_deref() != Some(did.as_str()) {
                        self.device_id = Some(did.clone());
                        if let Some(ref lan) = self.lan {
                            lan.set_local_device_id(Some(did));
                        }
                        changed = true;
                        // device_id 首次就绪后立即同步 host 状态，避免 allow_remote_assist=false 时自控监听长期未启动。
                        if let Some(retired) = self.ensure_host_state() {
                            if let Some(mut prev) = retired_host.replace(retired) {
                                prev.stop();
                            }
                        }
                        self.sync_signal_allow_assist();
                    }
                }
            }

            while let Some(dev_ev) = sig.try_recv_device_status() {
                if revoked_reason.is_some() {
                    break;
                }
                if self.apply_device_status_event(&dev_ev) {
                    changed = true;
                }
                self.enqueue_device_status_event(dev_ev);
            }

            while let Some(wol_ev) = sig.try_recv_device_wol_status() {
                if revoked_reason.is_some() {
                    break;
                }
                self.enqueue_device_wol_event(wol_ev);
            }

            let reject_mtls = if revoked_reason.is_some() || self.host.is_none() {
                self.build_mtls_bundle()
                    .map(|b| b.to_relay_mtls_config(None))
            } else {
                None
            };
            while let Some(incoming) = sig.try_recv_incoming() {
                if revoked_reason.is_none() {
                    if let Some(ref host) = self.host {
                        host.offer_relay_incoming(incoming);
                        continue;
                    }
                }
                rc_ui::relay_transport::reject_relay_incoming_fast(incoming, reject_mtls.clone());
            }
        }

        if let Some(reason) = revoked_reason {
            eprintln!("[rc-host-agent] 收到服务端强制下线通知 ({reason})，清空本地凭据");
            self.current_auth().wipe_local_credentials();
            let secret = self.ipc_secret.clone();
            let _ = std::thread::Builder::new()
                .name("rc-agent-daemon-kick".to_string())
                .spawn(move || {
                    let _ = IpcClient::call_with_secret(
                        &config_paths::daemon_socket_path(),
                        &secret,
                        ServiceRequest::RefreshAuthAndCerts,
                        Duration::from_secs(2),
                    );
                });
        }

        if !self.is_authenticated()
            && (self.host.is_some() || self.signal.is_some() || self.lan.is_some())
        {
            if let Some(mut old_retired) = retired_host.take() {
                old_retired.stop();
            }
            let retired_all = self.take_all_handles();
            self.signal_online = false;
            self.device_id = None;
            self.allow_owner_control = true;
            self.pending_device_events.clear();
            self.pending_device_wol_events.clear();
            self.listen_addr = None;
            self.display_addr = None;
            self.backend.clear();
            self.connected_peer = None;
            self.pending_approval = None;
            self.degraded_permissions = false;
            return (true, retired_all);
        }

        // 离线启动恢复兜底：若开机时无网且本地 access token 已过期，rc-daemon 恢复网络后会在后台静默刷新写入 auth.json；
        // 若 RefreshAuthAndCerts IPC 恰好赶在 agent.sock 就绪前完成，此处每 5s 仅做零网络开销的本地磁盘检测即可自动拉起托管。
        if !self.is_authenticated()
            && self.server_cfg.is_configured()
            && self.host.is_none()
            && self.signal.is_none()
            && self.lan.is_none()
            && self.last_offline_disk_check.elapsed() >= Duration::from_secs(5)
        {
            self.last_offline_disk_check = std::time::Instant::now();
            let now_sec = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let auth = self.current_auth();
            if let Some(sess) = auth_client::load_auth_session(&auth.auth_path()) {
                if !sess.must_change_password && sess.access_expires_at_unix_sec > now_sec + 90 {
                    self.server_cfg =
                        server_config::load_from_path(&server_config::default_config_path());
                    self.prefs = app_prefs::load_from_path(&app_prefs::default_prefs_path());
                    rc_ui::proxy::set_global_proxy_config(self.prefs.proxy.clone());
                    auth.update_server_config(self.server_cfg.clone());
                    auth.apply_external_session(sess);
                    let (pw, custom) = resolve_host_password_mode(
                        self.session_type,
                        self.prefs.custom_host_password.clone(),
                        self.custom_password,
                        &self.password,
                    );
                    self.password = pw;
                    self.custom_password = custom;
                    self.device_id = self.server_cfg.cached_device_id.clone();
                    if self.is_authenticated() {
                        if self.lan.is_none() {
                            self.start_lan();
                        }
                        let retired_all = RetiredHandles {
                            host: self.ensure_host_state(),
                            signal: self.start_signal(None),
                            lan: None,
                        };
                        if self.prefs.allow_wol {
                            if let Some(sig) = self.signal.clone() {
                                let _ = std::thread::Builder::new()
                                    .name("rc-agent-wol-mac".to_string())
                                    .spawn(move || {
                                        if let Some(mac) = rc_ui::platform::primary_mac_address() {
                                            let _ = sig.set_mac_addr(Some(mac));
                                        }
                                        let _ = sig.set_wol_enabled(true);
                                    });
                            }
                        }
                        self.sync_lan_advertising();
                        return (true, retired_all);
                    }
                }
            }
        }

        let allow_assist = self.effective_allow_remote_assist();
        let mut host_stopped = false;
        let mut lan_dirty = false;
        if let Some(ref host) = self.host {
            while let Some(upd) = host.try_recv_update() {
                changed = true;
                match upd {
                    HostUpdate::Listening {
                        listen_addr,
                        display_addr,
                        backend,
                    } => {
                        self.listen_addr = Some(listen_addr);
                        self.display_addr = Some(display_addr);
                        self.backend = backend;
                        lan_dirty = true;
                    }
                    HostUpdate::ApprovalRequested(prompt) => {
                        if !allow_assist {
                            let _ = host.respond_approval(prompt.generation, false);
                        } else {
                            self.pending_approval = Some(prompt);
                        }
                    }
                    HostUpdate::ApprovalCancelled { generation } => {
                        if self
                            .pending_approval
                            .as_ref()
                            .is_some_and(|p| p.generation == generation)
                        {
                            self.pending_approval = None;
                        }
                    }
                    HostUpdate::ClientConnected { remote_addr } => {
                        self.pending_approval = None;
                        self.connected_peer = Some(remote_addr);
                    }
                    HostUpdate::OwnerConnected {
                        caller_device_id, ..
                    } => {
                        self.pending_approval = None;
                        self.connected_peer = Some(format!("自控设备 {caller_device_id}"));
                    }
                    HostUpdate::ClientDisconnected { .. } => {
                        self.connected_peer = None;
                    }
                    HostUpdate::PasswordRotated { password } => {
                        if !self.custom_password {
                            self.password = password;
                        }
                    }
                    HostUpdate::Stopped => {
                        self.listen_addr = None;
                        self.display_addr = None;
                        self.backend.clear();
                        self.connected_peer = None;
                        self.pending_approval = None;
                        host_stopped = true;
                        lan_dirty = true;
                        break;
                    }
                    HostUpdate::Error { message, ui_error } => {
                        if let Some(ref ui) = ui_error {
                            eprintln!(
                                "[rc-host-agent] 被控端运行异常 [{}] {}: {}",
                                ui.code, ui.title, message
                            );
                        } else {
                            eprintln!("[rc-host-agent] 被控端运行异常: {message}");
                        }
                    }
                }
            }
        }
        if host_stopped {
            if let Some(old) = self.host.take() {
                if let Some(mut prev) = retired_host.replace(old) {
                    prev.stop();
                }
            }
            self.sync_signal_allow_assist();
        }
        if lan_dirty {
            self.sync_lan_advertising();
        }
        (
            changed,
            RetiredHandles {
                host: retired_host,
                signal: None,
                lan: None,
            },
        )
    }

    fn set_remote_assist(
        &mut self,
        enabled: bool,
    ) -> Result<(AgentSnapshot, Option<HostHandle>), String> {
        if !self.is_authenticated() {
            return Err("请先登录并完成初始化后再切换远程协助".to_string());
        }
        if enabled && self.session_type == ServiceSessionType::Aqua {
            self.permissions = permissions::check_permissions();
            permissions::ensure_host_ready(&self.permissions)?;
        }
        // 先从磁盘重读最新 prefs.json 并加跨进程锁，避免覆写前台保存的流参数/安全/端口映射/快照字段。
        let prefs_path = app_prefs::default_prefs_path();
        let prev_allow = {
            let _prefs_lock = config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
            let mut latest = app_prefs::load_from_path(&prefs_path);
            let prev = latest.allow_remote_assist;
            latest.allow_remote_assist = enabled;
            app_prefs::save_to_path(&prefs_path, &latest)?;
            self.prefs = latest;
            prev
        };
        match self.try_ensure_host_state() {
            Ok(retired) => {
                self.sync_signal_allow_assist();
                Ok((self.snapshot(), retired))
            }
            Err(err) => {
                {
                    let _prefs_lock =
                        config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
                    let mut reverted = app_prefs::load_from_path(&prefs_path);
                    reverted.allow_remote_assist = prev_allow;
                    let _ = app_prefs::save_to_path(&prefs_path, &reverted);
                    self.prefs = reverted;
                }
                if self.host.is_none() {
                    self.backend.clear();
                }
                self.sync_lan_advertising();
                self.sync_signal_allow_assist();
                Err(err)
            }
        }
    }

    fn set_assist_file_transfer(&mut self, enabled: bool) -> Result<AgentSnapshot, String> {
        let prefs_path = app_prefs::default_prefs_path();
        {
            let _prefs_lock = config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
            let mut latest = app_prefs::load_from_path(&prefs_path);
            latest.allow_assist_file_transfer = enabled;
            app_prefs::save_to_path(&prefs_path, &latest)?;
            self.prefs = latest;
        }
        if let Some(ref host) = self.host {
            host.set_allow_assist_file_transfer(enabled);
        }
        Ok(self.snapshot())
    }

    fn set_password(
        &mut self,
        pw: Option<String>,
    ) -> Result<(String, bool, Option<HostHandle>), String> {
        let cleaned = pw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        if let Some(ref p) = cleaned {
            if p.chars().count() > 64 || p.chars().any(|c| c.is_control() || c == '\0') {
                return Err("密码长度不能超过 64 个字符且不能包含控制字符".to_string());
            }
        }
        let persisted_pw = cleaned.clone().unwrap_or_default();
        let prefs_path = app_prefs::default_prefs_path();
        let _prefs_lock = config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
        let mut latest = app_prefs::load_from_path(&prefs_path);
        latest.custom_host_password = Some(persisted_pw.clone());
        app_prefs::save_to_path(&prefs_path, &latest)?;
        self.prefs = latest;

        self.custom_password = true;
        self.password = persisted_pw;
        let retired = self.ensure_host_state();
        self.sync_signal_allow_assist();
        if let Some(ref host) = self.host {
            let pw_opt = if self.password.is_empty() {
                None
            } else {
                Some(self.password.clone())
            };
            host.set_password_mode(pw_opt, false);
        }
        self.sync_lan_advertising();
        Ok((self.password.clone(), self.custom_password, retired))
    }

    fn rotate_temp_code(&mut self) -> Result<(String, bool), String> {
        if self.session_type == ServiceSessionType::LoginWindow {
            return Err("登录界面前会话不支持随机临时验证码，请使用账号免密或固定密码".to_string());
        }
        let prefs_path = app_prefs::default_prefs_path();
        let _prefs_lock = config_paths::acquire_file_lock(&prefs_path, Duration::from_secs(3));
        let mut latest = app_prefs::load_from_path(&prefs_path);
        latest.custom_host_password = None;
        app_prefs::save_to_path(&prefs_path, &latest)?;
        self.prefs = latest;
        let code = host_bridge::generate_temp_code();
        self.password = code.clone();
        self.custom_password = false;
        if let Some(ref host) = self.host {
            host.set_password_mode(Some(code), true);
        }
        self.sync_lan_advertising();
        Ok((self.password.clone(), false))
    }

    #[must_use]
    fn take_all_handles(&mut self) -> RetiredHandles {
        if let Some(ref s) = self.signal {
            s.request_stop();
        }
        RetiredHandles {
            host: self.host.take(),
            signal: self.signal.take(),
            lan: self.lan.take(),
        }
    }
}

fn perform_quit_all(agent: &Arc<Mutex<AgentInner>>, stop_flag: &Arc<AtomicBool>, secret: &str) {
    let _ = config_paths::write_private_file(&config_paths::manual_stop_path(), b"manual-stop\n");
    let _ = IpcClient::call_with_secret(
        &config_paths::daemon_socket_path(),
        secret,
        ServiceRequest::QuitAll,
        Duration::from_millis(800),
    );
    stop_flag.store(true, Ordering::Release);
    let retired = agent
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take_all_handles();
    retired.stop_all(ByeReason::Normal);
}

fn main() {
    rc_platform_macos::register_all();
    rc_ui::updater_macos::register_macos_update_hooks();

    if !should_run_at_startup() {
        return;
    }

    // 平台提供者注册完成后非阻塞预热本机硬件编解码能力缓存（绝不阻塞 Agent 启动或建连热路径）。
    let _ = rc_client_core::codec_selector::trigger_default_hardware_caps_probe();

    let binary_sha256 = current_exe_sha256();
    let session_type = detect_session_type();
    if session_type == ServiceSessionType::Aqua {
        // 在任何权限检测、WindowServer/CoreGraphics 初始化与 NSStatusItem 安装之前，
        // 率先以 Accessory 模式初始化 NSApplication 并挂载 Reopen 转发委托（配合 Info.plist LSUIElement 双重防占 Dock）。
        let _ = metal_attach::init_accessory_app_with_reopen_handler(Arc::new(launch_main_gui));
    }
    let secret_path = config_paths::ipc_secret_path();
    let ipc_secret = match read_or_create_ipc_secret(&secret_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[rc-host-agent] 初始化 IPC 密钥失败: {e}");
            std::process::exit(1);
        }
    };

    let mut inner = match AgentInner::new(session_type, ipc_secret.clone()) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[rc-host-agent] 初始化 Agent 失败: {e}");
            std::process::exit(1);
        }
    };
    if inner.is_authenticated() {
        inner.start_lan();
    }
    let initial_wol_mac = probe_local_wol_mac_if_enabled(inner.prefs.allow_wol);
    let _ = inner.ensure_host_state();
    let _ = inner.start_signal(initial_wol_mac);

    let initial_tray = inner.status_bar_display();
    let agent = Arc::new(Mutex::new(inner));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let _ = metal_attach::register_sigterm_flag(Arc::clone(&stop_flag));

    // 启动 agent.sock IPC 服务。
    let agent_ipc = Arc::clone(&agent);
    let stop_ipc = Arc::clone(&stop_flag);
    let secret_ipc = ipc_secret.clone();
    let binary_sha_ipc = binary_sha256.clone();
    let mut ipc_server = match IpcServer::bind(
        config_paths::agent_socket_path(),
        ipc_secret.clone(),
        move |req| match req {
            ServiceRequest::Ping => ServiceResponse::Pong {
                pid: std::process::id(),
                session_type,
                binary_sha256: binary_sha_ipc.clone(),
            },
            ServiceRequest::GetSnapshot => {
                let guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                ServiceResponse::Snapshot(Box::new(guard.snapshot()))
            }
            ServiceRequest::SetRemoteAssist { enabled } => {
                let (res, tray_snap, retired_host) = {
                    let mut guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                    match guard.set_remote_assist(enabled) {
                        Ok((snap, retired)) => (
                            ServiceResponse::Snapshot(Box::new(snap)),
                            Some(guard.status_bar_display()),
                            retired,
                        ),
                        Err(message) => (ServiceResponse::Error { message }, None, None),
                    }
                };
                if let Some(mut h) = retired_host {
                    h.stop();
                }
                if let Some(tray) = tray_snap {
                    metal_attach::update_status_bar(tray);
                }
                res
            }
            ServiceRequest::SetAssistFileTransfer { enabled } => {
                let mut guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                match guard.set_assist_file_transfer(enabled) {
                    Ok(snap) => ServiceResponse::Snapshot(Box::new(snap)),
                    Err(message) => ServiceResponse::Error { message },
                }
            }
            ServiceRequest::SetPassword { password } => {
                let (res, retired_host) = {
                    let mut guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                    match guard.set_password(password) {
                        Ok((pw, custom, retired)) => (
                            ServiceResponse::PasswordChanged {
                                password: pw,
                                custom_password: custom,
                            },
                            retired,
                        ),
                        Err(message) => (ServiceResponse::Error { message }, None),
                    }
                };
                if let Some(mut h) = retired_host {
                    h.stop();
                }
                res
            }
            ServiceRequest::RotateTempCode => {
                let mut guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                match guard.rotate_temp_code() {
                    Ok((pw, custom)) => ServiceResponse::PasswordChanged {
                        password: pw,
                        custom_password: custom,
                    },
                    Err(message) => ServiceResponse::Error { message },
                }
            }
            ServiceRequest::RespondApproval { generation, accept } => {
                let guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(ref host) = guard.host {
                    let _ = host.respond_approval(generation, accept);
                }
                ServiceResponse::Ok
            }
            ServiceRequest::DisconnectClient => {
                let guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(ref host) = guard.host {
                    host.disconnect_client();
                }
                ServiceResponse::Ok
            }
            ServiceRequest::RefreshAuthAndCerts => {
                // 磁盘与认证恢复在无锁上下文完成，避免阻塞 AppKit 主循环或 GetSnapshot IPC。
                let ctx = load_reloaded_context();
                let (retired, tray) = {
                    let mut guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                    let retired = guard.apply_reloaded_context(ctx);
                    (retired, guard.status_bar_display())
                };
                retired.stop_all(ByeReason::Normal);
                metal_attach::update_status_bar(tray);
                ServiceResponse::Ok
            }
            ServiceRequest::ReloadPrefs => {
                // 在获取 agent_ipc 锁前读取磁盘偏好并按需探测本机 MAC，严禁持锁派生外部子进程。
                let next_prefs = app_prefs::load_from_path(&app_prefs::default_prefs_path());
                let local_wol_mac = probe_local_wol_mac_if_enabled(next_prefs.allow_wol);
                let (retired_host, tray) = {
                    let mut guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                    let retired = guard.reload_prefs(next_prefs, local_wol_mac);
                    (retired, guard.status_bar_display())
                };
                if let Some(mut h) = retired_host {
                    h.stop();
                }
                metal_attach::update_status_bar(tray);
                ServiceResponse::Ok
            }
            ServiceRequest::TriggerPermissionCheck => {
                let (retired_host, snap, tray) = {
                    let mut guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                    let retired = guard.ensure_host_state();
                    guard.sync_signal_allow_assist();
                    (retired, guard.snapshot(), guard.status_bar_display())
                };
                if let Some(mut h) = retired_host {
                    h.stop();
                }
                metal_attach::update_status_bar(tray);
                ServiceResponse::Snapshot(Box::new(snap))
            }
            ServiceRequest::OpenMainGui => {
                launch_main_gui();
                ServiceResponse::Ok
            }
            ServiceRequest::GetServerStatus => {
                let guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                let snap = match guard.signal.as_ref() {
                    Some(sig) => sig.status(),
                    None => guard.server_cfg.disabled_snapshot(),
                };
                ServiceResponse::ServerStatus(Box::new(snap))
            }
            ServiceRequest::DrainDeviceStatusEvents => {
                let mut guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                let events: Vec<DeviceStatusEvent> =
                    guard.pending_device_events.drain(..).collect();
                ServiceResponse::DeviceStatusEvents(events)
            }
            ServiceRequest::DrainDeviceWolEvents => {
                let mut guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                let events: Vec<DeviceWolCapabilityEvent> =
                    guard.pending_device_wol_events.drain(..).collect();
                ServiceResponse::DeviceWolEvents { events }
            }
            ServiceRequest::GetWolCapableSnapshot => {
                let guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                let devices = guard
                    .signal
                    .as_ref()
                    .map(|s| s.device_wol_capable_snapshot())
                    .unwrap_or_default();
                ServiceResponse::WolCapableSnapshot { devices }
            }
            ServiceRequest::RequestWol {
                target_device_id,
                timeout_ms,
            } => {
                let sig_opt = {
                    let guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                    guard.signal.as_ref().cloned()
                };
                let Some(sig) = sig_opt else {
                    return ServiceResponse::WolError(Box::new(ui_error::resolve_ui_error(
                        codes::NETWORK_UNREACHABLE,
                        ErrorPlatform::Macos,
                        &[],
                        None,
                        Some("请先在「设置 → 网络」配置并连接服务器".to_string()),
                    )));
                };
                if sig.status().state != ServerConnectionState::Connected {
                    return ServiceResponse::WolError(Box::new(ui_error::resolve_ui_error(
                        codes::NETWORK_UNREACHABLE,
                        ErrorPlatform::Macos,
                        &[],
                        None,
                        Some("信令服务器尚未连接，暂无法发送远程开机指令".to_string()),
                    )));
                }
                let timeout = resolve_request_connect_timeout(timeout_ms);
                match sig.request_wol(&target_device_id, timeout) {
                    Ok(ack) => ServiceResponse::WolAck(Box::new(ack)),
                    Err(ui_err) => ServiceResponse::WolError(Box::new(ui_err)),
                }
            }
            ServiceRequest::RequestConnect {
                target_id,
                timeout_ms,
                force_relay,
            } => {
                let sig_opt = {
                    let guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                    guard.signal.as_ref().cloned()
                };
                let Some(sig) = sig_opt else {
                    return ServiceResponse::Error {
                        message: "请先在「设置 → 网络」配置并连接服务器".to_string(),
                    };
                };
                if sig.status().state != ServerConnectionState::Connected {
                    return ServiceResponse::Error {
                        message: "请先在「设置 → 网络」配置并连接服务器".to_string(),
                    };
                }
                let timeout = resolve_request_connect_timeout(timeout_ms);
                let res = if force_relay {
                    sig.request_relay(&target_id, timeout)
                } else {
                    sig.request_connect(&target_id, timeout)
                };
                match res {
                    Ok(grant) => ServiceResponse::ConnectGrant(Box::new(grant)),
                    Err(message) => ServiceResponse::Error { message },
                }
            }
            ServiceRequest::QuitAll => {
                perform_quit_all(&agent_ipc, &stop_ipc, &secret_ipc);
                ServiceResponse::Ok
            }
            ServiceRequest::RestartForUpgrade => {
                // 绝不写入 manual-stop：令主循环按 ByeReason::SessionSwitching 优雅退场，由 launchd 拉起新版。
                stop_ipc.store(true, Ordering::Release);
                ServiceResponse::Ok
            }
            ServiceRequest::EnsureAccessToken { .. }
            | ServiceRequest::InstallVerifiedUpdate { .. } => ServiceResponse::Error {
                message: "该请求仅由 rc-daemon 处理".to_string(),
            },
        },
    ) {
        Ok(srv) => srv,
        Err(e) => {
            eprintln!("[rc-host-agent] 绑定 agent.sock 失败: {e}");
            let retired = agent
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take_all_handles();
            retired.stop_all(ByeReason::Normal);
            std::process::exit(1);
        }
    };

    // 启动本机网络路由切换监测器：切网/恢复联网时触发信令快重连并清空刷新局域网节点表。
    let agent_net = Arc::clone(&agent);
    let mut net_monitor = NetworkMonitorHandle::spawn(move |ev| {
        eprintln!(
            "[rc-host-agent] 检测到本机网络变化 (connectivity: {} -> {}, addrs: {:?})，触发信令快重连与局域网刷新",
            ev.had_connectivity, ev.has_connectivity, ev.fingerprint
        );
        let sig_opt = {
            let guard = agent_net.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ref lan) = guard.lan {
                lan.clear_and_refresh();
            }
            guard.signal.clone()
        };
        if let Some(sig) = sig_opt {
            sig.trigger_reconnect("network_route_changed");
        }
    });

    if session_type == ServiceSessionType::Aqua {
        let agent_tray = Arc::clone(&agent);
        let stop_tray = Arc::clone(&stop_flag);
        let secret_tray = ipc_secret.clone();

        // NSApplication 已在 `main()` 开头（所有权限检查与宿主初始化之前）以 Accessory 模式初始化并挂载 Reopen 委托，
        // 配合主包 `Info.plist` 的 `LSUIElement=true` 确保 `rc-host-agent` 永不占用第二个 Dock 图标。
        let _ = metal_attach::install_status_bar(
            initial_tray,
            Arc::new(move |action| match action {
                StatusBarAction::OpenMainGui => {
                    launch_main_gui();
                }
                StatusBarAction::QuitAll => {
                    let agent_bg = Arc::clone(&agent_tray);
                    let stop_bg = Arc::clone(&stop_tray);
                    let secret_bg = secret_tray.clone();
                    let _ = std::thread::Builder::new()
                        .name("rc-agent-tray-quit".to_string())
                        .spawn(move || {
                            perform_quit_all(&agent_bg, &stop_bg, &secret_bg);
                        });
                }
            }),
        );

        let agent_tick = Arc::clone(&agent);
        let stop_tick = Arc::clone(&stop_flag);
        let _ = metal_attach::run_agent_app_loop(&stop_flag, move || {
            if config_paths::manual_stop_path().exists() {
                stop_tick.store(true, Ordering::Release);
                return;
            }
            let (tray_snap, retired) = {
                let mut guard = agent_tick.lock().unwrap_or_else(|e| e.into_inner());
                let (changed, retired) = guard.pump_events();
                (changed.then(|| guard.status_bar_display()), retired)
            };
            if !retired.is_empty() {
                let _ = std::thread::Builder::new()
                    .name("rc-agent-retire".to_string())
                    .spawn(move || retired.stop_all(ByeReason::Normal));
            }
            if let Some(snap) = tray_snap {
                metal_attach::update_status_bar(snap);
            }
        });
        metal_attach::remove_status_bar();
    } else {
        while !stop_flag.load(Ordering::Acquire) {
            if config_paths::manual_stop_path().exists() {
                stop_flag.store(true, Ordering::Release);
                break;
            }
            let retired = {
                let mut guard = agent.lock().unwrap_or_else(|e| e.into_inner());
                let (_, retired) = guard.pump_events();
                retired
            };
            if !retired.is_empty() {
                let _ = std::thread::Builder::new()
                    .name("rc-agent-retire".to_string())
                    .spawn(move || retired.stop_all(ByeReason::Normal));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    // 会话切换停机时优先释放被控监听（发出 ByeReason::SessionSwitching 并归还 9901 端口），
    // 再关闭本实例 IPC Server（inode 校验保护新 Aqua Agent 的 agent.sock 不被误删）。
    let exit_reason = if !config_paths::manual_stop_path().exists() {
        ByeReason::SessionSwitching
    } else {
        ByeReason::Normal
    };
    // 停机前先停止网络监测器，避免退出清理期间触发重连；并在主线程显式清理物理隐私幕布窗口。
    net_monitor.stop();
    #[cfg(target_os = "macos")]
    rc_capture_macos::clear_privacy_curtain();
    let retired = agent
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take_all_handles();
    retired.stop_all(exit_reason);
    ipc_server.stop();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_identity() -> SignalIdentityKey {
        SignalIdentityKey {
            authenticated: true,
            enabled: true,
            server_host: "example.com".to_string(),
            signal_port: 21116,
            relay_port: 21117,
            use_https: true,
            ca_cert_pem: Some(
                "-----BEGIN CERTIFICATE-----\nA\n-----END CERTIFICATE-----".to_string(),
            ),
            ca_fingerprint_sha256: None,
            device_key: "dev-key".to_string(),
            user_id: Some("user-1".to_string()),
            device_pubkey_hex: "pub-1".to_string(),
        }
    }

    #[test]
    fn should_restart_signal_preserves_healthy_connection_and_revives_error_state() {
        let key = sample_identity();
        assert!(!should_restart_signal(
            true,
            &key,
            &key,
            Some(ServerConnectionState::Connected)
        ));
        assert!(!should_restart_signal(
            true,
            &key,
            &key,
            Some(ServerConnectionState::Connecting)
        ));
        // 终态退出或错误态时，即便 identity 未变也必须触发重建复活。
        assert!(should_restart_signal(
            true,
            &key,
            &key,
            Some(ServerConnectionState::Error)
        ));
        assert!(should_restart_signal(true, &key, &key, None));
        assert!(should_restart_signal(
            false,
            &key,
            &key,
            Some(ServerConnectionState::Connected)
        ));
    }

    #[test]
    fn signal_identity_key_detects_ca_cert_pem_change_without_fingerprint() {
        let prev = sample_identity();
        let mut next = prev.clone();
        next.ca_cert_pem =
            Some("-----BEGIN CERTIFICATE-----\nB\n-----END CERTIFICATE-----".to_string());
        assert!(should_restart_signal(
            true,
            &prev,
            &next,
            Some(ServerConnectionState::Connected)
        ));
    }

    #[test]
    fn normalize_device_id_digits_strips_spaces_and_hyphens() {
        assert_eq!(normalize_device_id_digits("604 928 235"), "604928235");
        assert_eq!(normalize_device_id_digits("604-928-235"), "604928235");
        assert_eq!(normalize_device_id_digits("604928235"), "604928235");
    }

    #[test]
    fn resolve_request_connect_timeout_defaults_and_passes_through() {
        assert_eq!(
            resolve_request_connect_timeout(0),
            Duration::from_millis(DEFAULT_REQUEST_CONNECT_TIMEOUT_MS)
        );
        assert_eq!(
            resolve_request_connect_timeout(5_000),
            Duration::from_millis(5_000)
        );
        assert_eq!(
            resolve_request_connect_timeout(12_000),
            Duration::from_millis(12_000)
        );
        assert_eq!(
            resolve_request_connect_timeout(200),
            Duration::from_millis(1_000)
        );
        assert_eq!(
            resolve_request_connect_timeout(60_000),
            Duration::from_millis(30_000)
        );
    }

    #[test]
    fn start_signal_syncs_live_host_signal_client_on_start_replace_and_clear() {
        let base = std::env::temp_dir().join(format!(
            "rc-agent-signal-sync-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let certs_dir = base.join("certs");
        let auth_path = base.join("auth.json");
        std::fs::create_dir_all(&certs_dir).unwrap();

        let server_cfg = ServerConfig {
            enabled: true,
            server_host: "127.0.0.1".to_string(),
            signal_port: 1,
            relay_port: 21117,
            use_https: false,
            ..ServerConfig::default()
        };
        let cert_mgr = Arc::new(CertManager::load_or_init(certs_dir).unwrap());
        let auth = Arc::new(AuthSessionManager::new(
            server_cfg.clone(),
            auth_path,
            cert_mgr,
        ));
        let future_exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + 3600;
        auth.apply_external_session(rc_ui::auth_client::StoredAuthSession {
            user_id: "00000000-0000-0000-0000-000000000001".to_string(),
            email: "agent@example.com".to_string(),
            display_name: "Agent".to_string(),
            role: "user".to_string(),
            must_change_password: false,
            access_token: "tok-agent".to_string(),
            refresh_token: "ref-agent".to_string(),
            access_expires_at_unix_sec: future_exp,
            server_host: "127.0.0.1".to_string(),
            api_port: 21118,
        });

        let host = HostHandle::spawn(UiHostConfig {
            listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            prefer_hardware: false,
            require_approval: false,
            signal_client: None,
            ..UiHostConfig::default()
        })
        .expect("spawn test host");

        let sync_log = Arc::new(Mutex::new(Vec::new()));
        let mut inner = AgentInner {
            session_type: ServiceSessionType::Aqua,
            ipc_secret: "secret".to_string(),
            server_cfg,
            prefs: AppPrefs::default(),
            auth: Arc::new(RwLock::new(auth)),
            signal: None,
            lan: None,
            host: Some(host),
            allow_owner_control: true,
            pending_device_events: VecDeque::new(),
            pending_device_wol_events: VecDeque::new(),
            password: String::new(),
            custom_password: false,
            listen_addr: None,
            display_addr: None,
            backend: String::new(),
            connected_peer: None,
            pending_approval: None,
            device_id: None,
            signal_online: false,
            permissions: permissions::check_permissions(),
            degraded_permissions: false,
            last_offline_disk_check: std::time::Instant::now(),
            signal_sync_log: Some(Arc::clone(&sync_log)),
        };

        // 1) 后启动信令：应向已存在的 HostHandle 同步 Some(ptr1)。
        let old1 = inner.start_signal(None);
        assert!(old1.is_none());
        let ptr1 = Arc::as_ptr(inner.signal.as_ref().expect("signal spawned")) as usize;

        // 2) 替换信令：应向 HostHandle 同步新的 Some(ptr2)。
        let old2 = inner.start_signal(None);
        assert!(old2.is_some());
        let ptr2 = Arc::as_ptr(inner.signal.as_ref().expect("signal replaced")) as usize;
        assert_ne!(ptr1, ptr2);

        // 3) 停用/未认证清空信令：应向 HostHandle 同步 None。
        inner.server_cfg.enabled = false;
        let old3 = inner.start_signal(None);
        assert!(old3.is_some());
        assert!(inner.signal.is_none());

        let recorded = sync_log.lock().unwrap().clone();
        assert_eq!(recorded, vec![Some(ptr1), Some(ptr2), None]);

        drop(old2);
        drop(old3);
        let retired = inner.take_all_handles();
        retired.stop_all(ByeReason::Normal);
        let _ = std::fs::remove_dir_all(&base);
    }
}

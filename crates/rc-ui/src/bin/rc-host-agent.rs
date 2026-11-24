//! RabbitControl 双会话被控端宿主 Agent（`com.rabbitcontrol.agent`）。
//!
//! 根据所在 macOS 会话自动切换行为：
//! - **LoginWindow（登录密码界面前）**：只启用本人自控 `OwnerTicket` PoP 或已显式配置固定
//!   密码的无人值守连接；不创建 Menu Bar 图标，不弹出交互式审批。
//! - **Aqua（已登录桌面）**：检测屏幕录制与辅助功能权限；权限齐备时挂载硬件采集与注入被控
//!   服务，缺权限时进入安全降级模式避免无限重启；安装顶部 Menu Bar `NSStatusItem` 图标，支持
//!   打开控制端 GUI、复制设备码、打开权限向导、切换远程协助以及无密码「整体退出」。

#![deny(unsafe_code)]

use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
use rc_ui::permissions::{self, PermissionSnapshot};
use rc_ui::server_config::{self, ServerConfig, ServerConnectionState};
use rc_ui::service_ipc::{
    read_or_create_ipc_secret, AgentSnapshot, IpcClient, IpcServer, ServiceRequest,
    ServiceResponse, ServiceSessionType,
};
#[cfg(target_os = "macos")]
use rc_ui::service_manager;
use rc_ui::signal_client::{SignalClientHandle, SignalClientOptions};

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
}

fn load_reloaded_context() -> ReloadedContext {
    let server_cfg = server_config::load_from_path(&server_config::default_config_path());
    let prefs = app_prefs::load_from_path(&app_prefs::default_prefs_path());
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
        if let Some(bundle) = service_manager::locate_app_bundle() {
            let _ = std::process::Command::new("open")
                .arg("-a")
                .arg(&bundle)
                .spawn();
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
    auth: Arc<AuthSessionManager>,
    signal: Option<Arc<SignalClientHandle>>,
    lan: Option<LanDiscoveryHandle>,
    host: Option<HostHandle>,
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
}

impl AgentInner {
    fn new(session_type: ServiceSessionType, ipc_secret: String) -> Result<Self, String> {
        let cfg = server_config::load_from_path(&server_config::default_config_path());
        let prefs = app_prefs::load_from_path(&app_prefs::default_prefs_path());
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
            auth,
            signal: None,
            lan: None,
            host: None,
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

    fn is_authenticated(&self) -> bool {
        self.server_cfg.is_configured()
            && self
                .auth
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

    fn build_owner_verifier(&self) -> Option<Arc<dyn OwnerTicketVerifier>> {
        if !self.is_authenticated() || !self.prefs.allow_owner_self_control {
            return None;
        }
        let pubkey_hex = self
            .server_cfg
            .server_auth_pubkey_hex
            .clone()
            .or_else(|| self.auth.server_config().server_auth_pubkey_hex)?;
        let user_id = self.auth.current_session()?.user_id;
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
        let ca_cert_pem = self
            .server_cfg
            .ca_cert_pem
            .clone()
            .or_else(|| self.auth.server_config().ca_cert_pem)?;
        let user_id = self.auth.current_session()?.user_id;
        let cert_mgr = self.auth.cert_manager();
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

    #[must_use]
    fn start_signal(&mut self) -> Option<Arc<SignalClientHandle>> {
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
            return old;
        }

        let auth = Arc::clone(&self.auth);
        let token_provider: rc_ui::signal_client::TokenProvider = Arc::new(move || {
            let daemon_sock = config_paths::daemon_socket_path();
            let secret_path = config_paths::ipc_secret_path();
            if daemon_sock.exists() {
                match IpcClient::call(
                    &daemon_sock,
                    &secret_path,
                    ServiceRequest::EnsureAccessToken { failed_token: None },
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
            auth.ensure_valid_access_token()
                .map_err(|e| format!("{e:?}"))
        });
        let options = SignalClientOptions {
            token_provider: Some(token_provider),
            device_pubkey: Some(self.auth.cert_manager().device_pubkey_hex()),
            allow_remote_assist: self.signal_allow_remote_assist(),
            dynamic_lan_discovery: true,
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
        let snap = handle.status();
        self.signal_online = snap.state == ServerConnectionState::Connected;
        if let Some(did) = snap.device_id {
            self.device_id = Some(did.clone());
            if let Some(ref lan) = self.lan {
                lan.set_local_device_id(Some(did));
            }
        }
        self.signal = Some(Arc::new(handle));
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
        let want_host =
            self.is_authenticated() && (allow_remote_assist || owner_verifier.is_some());

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

        if let Some(ref host) = self.host {
            if !allow_remote_assist {
                if let Some(prompt) = self.pending_approval.take() {
                    let _ = host.respond_approval(prompt.generation, false);
                }
            }
            host.set_allow_remote_assist(allow_remote_assist);
            host.set_owner_verifier(owner_verifier);
            host.set_mtls_config(mtls_bundle, accept_mtls_lan);
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
            allow_remote_assist,
            owner_verifier,
            mtls_config: mtls_bundle,
            accept_mtls_lan,
            forbid_passwordless_auto_approve: self.session_type == ServiceSessionType::LoginWindow,
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

    #[must_use]
    fn apply_reloaded_context(&mut self, ctx: ReloadedContext) -> RetiredHandles {
        self.server_cfg = ctx.server_cfg;
        self.prefs = ctx.prefs;
        if let Some(auth) = ctx.auth {
            self.auth = auth;
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
        let signal = self.start_signal();
        self.sync_lan_advertising();
        RetiredHandles { host, signal, lan }
    }

    #[must_use]
    fn reload_prefs(&mut self) -> Option<HostHandle> {
        self.prefs = app_prefs::load_from_path(&app_prefs::default_prefs_path());
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
        retired
    }

    fn snapshot(&self) -> AgentSnapshot {
        AgentSnapshot {
            enabled: self.host.is_some() && self.effective_allow_remote_assist(),
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
            remote_assist_enabled: self.host.is_some() && self.effective_allow_remote_assist(),
            connected_peer: self.connected_peer.clone(),
            missing_permissions: self.degraded_permissions
                || (self.permissions.platform_supported && !self.permissions.host_ready),
            session_label,
        }
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
            self.auth.wipe_local_credentials();
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
            if let Some(sess) = auth_client::load_auth_session(&self.auth.auth_path()) {
                if !sess.must_change_password && sess.access_expires_at_unix_sec > now_sec + 90 {
                    self.server_cfg =
                        server_config::load_from_path(&server_config::default_config_path());
                    self.prefs = app_prefs::load_from_path(&app_prefs::default_prefs_path());
                    self.auth.update_server_config(self.server_cfg.clone());
                    self.auth.apply_external_session(sess);
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
                            signal: self.start_signal(),
                            lan: None,
                        };
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
                    HostUpdate::Error { message } => {
                        eprintln!("[rc-host-agent] 被控端运行异常: {message}");
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

    if !should_run_at_startup() {
        return;
    }

    let session_type = detect_session_type();
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
    let _ = inner.ensure_host_state();
    let _ = inner.start_signal();

    let initial_tray = inner.status_bar_display();
    let agent = Arc::new(Mutex::new(inner));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let _ = metal_attach::register_sigterm_flag(Arc::clone(&stop_flag));

    // 启动 agent.sock IPC 服务。
    let agent_ipc = Arc::clone(&agent);
    let stop_ipc = Arc::clone(&stop_flag);
    let secret_ipc = ipc_secret.clone();
    let mut ipc_server = match IpcServer::bind(
        config_paths::agent_socket_path(),
        ipc_secret.clone(),
        move |req| match req {
            ServiceRequest::Ping => ServiceResponse::Pong {
                pid: std::process::id(),
                session_type,
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
                let (retired_host, tray) = {
                    let mut guard = agent_ipc.lock().unwrap_or_else(|e| e.into_inner());
                    let retired = guard.reload_prefs();
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
            ServiceRequest::RequestConnect {
                target_id,
                timeout_ms,
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
                let timeout = Duration::from_millis(timeout_ms.clamp(1_000, 30_000));
                match sig.request_connect(&target_id, timeout) {
                    Ok(grant) => ServiceResponse::ConnectGrant(Box::new(grant)),
                    Err(message) => ServiceResponse::Error { message },
                }
            }
            ServiceRequest::QuitAll => {
                perform_quit_all(&agent_ipc, &stop_ipc, &secret_ipc);
                ServiceResponse::Ok
            }
            ServiceRequest::EnsureAccessToken { .. } => ServiceResponse::Error {
                message: "EnsureAccessToken 仅由 rc-daemon 处理".to_string(),
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

    if session_type == ServiceSessionType::Aqua {
        let agent_tray = Arc::clone(&agent);
        let stop_tray = Arc::clone(&stop_flag);
        let secret_tray = ipc_secret.clone();

        let _ = metal_attach::install_status_bar(
            initial_tray,
            Arc::new(move |action| match action {
                StatusBarAction::OpenMainGui => {
                    launch_main_gui();
                }
                StatusBarAction::CopyDeviceId => {
                    let did = agent_tray
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .device_id
                        .clone();
                    if let Some(id) = did {
                        let _ = metal_attach::copy_text_to_pasteboard(&id);
                    }
                }
                StatusBarAction::CheckPermissions => {
                    let marker = config_paths::run_dir().join("open-perm-check");
                    let _ = config_paths::write_private_file(&marker, b"1\n");
                    launch_main_gui();
                }
                StatusBarAction::ToggleRemoteAssist(enabled) => {
                    let agent_bg = Arc::clone(&agent_tray);
                    let _ = std::thread::Builder::new()
                        .name("rc-agent-tray-toggle".to_string())
                        .spawn(move || {
                            let (authenticated, perm_blocked, retired, tray) = {
                                let mut guard = agent_bg.lock().unwrap_or_else(|e| e.into_inner());
                                if !guard.is_authenticated() {
                                    (false, false, None, None)
                                } else {
                                    match guard.set_remote_assist(enabled) {
                                        Ok((_, retired)) => {
                                            (true, false, retired, Some(guard.status_bar_display()))
                                        }
                                        Err(_) => {
                                            let blocked = guard.session_type
                                                == ServiceSessionType::Aqua
                                                && permissions::ensure_host_ready(
                                                    &guard.permissions,
                                                )
                                                .is_err();
                                            (true, blocked, None, Some(guard.status_bar_display()))
                                        }
                                    }
                                }
                            };
                            if let Some(mut h) = retired {
                                h.stop();
                            }
                            if let Some(t) = tray {
                                metal_attach::update_status_bar(t);
                            }
                            if !authenticated {
                                launch_main_gui();
                            } else if perm_blocked {
                                let marker = config_paths::run_dir().join("open-perm-check");
                                let _ = config_paths::write_private_file(&marker, b"1\n");
                                launch_main_gui();
                            }
                        });
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
    let retired = agent
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take_all_handles();
    retired.stop_all(exit_reason);
    ipc_server.stop();
}

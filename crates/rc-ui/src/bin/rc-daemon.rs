//! RabbitControl 系统后台守护进程（`com.rabbitcontrol.daemon`）。
//!
//! 职责：
//! 1. 开机自检（对比 `kern.boottime` / `boot_id` 与 `run/boot-marker`），若系统重启则清除
//!    上一会话留下的 `manual-stop` 退出标记，恢复无人值守保活；若同一次开机内用户刚
//!    主动执行过「整体退出」且 `manual-stop` 存在，则主动退出避免被拉起。
//! 2. 维护高熵 IPC 共享密钥（`run/ipc.secret`）与 `daemon.sock` 本地控制接口。
//! 3. 作为认证与证书单点后台维护者，周期刷新 Access Token 并在 `< 48h` 时静默续签设备
//!    mTLS 证书，续签或配置变更后通知 `rc-host-agent` 热加载。

#![deny(unsafe_code)]

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rc_ui::auth_client::{AuthClientError, AuthMaintainerHandle, AuthSessionManager};
use rc_ui::cert_manager::CertManager;
use rc_ui::config_paths;
use rc_ui::metal_attach;
use rc_ui::server_config;
use rc_ui::service_ipc::{
    read_or_create_ipc_secret, IpcClient, IpcServer, ServiceRequest, ServiceResponse,
    ServiceSessionType,
};

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

fn ensure_boot_state_and_should_run() -> bool {
    let stop_path = config_paths::manual_stop_path();
    let boot_path = config_paths::boot_marker_path();

    if let Some(cur_boot) = current_os_boot_id() {
        let prev_boot = fs::read_to_string(&boot_path)
            .ok()
            .map(|s| s.trim().to_string());
        if prev_boot.as_deref() != Some(cur_boot.as_str()) {
            // 检测到系统已重启：清除上次关机前「整体退出」遗留标记，恢复无人值守。
            if stop_path.exists() {
                let _ = fs::remove_file(&stop_path);
            }
            if let Some(parent) = boot_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ =
                config_paths::write_private_file(&boot_path, format!("{cur_boot}\n").as_bytes());
            return true;
        }
    }

    !stop_path.exists()
}

struct DaemonRuntime {
    auth: Arc<AuthSessionManager>,
    maintainer: Mutex<Option<AuthMaintainerHandle>>,
    auth_sync_lock: Mutex<()>,
    ipc_secret: String,
    stop_flag: Arc<AtomicBool>,
}

impl DaemonRuntime {
    fn reload_auth_from_disk(
        &self,
        ensure_cert: bool,
    ) -> Result<Option<rc_ui::auth_client::StoredAuthSession>, rc_ui::auth_client::AuthClientError>
    {
        let cfg = server_config::load_from_path(&server_config::default_config_path());
        self.auth.update_server_config(cfg);
        if ensure_cert {
            self.auth.reload_from_disk()
        } else {
            self.auth.reload_session_from_disk_without_cert_check()
        }
    }

    fn notify_agent_refresh_async(&self) {
        let sock = config_paths::agent_socket_path();
        let sec = self.ipc_secret.clone();
        let _ = std::thread::Builder::new()
            .name("rc-daemon-kick-notify".to_string())
            .spawn(move || {
                let _ = IpcClient::call_with_secret(
                    &sock,
                    &sec,
                    ServiceRequest::RefreshAuthAndCerts,
                    Duration::from_secs(2),
                );
            });
    }

    fn restart_maintainer(&self) {
        let auth = Arc::clone(&self.auth);
        let agent_sock = config_paths::agent_socket_path();
        let secret = self.ipc_secret.clone();
        let agent_sock_kick = agent_sock.clone();
        let secret_kick = secret.clone();

        let new_handle = auth.spawn_background_maintainer_with_callbacks(
            Duration::from_secs(30),
            || {
                let cfg = server_config::load_from_path(&server_config::default_config_path());
                cfg.cached_device_id
            },
            move || {
                let _ = IpcClient::call_with_secret(
                    &agent_sock,
                    &secret,
                    ServiceRequest::RefreshAuthAndCerts,
                    Duration::from_secs(2),
                );
            },
            move |reason| {
                eprintln!("[rc-daemon] 认证需要重新登录: {reason}");
                let sock = agent_sock_kick.clone();
                let sec = secret_kick.clone();
                let _ = std::thread::Builder::new()
                    .name("rc-daemon-kick-notify".to_string())
                    .spawn(move || {
                        let _ = IpcClient::call_with_secret(
                            &sock,
                            &sec,
                            ServiceRequest::RefreshAuthAndCerts,
                            Duration::from_secs(2),
                        );
                    });
            },
        );

        let old = self
            .maintainer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(new_handle);
        if let Some(mut handle) = old {
            handle.stop();
        }
    }

    fn stop_maintainer(&self) {
        if let Some(mut handle) = self
            .maintainer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            handle.stop();
        }
    }
}

fn main() {
    if !ensure_boot_state_and_should_run() {
        return;
    }

    let secret_path = config_paths::ipc_secret_path();
    let ipc_secret = match read_or_create_ipc_secret(&secret_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[rc-daemon] 初始化 IPC 密钥失败: {e}");
            std::process::exit(1);
        }
    };

    let cfg_path = server_config::default_config_path();
    let cfg = server_config::load_from_path(&cfg_path);
    let certs_dir = rc_ui::cert_manager::default_certs_dir();
    let cert_mgr = match CertManager::load_or_init(certs_dir) {
        Ok(mgr) => Arc::new(mgr),
        Err(e) => {
            eprintln!("[rc-daemon] 初始化设备证书管理器失败: {e}");
            std::process::exit(1);
        }
    };
    let auth_path = rc_ui::auth_client::default_auth_path();
    let auth = Arc::new(AuthSessionManager::new(cfg, auth_path, cert_mgr));
    let _ = auth.restore_on_startup();

    let stop_flag = Arc::new(AtomicBool::new(false));
    let _ = metal_attach::register_sigterm_flag(Arc::clone(&stop_flag));

    let rt = Arc::new(DaemonRuntime {
        auth,
        maintainer: Mutex::new(None),
        auth_sync_lock: Mutex::new(()),
        ipc_secret: ipc_secret.clone(),
        stop_flag: Arc::clone(&stop_flag),
    });
    rt.restart_maintainer();

    let rt_ipc = Arc::clone(&rt);
    let mut ipc_server = match IpcServer::bind(
        config_paths::daemon_socket_path(),
        ipc_secret,
        move |req| match req {
            ServiceRequest::Ping => ServiceResponse::Pong {
                pid: std::process::id(),
                session_type: ServiceSessionType::Standalone,
            },
            ServiceRequest::RefreshAuthAndCerts => {
                let _sync = rt_ipc
                    .auth_sync_lock
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let _ = rt_ipc.reload_auth_from_disk(true);
                rt_ipc.restart_maintainer();
                ServiceResponse::Ok
            }
            ServiceRequest::EnsureAccessToken { failed_token } => {
                let _sync = rt_ipc
                    .auth_sync_lock
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let disk_sess = rc_ui::auth_client::load_auth_session(&rt_ipc.auth.auth_path());
                let mem_sess = rt_ipc.auth.current_session();
                let need_reload = match (mem_sess, disk_sess) {
                    (None, Some(_)) | (Some(_), None) => true,
                    (Some(m), Some(d)) => {
                        !m.user_id.eq_ignore_ascii_case(d.user_id.trim())
                            || m.access_token != d.access_token
                            || m.refresh_token != d.refresh_token
                            || m.server_host != d.server_host
                            || m.api_port != d.api_port
                            || m.must_change_password != d.must_change_password
                    }
                    (None, None) => false,
                };
                if need_reload {
                    if let Err(err) = rt_ipc.reload_auth_from_disk(false) {
                        return match err {
                            AuthClientError::ReloginRequired(msg) => {
                                rt_ipc.notify_agent_refresh_async();
                                ServiceResponse::ReloginRequired { message: msg }
                            }
                            other => ServiceResponse::Error {
                                message: other.to_string(),
                            },
                        };
                    }
                }
                let res = match failed_token.as_deref() {
                    Some(tok) if !tok.is_empty() => rt_ipc.auth.refresh_after_unauthorized(tok),
                    _ => rt_ipc.auth.ensure_valid_access_token(),
                };
                match res {
                    Ok(_) => match rt_ipc.auth.current_session() {
                        Some(sess) => ServiceResponse::AuthSession(Box::new(sess)),
                        None => ServiceResponse::ReloginRequired {
                            message: "当前未登录".to_string(),
                        },
                    },
                    Err(AuthClientError::ReloginRequired(msg)) => {
                        rt_ipc.notify_agent_refresh_async();
                        ServiceResponse::ReloginRequired { message: msg }
                    }
                    Err(err) => ServiceResponse::Error {
                        message: err.to_string(),
                    },
                }
            }
            ServiceRequest::QuitAll => {
                let _ = config_paths::write_private_file(
                    &config_paths::manual_stop_path(),
                    b"manual-stop\n",
                );
                rt_ipc.stop_flag.store(true, Ordering::Release);
                ServiceResponse::Ok
            }
            _ => ServiceResponse::Ok,
        },
    ) {
        Ok(srv) => srv,
        Err(e) => {
            eprintln!("[rc-daemon] 绑定 daemon.sock 失败: {e}");
            rt.stop_maintainer();
            std::process::exit(1);
        }
    };

    while !stop_flag.load(Ordering::Acquire) {
        if config_paths::manual_stop_path().exists() {
            stop_flag.store(true, Ordering::Release);
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    ipc_server.stop();
    rt.stop_maintainer();
}

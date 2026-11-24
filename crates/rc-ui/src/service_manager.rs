//! macOS 系统级后台服务（LaunchDaemon + 双会话 LaunchAgent）安装、卸载与状态探测。
//!
//! - `com.rabbitcontrol.daemon`（`LaunchDaemon`）：root 级保活、开机自启标记清理、认证与证书维护；
//! - `com.rabbitcontrol.agent`（`LaunchAgent`，`LimitLoadToSessionType = [LoginWindow, Aqua]`）：
//!   支持登录前（LoginWindow）与登录后（Aqua）双会话托管，并在 Aqua 下提供顶部菜单栏图标。
//!
//! 两者共享 `KeepAlive.PathState`（监听 `run/manual-stop == false`）：点击「整体退出」只需写入
//! `manual-stop` 并停止服务，无需管理员密码即可防止被 launchd 立即拉起；下次用户主动启动主程序
//! 删除 `manual-stop` 后即恢复后台托管。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config_paths::{ENV_CONFIG_DIR, ENV_DAEMON_MODE};
use crate::service_ipc::ServiceSessionType;

/// LaunchDaemon 任务标签。
pub const DAEMON_LABEL: &str = "com.rabbitcontrol.daemon";

/// LaunchAgent 任务标签。
pub const AGENT_LABEL: &str = "com.rabbitcontrol.agent";

/// LaunchDaemon plist 系统路径。
pub const DAEMON_PLIST_PATH: &str = "/Library/LaunchDaemons/com.rabbitcontrol.daemon.plist";

/// LaunchAgent plist 系统路径。
pub const AGENT_PLIST_PATH: &str = "/Library/LaunchAgents/com.rabbitcontrol.agent.plist";

/// 主应用 Bundle Identifier（与 `tauri.conf.json` / `scripts/package-macos.sh` 保持一致）。
pub const APP_BUNDLE_ID: &str = "com.rabbitcontrol.dev";

/// 系统后台服务安装与运行状态报告（供设置页与启动自检渲染）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatusReport {
    /// 当前操作系统是否支持系统级服务安装（macOS 为 `true`）。
    pub supported: bool,
    /// plist 与 `service-managed` 标记是否已安装。
    pub installed: bool,
    /// `rc-daemon` 当前是否正在运行并响应 IPC。
    pub daemon_running: bool,
    /// `rc-host-agent` 当前是否正在运行并响应 IPC。
    pub agent_running: bool,
    /// 控制端 GUI 当前是否可以通过 `agent.sock` 代理控制后台被控端。
    pub proxy_active: bool,
    /// 后台 Agent 当前所在会话类型（若在线）。
    pub session_type: Option<ServiceSessionType>,
    /// 本机是否启用了 macOS FileVault 磁盘加密（开启时冷启动输入密码前系统尚未解密启动卷）。
    pub filevault_enabled: bool,
    /// 探测到的完整 `RabbitControl.app` 路径（包含 `rc-daemon` 与 `rc-host-agent`）。
    pub app_bundle_path: Option<String>,
    /// 当前是否处于用户主动「整体退出」后的停止态（存在 `manual-stop` 标记）。
    pub manual_stopped: bool,
    /// 状态说明或操作提示信息。
    pub message: Option<String>,
}

/// 对 XML 文本内容做安全转义。
pub fn escape_xml(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// 为 POSIX Shell 参数生成单引号安全转义字面量。
pub fn shell_quote_posix(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('\'');
    for ch in raw.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// 解析 `fdesetup status` 命令输出，判断 FileVault 是否处于启用状态。
pub fn parse_fdesetup_output(stdout: &str) -> bool {
    stdout.to_ascii_lowercase().contains("filevault is on")
}

/// 生成 `com.rabbitcontrol.daemon` 的 LaunchDaemon plist XML 文本。
pub fn render_daemon_plist(
    daemon_bin: &Path,
    manual_stop_path: &Path,
    log_dir: &Path,
    config_dir: &Path,
) -> String {
    let bin = escape_xml(&daemon_bin.to_string_lossy());
    let stop_path = escape_xml(&manual_stop_path.to_string_lossy());
    let out_log = escape_xml(&log_dir.join("daemon.stdout.log").to_string_lossy());
    let err_log = escape_xml(&log_dir.join("daemon.stderr.log").to_string_lossy());
    let cfg = escape_xml(&config_dir.to_string_lossy());

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{DAEMON_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>{ENV_DAEMON_MODE}</key>
        <string>1</string>
        <key>{ENV_CONFIG_DIR}</key>
        <string>{cfg}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>PathState</key>
        <dict>
            <key>{stop_path}</key>
            <false/>
        </dict>
    </dict>
    <key>ProcessType</key>
    <string>Adaptive</string>
    <key>AssociatedBundleIdentifiers</key>
    <array>
        <string>{APP_BUNDLE_ID}</string>
    </array>
    <key>StandardOutPath</key>
    <string>{out_log}</string>
    <key>StandardErrorPath</key>
    <string>{err_log}</string>
</dict>
</plist>
"#
    )
}

/// 生成 `com.rabbitcontrol.agent` 的双会话 LaunchAgent plist XML 文本。
///
/// 关键属性：`LimitLoadToSessionType = [LoginWindow, Aqua]` 与 `ProcessType = Interactive`，
/// 使 macOS 在登录密码输入界面即可拉起被控端 Agent，并在桌面登录后无缝迁移至 Aqua 会话。
pub fn render_agent_plist(
    agent_bin: &Path,
    manual_stop_path: &Path,
    log_dir: &Path,
    config_dir: &Path,
) -> String {
    let bin = escape_xml(&agent_bin.to_string_lossy());
    let stop_path = escape_xml(&manual_stop_path.to_string_lossy());
    let out_log = escape_xml(&log_dir.join("agent.stdout.log").to_string_lossy());
    let err_log = escape_xml(&log_dir.join("agent.stderr.log").to_string_lossy());
    let cfg = escape_xml(&config_dir.to_string_lossy());

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>{ENV_DAEMON_MODE}</key>
        <string>1</string>
        <key>{ENV_CONFIG_DIR}</key>
        <string>{cfg}</string>
    </dict>
    <key>LimitLoadToSessionType</key>
    <array>
        <string>LoginWindow</string>
        <string>Aqua</string>
    </array>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>PathState</key>
        <dict>
            <key>{stop_path}</key>
            <false/>
        </dict>
    </dict>
    <key>AssociatedBundleIdentifiers</key>
    <array>
        <string>{APP_BUNDLE_ID}</string>
    </array>
    <key>StandardOutPath</key>
    <string>{out_log}</string>
    <key>StandardErrorPath</key>
    <string>{err_log}</string>
</dict>
</plist>
"#
    )
}

/// 根据可执行文件路径定位同时打包了 `rc-daemon` 与 `rc-host-agent` 的 `.app` Bundle 路径。
pub fn locate_app_bundle_from_exe(exe_path: &Path) -> Option<PathBuf> {
    let macos_dir = exe_path.parent()?;
    let contents_dir = macos_dir.parent()?;
    let bundle_dir = contents_dir.parent()?;
    let is_app = bundle_dir
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("app"));
    if !is_app {
        return None;
    }
    let daemon = macos_dir.join("rc-daemon");
    let agent = macos_dir.join("rc-host-agent");
    if daemon.is_file() && agent.is_file() {
        Some(bundle_dir.to_path_buf())
    } else {
        None
    }
}

/// 查找当前本机可用于注册系统后台服务的 `RabbitControl.app` Bundle。
pub fn locate_app_bundle() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(found) = locate_app_bundle_from_exe(&exe) {
            return Some(found);
        }
    }
    #[cfg(target_os = "macos")]
    {
        let std_app = PathBuf::from("/Applications/RabbitControl.app");
        let macos_dir = std_app.join("Contents/MacOS");
        if macos_dir.join("rc-daemon").is_file() && macos_dir.join("rc-host-agent").is_file() {
            return Some(std_app);
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(target_dir) = exe.parent() {
                let bundled_dist = target_dir.join("../dist/RabbitControl.app");
                let dm = bundled_dist.join("Contents/MacOS");
                if dm.join("rc-daemon").is_file() && dm.join("rc-host-agent").is_file() {
                    if let Ok(canon) = std::fs::canonicalize(&bundled_dist) {
                        return Some(canon);
                    }
                    return Some(bundled_dist);
                }
            }
        }
    }
    None
}

/// 判断本机是否开启了 FileVault 磁盘加密（非 macOS 恒返回 `false`）。
pub fn detect_filevault_enabled() -> bool {
    #[cfg(target_os = "macos")]
    {
        let Ok(out) = std::process::Command::new("fdesetup")
            .arg("status")
            .output()
        else {
            return false;
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        parse_fdesetup_output(&stdout)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(target_os = "macos")]
mod macos_imp {
    use super::*;
    use crate::config_paths::{self, MACOS_SYSTEM_CONFIG_DIR, SERVICE_MANAGED_MARKER};
    use crate::service_ipc::{IpcClient, ServiceRequest, ServiceResponse};
    use std::process::Command;
    use std::time::Duration;

    fn run_osascript_admin_shell(shell_script: &str) -> Result<(), String> {
        let escaped_for_as = shell_script.replace('\\', "\\\\").replace('"', "\\\"");
        let apple_script =
            format!("do shell script \"{escaped_for_as}\" with administrator privileges");
        let out = Command::new("osascript")
            .arg("-e")
            .arg(&apple_script)
            .output()
            .map_err(|e| format!("调用 macOS 管理员授权失败: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if stderr.contains("-128") || stderr.contains("User canceled") {
                Err("已取消管理员授权，系统后台服务配置未变更".to_string())
            } else if stderr.is_empty() {
                Err(format!(
                    "管理员命令执行失败（退出码 {:?}）",
                    out.status.code()
                ))
            } else {
                Err(format!("管理员命令执行失败: {stderr}"))
            }
        }
    }

    /// 查询当前系统级后台服务安装、运行及 FileVault 状态。
    pub fn query_service_status() -> ServiceStatusReport {
        let sys_dir = PathBuf::from(MACOS_SYSTEM_CONFIG_DIR);
        let installed = Path::new(DAEMON_PLIST_PATH).is_file()
            && Path::new(AGENT_PLIST_PATH).is_file()
            && sys_dir.join(SERVICE_MANAGED_MARKER).is_file();

        let secret_path = config_paths::ipc_secret_path();
        let daemon_running = matches!(
            IpcClient::call(
                &config_paths::daemon_socket_path(),
                &secret_path,
                ServiceRequest::Ping,
                Duration::from_millis(350),
            ),
            Ok(ServiceResponse::Pong { .. })
        );

        let agent_pong = IpcClient::call(
            &config_paths::agent_socket_path(),
            &secret_path,
            ServiceRequest::Ping,
            Duration::from_millis(350),
        );
        let (agent_running, session_type) = match agent_pong {
            Ok(ServiceResponse::Pong { session_type, .. }) => (true, Some(session_type)),
            _ => (false, None),
        };

        let bundle = locate_app_bundle();
        let manual_stopped = config_paths::manual_stop_path().exists();
        let message = if bundle.is_none() && !installed {
            Some(
                "请先使用打包后的 RabbitControl.app（或将其拖入 /Applications）后再安装系统服务"
                    .to_string(),
            )
        } else if installed && manual_stopped {
            Some(
                "已通过「整体退出」暂停系统后台服务；重新启动应用或关闭后再开启即可恢复"
                    .to_string(),
            )
        } else {
            None
        };

        ServiceStatusReport {
            supported: true,
            installed,
            daemon_running,
            agent_running,
            proxy_active: agent_running,
            session_type,
            filevault_enabled: detect_filevault_enabled(),
            app_bundle_path: bundle.map(|p| p.to_string_lossy().to_string()),
            manual_stopped,
            message,
        }
    }

    /// 安装并立即加载 macOS 系统级 `LaunchDaemon` 与双会话 `LaunchAgent`（触发一次系统管理员授权）。
    pub fn install_system_service() -> Result<ServiceStatusReport, String> {
        let bundle = locate_app_bundle().ok_or_else(|| {
            "未找到包含 rc-daemon 与 rc-host-agent 的 RabbitControl.app，请先运行打包脚本并将应用放入 /Applications".to_string()
        })?;
        let daemon_bin = bundle.join("Contents/MacOS/rc-daemon");
        let agent_bin = bundle.join("Contents/MacOS/rc-host-agent");
        if !daemon_bin.is_file() || !agent_bin.is_file() {
            return Err(
                "当前 RabbitControl.app 缺少后台服务二进制（rc-daemon / rc-host-agent）"
                    .to_string(),
            );
        }

        let sys_cfg = PathBuf::from(MACOS_SYSTEM_CONFIG_DIR);
        let run_dir = sys_cfg.join("run");
        let certs_dir = sys_cfg.join("certs");
        let log_dir = sys_cfg.join("logs");
        let manual_stop = run_dir.join("manual-stop");
        let marker = sys_cfg.join(SERVICE_MANAGED_MARKER);

        let daemon_plist = render_daemon_plist(&daemon_bin, &manual_stop, &log_dir, &sys_cfg);
        let agent_plist = render_agent_plist(&agent_bin, &manual_stop, &log_dir, &sys_cfg);

        // 先写到用户临时目录，再由单次提权命令复制并配置权限。
        let nonce = format!("rc-svc-install-{}", std::process::id());
        let stage_dir = std::env::temp_dir().join(&nonce);
        std::fs::create_dir_all(&stage_dir).map_err(|e| format!("创建临时暂存目录失败: {e}"))?;
        let tmp_daemon = stage_dir.join("com.rabbitcontrol.daemon.plist");
        let tmp_agent = stage_dir.join("com.rabbitcontrol.agent.plist");
        std::fs::write(&tmp_daemon, daemon_plist.as_bytes())
            .map_err(|e| format!("写入临时 daemon plist 失败: {e}"))?;
        std::fs::write(&tmp_agent, agent_plist.as_bytes())
            .map_err(|e| format!("写入临时 agent plist 失败: {e}"))?;

        let fresh_install = !marker.exists();
        let user_cfg = config_paths::user_config_dir();
        let copy_files = ["server.json", "auth.json", "prefs.json"]
            .into_iter()
            .filter_map(|name| {
                let src = user_cfg.join(name);
                let dst = sys_cfg.join(name);
                let src_q = shell_quote_posix(&src.to_string_lossy());
                let dst_q = shell_quote_posix(&dst.to_string_lossy());
                if src.is_file() {
                    if fresh_install {
                        Some(format!(
                            "if [ -f {src} ] && [ ! -L {src} ]; then rm -f {dst}; cp -P {src} {dst}; fi",
                            src = src_q,
                            dst = dst_q,
                        ))
                    } else {
                        Some(format!(
                            "if [ -f {src} ] && [ ! -L {src} ]; then if [ -L {dst} ]; then rm -f {dst}; fi; if [ ! -e {dst} ]; then cp -P {src} {dst}; fi; fi",
                            src = src_q,
                            dst = dst_q,
                        ))
                    }
                } else if fresh_install && name == "auth.json" {
                    Some(format!("rm -f {dst}", dst = dst_q))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("; ");

        let user_certs = user_cfg.join("certs");
        let copy_certs = if user_certs.is_dir() {
            let src_q = shell_quote_posix(&user_certs.to_string_lossy());
            let dst_q = shell_quote_posix(&certs_dir.to_string_lossy());
            if fresh_install {
                format!(
                    concat!(
                        "if [ -d {src} ] && [ ! -L {src} ]; then ",
                        "if [ -L {dst} ]; then rm -f {dst}; fi; ",
                        "mkdir -p {dst} && find {dst} -maxdepth 1 -type l -delete 2>/dev/null || true && ",
                        "cp -RP {src}/. {dst}/; fi"
                    ),
                    src = src_q,
                    dst = dst_q,
                )
            } else {
                format!(
                    concat!(
                        "if [ -d {src} ] && [ ! -L {src} ]; then ",
                        "if [ -L {dst} ]; then rm -f {dst}; fi; ",
                        "mkdir -p {dst} && find {dst} -maxdepth 1 -type l -delete 2>/dev/null || true; ",
                        "if [ ! -e {dst_meta} ]; then cp -RP {src}/. {dst}/; fi; fi"
                    ),
                    src = src_q,
                    dst = dst_q,
                    dst_meta = shell_quote_posix(&certs_dir.join("device_cert.pem").to_string_lossy()),
                )
            }
        } else {
            String::new()
        };

        let script = format!(
            concat!(
                "set -e; ",
                "CONSOLE_UID=$(stat -f '%u' /dev/console 2>/dev/null || echo 501); ",
                "for d in {sys_cfg} {run_dir} {certs_dir} {log_dir}; do if [ -L \"$d\" ]; then rm -f \"$d\"; fi; done; ",
                "mkdir -p {sys_cfg} {run_dir} {certs_dir} {log_dir}; ",
                "{copy_files} ",
                "{copy_certs} ",
                "rm -f {manual_stop} {marker}; ",
                "printf '1\\n' > {marker}; ",
                "chown -RP \"$CONSOLE_UID:20\" {sys_cfg}; ",
                "chmod 2770 {sys_cfg} {run_dir} {certs_dir} {log_dir}; ",
                "find {sys_cfg} -maxdepth 2 -type f ! -type l -exec chmod 660 {{}} +; ",
                "cp {tmp_daemon} {daemon_plist}; ",
                "cp {tmp_agent} {agent_plist}; ",
                "chown root:wheel {daemon_plist} {agent_plist}; ",
                "chmod 644 {daemon_plist} {agent_plist}; ",
                "launchctl bootout system/{daemon_label} >/dev/null 2>&1 || true; ",
                "launchctl bootout \"gui/$CONSOLE_UID/{agent_label}\" >/dev/null 2>&1 || true; ",
                "launchctl bootstrap system {daemon_plist}; ",
                "launchctl bootstrap \"gui/$CONSOLE_UID\" {agent_plist}"
            ),
            sys_cfg = shell_quote_posix(&sys_cfg.to_string_lossy()),
            run_dir = shell_quote_posix(&run_dir.to_string_lossy()),
            certs_dir = shell_quote_posix(&certs_dir.to_string_lossy()),
            log_dir = shell_quote_posix(&log_dir.to_string_lossy()),
            copy_files = if copy_files.is_empty() {
                String::new()
            } else {
                format!("{copy_files};")
            },
            copy_certs = if copy_certs.is_empty() {
                String::new()
            } else {
                format!("{copy_certs};")
            },
            manual_stop = shell_quote_posix(&manual_stop.to_string_lossy()),
            marker = shell_quote_posix(&marker.to_string_lossy()),
            tmp_daemon = shell_quote_posix(&tmp_daemon.to_string_lossy()),
            tmp_agent = shell_quote_posix(&tmp_agent.to_string_lossy()),
            daemon_plist = shell_quote_posix(DAEMON_PLIST_PATH),
            agent_plist = shell_quote_posix(AGENT_PLIST_PATH),
            daemon_label = DAEMON_LABEL,
            agent_label = AGENT_LABEL,
        );

        let res = run_osascript_admin_shell(&script);
        let _ = std::fs::remove_dir_all(&stage_dir);
        res?;

        // 等待 agent.sock 就绪（最多 ~3s），确保安装完前端立即看到在线状态。
        for _ in 0..30 {
            let status = query_service_status();
            if status.daemon_running && status.agent_running {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(query_service_status())
    }

    /// 卸载 macOS 系统级后台服务并恢复配置至用户态目录。
    pub fn uninstall_system_service() -> Result<ServiceStatusReport, String> {
        let sys_cfg = PathBuf::from(MACOS_SYSTEM_CONFIG_DIR);
        let marker = sys_cfg.join(SERVICE_MANAGED_MARKER);
        let manual_stop = sys_cfg.join("run/manual-stop");
        let user_cfg = config_paths::user_config_dir();

        let sys_certs = sys_cfg.join("certs");
        let usr_certs = user_cfg.join("certs");
        let restore_certs = format!(
            concat!(
                "if [ -L {usr_certs} ]; then rm -f {usr_certs}; fi; ",
                "if [ ! -L {usr_certs} ]; then ",
                "if [ ! -f {sys_cert_pem} ] || [ -L {sys_cert_pem} ]; then rm -f {usr_cert_pem} {usr_cert_meta}; fi; ",
                "if [ ! -f {sys_cert_meta} ] || [ -L {sys_cert_meta} ]; then rm -f {usr_cert_meta}; fi; ",
                "fi; ",
                "if [ -d {sys_certs} ] && [ ! -L {sys_certs} ]; then ",
                "mkdir -p {usr_certs} && ",
                "find {usr_certs} -maxdepth 1 -type l -delete 2>/dev/null || true && ",
                "cp -RP {sys_certs}/. {usr_certs}/ && ",
                "chown -RP \"$CONSOLE_UID:20\" {usr_certs} && chmod 700 {usr_certs} && ",
                "find {usr_certs} -maxdepth 1 -type f ! -type l -exec chmod 600 {{}} +; fi"
            ),
            sys_certs = shell_quote_posix(&sys_certs.to_string_lossy()),
            usr_certs = shell_quote_posix(&usr_certs.to_string_lossy()),
            sys_cert_pem = shell_quote_posix(&sys_certs.join("device_cert.pem").to_string_lossy()),
            sys_cert_meta = shell_quote_posix(&sys_certs.join("device_cert_meta.json").to_string_lossy()),
            usr_cert_pem = shell_quote_posix(&usr_certs.join("device_cert.pem").to_string_lossy()),
            usr_cert_meta = shell_quote_posix(&usr_certs.join("device_cert_meta.json").to_string_lossy()),
        );

        // 若系统目录已有较新配置，安全写回至用户态保留用户账号和设置（若服务态已登出则清理旧用户凭据）。
        let restore_user_cfg = ["server.json", "auth.json", "prefs.json"]
            .into_iter()
            .map(|name| {
                let sys_f = sys_cfg.join(name);
                let usr_f = user_cfg.join(name);
                let sys_q = shell_quote_posix(&sys_f.to_string_lossy());
                let usr_q = shell_quote_posix(&usr_f.to_string_lossy());
                if name == "auth.json" {
                    format!(
                        "if [ -f {sys_f} ] && [ ! -L {sys_f} ]; then rm -f {usr_f}; cp -P {sys_f} {usr_f} && chown -h \"$CONSOLE_UID:20\" {usr_f} && chmod 600 {usr_f}; else rm -f {usr_f}; fi",
                        sys_f = sys_q,
                        usr_f = usr_q,
                    )
                } else {
                    format!(
                        "if [ -f {sys_f} ] && [ ! -L {sys_f} ]; then rm -f {usr_f}; cp -P {sys_f} {usr_f} && chown -h \"$CONSOLE_UID:20\" {usr_f} && chmod 600 {usr_f}; fi",
                        sys_f = sys_q,
                        usr_f = usr_q,
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("; ");

        let cleanup_sys_cfg = format!(
            "rm -f {sys_srv} {sys_auth} {sys_prefs}; rm -rf {sys_certs}",
            sys_srv = shell_quote_posix(&sys_cfg.join("server.json").to_string_lossy()),
            sys_auth = shell_quote_posix(&sys_cfg.join("auth.json").to_string_lossy()),
            sys_prefs = shell_quote_posix(&sys_cfg.join("prefs.json").to_string_lossy()),
            sys_certs = shell_quote_posix(&sys_certs.to_string_lossy()),
        );

        let script = format!(
            concat!(
                "set -e; ",
                "CONSOLE_UID=$(stat -f '%u' /dev/console 2>/dev/null || echo 501); ",
                "if [ -L {user_cfg} ]; then echo 'refusing symlink user config dir' >&2; exit 1; fi; ",
                "mkdir -p {user_cfg}; ",
                "chown -h \"$CONSOLE_UID:20\" {user_cfg} 2>/dev/null || true; ",
                "launchctl bootout \"gui/$CONSOLE_UID/{agent_label}\" >/dev/null 2>&1 || true; ",
                "launchctl bootout system/{daemon_label} >/dev/null 2>&1 || true; ",
                "{restore_user_cfg}; ",
                "{restore_certs}; ",
                "rm -f {agent_plist} {daemon_plist} {marker} {manual_stop}; ",
                "{cleanup_sys_cfg}"
            ),
            user_cfg = shell_quote_posix(&user_cfg.to_string_lossy()),
            restore_user_cfg = restore_user_cfg,
            restore_certs = restore_certs,
            cleanup_sys_cfg = cleanup_sys_cfg,
            agent_label = AGENT_LABEL,
            daemon_label = DAEMON_LABEL,
            agent_plist = shell_quote_posix(AGENT_PLIST_PATH),
            daemon_plist = shell_quote_posix(DAEMON_PLIST_PATH),
            marker = shell_quote_posix(&marker.to_string_lossy()),
            manual_stop = shell_quote_posix(&manual_stop.to_string_lossy()),
        );
        run_osascript_admin_shell(&script)?;
        Ok(query_service_status())
    }

    /// 清除「整体退出」标记并唤醒被控后台服务（无需管理员密码）。
    pub fn wake_services_on_launch() {
        let stop_path = config_paths::manual_stop_path();
        let had_manual_stop = stop_path.exists();
        if had_manual_stop {
            let _ = std::fs::remove_file(&stop_path);
        }
        // launchd PathState 在路径删除时会自动触发；顺带 kickstart gui agent 以消除延迟。
        let status = query_service_status();
        if status.installed && !status.agent_running {
            if let Ok(out) = Command::new("id").arg("-u").output() {
                let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !uid.is_empty() {
                    let _ = Command::new("launchctl")
                        .args(["kickstart", "-p", &format!("gui/{uid}/{AGENT_LABEL}")])
                        .output();
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod macos_imp {
    use super::*;

    /// 查询当前系统级后台服务安装、运行及 FileVault 状态。
    pub fn query_service_status() -> ServiceStatusReport {
        ServiceStatusReport {
            supported: false,
            installed: false,
            daemon_running: false,
            agent_running: false,
            proxy_active: false,
            session_type: None,
            filevault_enabled: false,
            app_bundle_path: None,
            manual_stopped: false,
            message: Some("仅 macOS 支持开机与锁屏前系统级后台服务注册".to_string()),
        }
    }

    /// 安装并立即加载 macOS 系统级 `LaunchDaemon` 与双会话 `LaunchAgent`（触发一次系统管理员授权）。
    pub fn install_system_service() -> Result<ServiceStatusReport, String> {
        Err("当前操作系统不支持 macOS 系统级后台服务安装".to_string())
    }

    /// 卸载 macOS 系统级后台服务并恢复配置至用户态目录。
    pub fn uninstall_system_service() -> Result<ServiceStatusReport, String> {
        Err("当前操作系统不支持 macOS 系统级后台服务卸载".to_string())
    }

    /// 非 macOS 平台空实现：唤醒系统后台服务占位函数。
    pub fn wake_services_on_launch() {}
}

pub use macos_imp::{
    install_system_service, query_service_status, uninstall_system_service, wake_services_on_launch,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_valid_plists_with_dual_session_and_xml_escaping() {
        let daemon_bin =
            Path::new("/Applications/RabbitControl & Dev.app/Contents/MacOS/rc-daemon");
        let agent_bin =
            Path::new("/Applications/RabbitControl & Dev.app/Contents/MacOS/rc-host-agent");
        let stop_path = Path::new("/Library/Application Support/RabbitControl/run/manual-stop");
        let log_dir = Path::new("/Library/Application Support/RabbitControl/logs");
        let cfg_dir = Path::new("/Library/Application Support/RabbitControl");

        let d = render_daemon_plist(daemon_bin, stop_path, log_dir, cfg_dir);
        assert!(d.contains(DAEMON_LABEL));
        assert!(d.contains("RabbitControl &amp; Dev.app"));
        assert!(d.contains("<key>PathState</key>"));
        assert!(d.contains("<false/>"));
        assert!(d.contains(APP_BUNDLE_ID));

        let a = render_agent_plist(agent_bin, stop_path, log_dir, cfg_dir);
        assert!(a.contains(AGENT_LABEL));
        assert!(a.contains("<string>LoginWindow</string>"));
        assert!(a.contains("<string>Aqua</string>"));
        assert!(a.contains("<string>Interactive</string>"));
        assert!(a.contains(ENV_DAEMON_MODE));
    }

    #[test]
    fn parses_fdesetup_and_shell_quotes() {
        assert!(parse_fdesetup_output("FileVault is On.\n"));
        assert!(!parse_fdesetup_output("FileVault is Off.\n"));
        assert!(!parse_fdesetup_output(""));

        assert_eq!(shell_quote_posix("a'b c"), "'a'\\''b c'");
    }

    #[test]
    fn locates_app_bundle_only_when_both_daemons_present() {
        use rand::rngs::OsRng;
        use rand::RngCore;
        let mut b = [0u8; 8];
        OsRng.fill_bytes(&mut b);
        let root = std::env::temp_dir().join(format!(
            "rc-ui-bundle-test-{}.app",
            crate::service_ipc::encode_hex_lower(&b)
        ));
        let macos = root.join("Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        let gui_exe = macos.join("RabbitControl");
        std::fs::write(&gui_exe, b"gui").unwrap();
        assert!(locate_app_bundle_from_exe(&gui_exe).is_none());

        std::fs::write(macos.join("rc-daemon"), b"d").unwrap();
        std::fs::write(macos.join("rc-host-agent"), b"a").unwrap();
        assert_eq!(locate_app_bundle_from_exe(&gui_exe), Some(root.clone()));

        let _ = std::fs::remove_dir_all(&root);
    }
}

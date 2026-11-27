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
use crate::service_ipc::{compute_file_sha256, ServiceSessionType};

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

/// 还原 XML 文本中的常见实体转义（单遍扫描，避免多重反转义与裸 `&` 误吞后续实体）。
pub fn unescape_xml(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp_idx) = rest.find('&') {
        out.push_str(&rest[..amp_idx]);
        let after_amp = &rest[amp_idx..];
        let matched = [
            ("&amp;", '&'),
            ("&lt;", '<'),
            ("&gt;", '>'),
            ("&quot;", '"'),
            ("&apos;", '\''),
        ]
        .into_iter()
        .find_map(|(pat, ch)| after_amp.strip_prefix(pat).map(|tail| (ch, tail)));
        if let Some((ch, tail)) = matched {
            out.push(ch);
            rest = tail;
        } else {
            out.push('&');
            rest = &after_amp[1..];
        }
    }
    out.push_str(rest);
    out
}

/// 从 launchd plist XML 中提取 `<key>ProgramArguments</key>` 首个 `<string>` 可执行文件路径。
pub fn parse_plist_program_path(plist_xml: &str) -> Option<PathBuf> {
    let key_pos = plist_xml.find("<key>ProgramArguments</key>")?;
    let after_key = &plist_xml[key_pos + "<key>ProgramArguments</key>".len()..];
    let array_start = after_key.find("<array>")?;
    let after_array = &after_key[array_start + "<array>".len()..];
    let array_end = after_array.find("</array>")?;
    let array_body = &after_array[..array_end];
    let str_start = array_body.find("<string>")?;
    let after_str = &array_body[str_start + "<string>".len()..];
    let str_end = after_str.find("</string>")?;
    let unescaped = unescape_xml(&after_str[..str_end]);
    let trimmed = unescaped.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// 判断运行中后台进程指纹与磁盘二进制 SHA-256 是否不一致（需触发升级重启）。
pub fn needs_binary_upgrade(running_sha: Option<&str>, disk_sha: &str) -> bool {
    if disk_sha.is_empty() {
        return false;
    }
    running_sha != Some(disk_sha)
}

/// 判断已安装 plist 指向的二进制是否落在了当前 `.app` Bundle 之外的其它应用包路径。
pub fn is_plist_pointing_to_other_bundle(plist_bin: &Path, current_bundle: Option<&Path>) -> bool {
    let Some(bundle) = current_bundle else {
        return false;
    };
    let bin_resolved = std::fs::canonicalize(plist_bin).unwrap_or_else(|_| plist_bin.to_path_buf());
    let bundle_resolved = std::fs::canonicalize(bundle).unwrap_or_else(|_| bundle.to_path_buf());
    !bin_resolved.starts_with(&bundle_resolved)
}

#[cfg(any(target_os = "macos", test))]
fn render_launchctl_wait_unloaded(service_target_expr: &str) -> String {
    // macOS `launchctl bootout` 异步卸载旧进程，须轮询 `launchctl print` 确认服务彻底移出域（最多 3s）
    // 再继续 `bootstrap` 或配置回写，避免撞 `Bootstrap failed: 5: Input/output error`。
    format!(
        "i=0; while [ \"$i\" -lt 30 ] && launchctl print {target} >/dev/null 2>&1; do sleep 0.1; i=$((i + 1)); done",
        target = service_target_expr,
    )
}

#[cfg(any(target_os = "macos", test))]
fn render_launchctl_bootstrap_with_retry(
    domain_expr: &str,
    service_target_expr: &str,
    plist_quoted: &str,
) -> String {
    // 先解除可能存在的 disable 状态，再对 `launchctl bootstrap` 做最多 10 次短退避重试；
    // 若全部失败则不重定向 stderr 再执行一次，把真实错误透传给调用方。
    format!(
        concat!(
            "launchctl enable {target} >/dev/null 2>&1 || true; ",
            "boot_ok=0; ",
            "for attempt in 1 2 3 4 5 6 7 8 9 10; do ",
            "if launchctl bootstrap {domain} {plist} >/dev/null 2>&1; then boot_ok=1; break; fi; ",
            "sleep 0.2; ",
            "done; ",
            "if [ \"$boot_ok\" -ne 1 ]; then launchctl bootstrap {domain} {plist}; fi"
        ),
        target = service_target_expr,
        domain = domain_expr,
        plist = plist_quoted,
    )
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpgradeRestartOutcome {
    UpToDate,
    NotRunning,
    RestartRequested { old_pid: u32 },
    UnsupportedByLegacy { old_pid: u32 },
}

#[cfg(any(target_os = "macos", test))]
fn request_restart_if_outdated(
    sock_path: &Path,
    secret_path: &Path,
    disk_sha: &str,
) -> UpgradeRestartOutcome {
    use crate::service_ipc::{IpcClient, ServiceRequest, ServiceResponse};
    use std::time::Duration;

    let Ok(ServiceResponse::Pong {
        pid, binary_sha256, ..
    }) = IpcClient::call(
        sock_path,
        secret_path,
        ServiceRequest::Ping,
        Duration::from_millis(350),
    )
    else {
        return UpgradeRestartOutcome::NotRunning;
    };

    if !needs_binary_upgrade(binary_sha256.as_deref(), disk_sha) {
        return UpgradeRestartOutcome::UpToDate;
    }

    match IpcClient::call(
        sock_path,
        secret_path,
        ServiceRequest::RestartForUpgrade,
        Duration::from_secs(2),
    ) {
        // 连接被服务端在退出瞬间提前关闭（Err）或明确收到 Ok，均按已触发重启处理；
        // 只有服务端明确返回 Error（如旧版本不认识 RestartForUpgrade）才判定为旧版本不兼容。
        Ok(ServiceResponse::Ok) | Err(_) => {
            UpgradeRestartOutcome::RestartRequested { old_pid: pid }
        }
        Ok(_) => UpgradeRestartOutcome::UnsupportedByLegacy { old_pid: pid },
    }
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
    <key>SoftResourceLimits</key>
    <dict>
        <key>NumberOfFiles</key>
        <integer>4096</integer>
    </dict>
    <key>HardResourceLimits</key>
    <dict>
        <key>NumberOfFiles</key>
        <integer>8192</integer>
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
    <key>SoftResourceLimits</key>
    <dict>
        <key>NumberOfFiles</key>
        <integer>4096</integer>
    </dict>
    <key>HardResourceLimits</key>
    <dict>
        <key>NumberOfFiles</key>
        <integer>8192</integer>
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
        let (daemon_running, daemon_sha) = match IpcClient::call(
            &config_paths::daemon_socket_path(),
            &secret_path,
            ServiceRequest::Ping,
            Duration::from_millis(350),
        ) {
            Ok(ServiceResponse::Pong { binary_sha256, .. }) => (true, binary_sha256),
            _ => (false, None),
        };

        let agent_pong = IpcClient::call(
            &config_paths::agent_socket_path(),
            &secret_path,
            ServiceRequest::Ping,
            Duration::from_millis(350),
        );
        let (agent_running, session_type, agent_sha) = match agent_pong {
            Ok(ServiceResponse::Pong {
                session_type,
                binary_sha256,
                ..
            }) => (true, Some(session_type), binary_sha256),
            _ => (false, None, None),
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
        } else if installed {
            let current_bundle = std::env::current_exe()
                .ok()
                .and_then(|exe| locate_app_bundle_from_exe(&exe));
            let daemon_plist_bin = std::fs::read_to_string(DAEMON_PLIST_PATH)
                .ok()
                .and_then(|xml| parse_plist_program_path(&xml));
            let agent_plist_bin = std::fs::read_to_string(AGENT_PLIST_PATH)
                .ok()
                .and_then(|xml| parse_plist_program_path(&xml));

            let other_bundle = current_bundle.as_deref().is_some_and(|cur| {
                daemon_plist_bin
                    .as_deref()
                    .map_or(true, |b| is_plist_pointing_to_other_bundle(b, Some(cur)))
                    || agent_plist_bin
                        .as_deref()
                        .map_or(true, |b| is_plist_pointing_to_other_bundle(b, Some(cur)))
            });

            if other_bundle {
                Some(
                    "系统后台服务当前指向其他路径的应用包，请点击「重新安装服务」以切换至当前应用"
                        .to_string(),
                )
            } else {
                let daemon_outdated = daemon_running
                    && daemon_plist_bin
                        .as_deref()
                        .and_then(compute_file_sha256)
                        .is_some_and(|disk| needs_binary_upgrade(daemon_sha.as_deref(), &disk));
                let agent_outdated = agent_running
                    && agent_plist_bin
                        .as_deref()
                        .and_then(compute_file_sha256)
                        .is_some_and(|disk| needs_binary_upgrade(agent_sha.as_deref(), &disk));
                if daemon_outdated || agent_outdated {
                    Some(
                        "检测到后台服务仍为旧版本进程，重启电脑或点击「重新安装服务」即可切换至最新版本"
                            .to_string(),
                    )
                } else {
                    None
                }
            }
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
                "{wait_daemon_unloaded}; ",
                "{wait_agent_unloaded}; ",
                "{bootstrap_daemon}; ",
                "{bootstrap_agent}"
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
            wait_daemon_unloaded = render_launchctl_wait_unloaded(&format!("system/{DAEMON_LABEL}")),
            wait_agent_unloaded =
                render_launchctl_wait_unloaded(&format!("\"gui/$CONSOLE_UID/{AGENT_LABEL}\"")),
            bootstrap_daemon = render_launchctl_bootstrap_with_retry(
                "system",
                &format!("system/{DAEMON_LABEL}"),
                &shell_quote_posix(DAEMON_PLIST_PATH),
            ),
            bootstrap_agent = render_launchctl_bootstrap_with_retry(
                "\"gui/$CONSOLE_UID\"",
                &format!("\"gui/$CONSOLE_UID/{AGENT_LABEL}\""),
                &shell_quote_posix(AGENT_PLIST_PATH),
            ),
        );

        let res = run_osascript_admin_shell(&script);
        let _ = std::fs::remove_dir_all(&stage_dir);
        res?;

        // 轻量等待 daemon.sock 与 agent.sock 均就绪（最多 ~3s），避免轮询期间重复哈希大二进制。
        let secret_path = config_paths::ipc_secret_path();
        let daemon_sock = config_paths::daemon_socket_path();
        let agent_sock = config_paths::agent_socket_path();
        for _ in 0..30 {
            let daemon_ok = matches!(
                IpcClient::call(
                    &daemon_sock,
                    &secret_path,
                    ServiceRequest::Ping,
                    Duration::from_millis(200),
                ),
                Ok(ServiceResponse::Pong { .. })
            );
            let agent_ok = matches!(
                IpcClient::call(
                    &agent_sock,
                    &secret_path,
                    ServiceRequest::Ping,
                    Duration::from_millis(200),
                ),
                Ok(ServiceResponse::Pong { .. })
            );
            if daemon_ok && agent_ok {
                break;
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
                "{wait_agent_unloaded}; ",
                "{wait_daemon_unloaded}; ",
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
            wait_agent_unloaded =
                render_launchctl_wait_unloaded(&format!("\"gui/$CONSOLE_UID/{AGENT_LABEL}\"")),
            wait_daemon_unloaded = render_launchctl_wait_unloaded(&format!("system/{DAEMON_LABEL}")),
            agent_plist = shell_quote_posix(AGENT_PLIST_PATH),
            daemon_plist = shell_quote_posix(DAEMON_PLIST_PATH),
            marker = shell_quote_posix(&marker.to_string_lossy()),
            manual_stop = shell_quote_posix(&manual_stop.to_string_lossy()),
        );
        run_osascript_admin_shell(&script)?;
        Ok(query_service_status())
    }

    fn current_console_uid() -> Option<String> {
        let out = Command::new("id").arg("-u").output().ok()?;
        let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if uid.is_empty() {
            None
        } else {
            Some(uid)
        }
    }

    fn kickstart_agent(uid: &str, kill_existing: bool) {
        let target = format!("gui/{uid}/{AGENT_LABEL}");
        let args: &[&str] = if kill_existing {
            &["kickstart", "-k", "-p", &target]
        } else {
            &["kickstart", "-p", &target]
        };
        let _ = Command::new("launchctl").args(args).output();
    }

    fn wait_for_service_ready(
        sock_path: &Path,
        secret_path: &Path,
        old_pid: u32,
        disk_sha: &str,
        max_wait: Duration,
        kick_agent_after: Option<(Duration, &str)>,
    ) {
        let start = std::time::Instant::now();
        let mut gone_since: Option<std::time::Instant> = None;
        let mut kicked = false;
        while start.elapsed() < max_wait {
            match IpcClient::call(
                sock_path,
                secret_path,
                ServiceRequest::Ping,
                Duration::from_millis(250),
            ) {
                Ok(ServiceResponse::Pong {
                    pid, binary_sha256, ..
                }) => {
                    if pid != old_pid || binary_sha256.as_deref() == Some(disk_sha) {
                        return;
                    }
                    // 旧进程仍在优雅退场中，清空下线计时避免在旧进程活着时提前空耗 kickstart -p。
                    gone_since = None;
                }
                _ => {
                    let since = *gone_since.get_or_insert_with(std::time::Instant::now);
                    if !kicked {
                        if let Some((after, uid)) = kick_agent_after {
                            if since.elapsed() >= after {
                                kickstart_agent(uid, false);
                                kicked = true;
                            }
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(125));
        }
    }

    /// 清除「整体退出」标记、按磁盘二进制指纹自动热切换新版后台并唤醒服务（无需管理员密码）。
    pub fn wake_services_on_launch() {
        let stop_path = config_paths::manual_stop_path();
        if stop_path.exists() {
            let _ = std::fs::remove_file(&stop_path);
        }

        let sys_dir = PathBuf::from(MACOS_SYSTEM_CONFIG_DIR);
        let installed = Path::new(DAEMON_PLIST_PATH).is_file()
            && Path::new(AGENT_PLIST_PATH).is_file()
            && sys_dir.join(SERVICE_MANAGED_MARKER).is_file();
        if !installed {
            return;
        }

        let secret_path = config_paths::ipc_secret_path();
        let daemon_sock = config_paths::daemon_socket_path();
        let agent_sock = config_paths::agent_socket_path();

        // 1) 先检查 rc-daemon：与 plist 指向的磁盘二进制对比，不一致则优雅退出由 launchd 拉起新版。
        if let Some(disk_sha) = std::fs::read_to_string(DAEMON_PLIST_PATH)
            .ok()
            .and_then(|xml| parse_plist_program_path(&xml))
            .and_then(|bin| compute_file_sha256(&bin))
        {
            match request_restart_if_outdated(&daemon_sock, &secret_path, &disk_sha) {
                UpgradeRestartOutcome::RestartRequested { old_pid } => {
                    wait_for_service_ready(
                        &daemon_sock,
                        &secret_path,
                        old_pid,
                        &disk_sha,
                        Duration::from_millis(1_500),
                        None,
                    );
                }
                UpgradeRestartOutcome::UnsupportedByLegacy { old_pid } => {
                    eprintln!(
                        "[rc-ui] 后台 rc-daemon (pid={old_pid}) 为旧版本且不支持自动热重启，请在设置页重新安装或重启系统"
                    );
                }
                UpgradeRestartOutcome::UpToDate | UpgradeRestartOutcome::NotRunning => {}
            }
        }

        // 2) 再检查 rc-host-agent：不一致则优雅重启（或对旧版 agent 免密 kickstart -k），未运行则 kickstart -p。
        let uid = current_console_uid();
        if let Some(disk_sha) = std::fs::read_to_string(AGENT_PLIST_PATH)
            .ok()
            .and_then(|xml| parse_plist_program_path(&xml))
            .and_then(|bin| compute_file_sha256(&bin))
        {
            match request_restart_if_outdated(&agent_sock, &secret_path, &disk_sha) {
                UpgradeRestartOutcome::RestartRequested { old_pid } => {
                    let kick_opt = uid.as_deref().map(|u| (Duration::from_millis(400), u));
                    wait_for_service_ready(
                        &agent_sock,
                        &secret_path,
                        old_pid,
                        &disk_sha,
                        Duration::from_millis(2_500),
                        kick_opt,
                    );
                }
                UpgradeRestartOutcome::UnsupportedByLegacy { old_pid } => {
                    if let Some(u) = uid.as_deref() {
                        kickstart_agent(u, true);
                        wait_for_service_ready(
                            &agent_sock,
                            &secret_path,
                            old_pid,
                            &disk_sha,
                            Duration::from_millis(2_500),
                            None,
                        );
                    }
                }
                UpgradeRestartOutcome::NotRunning => {
                    if let Some(u) = uid.as_deref() {
                        kickstart_agent(u, false);
                    }
                }
                UpgradeRestartOutcome::UpToDate => {}
            }
        } else {
            let status = query_service_status();
            if status.installed && !status.agent_running {
                if let Some(u) = uid.as_deref() {
                    kickstart_agent(u, false);
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

        let expected_soft_limits = concat!(
            "<key>SoftResourceLimits</key>\n",
            "    <dict>\n",
            "        <key>NumberOfFiles</key>\n",
            "        <integer>4096</integer>\n",
            "    </dict>",
        );
        let expected_hard_limits = concat!(
            "<key>HardResourceLimits</key>\n",
            "    <dict>\n",
            "        <key>NumberOfFiles</key>\n",
            "        <integer>8192</integer>\n",
            "    </dict>",
        );

        let d = render_daemon_plist(daemon_bin, stop_path, log_dir, cfg_dir);
        assert!(d.contains(DAEMON_LABEL));
        assert!(d.contains("RabbitControl &amp; Dev.app"));
        assert!(d.contains("<key>PathState</key>"));
        assert!(d.contains("<false/>"));
        assert!(d.contains(APP_BUNDLE_ID));
        assert!(d.contains(expected_soft_limits));
        assert!(d.contains(expected_hard_limits));

        let a = render_agent_plist(agent_bin, stop_path, log_dir, cfg_dir);
        assert!(a.contains(AGENT_LABEL));
        assert!(a.contains("<string>LoginWindow</string>"));
        assert!(a.contains("<string>Aqua</string>"));
        assert!(a.contains("<string>Interactive</string>"));
        assert!(a.contains(ENV_DAEMON_MODE));
        assert!(a.contains(expected_soft_limits));
        assert!(a.contains(expected_hard_limits));

        #[cfg(target_os = "macos")]
        {
            use std::io::Write;
            use std::process::{Command, Stdio};
            for (name, xml) in [("daemon", &d), ("agent", &a)] {
                let mut child = Command::new("plutil")
                    .args(["-lint", "-s", "-"])
                    .stdin(Stdio::piped())
                    .spawn()
                    .expect("spawn plutil");
                child
                    .stdin
                    .as_mut()
                    .expect("plutil stdin")
                    .write_all(xml.as_bytes())
                    .expect("write plist to plutil");
                let status = child.wait().expect("wait plutil");
                assert!(status.success(), "{name} plist XML must pass plutil -lint");
            }
        }
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

    #[test]
    fn parses_plist_program_path_with_xml_escaping_and_rejects_invalid() {
        let daemon_bin =
            Path::new("/Applications/RabbitControl & Dev.app/Contents/MacOS/rc-daemon");
        let stop_path = Path::new("/Library/Application Support/RabbitControl/run/manual-stop");
        let log_dir = Path::new("/Library/Application Support/RabbitControl/logs");
        let cfg_dir = Path::new("/Library/Application Support/RabbitControl");

        let plist = render_daemon_plist(daemon_bin, stop_path, log_dir, cfg_dir);
        assert_eq!(
            parse_plist_program_path(&plist),
            Some(daemon_bin.to_path_buf())
        );

        assert_eq!(unescape_xml("a&amp;lt;b"), "a&lt;b");
        assert_eq!(unescape_xml("a & b &amp; c"), "a & b & c");
        assert!(parse_plist_program_path("<dict></dict>").is_none());
        assert!(parse_plist_program_path(
            "<key>ProgramArguments</key><array><string>   </string></array>"
        )
        .is_none());
    }

    #[test]
    fn needs_binary_upgrade_and_bundle_mismatch_checks() {
        assert!(!needs_binary_upgrade(Some("abc"), ""));
        assert!(needs_binary_upgrade(None, "abc"));
        assert!(needs_binary_upgrade(Some("old"), "new"));
        assert!(!needs_binary_upgrade(Some("same"), "same"));

        let cur_bundle = Path::new("/Applications/RabbitControl.app");
        let inside = Path::new("/Applications/RabbitControl.app/Contents/MacOS/rc-daemon");
        let sibling_prefix =
            Path::new("/Applications/RabbitControl.app.bak/Contents/MacOS/rc-daemon");
        let outside = Path::new("/Users/tester/Desktop/RabbitControl.app/Contents/MacOS/rc-daemon");
        assert!(!is_plist_pointing_to_other_bundle(inside, Some(cur_bundle)));
        assert!(is_plist_pointing_to_other_bundle(
            sibling_prefix,
            Some(cur_bundle)
        ));
        assert!(is_plist_pointing_to_other_bundle(outside, Some(cur_bundle)));
        assert!(!is_plist_pointing_to_other_bundle(outside, None));
    }

    #[cfg(unix)]
    #[test]
    fn request_restart_if_outdated_handles_match_mismatch_and_legacy_error() {
        use crate::service_ipc::{
            encode_hex_lower, read_or_create_ipc_secret, IpcServer, ServiceRequest, ServiceResponse,
        };
        use rand::rngs::OsRng;
        use rand::RngCore;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let mut b = [0u8; 8];
        OsRng.fill_bytes(&mut b);
        let root = std::env::temp_dir().join(format!("rc-ui-upgrade-mgr-{}", encode_hex_lower(&b)));
        std::fs::create_dir_all(&root).unwrap();
        let sock = root.join("run/agent.sock");
        let secret_path = root.join("run/ipc.secret");
        let manual_stop = root.join("run/manual-stop");
        let secret = read_or_create_ipc_secret(&secret_path).unwrap();

        // 1) 未启动时返回 NotRunning
        assert_eq!(
            request_restart_if_outdated(&sock, &secret_path, "sha_new"),
            UpgradeRestartOutcome::NotRunning
        );

        // 2) 指纹一致时返回 UpToDate 且不触发重启
        let restarted = Arc::new(AtomicBool::new(false));
        let restarted_flag = Arc::clone(&restarted);
        let mut srv_ok = IpcServer::bind(sock.clone(), secret.clone(), move |req| match req {
            ServiceRequest::Ping => ServiceResponse::Pong {
                pid: 101,
                session_type: ServiceSessionType::Aqua,
                binary_sha256: Some("sha_new".to_string()),
            },
            ServiceRequest::RestartForUpgrade => {
                restarted_flag.store(true, Ordering::Release);
                ServiceResponse::Ok
            }
            _ => ServiceResponse::Error {
                message: "unexpected".to_string(),
            },
        })
        .unwrap();
        assert_eq!(
            request_restart_if_outdated(&sock, &secret_path, "sha_new"),
            UpgradeRestartOutcome::UpToDate
        );
        assert!(!restarted.load(Ordering::Acquire));
        srv_ok.stop();

        // 3) 指纹不一致时发送 RestartForUpgrade 且绝不创建 manual-stop
        let restarted2 = Arc::new(AtomicBool::new(false));
        let restarted2_flag = Arc::clone(&restarted2);
        let mut srv_outdated =
            IpcServer::bind(sock.clone(), secret.clone(), move |req| match req {
                ServiceRequest::Ping => ServiceResponse::Pong {
                    pid: 202,
                    session_type: ServiceSessionType::Aqua,
                    binary_sha256: Some("sha_old".to_string()),
                },
                ServiceRequest::RestartForUpgrade => {
                    restarted2_flag.store(true, Ordering::Release);
                    ServiceResponse::Ok
                }
                _ => ServiceResponse::Error {
                    message: "unexpected".to_string(),
                },
            })
            .unwrap();
        assert_eq!(
            request_restart_if_outdated(&sock, &secret_path, "sha_new"),
            UpgradeRestartOutcome::RestartRequested { old_pid: 202 }
        );
        assert!(restarted2.load(Ordering::Acquire));
        assert!(!manual_stop.exists());
        srv_outdated.stop();

        // 4) 旧版本后台（binary_sha256: None 且拒绝 RestartForUpgrade）返回 UnsupportedByLegacy
        let mut srv_legacy = IpcServer::bind(sock.clone(), secret, move |req| match req {
            ServiceRequest::Ping => ServiceResponse::Pong {
                pid: 303,
                session_type: ServiceSessionType::Aqua,
                binary_sha256: None,
            },
            ServiceRequest::RestartForUpgrade => ServiceResponse::Error {
                message: "unknown op".to_string(),
            },
            _ => ServiceResponse::Ok,
        })
        .unwrap();
        assert_eq!(
            request_restart_if_outdated(&sock, &secret_path, "sha_new"),
            UpgradeRestartOutcome::UnsupportedByLegacy { old_pid: 303 }
        );
        srv_legacy.stop();

        // 5) 服务端收到 RestartForUpgrade 后在写回 Ok 前提前断连（EOF / Err(_)），仍应视为 RestartRequested
        {
            use crate::service_ipc::ServiceResponse;
            use std::io::{BufRead, BufReader, Write};
            use std::os::unix::net::UnixListener;

            let listener = UnixListener::bind(&sock).unwrap();
            let eof_jh = std::thread::spawn(move || {
                // 连接 1：Ping -> 返回旧 SHA-256 的 Pong
                let (mut s1, _) = listener.accept().unwrap();
                let mut r1 = BufReader::new(s1.try_clone().unwrap());
                let mut line = String::new();
                let _ = r1.read_line(&mut line);
                let mut pong = serde_json::to_vec(&ServiceResponse::Pong {
                    pid: 404,
                    session_type: ServiceSessionType::Aqua,
                    binary_sha256: Some("sha_old".to_string()),
                })
                .unwrap();
                pong.push(b'\n');
                s1.write_all(&pong).unwrap();
                s1.flush().unwrap();
                drop(s1);

                // 连接 2：RestartForUpgrade -> 读取请求后直接断开不写回，模拟进程退出竞态 EOF
                let (s2, _) = listener.accept().unwrap();
                let mut r2 = BufReader::new(s2);
                line.clear();
                let _ = r2.read_line(&mut line);
            });

            assert_eq!(
                request_restart_if_outdated(&sock, &secret_path, "sha_new"),
                UpgradeRestartOutcome::RestartRequested { old_pid: 404 }
            );
            eof_jh.join().unwrap();
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn launchctl_reload_snippets_wait_unload_retry_bootstrap_and_pass_sh_syntax() {
        let wait_daemon = render_launchctl_wait_unloaded("system/com.rabbitcontrol.daemon");
        let wait_agent =
            render_launchctl_wait_unloaded("\"gui/$CONSOLE_UID/com.rabbitcontrol.agent\"");
        let boot_daemon = render_launchctl_bootstrap_with_retry(
            "system",
            "system/com.rabbitcontrol.daemon",
            "'/Library/LaunchDaemons/com.rabbitcontrol.daemon.plist'",
        );
        let boot_agent = render_launchctl_bootstrap_with_retry(
            "\"gui/$CONSOLE_UID\"",
            "\"gui/$CONSOLE_UID/com.rabbitcontrol.agent\"",
            "'/Library/LaunchAgents/com.rabbitcontrol.agent.plist'",
        );

        assert!(wait_daemon.contains("launchctl print system/com.rabbitcontrol.daemon"));
        assert!(wait_agent.contains("launchctl print \"gui/$CONSOLE_UID/com.rabbitcontrol.agent\""));
        assert!(boot_daemon.contains("launchctl enable system/com.rabbitcontrol.daemon"));
        assert!(boot_daemon.contains(
            "launchctl bootstrap system '/Library/LaunchDaemons/com.rabbitcontrol.daemon.plist'"
        ));
        assert!(
            boot_agent.contains("launchctl enable \"gui/$CONSOLE_UID/com.rabbitcontrol.agent\"")
        );
        assert!(boot_agent.contains("launchctl bootstrap \"gui/$CONSOLE_UID\" '/Library/LaunchAgents/com.rabbitcontrol.agent.plist'"));

        #[cfg(unix)]
        {
            let full_script = format!(
                "set -e; CONSOLE_UID=501; {wait_daemon}; {wait_agent}; {boot_daemon}; {boot_agent}"
            );
            let status = std::process::Command::new("/bin/sh")
                .args(["-n", "-c", &full_script])
                .status()
                .expect("spawn /bin/sh -n");
            assert!(
                status.success(),
                "生成的 POSIX sh 脚本必须语法合法: {full_script}"
            );
        }
    }
}

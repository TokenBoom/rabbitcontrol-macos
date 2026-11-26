//! 本地后台服务（`rc-daemon` / `rc-host-agent`）与控制端 GUI 之间的安全 IPC 协议。
//!
//! 通信介质：Unix Domain Socket（`$run_dir/agent.sock`、`daemon.sock`），每条消息为单行 JSON，
//! 并附带由后台服务生成的 256-bit 随机十六进制共享密钥（`run/ipc.secret`）做常量时间校验，
//! 防止同机低权限进程冒充控制指令。非 Unix 平台提供零开销编译占位桩。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::host_bridge::HostApprovalPrompt;
use crate::permissions::PermissionSnapshot;
use crate::server_config::ServerStatusSnapshot;
use crate::signal_client::ConnectGrant;

/// 单条 IPC 请求/响应最大行字节数（256 KiB，防止异常写入耗尽内存）。
pub const MAX_IPC_LINE_BYTES: usize = 256 * 1024;

/// 共享 IPC 密钥随机字节长度（32 字节 = 64 位小写 hex）。
pub const IPC_SECRET_BYTES: usize = 32;

/// 后台被控服务当前所处会话类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceSessionType {
    /// macOS 登录窗口会话（密码登录前，仅限本人自控或显式固定密码）。
    LoginWindow,
    /// macOS 已登录桌面会话（拥有完整屏幕录制与辅助功能上下文）。
    Aqua,
    /// 未安装系统服务时由前台 GUI 内嵌托管的独立模式。
    Standalone,
}

/// 后台 `rc-host-agent` 暴露给前台 GUI 与状态栏菜单的状态快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSnapshot {
    /// 「允许远程协助」开关当前状态（仅本人自控监听运行但未开放他人协助时为 `false`）。
    pub enabled: bool,
    /// 被控端监听/信令循环是否在运行中。
    pub host_running: bool,
    /// 实际绑定的监听地址。
    pub listen_addr: Option<String>,
    /// 展示给对端的局域网直连地址。
    pub display_addr: Option<String>,
    /// 当前生效访问密码（临时验证码或自定义密码）。
    pub password: String,
    /// 是否为用户自定义密码。
    pub custom_password: bool,
    /// 当前视频后端标签（如 `"sck+videotoolbox"` / `"loginwindow"` / `"degraded"`）。
    pub backend: String,
    /// 当前已连接控制端描述。
    pub connected_peer: Option<String>,
    /// 当前待审批连接请求。
    pub pending_approval: Option<HostApprovalPrompt>,
    /// 当前本机 9 位设备码。
    pub device_id: Option<String>,
    /// 信令服务是否在线。
    pub signal_online: bool,
    /// 后台 Agent 运行所在会话类型。
    pub session_type: ServiceSessionType,
    /// 是否因缺少系统权限而处于受限降级状态。
    pub degraded_permissions: bool,
    /// 后台 Agent 实测系统权限快照。
    pub permissions: PermissionSnapshot,
}

/// 前台 GUI 或状态栏发往后台服务的请求消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ServiceRequest {
    /// 存活探针。
    Ping,
    /// 查询后台被控服务完整快照。
    GetSnapshot,
    /// 开启或关闭「允许他人远程协助」。
    SetRemoteAssist {
        /// 是否允许非同账号他人协助。
        enabled: bool,
    },
    /// 更新访问密码（`None` 或空串表示无密码/仅机主审批；临时验证码刷新请调用 `RotateTempCode`）。
    SetPassword {
        /// 新自定义密码；`None` 或空串表示仅机主审批无密码模式。
        password: Option<String>,
    },
    /// 立即刷新 8 位临时验证码。
    RotateTempCode,
    /// 请求 `rc-daemon` 统一串行刷新并返回有效登录会话（消除多进程单次 Refresh Token 重放）。
    EnsureAccessToken {
        /// 若调用方刚因某个旧 Access Token 收到 `401`，传入以便 `rc-daemon` 判断是否已由其他线程刷新过。
        failed_token: Option<String>,
    },
    /// 响应待审批连接请求。
    RespondApproval {
        /// 审批代次。
        generation: u64,
        /// `true` 放行，`false` 拒绝。
        accept: bool,
    },
    /// 主动断开当前正在控制本机的远端会话。
    DisconnectClient,
    /// 通知后台重新加载 `server.json` / `auth.json` / `certs` 并重连信令。
    RefreshAuthAndCerts,
    /// 通知后台重新加载 `prefs.json` 中的偏好配置。
    ReloadPrefs,
    /// 请求打开或激活前台控制端主界面。
    OpenMainGui,
    /// 通过后台信令长连接发起出站连接坐标申请（避免前台重复建立信令连接自踢）。
    RequestConnect {
        /// 目标 9 位设备码。
        target_id: String,
        /// 超时毫秒数。
        timeout_ms: u64,
    },
    /// 查询后台信令服务实时状态快照。
    GetServerStatus,
    /// 执行「整体退出」：写 `manual-stop` 标记并优雅关闭 agent 与 daemon。
    QuitAll,
    /// 检测到磁盘二进制已更新时请求平滑重启：不写 `manual-stop` 优雅退出，由 `launchd` 拉起新版。
    RestartForUpgrade,
    /// 请求 root `rc-daemon` 独立复核 DMG 的 SHA-256 与代码签名证书钉扎，并原子替换已安装的 `RabbitControl.app`。
    InstallVerifiedUpdate {
        /// 本地待验证安装的 DMG 路径。
        dmg_path: String,
        /// 期望的 64 位小写 SHA-256 十六进制摘要。
        expected_sha256: String,
        /// 请求替换的目标 `.app` 路径（由 `rc-daemon` 按白名单独立复核）。
        target_app_path: String,
    },
    /// 请求后台立刻重新检查系统权限快照。
    TriggerPermissionCheck,
}

/// 后台服务返回的 IPC 响应消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServiceResponse {
    /// 通用成功应答。
    Ok,
    /// `Ping` 存活应答。
    Pong {
        /// 服务端进程 PID。
        pid: u32,
        /// 服务端当前会话类型。
        session_type: ServiceSessionType,
        /// 服务端进程启动时自身可执行文件的 SHA-256 十六进制指纹（旧版本后台缺省为 `None`）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binary_sha256: Option<String>,
    },
    /// 状态快照应答。
    Snapshot(Box<AgentSnapshot>),
    /// 密码变更或临时验证码刷新应答。
    PasswordChanged {
        /// 最新生效密码。
        password: String,
        /// 是否为自定义密码。
        custom_password: bool,
    },
    /// 出站连接信令坐标应答。
    ConnectGrant(Box<ConnectGrant>),
    /// 后台信令服务器状态快照。
    ServerStatus(Box<ServerStatusSnapshot>),
    /// 最新有效的认证登录会话快照（由 `rc-daemon` 单点刷新返回）。
    AuthSession(Box<crate::auth_client::StoredAuthSession>),
    /// Refresh Token 已被吊销或需重新登录。
    ReloginRequired {
        /// 需要重新登录的原因。
        message: String,
    },
    /// 请求被拒绝或执行失败。
    Error {
        /// 人类可读错误原因。
        message: String,
    },
}

/// 带共享密钥鉴权信封的单条 IPC 请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcEnvelope {
    /// 与 `ipc.secret` 一致的共享密钥。
    pub secret: String,
    /// 实际请求载荷。
    pub req: ServiceRequest,
}

/// 常量时间校验客户端传入密钥是否与预期密钥一致。
pub fn verify_secret(expected: &str, provided: &str) -> bool {
    let a = expected.as_bytes();
    let b = provided.as_bytes();
    if a.is_empty() || a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub(crate) fn encode_hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

/// 流式计算指定路径文件的 SHA-256（64 位小写十六进制）；文件不存在或不可读时返回 `None`。
pub fn compute_file_sha256(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::{BufReader, Read};

    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(encode_hex_lower(&hasher.finalize()))
}

/// 计算当前进程可执行文件在启动时的 SHA-256 指纹；无法定位或读取时返回 `None`。
pub fn current_exe_sha256() -> Option<String> {
    std::env::current_exe()
        .ok()
        .and_then(|p| compute_file_sha256(&p))
}

/// 读取现有 IPC 共享密钥，或以安全随机源生成新密钥并写入 `path`。
///
/// 在系统服务托管目录（[`crate::config_paths::is_shared_system_path`]）下使用 `0660`，
/// 用户态目录下使用 `0600`，避免同机其他普通用户读取本地服务密钥。
pub fn read_or_create_ipc_secret(path: &Path) -> Result<String, String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let trimmed = existing.trim();
        if trimmed.len() >= 32 && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(trimmed.to_string());
        }
    }

    use rand::rngs::OsRng;
    use rand::RngCore;
    use std::fs::OpenOptions;
    use std::io::Write;
    let mut raw = [0u8; IPC_SECRET_BYTES];
    OsRng.fill_bytes(&mut raw);
    let hex = encode_hex_lower(&raw);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建 IPC 运行时目录失败 {}: {e}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = crate::config_paths::dir_mode_for(parent);
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(mode));
        }
    }

    let mut suffix = [0u8; 8];
    OsRng.fill_bytes(&mut suffix);
    let suffix_hex = encode_hex_lower(&suffix);
    let tmp_path = path.with_extension(format!("tmp.{}.{suffix_hex}", std::process::id()));
    #[cfg(unix)]
    let mode = crate::config_paths::file_mode_for(path);

    let write_res = (|| -> std::io::Result<()> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        let mut file = opts.open(&tmp_path)?;
        file.write_all(format!("{hex}\n").as_bytes())?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(mode));
        }
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "写入 IPC 临时密钥失败 {}: {err}",
            tmp_path.display()
        ));
    }

    // 使用 hard_link 原子抢占目标路径（仅当目标不存在时成功），彻底消除并发进程启动时的密钥脑裂；
    // 若已有同名文件但校验发现为空或损坏，则原子覆盖修复。
    match std::fs::hard_link(&tmp_path, path) {
        Ok(()) => {
            let _ = std::fs::remove_file(&tmp_path);
            Ok(hex)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match read_ipc_secret(path) {
            Ok(existing) => {
                let _ = std::fs::remove_file(&tmp_path);
                Ok(existing)
            }
            Err(_) => {
                // 使用跨进程文件锁串行化损坏密钥自愈，避免两个进程并发 rename 各自保留不同内存密钥。
                let _repair_lock =
                    crate::config_paths::acquire_file_lock(path, std::time::Duration::from_secs(3));
                if let Ok(repaired) = read_ipc_secret(path) {
                    let _ = std::fs::remove_file(&tmp_path);
                    return Ok(repaired);
                }
                if let Err(err) = std::fs::rename(&tmp_path, path) {
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(format!("修复损坏的 IPC 密钥失败 {}: {err}", path.display()));
                }
                Ok(hex)
            }
        },
        Err(_) => {
            let res = std::fs::rename(&tmp_path, path);
            if let Err(err) = res {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(format!("原子落盘 IPC 密钥失败 {}: {err}", path.display()));
            }
            Ok(hex)
        }
    }
}

/// 读取并校验现有 IPC 共享密钥（至少 32 位十六进制字符）。
pub fn read_ipc_secret(path: &Path) -> Result<String, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("读取 IPC 密钥失败 {}: {e}", path.display()))?;
    let trimmed = raw.trim();
    if trimmed.len() < 32 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("IPC 密钥格式无效或长度不足 {}", path.display()));
    }
    Ok(trimmed.to_string())
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    const MAX_INFLIGHT_IPC: usize = 16;

    struct InflightGuard(Arc<AtomicUsize>);
    impl Drop for InflightGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// 本地 Unix Domain Socket IPC 服务端。
    pub struct IpcServer {
        socket_path: PathBuf,
        sock_id: Option<(u64, u64)>,
        stop: Arc<AtomicBool>,
        inflight: Arc<AtomicUsize>,
        join: Option<JoinHandle<()>>,
    }

    impl IpcServer {
        /// 在 `socket_path` 启动单线非阻塞 accept 轮询的 IPC 服务端。
        pub fn bind<F>(socket_path: PathBuf, secret: String, handler: F) -> Result<Self, String>
        where
            F: Fn(ServiceRequest) -> ServiceResponse + Send + Sync + 'static,
        {
            if let Some(parent) = socket_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("创建 socket 目录失败 {}: {e}", parent.display()))?;
                let dir_mode = crate::config_paths::dir_mode_for(parent);
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(dir_mode));
            }
            if std::fs::symlink_metadata(&socket_path).is_ok() {
                let _ = std::fs::remove_file(&socket_path);
            }

            let listener = UnixListener::bind(&socket_path)
                .map_err(|e| format!("绑定 Unix Socket 失败 {}: {e}", socket_path.display()))?;
            let sock_mode = crate::config_paths::file_mode_for(&socket_path);
            let _ =
                std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(sock_mode));
            let sock_id = std::fs::symlink_metadata(&socket_path)
                .ok()
                .map(|m| (m.dev(), m.ino()));
            listener
                .set_nonblocking(true)
                .map_err(|e| format!("设置 Unix Socket 非阻塞失败: {e}"))?;

            let stop = Arc::new(AtomicBool::new(false));
            let stop_bg = Arc::clone(&stop);
            let handler = Arc::new(handler);
            let secret = Arc::new(secret);
            let inflight = Arc::new(AtomicUsize::new(0));
            let inflight_bg = Arc::clone(&inflight);
            let sock_clone = socket_path.clone();

            let join = thread::Builder::new()
                .name(format!(
                    "rc-ipc-{}",
                    sock_clone
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("srv")
                ))
                .spawn(move || {
                    while !stop_bg.load(Ordering::Acquire) {
                        match listener.accept() {
                            Ok((mut stream, _addr)) => {
                                let reserved = {
                                    let mut cur = inflight_bg.load(Ordering::Acquire);
                                    loop {
                                        if cur >= MAX_INFLIGHT_IPC {
                                            break false;
                                        }
                                        match inflight_bg.compare_exchange_weak(
                                            cur,
                                            cur + 1,
                                            Ordering::AcqRel,
                                            Ordering::Acquire,
                                        ) {
                                            Ok(_) => break true,
                                            Err(observed) => cur = observed,
                                        }
                                    }
                                };
                                if !reserved {
                                    if let Ok(mut busy) =
                                        serde_json::to_vec(&ServiceResponse::Error {
                                            message: "本地服务并发请求过多，请稍后重试".to_string(),
                                        })
                                    {
                                        busy.push(b'\n');
                                        let _ = stream.write_all(&busy);
                                    }
                                    continue;
                                }
                                let guard = InflightGuard(Arc::clone(&inflight_bg));
                                let sec = Arc::clone(&secret);
                                let h = Arc::clone(&handler);
                                if let Err(_e) = thread::Builder::new()
                                    .name("rc-ipc-worker".to_string())
                                    .spawn(move || {
                                        let _guard = guard;
                                        handle_client_stream(stream, sec.as_str(), h.as_ref());
                                    })
                                {
                                    // 线程创建失败时 guard 随闭包丢弃自动归还计数。
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(4));
                            }
                            Err(_) => {
                                thread::sleep(Duration::from_millis(50));
                            }
                        }
                    }
                })
                .map_err(|e| format!("启动 IPC 服务线程失败: {e}"))?;

            Ok(Self {
                socket_path,
                sock_id,
                stop,
                inflight,
                join: Some(join),
            })
        }

        /// 主动停止监听线程、短暂等待在途响应写回并清理 socket 文件（仅当 inode 仍属于本实例时才删除）。
        pub fn stop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(handle) = self.join.take() {
                let _ = handle.join();
            }
            if let Some(expected_id) = self.sock_id {
                let matches = std::fs::symlink_metadata(&self.socket_path)
                    .ok()
                    .map(|m| (m.dev(), m.ino()))
                    == Some(expected_id);
                if matches {
                    let _ = std::fs::remove_file(&self.socket_path);
                }
            }
            let deadline = std::time::Instant::now() + Duration::from_millis(150);
            while self.inflight.load(Ordering::Acquire) > 0 && std::time::Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(5));
            }
        }
    }

    impl Drop for IpcServer {
        fn drop(&mut self) {
            self.stop();
        }
    }

    fn handle_client_stream<F>(mut stream: UnixStream, expected_secret: &str, handler: &F)
    where
        F: Fn(ServiceRequest) -> ServiceResponse,
    {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));

        let Ok(cloned) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(cloned.take((MAX_IPC_LINE_BYTES + 1) as u64));
        let mut line = String::new();
        let response = match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(n) if n > MAX_IPC_LINE_BYTES => ServiceResponse::Error {
                message: "IPC 请求体过大".to_string(),
            },
            Ok(_) => match serde_json::from_str::<IpcEnvelope>(line.trim_end()) {
                Ok(env) => {
                    if !verify_secret(expected_secret, &env.secret) {
                        ServiceResponse::Error {
                            message: "IPC 鉴权失败".to_string(),
                        }
                    } else {
                        handler(env.req)
                    }
                }
                Err(e) => ServiceResponse::Error {
                    message: format!("IPC 请求解析失败: {e}"),
                },
            },
            Err(e) => ServiceResponse::Error {
                message: format!("IPC 读取失败: {e}"),
            },
        };

        if let Ok(mut bytes) = serde_json::to_vec(&response) {
            bytes.push(b'\n');
            let _ = stream.write_all(&bytes);
            let _ = stream.flush();
        }
    }

    /// 本地 Unix Domain Socket IPC 客户端。
    pub struct IpcClient;

    impl IpcClient {
        /// 向指定 Unix Socket 发送单次 IPC 请求并等待响应。
        pub fn call(
            socket_path: &Path,
            secret_path: &Path,
            req: ServiceRequest,
            timeout: Duration,
        ) -> Result<ServiceResponse, String> {
            let secret = read_ipc_secret(secret_path)?;
            Self::call_with_secret(socket_path, &secret, req, timeout)
        }

        /// 使用内存中的共享密钥向指定 Unix Socket 发送单次 IPC 请求。
        pub fn call_with_secret(
            socket_path: &Path,
            secret: &str,
            req: ServiceRequest,
            timeout: Duration,
        ) -> Result<ServiceResponse, String> {
            let mut stream = UnixStream::connect(socket_path)
                .map_err(|e| format!("连接本地服务失败 {}: {e}", socket_path.display()))?;
            let _ = stream.set_read_timeout(Some(timeout));
            let _ = stream.set_write_timeout(Some(timeout));

            let envelope = IpcEnvelope {
                secret: secret.to_string(),
                req,
            };
            let mut payload =
                serde_json::to_vec(&envelope).map_err(|e| format!("序列化 IPC 请求失败: {e}"))?;
            payload.push(b'\n');
            stream
                .write_all(&payload)
                .map_err(|e| format!("发送 IPC 请求失败: {e}"))?;
            stream
                .flush()
                .map_err(|e| format!("刷新 IPC 请求失败: {e}"))?;

            let mut reader = BufReader::new(stream.take((MAX_IPC_LINE_BYTES + 1) as u64));
            let mut line = String::new();
            let n = reader
                .read_line(&mut line)
                .map_err(|e| format!("读取 IPC 响应失败: {e}"))?;
            if n == 0 {
                return Err("本地服务提前关闭连接".to_string());
            }
            if n > MAX_IPC_LINE_BYTES {
                return Err("本地服务响应体积超限".to_string());
            }
            serde_json::from_str::<ServiceResponse>(line.trim_end())
                .map_err(|e| format!("解析 IPC 响应失败: {e}"))
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    /// 非 Unix 平台 IPC 服务端占位桩。
    pub struct IpcServer;

    impl IpcServer {
        /// 非 Unix 平台暂不支持 Unix Domain Socket IPC。
        pub fn bind<F>(_socket_path: PathBuf, _secret: String, _handler: F) -> Result<Self, String>
        where
            F: Fn(ServiceRequest) -> ServiceResponse + Send + Sync + 'static,
        {
            Err("当前平台暂不支持本地后台服务 Unix Socket IPC".to_string())
        }

        /// 占位停止方法。
        pub fn stop(&mut self) {}
    }

    /// 非 Unix 平台 IPC 客户端占位桩。
    pub struct IpcClient;

    impl IpcClient {
        /// 非 Unix 平台返回平台不支持错误。
        pub fn call(
            _socket_path: &Path,
            _secret_path: &Path,
            _req: ServiceRequest,
            _timeout: Duration,
        ) -> Result<ServiceResponse, String> {
            Err("当前平台暂不支持本地后台服务 Unix Socket IPC".to_string())
        }

        /// 非 Unix 平台返回平台不支持错误。
        pub fn call_with_secret(
            _socket_path: &Path,
            _secret: &str,
            _req: ServiceRequest,
            _timeout: Duration,
        ) -> Result<ServiceResponse, String> {
            Err("当前平台暂不支持本地后台服务 Unix Socket IPC".to_string())
        }
    }
}

pub use imp::{IpcClient, IpcServer};

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn secret_verification_is_strict() {
        assert!(!verify_secret("", ""));
        assert!(!verify_secret("abc", ""));
        assert!(!verify_secret("abc", "abd"));
        assert!(verify_secret("deadbeef01234567", "deadbeef01234567"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_ipc_roundtrip_and_auth_rejection() {
        use rand::rngs::OsRng;
        use rand::RngCore;
        use std::time::Duration;

        let mut b = [0u8; 8];
        OsRng.fill_bytes(&mut b);
        let root = std::env::temp_dir().join(format!("rc-ui-ipc-test-{}", encode_hex_lower(&b)));
        std::fs::create_dir_all(&root).unwrap();
        let sock = root.join("run/agent.sock");
        let secret_path = root.join("run/ipc.secret");
        let secret = read_or_create_ipc_secret(&secret_path).unwrap();

        let mut server = IpcServer::bind(sock.clone(), secret.clone(), |req| match req {
            ServiceRequest::Ping => ServiceResponse::Pong {
                pid: 42,
                session_type: ServiceSessionType::Aqua,
                binary_sha256: Some("abcd".repeat(16)),
            },
            ServiceRequest::SetRemoteAssist { enabled } => {
                ServiceResponse::Snapshot(Box::new(AgentSnapshot {
                    enabled,
                    host_running: true,
                    listen_addr: Some("0.0.0.0:9901".to_string()),
                    display_addr: Some("192.168.1.10:9901".to_string()),
                    password: "ABCD2345".to_string(),
                    custom_password: false,
                    backend: "sck+videotoolbox".to_string(),
                    connected_peer: None,
                    pending_approval: None,
                    device_id: Some("604928235".to_string()),
                    signal_online: true,
                    session_type: ServiceSessionType::Aqua,
                    degraded_permissions: false,
                    permissions: PermissionSnapshot::from_flags(true, true, true, true, None),
                }))
            }
            _ => ServiceResponse::Ok,
        })
        .unwrap();

        let resp = IpcClient::call(
            &sock,
            &secret_path,
            ServiceRequest::Ping,
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(
            resp,
            ServiceResponse::Pong {
                pid: 42,
                session_type: ServiceSessionType::Aqua,
                binary_sha256: Some("abcd".repeat(16)),
            }
        );

        let snap_resp = IpcClient::call(
            &sock,
            &secret_path,
            ServiceRequest::SetRemoteAssist { enabled: true },
            Duration::from_secs(2),
        )
        .unwrap();
        match snap_resp {
            ServiceResponse::Snapshot(snap) => {
                assert!(snap.enabled);
                assert_eq!(snap.device_id.as_deref(), Some("604928235"));
            }
            other => panic!("unexpected response: {other:?}"),
        }

        let bad_resp = IpcClient::call_with_secret(
            &sock,
            "wrong-secret",
            ServiceRequest::Ping,
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(matches!(bad_resp, ServiceResponse::Error { .. }));

        server.stop();
        assert!(!sock.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_ipc_secret_creation_converges_and_inode_guard_preserves_new_socket() {
        use rand::rngs::OsRng;
        use rand::RngCore;
        use std::sync::Barrier;
        use std::time::Duration;

        let mut b = [0u8; 8];
        OsRng.fill_bytes(&mut b);
        let root = std::env::temp_dir().join(format!("rc-ui-ipc-race-{}", encode_hex_lower(&b)));
        std::fs::create_dir_all(&root).unwrap();
        let secret_path = root.join("run/ipc.secret");
        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let p = secret_path.clone();
            let bar = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                bar.wait();
                read_or_create_ipc_secret(&p).unwrap()
            }));
        }
        let secrets: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for s in &secrets {
            assert_eq!(
                s, &secrets[0],
                "并发生成/读取 ipc.secret 必须收敛至同一密钥"
            );
        }

        let sock = root.join("run/agent.sock");
        let mut old_srv =
            IpcServer::bind(sock.clone(), secrets[0].clone(), |_| ServiceResponse::Ok).unwrap();
        let mut new_srv = IpcServer::bind(sock.clone(), secrets[0].clone(), |_| {
            ServiceResponse::Pong {
                pid: 99,
                session_type: ServiceSessionType::Aqua,
                binary_sha256: None,
            }
        })
        .unwrap();

        // 旧进程退出调用 stop() 时，绝不可删除新进程已重绑的 agent.sock。
        old_srv.stop();
        assert!(
            sock.exists(),
            "旧 IpcServer::stop() 不应删除新实例的 socket"
        );
        let pong = IpcClient::call(
            &sock,
            &secret_path,
            ServiceRequest::Ping,
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(
            pong,
            ServiceResponse::Pong {
                pid: 99,
                session_type: ServiceSessionType::Aqua,
                binary_sha256: None,
            }
        );

        new_srv.stop();
        assert!(!sock.exists());

        // 若原有 ipc.secret 为空或损坏短串，read_or_create_ipc_secret 必须自愈覆盖为合法密钥。
        let corrupt_path = root.join("run/corrupt.secret");
        std::fs::write(&corrupt_path, b"bad\n").unwrap();
        let repaired = read_or_create_ipc_secret(&corrupt_path).unwrap();
        assert!(repaired.len() >= 32 && repaired.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(read_ipc_secret(&corrupt_path).unwrap(), repaired);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn compute_file_sha256_matches_known_vector_and_handles_missing_file() {
        use rand::rngs::OsRng;
        use rand::RngCore;

        let mut b = [0u8; 8];
        OsRng.fill_bytes(&mut b);
        let root = std::env::temp_dir().join(format!("rc-ui-sha256-test-{}", encode_hex_lower(&b)));
        std::fs::create_dir_all(&root).unwrap();

        let missing = root.join("nonexistent.bin");
        assert!(compute_file_sha256(&missing).is_none());

        let sample = root.join("abc.bin");
        std::fs::write(&sample, b"abc").unwrap();
        assert_eq!(
            compute_file_sha256(&sample).as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn pong_legacy_json_compat_and_restart_for_upgrade_roundtrip() {
        use rand::rngs::OsRng;
        use rand::RngCore;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        // 1) 无 binary_sha256 字段的旧版本 Pong JSON 必须能兼容反序列化为 None。
        let legacy_json = r#"{"kind":"pong","pid":123,"session_type":"aqua"}"#;
        let parsed: ServiceResponse = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(
            parsed,
            ServiceResponse::Pong {
                pid: 123,
                session_type: ServiceSessionType::Aqua,
                binary_sha256: None,
            }
        );

        // 2) RestartForUpgrade 经 IpcServer/IpcClient 正常往返，且由服务端标记升级退出。
        let mut b = [0u8; 8];
        OsRng.fill_bytes(&mut b);
        let root = std::env::temp_dir().join(format!("rc-ui-ipc-upgrade-{}", encode_hex_lower(&b)));
        std::fs::create_dir_all(&root).unwrap();
        let sock = root.join("run/agent.sock");
        let secret_path = root.join("run/ipc.secret");
        let secret = read_or_create_ipc_secret(&secret_path).unwrap();

        let restart_called = Arc::new(AtomicBool::new(false));
        let restart_flag = Arc::clone(&restart_called);
        let mut server = IpcServer::bind(sock.clone(), secret, move |req| match req {
            ServiceRequest::RestartForUpgrade => {
                restart_flag.store(true, Ordering::Release);
                ServiceResponse::Ok
            }
            _ => ServiceResponse::Error {
                message: "unexpected".to_string(),
            },
        })
        .unwrap();

        let resp = IpcClient::call(
            &sock,
            &secret_path,
            ServiceRequest::RestartForUpgrade,
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(resp, ServiceResponse::Ok);
        assert!(restart_called.load(Ordering::Acquire));

        server.stop();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn ipc_server_stop_drains_inflight_response_before_returning() {
        use rand::rngs::OsRng;
        use rand::RngCore;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::time::Duration;

        let mut b = [0u8; 8];
        OsRng.fill_bytes(&mut b);
        let root = std::env::temp_dir().join(format!("rc-ui-ipc-drain-{}", encode_hex_lower(&b)));
        std::fs::create_dir_all(&root).unwrap();
        let sock = root.join("run/agent.sock");
        let secret_path = root.join("run/ipc.secret");
        let secret = read_or_create_ipc_secret(&secret_path).unwrap();

        let (entered_tx, entered_rx) = mpsc::channel();
        let handler_finished = Arc::new(AtomicBool::new(false));
        let finished_flag = Arc::clone(&handler_finished);
        let mut server = IpcServer::bind(sock.clone(), secret, move |req| match req {
            ServiceRequest::RestartForUpgrade => {
                let _ = entered_tx.send(());
                std::thread::sleep(Duration::from_millis(40));
                finished_flag.store(true, Ordering::Release);
                ServiceResponse::Ok
            }
            _ => ServiceResponse::Error {
                message: "unexpected".to_string(),
            },
        })
        .unwrap();

        let sock_c = sock.clone();
        let sec_c = secret_path.clone();
        let client_jh = std::thread::spawn(move || {
            IpcClient::call(
                &sock_c,
                &sec_c,
                ServiceRequest::RestartForUpgrade,
                Duration::from_secs(2),
            )
        });

        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker must enter handler");
        server.stop();
        assert!(
            handler_finished.load(Ordering::Acquire),
            "stop() 必须在等待在途 worker 完成处理并释放 InflightGuard 后才返回"
        );
        let resp = client_jh
            .join()
            .unwrap()
            .expect("client must receive complete response before stop returns");
        assert_eq!(resp, ServiceResponse::Ok);

        let _ = std::fs::remove_dir_all(&root);
    }
}

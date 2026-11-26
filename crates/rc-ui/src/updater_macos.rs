//! macOS 原生自动更新与远程更新执行器。
//!
//! 职责：
//! 1. 通过 macOS 系统 `/usr/bin/curl`（`-q --globoff --proto =https --proto-redir =https --tlsv1.2`，
//!    复用系统代理与根证书链，流式限额读取防止 OOM）拉取 GitHub Releases 最新版本元数据与
//!    `SHA256SUMS.txt` / `.dmg.sha256` 校验摘要；
//! 2. 流式下载安装包至临时文件（支持取消与进度回调，硬性限制最大 200 MiB），完成后做流式
//!    SHA-256 常量时间校验；
//! 3. 使用 `DirBuilder::mode(0o700)` 原子创建私有隔离暂存目录，以 `O_NOFOLLOW | O_NONBLOCK`
//!    与 `fstat` 防 TOCTOU/FIFO 阻塞复制并重算 SHA-256（daemon 侧额外限制输入路径白名单且不回显实际摘要
//!    以消除 root 文件哈希预言机），只读挂载（RAII 自动 `hdiutil detach`），校验内部 `RabbitControl.app`
//!    的版本号严格防降级，并通过 macOS Security.framework 原生 `codesign --verify --deep --strict -R`
//!    对主包、`rc-daemon` 与 `rc-host-agent` 执行密码学证书叶钉扎校验；
//! 4. 原子替换目标 `.app`（同目录 staging + 失败自动回滚 `.bak`）：
//!    - 已安装系统级后台服务且 `daemon.sock` 在线时，经认证 IPC 交由 root `rc-daemon` 独立复核并
//!      免密替换，随后先重启 `rc-daemon` 再重启 `rc-host-agent`（触发 `ByeReason::SessionSwitching`
//!      供控制端无缝重连）；
//!    - 独立运行时若目标目录可写则直接原子替换并调用平滑退出钩子（发送 `ByeReason::SessionSwitching`）
//!      后重启；若无写权限且为前台本机自更新则经单次 `osascript` 管理员授权，在 root 脚本内拷贝至
//!      root-owned `0700` 目录并二次执行 `codesign -R` 校验后再原子替换；若为远控无头模式则快速失败
//!      并引导安装系统级后台服务。

#![deny(unsafe_code)]
#![deny(missing_docs)]

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::config_paths;
use crate::platform::{register_update_hooks, ClientUpdateHooks};
use crate::service_ipc::{IpcClient, ServiceRequest, ServiceResponse};
use crate::service_manager::{self, APP_BUNDLE_ID};
use crate::updater::{
    apply_mirror_prefix, attach_sha256sums, constant_time_eq_hex, find_sha256sums_asset,
    find_sidecar_sha256_asset, is_newer_version, is_valid_sha256_hex, parse_github_release_json,
    sanitize_update_mirror_url, validate_update_https_url, ReleaseInfo, SemVer,
    MAX_UPDATE_PACKAGE_BYTES,
};

/// GitHub 最新 Release API 地址。
pub const GITHUB_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/TokenBoom/rabbitcontrol-macos/releases/latest";

/// CI 官方 10 年发布证书叶 SHA-1（`RabbitControl Release`，见 `AGENTS.md §8`）。
pub const RELEASE_CERT_LEAF_SHA1: &str = "51f7f9b383251796473ae3d9cc3b1fd37e0be3fa";

/// 本机开发固定签名证书叶 SHA-1（`RabbitControl Dev`，仅限当前进程自身同样以此开发签运行时自测）。
pub const DEV_CERT_LEAF_SHA1: &str = "51f7737f3037ef8858974ff226ac37531b66eea3";

/// Release JSON 最大允许拉取体积（2 MiB）。
const MAX_RELEASE_JSON_BYTES: usize = 2 * 1024 * 1024;

/// `SHA256SUMS.txt` / `.sha256` 最大允许拉取体积（256 KiB）。
const MAX_SHA256_TEXT_BYTES: usize = 256 * 1024;

/// curl 请求元数据超时（秒）。
const CURL_META_TIMEOUT_SECS: u64 = 25;

/// curl 下载安装包总超时（秒）。
const CURL_DOWNLOAD_TIMEOUT_SECS: u64 = 600;

/// macOS `open(2)` `O_NONBLOCK` 标志位常量（防止 FIFO 永久阻塞）。
#[cfg(target_os = "macos")]
const MACOS_O_NONBLOCK: i32 = 0x0004;

/// macOS `open(2)` `O_NOFOLLOW` 标志位常量（拒绝末端符号链接 TOCTOU）。
#[cfg(target_os = "macos")]
const MACOS_O_NOFOLLOW: i32 = 0x0100;

/// 独立模式升级重启前的平滑停机回调（由 `rc-ui` 注册，用于向对端发送 `ByeReason::SessionSwitching`）。
static STANDALONE_PRE_RESTART_HOOK: OnceLock<Arc<dyn Fn() + Send + Sync>> = OnceLock::new();

/// 注册独立模式升级重启前的平滑停机回调（仅首次调用生效）。
pub fn register_standalone_pre_restart_hook(hook: Arc<dyn Fn() + Send + Sync>) -> bool {
    STANDALONE_PRE_RESTART_HOOK.set(hook).is_ok()
}

/// 生成 16 位随机十六进制后缀，用于临时目录与 staging 命名。
fn random_hex8() -> String {
    let mut b = [0u8; 8];
    OsRng.fill_bytes(&mut b);
    crate::service_ipc::encode_hex_lower(&b)
}

/// 在 Unix 上以原子 `0700` 权限创建全新私有临时目录（绝不先以 `0755` 创建再 `chmod`）。
fn create_private_dir_0700(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|e| format!("创建私有临时目录失败 {}: {e}", path.display()))
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path).map_err(|e| format!("创建临时目录失败 {}: {e}", path.display()))
    }
}

/// 安全打开普通文件（在 macOS 上附加 `O_NOFOLLOW | O_NONBLOCK`，并在打开后以 `fstat` 复核常规文件类型）。
fn open_regular_file_no_follow(path: &Path) -> Result<(File, u64), String> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(MACOS_O_NOFOLLOW | MACOS_O_NONBLOCK);
    }
    let file = opts
        .open(path)
        .map_err(|e| format!("打开安装包失败 {}: {e}", path.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("读取安装包文件属性失败 {}: {e}", path.display()))?;
    if !meta.file_type().is_file() {
        return Err("安装包路径必须为普通文件（拒绝符号链接、管道或设备文件）".to_string());
    }
    Ok((file, meta.len()))
}

/// 从 `Info.plist` XML 文本中提取指定 `<key>` 对应的 `<string>` 值。
pub fn parse_plist_string_value(plist_xml: &str, key: &str) -> Option<String> {
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

/// 从指定 `.app` Bundle 内的 `Contents/Info.plist` 读取语义版本号（`CFBundleShortVersionString`）。
pub fn read_app_bundle_version(app_path: &Path) -> Option<String> {
    let plist_path = app_path.join("Contents/Info.plist");
    let xml = fs::read_to_string(plist_path).ok()?;
    parse_plist_string_value(&xml, "CFBundleShortVersionString")
        .map(|s| s.trim().trim_start_matches(['v', 'V']).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 解析当前运行进程所属应用包的真实语义版本号（`Info.plist` → `RC_APP_VERSION` → `CARGO_PKG_VERSION`）。
pub fn resolve_current_app_version() -> String {
    let plist_ver = std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.parent()?
                .parent()
                .map(|contents| contents.join("Info.plist"))
        })
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|xml| parse_plist_string_value(&xml, "CFBundleShortVersionString"))
        .map(|s| s.trim().trim_start_matches(['v', 'V']).trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(v) = plist_ver {
        return v;
    }
    let compile_v = env!("RC_APP_VERSION")
        .trim()
        .trim_start_matches(['v', 'V'])
        .trim();
    if !compile_v.is_empty() {
        return compile_v.to_string();
    }
    env!("CARGO_PKG_VERSION")
        .trim()
        .trim_start_matches(['v', 'V'])
        .trim()
        .to_string()
}

/// 返回规范化 CPU 架构（`"arm64"` / `"x86_64"`）。
pub fn current_macos_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        std::env::consts::ARCH
    }
}

/// 根据当前可执行文件路径推断其所属 `.app` Bundle 根目录（不强制要求包含全部子二进制）。
pub fn current_exe_app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let macos_dir = exe.parent()?;
    if macos_dir.file_name().and_then(|s| s.to_str()) != Some("MacOS") {
        return None;
    }
    let contents_dir = macos_dir.parent()?;
    if contents_dir.file_name().and_then(|s| s.to_str()) != Some("Contents") {
        return None;
    }
    let bundle_dir = contents_dir.parent()?;
    let is_app = bundle_dir
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("app"));
    is_app.then(|| bundle_dir.to_path_buf())
}

/// 判断当前运行的可执行文件是否为 `rc-host-agent`。
fn is_running_as_host_agent() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .is_some_and(|name| name == "rc-host-agent")
}

/// 定位当前应被升级替换的目标 `RabbitControl.app` 路径。
pub fn resolve_target_app_bundle() -> PathBuf {
    if let Some(bundle) = current_exe_app_bundle() {
        return bundle;
    }
    if let Some(bundle) = service_manager::locate_app_bundle() {
        return bundle;
    }
    PathBuf::from("/Applications/RabbitControl.app")
}

/// 校验目标 `.app` 路径格式是否安全合法（绝对路径、无控制字符、无 `..`、以 `.app` 结尾且非根或 `/Applications`）。
pub fn validate_target_app_path(path: &Path) -> Result<(), String> {
    let raw = path
        .to_str()
        .ok_or_else(|| "目标应用路径不是合法 UTF-8".to_string())?;
    if raw.chars().any(|c| c.is_control()) {
        return Err("目标应用路径包含非法控制字符".to_string());
    }
    if !path.is_absolute() {
        return Err("目标应用路径必须为绝对路径".to_string());
    }
    for comp in path.components() {
        if matches!(comp, Component::ParentDir | Component::CurDir) {
            return Err("目标应用路径包含非法相对路径段".to_string());
        }
    }
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return Err("目标应用路径缺少文件名".to_string());
    };
    if !name.ends_with(".app") || name == ".app" {
        return Err("目标应用路径必须以 .app 结尾".to_string());
    }
    if path.parent().is_none() || path == Path::new("/Applications") {
        return Err("目标应用路径不合法".to_string());
    }
    Ok(())
}

/// 校验 `rc-daemon`（root 权限）允许替换的目标路径白名单：
/// 仅允许替换当前 `rc-daemon` 所属 `.app` Bundle 或 `/Applications/RabbitControl.app`，
/// 防止即使本机低权限进程拿到 IPC 密钥也无法覆写系统任意路径。
pub fn validate_daemon_target_app_path(requested: &Path) -> Result<PathBuf, String> {
    validate_target_app_path(requested)?;
    let requested_canon = canonicalize_app_target(requested)?;
    let mut allowed: Vec<PathBuf> = Vec::new();
    if let Ok(p) = canonicalize_app_target(Path::new("/Applications/RabbitControl.app")) {
        allowed.push(p);
    }
    if let Some(own) = current_exe_app_bundle() {
        if let Ok(p) = canonicalize_app_target(&own) {
            allowed.push(p);
        }
    }
    if allowed.iter().any(|a| a == &requested_canon) {
        Ok(requested_canon)
    } else {
        Err(format!(
            "拒绝替换非白名单路径（仅允许更新当前已安装的 RabbitControl.app）: {}",
            requested.display()
        ))
    }
}

/// 校验 `rc-daemon`（root 权限）接受的输入 DMG 路径：
/// 必须位于受信任临时目录（`/tmp` / `/private/tmp` / `/var/folders` / `/private/var/folders`）且文件名为 `rc-*.dmg`，
/// 防止非特权用户借由 root 守护进程探测任意系统敏感文件。
pub fn validate_daemon_dmg_path(dmg_path: &Path) -> Result<PathBuf, String> {
    let raw = dmg_path
        .to_str()
        .ok_or_else(|| "安装包路径不是合法 UTF-8".to_string())?;
    if raw.chars().any(|c| c.is_control()) || !dmg_path.is_absolute() {
        return Err("安装包路径格式无效".to_string());
    }
    for comp in dmg_path.components() {
        if matches!(comp, Component::ParentDir | Component::CurDir) {
            return Err("安装包路径包含非法相对路径段".to_string());
        }
    }
    let Some(file_name) = dmg_path.file_name().and_then(|s| s.to_str()) else {
        return Err("安装包缺少文件名".to_string());
    };
    let lower_name = file_name.to_ascii_lowercase();
    if !lower_name.starts_with("rc-") || !lower_name.ends_with(".dmg") {
        return Err("仅允许从受控临时更新安装包（rc-*.dmg）执行升级".to_string());
    }
    let parent = dmg_path
        .parent()
        .ok_or_else(|| "安装包路径缺少父目录".to_string())?;
    let canon_parent = fs::canonicalize(parent)
        .map_err(|e| format!("解析安装包父目录失败 {}: {e}", parent.display()))?;
    let allowed_roots = [
        Path::new("/tmp"),
        Path::new("/private/tmp"),
        Path::new("/var/folders"),
        Path::new("/private/var/folders"),
    ];
    if !allowed_roots
        .iter()
        .any(|root| canon_parent.starts_with(root))
    {
        return Err("仅允许从系统临时目录中的更新包执行升级".to_string());
    }
    Ok(canon_parent.join(file_name))
}

fn canonicalize_app_target(path: &Path) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "目标应用路径缺少父目录".to_string())?;
    let name = path
        .file_name()
        .ok_or_else(|| "目标应用路径缺少文件名".to_string())?;
    let canon_parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    Ok(canon_parent.join(name))
}

/// 构造供 macOS Security.framework `codesign --verify -R` 直接求值的显式内联证书叶钉扎表达式。
///
/// 必须带前导 `=`（否则 `codesign -R` 会将参数当作文件路径解析！）。
/// 相比解析 `codesign -dvv` 文本输出，`-R '=...'` 由系统内核/Security.framework 对真实 CMS 证书叶做密码学
/// 核验，彻底免疫 `or always`、多分支 designated requirement、CWD 文件注入或 `Authority=` 文本伪造攻击。
pub fn build_codesign_requirement(identifier: &str, leaf_sha1: &str) -> String {
    format!("=identifier \"{identifier}\" and certificate leaf = H\"{leaf_sha1}\"")
}

/// 返回当前运行二进制自身编译期固化版本号（不可被外部篡改的防降级下界）。
pub fn compiled_app_version() -> String {
    let compile_v = env!("RC_APP_VERSION")
        .trim()
        .trim_start_matches(['v', 'V'])
        .trim();
    if !compile_v.is_empty() {
        return compile_v.to_string();
    }
    env!("CARGO_PKG_VERSION")
        .trim()
        .trim_start_matches(['v', 'V'])
        .trim()
        .to_string()
}

/// 计算已安装版本防降级下界：取目标 `.app` 磁盘 `Info.plist`、当前运行 `.app` `Info.plist` 与
/// 编译期固化版本三者中的最高合法 SemVer；若目标 `.app` 存在但其 `Info.plist` 损坏无法解析则 fail-closed 报错。
pub fn resolve_installed_version_floor(
    target_app_path: &Path,
    current_app_bundle: Option<&Path>,
) -> Result<String, String> {
    let mut best: Option<(SemVer, String)> = None;
    let mut consider = |raw: String| {
        if let Some(sv) = SemVer::parse(&raw) {
            let replace = match &best {
                Some((cur_sv, _)) => sv > *cur_sv,
                None => true,
            };
            if replace {
                best = Some((sv, raw));
            }
        }
    };

    if target_app_path.exists() {
        let on_disk = read_app_bundle_version(target_app_path).ok_or_else(|| {
            "现有应用 Info.plist 缺失或版本字段无效，拒绝执行降级风险安装".to_string()
        })?;
        if SemVer::parse(&on_disk).is_none() {
            return Err(format!(
                "现有应用版本号无法解析（{on_disk}），拒绝执行降级风险安装"
            ));
        }
        consider(on_disk);
    }
    if let Some(cur) = current_app_bundle {
        if let Some(v) = read_app_bundle_version(cur) {
            consider(v);
        }
    }
    consider(compiled_app_version());

    best.map(|(_, s)| s)
        .ok_or_else(|| "无法确定当前已安装版本下界，已拒绝安装".to_string())
}

/// 校验候选版本相对当前已安装版本满足严格升级单调性（防官方旧版签名包回滚降级攻击，无法解析时 fail-closed）。
pub fn check_version_monotonicity(
    candidate_version: &str,
    installed_version: Option<&str>,
) -> Result<(), String> {
    if SemVer::parse(candidate_version).is_none() {
        return Err(format!("更新包版本号无效: {candidate_version}"));
    }
    if let Some(installed) = installed_version {
        if SemVer::parse(installed).is_none() {
            return Err(format!("已安装版本号无效（{installed}），已拒绝安装"));
        }
        if !is_newer_version(candidate_version, installed) {
            return Err(format!(
                "拒绝降级或重复安装：目标版本 v{candidate_version} 不高于已安装版本 v{installed}"
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_codesign_requirement(path: &Path, identifier: &str, leaf_sha1: &str, deep: bool) -> bool {
    let req = build_codesign_requirement(identifier, leaf_sha1);
    let mut cmd = Command::new("/usr/bin/codesign");
    cmd.arg("--verify");
    if deep {
        cmd.arg("--deep");
    }
    cmd.args(["--strict", "-R", &req]).arg(path);
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// 根据当前运行的应用包自身已验证签名，计算允许的证书叶 SHA-1 列表：
/// - 永远允许官方发布证书 `RELEASE_CERT_LEAF_SHA1`；
/// - 仅当当前运行中的 `.app` 自身通过 `codesign -R` 证实为 `DEV_CERT_LEAF_SHA1` 时，才额外允许 dev-to-dev 自测。
#[cfg(target_os = "macos")]
fn allowed_leaf_sha1s(current_app_bundle: Option<&Path>) -> Vec<&'static str> {
    let mut list = vec![RELEASE_CERT_LEAF_SHA1];
    if let Some(cur) = current_app_bundle {
        if verify_codesign_requirement(cur, APP_BUNDLE_ID, DEV_CERT_LEAF_SHA1, true) {
            list.push(DEV_CERT_LEAF_SHA1);
        }
    }
    list
}

/// 对候选用 `.app` 及其内嵌 `rc-daemon` / `rc-host-agent` 执行 macOS 原生 `codesign -R` 密码学证书叶钉扎校验。
///
/// 校验通过后返回命中的受信任证书叶 SHA-1（供后续 root 脚本二次复核复用）。
#[cfg(target_os = "macos")]
fn verify_candidate_app_codesign(
    candidate_app: &Path,
    current_app_bundle: Option<&Path>,
) -> Result<&'static str, String> {
    let daemon_bin = candidate_app.join("Contents/MacOS/rc-daemon");
    let agent_bin = candidate_app.join("Contents/MacOS/rc-host-agent");
    let daemon_id = format!("{APP_BUNDLE_ID}.daemon");
    let agent_id = format!("{APP_BUNDLE_ID}.agent");

    for leaf in allowed_leaf_sha1s(current_app_bundle) {
        let app_ok = verify_codesign_requirement(candidate_app, APP_BUNDLE_ID, leaf, true);
        let daemon_ok = verify_codesign_requirement(&daemon_bin, &daemon_id, leaf, false);
        let agent_ok = verify_codesign_requirement(&agent_bin, &agent_id, leaf, false);
        if app_ok && daemon_ok && agent_ok {
            return Ok(leaf);
        }
    }
    Err("更新包或其后台服务组件未通过官方证书钉扎校验（codesign -R），已拒绝安装".to_string())
}

/// 从 `curl -D` 响应头文件中解析最后一次（重定向后的）`Content-Length`。
///
/// 每次遇到新的 `HTTP/` 状态行时归零，避免最终响应采用 chunked 传输时误继承重定向响应体的小长度。
pub fn parse_last_content_length(headers_text: &str) -> u64 {
    let mut last = 0u64;
    for line in headers_text.lines() {
        let trimmed = line.trim();
        if trimmed
            .as_bytes()
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"HTTP/"))
        {
            last = 0;
            continue;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                if let Ok(val) = v.trim().parse::<u64>() {
                    if val > 0 {
                        last = val;
                    }
                }
            }
        }
    }
    last
}

/// 解析 `.sha256` sidecar 单行校验文件中的 64 位 SHA-256 十六进制串。
pub fn parse_single_sha256_sidecar(text: &str) -> Option<String> {
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let first = line.split_whitespace().next()?;
        let lower = first.to_ascii_lowercase();
        if is_valid_sha256_hex(&lower) {
            return Some(lower);
        }
    }
    None
}

/// 通过 `/usr/bin/curl` 流式限额拉取受限体积的 HTTPS 文本响应（禁用 `.curlrc` 与 glob 展开，超出 `max_bytes` 立即终止防 OOM）。
fn curl_get_text(
    url: &str,
    accept: Option<&str>,
    timeout_secs: u64,
    max_bytes: usize,
) -> Result<String, String> {
    validate_update_https_url(url)?;
    let mut cmd = Command::new("/usr/bin/curl");
    cmd.args([
        "-q",
        "--globoff",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--tlsv1.2",
        "--fail",
        "--location",
        "--max-redirs",
        "5",
        "--silent",
        "--show-error",
        "--connect-timeout",
        "15",
        "--max-time",
        &timeout_secs.to_string(),
        "--max-filesize",
        &max_bytes.to_string(),
        "--user-agent",
        "RabbitControl-Updater",
    ]);
    if let Some(a) = accept {
        cmd.args(["-H", a]);
    }
    cmd.arg("--")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("调用 curl 请求更新信息失败: {e}"))?;

    let mut stdout_buf = Vec::new();
    let mut exceeded = false;
    if let Some(mut stdout) = child.stdout.take() {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            let n = stdout
                .read(&mut chunk)
                .map_err(|e| format!("读取更新元数据失败: {e}"))?;
            if n == 0 {
                break;
            }
            if stdout_buf.len().saturating_add(n) > max_bytes {
                exceeded = true;
                let _ = child.kill();
                break;
            }
            stdout_buf.extend_from_slice(&chunk[..n]);
        }
    }

    let mut stderr_buf = String::new();
    if let Some(stderr) = child.stderr.take() {
        let _ = stderr.take(16 * 1024).read_to_string(&mut stderr_buf);
    }

    let status = child
        .wait()
        .map_err(|e| format!("等待 curl 进程退出失败: {e}"))?;
    if exceeded {
        return Err("更新元数据响应体积超限".to_string());
    }
    if !status.success() {
        let stderr = stderr_buf.trim();
        return Err(if stderr.is_empty() {
            format!("请求更新服务器失败（退出码 {:?}）", status.code())
        } else {
            format!("请求更新服务器失败: {stderr}")
        });
    }
    String::from_utf8(stdout_buf).map_err(|e| format!("更新元数据不是合法 UTF-8: {e}"))
}

/// 拉取 GitHub 最新 Release 元数据并附带各架构安装包的 SHA-256 校验值。
pub fn fetch_latest_release(mirror_url: &str) -> Result<ReleaseInfo, String> {
    let clean_mirror = sanitize_update_mirror_url(mirror_url)?;
    let json = match curl_get_text(
        GITHUB_LATEST_RELEASE_URL,
        Some("Accept: application/vnd.github+json"),
        CURL_META_TIMEOUT_SECS,
        MAX_RELEASE_JSON_BYTES,
    ) {
        Ok(body) => body,
        Err(direct_err) => {
            if clean_mirror.is_empty() {
                return Err(direct_err);
            }
            let mirrored = apply_mirror_prefix(&clean_mirror, GITHUB_LATEST_RELEASE_URL)?;
            curl_get_text(
                &mirrored,
                Some("Accept: application/vnd.github+json"),
                CURL_META_TIMEOUT_SECS,
                MAX_RELEASE_JSON_BYTES,
            )
            .map_err(|mirror_err| {
                format!(
                    "直连与镜像均无法获取最新版本信息（直连: {direct_err}；镜像: {mirror_err}）"
                )
            })?
        }
    };

    let mut info = parse_github_release_json(&json)?;

    // 优先拉取 SHA256SUMS.txt 批量绑定各资产摘要。
    if let Some(sums_asset) = find_sha256sums_asset(&info).cloned() {
        if let Ok(sums_text) =
            fetch_text_with_optional_mirror(&clean_mirror, &sums_asset.download_url)
        {
            attach_sha256sums(&mut info, &sums_text);
        }
    }

    // 对尚未附带 SHA-256 的 `.dmg` 资产，回退拉取同名 `.dmg.sha256` sidecar 校验文件。
    let missing_dmgs: Vec<String> = info
        .assets
        .iter()
        .filter(|a| a.name.to_ascii_lowercase().ends_with(".dmg") && a.sha256.is_none())
        .map(|a| a.name.clone())
        .collect();
    for dmg_name in missing_dmgs {
        if let Some(sidecar) = find_sidecar_sha256_asset(&info, &dmg_name).cloned() {
            if let Ok(text) = fetch_text_with_optional_mirror(&clean_mirror, &sidecar.download_url)
            {
                if let Some(hash) = parse_single_sha256_sidecar(&text) {
                    if let Some(target) = info.assets.iter_mut().find(|a| a.name == dmg_name) {
                        target.sha256 = Some(hash);
                    }
                }
            }
        }
    }

    Ok(info)
}

fn fetch_text_with_optional_mirror(clean_mirror: &str, raw_url: &str) -> Result<String, String> {
    if !clean_mirror.is_empty() {
        if let Ok(mirrored) = apply_mirror_prefix(clean_mirror, raw_url) {
            if let Ok(body) = curl_get_text(
                &mirrored,
                None,
                CURL_META_TIMEOUT_SECS,
                MAX_SHA256_TEXT_BYTES,
            ) {
                return Ok(body);
            }
        }
    }
    curl_get_text(raw_url, None, CURL_META_TIMEOUT_SECS, MAX_SHA256_TEXT_BYTES)
}

/// 流式校验单个常规文件（使用 `O_NOFOLLOW | O_NONBLOCK` + `fstat` 拒绝软链与 FIFO，限制最大体积）的 SHA-256，返回文件字节大小。
pub fn verify_regular_file_sha256(
    path: &Path,
    expected_sha256: &str,
    max_bytes: u64,
) -> Result<u64, String> {
    if !is_valid_sha256_hex(expected_sha256) {
        return Err("期望 SHA-256 摘要格式无效".to_string());
    }
    let (file, size) = open_regular_file_no_follow(path)?;
    if size == 0 || size > max_bytes {
        return Err(format!(
            "安装包体积异常（{size} 字节，允许范围 1..={max_bytes}）"
        ));
    }

    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut read_total = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("读取安装包计算 SHA-256 失败: {e}"))?;
        if n == 0 {
            break;
        }
        read_total = read_total.saturating_add(n as u64);
        if read_total > max_bytes {
            return Err("安装包体积超出安全上限".to_string());
        }
        hasher.update(&buf[..n]);
    }
    let actual = crate::service_ipc::encode_hex_lower(&hasher.finalize());
    if !constant_time_eq_hex(&actual, expected_sha256) {
        return Err("安装包 SHA-256 校验失败（文件摘要与官方发布清单不一致）".to_string());
    }
    Ok(read_total)
}

/// 下载更新安装包至 `dest_path` 并校验 SHA-256，支持取消与实时进度回调。
pub fn download_update_package(
    url: &str,
    expected_sha256: &str,
    dest_path: &Path,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<(), String> {
    validate_update_https_url(url)?;
    if !is_valid_sha256_hex(expected_sha256) {
        return Err("期望 SHA-256 摘要格式无效".to_string());
    }
    if cancel.load(Ordering::Acquire) {
        return Err("已取消更新下载".to_string());
    }
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("创建下载目录失败 {}: {e}", parent.display()))?;
    }
    let _ = fs::remove_file(dest_path);

    let header_path = std::env::temp_dir().join(format!(
        "rc-update-hdr-{}-{}.txt",
        std::process::id(),
        random_hex8()
    ));
    struct CleanupFile(PathBuf);
    impl Drop for CleanupFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _hdr_guard = CleanupFile(header_path.clone());

    let mut child = Command::new("/usr/bin/curl")
        .args([
            "-q",
            "--globoff",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--location",
            "--max-redirs",
            "5",
            "--silent",
            "--show-error",
            "--connect-timeout",
            "15",
            "--max-time",
            &CURL_DOWNLOAD_TIMEOUT_SECS.to_string(),
            "--max-filesize",
            &MAX_UPDATE_PACKAGE_BYTES.to_string(),
            "--user-agent",
            "RabbitControl-Updater",
            "-D",
        ])
        .arg(&header_path)
        .arg("-o")
        .arg(dest_path)
        .arg("--")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 curl 下载更新包失败: {e}"))?;

    let mut total_bytes = 0u64;
    let mut last_reported = 0u64;
    let mut last_tick = Instant::now();

    loop {
        if cancel.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(dest_path);
            return Err("已取消更新下载".to_string());
        }

        if let Ok(hdr) = fs::read_to_string(&header_path) {
            total_bytes = parse_last_content_length(&hdr);
        }

        if let Ok(meta) = fs::symlink_metadata(dest_path) {
            let cur = meta.len();
            if cur > MAX_UPDATE_PACKAGE_BYTES {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(dest_path);
                return Err("下载文件体积超出 200 MiB 安全上限".to_string());
            }
            if cur != last_reported || last_tick.elapsed() >= Duration::from_millis(500) {
                last_reported = cur;
                last_tick = Instant::now();
                on_progress(cur, total_bytes);
            }
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let mut err_buf = String::new();
                    if let Some(stderr) = child.stderr.take() {
                        let _ = stderr.take(16 * 1024).read_to_string(&mut err_buf);
                    }
                    let _ = fs::remove_file(dest_path);
                    let detail = err_buf.trim();
                    return Err(if detail.is_empty() {
                        format!("下载更新包失败（退出码 {:?}）", status.code())
                    } else {
                        format!("下载更新包失败: {detail}")
                    });
                }
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(120)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(dest_path);
                return Err(format!("等待下载进程失败: {e}"));
            }
        }
    }

    if cancel.load(Ordering::Acquire) {
        let _ = fs::remove_file(dest_path);
        return Err("已取消更新下载".to_string());
    }

    let final_size =
        match verify_regular_file_sha256(dest_path, expected_sha256, MAX_UPDATE_PACKAGE_BYTES) {
            Ok(sz) => sz,
            Err(e) => {
                let _ = fs::remove_file(dest_path);
                return Err(e);
            }
        };
    on_progress(final_size, final_size);
    Ok(())
}

/// 将外部传入的 DMG 安全复制到原子 `0700` 私有临时目录并同步计算校验 SHA-256（防 TOCTOU 与 FIFO 阻塞，且绝不泄露文件真实摘要）。
fn copy_dmg_to_private_staging_and_verify(
    src_dmg: &Path,
    expected_sha256: &str,
) -> Result<(PathBuf, PathBuf), String> {
    if !is_valid_sha256_hex(expected_sha256) {
        return Err("期望 SHA-256 摘要格式无效".to_string());
    }
    let (mut src, meta_len) = open_regular_file_no_follow(src_dmg)?;
    if meta_len == 0 || meta_len > MAX_UPDATE_PACKAGE_BYTES {
        return Err("安装包体积异常".to_string());
    }

    let staging_dir = std::env::temp_dir().join(format!(
        "rc-dmg-stage-{}-{}",
        std::process::id(),
        random_hex8()
    ));
    create_private_dir_0700(&staging_dir)?;
    let staged_dmg = staging_dir.join("package.dmg");

    let copy_res = (|| -> Result<(), String> {
        let mut dst_opts = OpenOptions::new();
        dst_opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            dst_opts.mode(0o600);
        }
        let mut dst = dst_opts
            .open(&staged_dmg)
            .map_err(|e| format!("创建隔离副本失败: {e}"))?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let n = src
                .read(&mut buf)
                .map_err(|e| format!("读取安装包失败: {e}"))?;
            if n == 0 {
                break;
            }
            total = total.saturating_add(n as u64);
            if total > MAX_UPDATE_PACKAGE_BYTES {
                return Err("安装包体积超出 200 MiB 安全上限".to_string());
            }
            hasher.update(&buf[..n]);
            dst.write_all(&buf[..n])
                .map_err(|e| format!("写入隔离副本失败: {e}"))?;
        }
        dst.sync_all()
            .map_err(|e| format!("落盘隔离副本失败: {e}"))?;
        if total == 0 {
            return Err("安装包为空文件".to_string());
        }
        let actual = crate::service_ipc::encode_hex_lower(&hasher.finalize());
        if !constant_time_eq_hex(&actual, expected_sha256) {
            return Err("安装包 SHA-256 校验失败（摘要不匹配）".to_string());
        }
        Ok(())
    })();

    if let Err(e) = copy_res {
        let _ = fs::remove_dir_all(&staging_dir);
        return Err(e);
    }

    Ok((staging_dir, staged_dmg))
}

#[cfg(target_os = "macos")]
struct DmgMountGuard {
    mount_point: PathBuf,
}

#[cfg(target_os = "macos")]
impl DmgMountGuard {
    /// 在已有的 `0700` 私有 `staging_dir` 内原子创建挂载点并只读挂载 DMG。
    fn attach_in(staging_dir: &Path, dmg_path: &Path) -> Result<Self, String> {
        let mount_point = staging_dir.join("mnt");
        create_private_dir_0700(&mount_point)?;

        let out = Command::new("/usr/bin/hdiutil")
            .args([
                "attach",
                "-nobrowse",
                "-readonly",
                "-noverify",
                "-noautoopen",
                "-mountpoint",
            ])
            .arg(&mount_point)
            .arg(dmg_path)
            .output()
            .map_err(|e| {
                let _ = fs::remove_dir_all(&mount_point);
                format!("调用 hdiutil 挂载 DMG 失败: {e}")
            })?;

        if !out.status.success() {
            let _ = fs::remove_dir_all(&mount_point);
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(format!("挂载 DMG 失败: {stderr}"));
        }

        Ok(Self { mount_point })
    }

    fn path(&self) -> &Path {
        &self.mount_point
    }
}

#[cfg(target_os = "macos")]
impl Drop for DmgMountGuard {
    fn drop(&mut self) {
        for attempt in 0..2 {
            let status = Command::new("/usr/bin/hdiutil")
                .args(["detach", "-force"])
                .arg(&self.mount_point)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if status.is_ok_and(|s| s.success()) {
                break;
            }
            if attempt == 0 {
                thread::sleep(Duration::from_millis(200));
            }
        }
        let _ = fs::remove_dir_all(&self.mount_point);
    }
}

/// 校验挂载后的 DMG 内容（目录完整性 + 严格版本防降级 + `codesign -R` 证书叶钉扎），返回 `(app_path, verified_leaf_sha1, candidate_version)`。
#[cfg(target_os = "macos")]
fn locate_and_verify_app_in_mount(
    mount_point: &Path,
    target_app_path: &Path,
    current_app_bundle: Option<&Path>,
) -> Result<(PathBuf, &'static str, String), String> {
    let app_path = mount_point.join("RabbitControl.app");
    let meta = fs::symlink_metadata(&app_path)
        .map_err(|_| "DMG 镜像内未找到 RabbitControl.app".to_string())?;
    if !meta.file_type().is_dir() {
        return Err("DMG 镜像内的 RabbitControl.app 不是有效目录".to_string());
    }
    let plist_path = app_path.join("Contents/Info.plist");
    let plist_meta = fs::symlink_metadata(&plist_path)
        .map_err(|_| "更新包缺少 Contents/Info.plist".to_string())?;
    if !plist_meta.file_type().is_file() {
        return Err("更新包 Contents/Info.plist 非普通文件".to_string());
    }
    let macos_dir = app_path.join("Contents/MacOS");
    for required in ["rc-daemon", "rc-host-agent"] {
        let p = macos_dir.join(required);
        let m =
            fs::symlink_metadata(&p).map_err(|_| format!("更新包缺少核心可执行文件 {required}"))?;
        if !m.file_type().is_file() {
            return Err(format!("更新包 {required} 非普通文件"));
        }
    }

    let candidate_ver = read_app_bundle_version(&app_path)
        .ok_or_else(|| "更新包 Info.plist 缺少有效的 CFBundleShortVersionString".to_string())?;
    let floor_ver = resolve_installed_version_floor(target_app_path, current_app_bundle)?;
    check_version_monotonicity(&candidate_ver, Some(&floor_ver))?;

    let verified_leaf = verify_candidate_app_codesign(&app_path, current_app_bundle)?;
    Ok((app_path, verified_leaf, candidate_ver))
}

/// 使用 `ditto` 在目标父目录原子暂存并替换目标 `.app`（root 安装时使用 root-only `0700` 暂存目录 + 收紧归属与权限 + 二次 `codesign -R` 复核 + 验签后 `xattr -drs` + 失败自动回滚）。
#[cfg(target_os = "macos")]
fn atomic_replace_app_bundle(
    verified_src_app: &Path,
    target_app_path: &Path,
    current_app_bundle: Option<&Path>,
    harden_root_ownership: bool,
) -> Result<(), String> {
    validate_target_app_path(target_app_path)?;
    let parent = target_app_path
        .parent()
        .ok_or_else(|| "目标应用缺少父目录".to_string())?;
    let suffix = format!("{}.{}", std::process::id(), random_hex8());
    let stage_dir = parent.join(format!(".rc-upd-stage.{suffix}"));
    create_private_dir_0700(&stage_dir)?;
    struct StageDirGuard(PathBuf);
    impl Drop for StageDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _stage_guard = StageDirGuard(stage_dir.clone());
    let staging_new = stage_dir.join("RabbitControl.app");
    let backup_old = stage_dir.join("RabbitControl.app.bak");

    let mut ditto_cmd = Command::new("/usr/bin/ditto");
    if harden_root_ownership {
        ditto_cmd.arg("--noacl");
    }
    let ditto = ditto_cmd
        .arg(verified_src_app)
        .arg(&staging_new)
        .output()
        .map_err(|e| format!("复制更新包到应用目录失败: {e}"))?;
    if !ditto.status.success() {
        let stderr = String::from_utf8_lossy(&ditto.stderr).trim().to_string();
        return Err(format!("复制更新包到应用目录失败: {stderr}"));
    }

    if harden_root_ownership {
        let chown = Command::new("/usr/sbin/chown")
            .args(["-Rh", "root:wheel"])
            .arg(&staging_new)
            .output()
            .map_err(|e| format!("设置应用归属 root:wheel 失败: {e}"))?;
        if !chown.status.success() {
            let stderr = String::from_utf8_lossy(&chown.stderr).trim().to_string();
            return Err(format!("设置应用归属 root:wheel 失败: {stderr}"));
        }
        // 显式清除可能继承的 ACL（macOS 上 ACL 优先于 POSIX 权限位），并剥离 setuid/setgid 与组/其他写权限。
        let _ = Command::new("/bin/chmod")
            .arg("-RN")
            .arg(&staging_new)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let chmod = Command::new("/bin/chmod")
            .args(["-R", "ug-s,go-w"])
            .arg(&staging_new)
            .output()
            .map_err(|e| format!("收紧应用目录写权限失败: {e}"))?;
        if !chmod.status.success() {
            let stderr = String::from_utf8_lossy(&chmod.stderr).trim().to_string();
            return Err(format!("收紧应用目录写权限失败: {stderr}"));
        }
    }

    // 先完成副本二次签名与证书叶钉扎复核，通过后再以 `-s`（不跟随符号链接）清除 quarantine 扩展属性。
    verify_candidate_app_codesign(&staging_new, current_app_bundle)
        .map_err(|e| format!("复制后的更新包二次签名复核失败: {e}"))?;
    let _ = Command::new("/usr/bin/xattr")
        .args(["-drs", "com.apple.quarantine"])
        .arg(&staging_new)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let had_existing = fs::symlink_metadata(target_app_path).is_ok();
    if had_existing {
        if let Err(e) = fs::rename(target_app_path, &backup_old) {
            return Err(format!("备份旧版应用失败（可能缺少目录写权限）: {e}"));
        }
    }

    if let Err(e) = fs::rename(&staging_new, target_app_path) {
        if had_existing {
            let _ = fs::rename(&backup_old, target_app_path);
        }
        return Err(format!("原子替换目标应用失败（已回滚旧版本）: {e}"));
    }

    Ok(())
}

fn install_verified_dmg_internal(
    dmg_path: &Path,
    expected_sha256: &str,
    target_app_path: &Path,
    harden_root_ownership: bool,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        validate_target_app_path(target_app_path)?;
        let (staging_dir, staged_dmg) =
            copy_dmg_to_private_staging_and_verify(dmg_path, expected_sha256)?;
        struct CleanupDir(PathBuf);
        impl Drop for CleanupDir {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _stage_guard = CleanupDir(staging_dir.clone());

        let mount = DmgMountGuard::attach_in(&staging_dir, &staged_dmg)?;
        let current_bundle = current_exe_app_bundle();
        let (src_app, _leaf, _ver) = locate_and_verify_app_in_mount(
            mount.path(),
            target_app_path,
            current_bundle.as_deref(),
        )?;
        atomic_replace_app_bundle(
            &src_app,
            target_app_path,
            current_bundle.as_deref(),
            harden_root_ownership,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (
            dmg_path,
            expected_sha256,
            target_app_path,
            harden_root_ownership,
        );
        Err("当前仅支持在 macOS 上安装 DMG 更新包".to_string())
    }
}

/// 在本进程内直接验证并原子安装 DMG 到 `target_app_path`。
pub fn install_verified_dmg_direct(
    dmg_path: &Path,
    expected_sha256: &str,
    target_app_path: &Path,
) -> Result<(), String> {
    install_verified_dmg_internal(dmg_path, expected_sha256, target_app_path, false)
}

/// 供 root `rc-daemon` 响应 `ServiceRequest::InstallVerifiedUpdate` 时调用：
/// 严格执行目标路径白名单 + 临时 DMG 路径白名单 + 隔离复制重算 SHA-256 + 版本防降级 + `chown root:wheel` + `chmod go-w` + `codesign -R` 钉扎 + 原子替换。
pub fn install_verified_dmg_from_daemon(
    dmg_path: &Path,
    expected_sha256: &str,
    requested_target_app: &Path,
) -> Result<(), String> {
    let allowed_target = validate_daemon_target_app_path(requested_target_app)?;
    let allowed_dmg = validate_daemon_dmg_path(dmg_path)?;
    install_verified_dmg_internal(&allowed_dmg, expected_sha256, &allowed_target, true)
}

/// 检测当前进程是否可以在不提权的情况下直接替换 `target_app_path`。
///
/// 不仅探测父目录可写性，若 `target_app_path` 已存在（例如先前经 root/osascript 安装为 `root:wheel 0755`），
/// 还会核验其自身及 `Contents` 均归属当前有效 UID 且具备所有者写权限；否则返回 `false`，交由 Path C 管理员提权替换，
/// 避免 `rename(2)` 跨父目录移动 `root:wheel` 目录时触发 `EACCES`。
fn can_replace_app_directly(target_app_path: &Path) -> bool {
    let Some(parent) = target_app_path.parent() else {
        return false;
    };
    let probe = parent.join(format!(
        ".rc-write-probe-{}-{}",
        std::process::id(),
        random_hex8()
    ));
    let my_uid_opt: Option<u32> = match File::create(&probe) {
        Ok(f) => {
            #[cfg(unix)]
            let uid = {
                use std::os::unix::fs::MetadataExt;
                f.metadata().ok().map(|m| m.uid())
            };
            #[cfg(not(unix))]
            let uid = None;
            drop(f);
            let _ = fs::remove_file(&probe);
            uid
        }
        Err(_) => return false,
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Some(my_uid) = my_uid_opt else {
            return false;
        };
        if let Ok(meta) = fs::symlink_metadata(target_app_path) {
            if meta.uid() != my_uid || (meta.mode() & 0o200) == 0 {
                return false;
            }
            if meta.file_type().is_dir() {
                if let Ok(contents_meta) = fs::symlink_metadata(target_app_path.join("Contents")) {
                    if contents_meta.uid() != my_uid || (contents_meta.mode() & 0o200) == 0 {
                        return false;
                    }
                }
            }
        }
        true
    }
    #[cfg(not(unix))]
    {
        let _ = my_uid_opt;
        true
    }
}

/// 在独立前台模式下，当目标目录（如 `/Applications`）需要管理员权限时，先完成全部 SHA-256 与
/// 证书钉扎校验并暂存 `.app`，再弹出一次 macOS 管理员授权；提权后的 root 脚本先复制到 root-owned `0700`
/// 目录并在 root 上下文中重新执行 `codesign --verify --deep --strict -R` 复核（彻底消除密码输入窗口期 TOCTOU）
/// 后再原子替换目标应用。
#[cfg(target_os = "macos")]
fn install_verified_dmg_via_osascript(
    dmg_path: &Path,
    expected_sha256: &str,
    target_app_path: &Path,
) -> Result<(), String> {
    validate_target_app_path(target_app_path)?;
    let (staging_dir, staged_dmg) =
        copy_dmg_to_private_staging_and_verify(dmg_path, expected_sha256)?;
    struct CleanupDir(PathBuf);
    impl Drop for CleanupDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _stage_guard = CleanupDir(staging_dir.clone());

    let mount = DmgMountGuard::attach_in(&staging_dir, &staged_dmg)?;
    let current_bundle = current_exe_app_bundle();
    let (src_app, verified_leaf, expected_ver) =
        locate_and_verify_app_in_mount(mount.path(), target_app_path, current_bundle.as_deref())?;

    let verified_copy = staging_dir.join("RabbitControl.app");
    let ditto = Command::new("/usr/bin/ditto")
        .arg(&src_app)
        .arg(&verified_copy)
        .output()
        .map_err(|e| format!("暂存更新应用失败: {e}"))?;
    if !ditto.status.success() {
        return Err("暂存更新应用失败".to_string());
    }
    drop(mount);

    let target_parent = target_app_path
        .parent()
        .ok_or_else(|| "目标应用缺少父目录".to_string())?;
    let verified_copy_str = verified_copy.to_string_lossy();
    let target_parent_str = target_parent.to_string_lossy();
    let target_app_str = target_app_path.to_string_lossy();
    if verified_copy_str.chars().any(|c| c.is_control())
        || target_parent_str.chars().any(|c| c.is_control())
        || target_app_str.chars().any(|c| c.is_control())
    {
        return Err("更新安装路径包含非法控制字符".to_string());
    }

    let q_src = service_manager::shell_quote_posix(&verified_copy_str);
    let q_parent = service_manager::shell_quote_posix(&target_parent_str);
    let q_dst = service_manager::shell_quote_posix(&target_app_str);
    let q_ver = service_manager::shell_quote_posix(&expected_ver);
    let q_req_app = service_manager::shell_quote_posix(&build_codesign_requirement(
        APP_BUNDLE_ID,
        verified_leaf,
    ));
    let q_req_daemon = service_manager::shell_quote_posix(&build_codesign_requirement(
        &format!("{APP_BUNDLE_ID}.daemon"),
        verified_leaf,
    ));
    let q_req_agent = service_manager::shell_quote_posix(&build_codesign_requirement(
        &format!("{APP_BUNDLE_ID}.agent"),
        verified_leaf,
    ));

    // 在目标父目录内由 root 创建 0700 私有暂存目录（同文件系统原子 mv），使用 `ditto --noacl` 剥离用户侧 ACL，
    // 收紧 root:wheel / ug-s,go-w 权限并复核 codesign -R + 版本号，验签通过后再以 xattr -drs（不跟随符号链接）清 quarantine。
    let shell_cmd = format!(
        "set -e; \
         ROOT_STAGE=$(/usr/bin/mktemp -d {q_parent}/.rc-root-upd.XXXXXX); \
         trap '/bin/rm -rf \"$ROOT_STAGE\"' EXIT; \
         /bin/chmod 0700 \"$ROOT_STAGE\"; \
         /usr/bin/ditto --noacl {q_src} \"$ROOT_STAGE/RabbitControl.app\"; \
         /usr/sbin/chown -Rh root:wheel \"$ROOT_STAGE/RabbitControl.app\"; \
         /bin/chmod -RN \"$ROOT_STAGE/RabbitControl.app\" 2>/dev/null || true; \
         /bin/chmod -R ug-s,go-w \"$ROOT_STAGE/RabbitControl.app\"; \
         /usr/bin/codesign --verify --deep --strict -R {q_req_app} \"$ROOT_STAGE/RabbitControl.app\"; \
         /usr/bin/codesign --verify --strict -R {q_req_daemon} \"$ROOT_STAGE/RabbitControl.app/Contents/MacOS/rc-daemon\"; \
         /usr/bin/codesign --verify --strict -R {q_req_agent} \"$ROOT_STAGE/RabbitControl.app/Contents/MacOS/rc-host-agent\"; \
         VER=$(/usr/bin/plutil -extract CFBundleShortVersionString raw -o - \"$ROOT_STAGE/RabbitControl.app/Contents/Info.plist\"); \
         VER=${{VER#v}}; VER=${{VER#V}}; \
         [ \"$VER\" = {q_ver} ]; \
         /usr/bin/xattr -drs com.apple.quarantine \"$ROOT_STAGE/RabbitControl.app\" 2>/dev/null || true; \
         if [ -e {q_dst} ] || [ -L {q_dst} ]; then /bin/mv {q_dst} \"$ROOT_STAGE/RabbitControl.app.bak\"; fi; \
         if ! /bin/mv \"$ROOT_STAGE/RabbitControl.app\" {q_dst}; then \
           if [ -e \"$ROOT_STAGE/RabbitControl.app.bak\" ] || [ -L \"$ROOT_STAGE/RabbitControl.app.bak\" ]; then /bin/mv \"$ROOT_STAGE/RabbitControl.app.bak\" {q_dst}; fi; \
           exit 1; \
         fi"
    );
    let escaped_applescript = shell_cmd.replace('\\', "\\\\").replace('"', "\\\"");
    let script =
        format!("do shell script \"{escaped_applescript}\" with administrator privileges\n");

    // 经 stdin 传入脚本而非 `-e` argv，避免在授权弹窗期间通过 `ps` 暴露暂存路径。
    let mut child = Command::new("/usr/bin/osascript")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("调用系统管理员授权失败: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(script.as_bytes())
            .map_err(|e| format!("写入管理员授权脚本失败: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("等待系统管理员授权失败: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if stderr.contains("-128") || stderr.to_ascii_lowercase().contains("cancel") {
            return Err("用户已取消管理员授权".to_string());
        }
        return Err(format!("管理员授权替换应用失败: {stderr}"));
    }
    Ok(())
}

/// 异步触发升级后重启（先短暂等待 650ms，确保 `UpdateStatus(Restarting)` 线帧送达控制端或 UI）。
fn schedule_post_install_restart(
    target_app_path: PathBuf,
    used_daemon: bool,
    is_remote_headless: bool,
) {
    let _ = thread::Builder::new()
        .name("rc-updater-restart".to_string())
        .spawn(move || {
            thread::sleep(Duration::from_millis(650));
            if used_daemon {
                let secret_path = config_paths::ipc_secret_path();
                let agent_sock = config_paths::agent_socket_path();
                let daemon_sock = config_paths::daemon_socket_path();
                // 必须先通知 rc-daemon 升级退出，再通知 rc-host-agent 优雅退出（触发 ByeReason::SessionSwitching）；
                // 避免在 rc-host-agent 进程内调用时因自身先退出导致 daemon.sock 通知丢失。
                if daemon_sock.exists() {
                    let _ = IpcClient::call(
                        &daemon_sock,
                        &secret_path,
                        ServiceRequest::RestartForUpgrade,
                        Duration::from_secs(2),
                    );
                }
                if agent_sock.exists() {
                    let _ = IpcClient::call(
                        &agent_sock,
                        &secret_path,
                        ServiceRequest::RestartForUpgrade,
                        Duration::from_secs(2),
                    );
                }
                if !is_remote_headless {
                    spawn_detached_relaunch(&target_app_path);
                    std::process::exit(0);
                }
                return;
            }

            // 若由 rc-host-agent 直接执行（daemon.sock 异常缺席），只重启 agent 自身，绝不弹出前台 GUI 窗口。
            if is_running_as_host_agent() {
                let secret_path = config_paths::ipc_secret_path();
                let agent_sock = config_paths::agent_socket_path();
                if agent_sock.exists() {
                    let _ = IpcClient::call(
                        &agent_sock,
                        &secret_path,
                        ServiceRequest::RestartForUpgrade,
                        Duration::from_secs(2),
                    );
                }
                return;
            }

            // 独立模式（rc-ui 单进程）：先调用平滑停机回调向对端发送 ByeReason::SessionSwitching，再拉起新版退出。
            if let Some(hook) = STANDALONE_PRE_RESTART_HOOK.get() {
                hook();
            }
            spawn_detached_relaunch(&target_app_path);
            std::process::exit(0);
        });
}

fn spawn_detached_relaunch(target_app_path: &Path) {
    #[cfg(target_os = "macos")]
    {
        let q_app = service_manager::shell_quote_posix(&target_app_path.to_string_lossy());
        let cmd = format!("sleep 0.7; /usr/bin/open -n {q_app}");
        let _ = Command::new("/bin/sh")
            .args(["-c", &cmd])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = target_app_path;
    }
}

/// 统一执行已校验安装包的安装与可选重启。
pub fn install_verified_update(
    package_path: &Path,
    expected_sha256: &str,
    restart_after: bool,
    is_remote_headless: bool,
) -> Result<(), String> {
    let target_app = resolve_target_app_bundle();
    let svc_status = service_manager::query_service_status();
    let daemon_sock = config_paths::daemon_socket_path();
    let secret_path = config_paths::ipc_secret_path();

    // 路径 A：已安装系统级后台服务 → 严格仅允许交由 root rc-daemon 复核并维持 root:wheel 归属，daemon 离线时 fail-closed，严禁降权回退。
    if svc_status.installed {
        if !daemon_sock.exists() {
            return Err(
                "系统级后台守护服务（rc-daemon）当前未运行，已拒绝降权替换应用；请先在设置中重启系统后台服务后再试"
                    .to_string(),
            );
        }
        let resp = IpcClient::call(
            &daemon_sock,
            &secret_path,
            ServiceRequest::InstallVerifiedUpdate {
                dmg_path: package_path.to_string_lossy().to_string(),
                expected_sha256: expected_sha256.to_ascii_lowercase(),
                target_app_path: target_app.to_string_lossy().to_string(),
            },
            Duration::from_secs(120),
        )?;
        match resp {
            ServiceResponse::Ok => {
                if restart_after {
                    schedule_post_install_restart(target_app, true, is_remote_headless);
                }
                return Ok(());
            }
            ServiceResponse::Error { message } => return Err(message),
            other => {
                return Err(format!("后台守护服务返回异常更新响应: {other:?}"));
            }
        }
    }

    // 路径 B：独立模式且目标父目录与既有应用均归属当前用户可直接替换 → 本进程直接验证并原子替换。
    if can_replace_app_directly(&target_app) {
        install_verified_dmg_direct(package_path, expected_sha256, &target_app)?;
        if restart_after {
            schedule_post_install_restart(target_app, false, is_remote_headless);
        }
        return Ok(());
    }

    // 路径 C：独立模式且目标目录需要管理员权限。
    if is_remote_headless {
        return Err(
            "被控端未安装系统级后台服务且当前应用目录需要管理员权限，请在被控端「设置 → 常规」安装系统级后台服务后再远程更新"
                .to_string(),
        );
    }

    #[cfg(target_os = "macos")]
    {
        install_verified_dmg_via_osascript(package_path, expected_sha256, &target_app)?;
        if restart_after {
            schedule_post_install_restart(target_app, false, false);
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("当前仅支持在 macOS 上执行自动更新".to_string())
    }
}

/// 注入到 `rc_client_core::platform` 的 macOS 更新钩子实现。
pub struct MacosUpdateHooks;

impl ClientUpdateHooks for MacosUpdateHooks {
    fn current_app_version(&self) -> String {
        resolve_current_app_version()
    }

    fn current_app_arch(&self) -> &'static str {
        current_macos_arch()
    }

    fn can_self_update(&self) -> bool {
        cfg!(target_os = "macos")
    }

    fn is_system_service_installed(&self) -> bool {
        service_manager::query_service_status().installed
    }

    fn fetch_latest_release(&self, mirror_url: &str) -> Result<ReleaseInfo, String> {
        fetch_latest_release(mirror_url)
    }

    fn download_update_package(
        &self,
        url: &str,
        expected_sha256: &str,
        dest_path: &Path,
        cancel: &AtomicBool,
        on_progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), String> {
        download_update_package(url, expected_sha256, dest_path, cancel, on_progress)
    }

    fn install_verified_update(
        &self,
        package_path: &Path,
        expected_sha256: &str,
        restart_after: bool,
        is_remote_headless: bool,
    ) -> Result<(), String> {
        install_verified_update(
            package_path,
            expected_sha256,
            restart_after,
            is_remote_headless,
        )
    }
}

/// 在 `rc-ui` 与 `rc-host-agent` 启动时注册 macOS 自动更新钩子（幂等）。
pub fn register_macos_update_hooks() -> bool {
    register_update_hooks(Arc::new(MacosUpdateHooks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codesign_requirement_and_monotonicity_enforced() {
        let req = build_codesign_requirement(APP_BUNDLE_ID, RELEASE_CERT_LEAF_SHA1);
        assert_eq!(
            req,
            format!(
                "=identifier \"{APP_BUNDLE_ID}\" and certificate leaf = H\"{RELEASE_CERT_LEAF_SHA1}\""
            )
        );

        assert!(check_version_monotonicity("0.1.10", Some("0.1.9")).is_ok());
        assert!(check_version_monotonicity("0.1.9", Some("0.1.9")).is_err());
        assert!(check_version_monotonicity("0.1.8", Some("0.1.9")).is_err());
        assert!(check_version_monotonicity("bad-ver", Some("0.1.9")).is_err());
        assert!(check_version_monotonicity("0.1.10", Some("corrupt-installed")).is_err());
        assert!(check_version_monotonicity("0.1.10", None).is_ok());
    }

    #[test]
    fn validate_paths_reject_traversal_control_chars_and_arbitrary_daemon_inputs() {
        assert!(validate_target_app_path(Path::new("/Applications/RabbitControl.app")).is_ok());
        assert!(validate_target_app_path(Path::new("relative/RabbitControl.app")).is_err());
        assert!(validate_target_app_path(Path::new("/Applications/../tmp/Foo.app")).is_err());
        assert!(validate_target_app_path(Path::new("/Applications/Evil\n.app")).is_err());
        assert!(validate_target_app_path(Path::new("/Applications/RabbitControl")).is_err());
        assert!(validate_target_app_path(Path::new("/Applications")).is_err());
        assert!(validate_daemon_target_app_path(Path::new("/tmp/Evil.app")).is_err());
        assert!(
            validate_daemon_target_app_path(Path::new("/Applications/RabbitControl.app")).is_ok()
        );

        // Daemon DMG path whitelist blocks root-only system files (eliminating SHA-256 oracle).
        assert!(validate_daemon_dmg_path(Path::new("/etc/master.passwd")).is_err());
        assert!(validate_daemon_dmg_path(Path::new("/tmp/not-rc.dmg")).is_err());
        assert!(validate_daemon_dmg_path(Path::new("/tmp/rc-upd-test.dmg")).is_ok());
    }

    #[test]
    fn parse_headers_resets_across_redirects_and_parses_sidecar() {
        let headers =
            "HTTP/2 302\r\ncontent-length: 128\r\n\r\nHTTP/2 200\r\nContent-Length: 42190848\r\n";
        assert_eq!(parse_last_content_length(headers), 42_190_848);

        // Chunked final response must NOT inherit redirect's Content-Length.
        let chunked_final =
            "HTTP/1.1 302 Found\r\nContent-Length: 215\r\n\r\nHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n";
        assert_eq!(parse_last_content_length(chunked_final), 0);

        // Multi-byte UTF-8 header straddling byte index 5 must never panic.
        let utf8_headers = "HTTP/2 200\r\nX-A中: 1\r\nContent-Length: 1024\r\n";
        assert_eq!(parse_last_content_length(utf8_headers), 1024);

        let sidecar = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  RabbitControl-v0.1.10-macos-arm64.dmg\n";
        assert_eq!(
            parse_single_sha256_sidecar(sidecar).as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_regular_file_sha256_rejects_symlink_and_mismatch() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("rc-upd-test-{}", random_hex8()));
        create_private_dir_0700(&dir).unwrap();
        let real = dir.join("rc-pkg.dmg");
        fs::write(&real, b"hello-update-dmg").unwrap();
        let mut h = Sha256::new();
        h.update(b"hello-update-dmg");
        let good_sha = crate::service_ipc::encode_hex_lower(&h.finalize());

        assert_eq!(
            verify_regular_file_sha256(&real, &good_sha, MAX_UPDATE_PACKAGE_BYTES).unwrap(),
            16
        );
        let err = verify_regular_file_sha256(&real, &"0".repeat(64), MAX_UPDATE_PACKAGE_BYTES)
            .unwrap_err();
        assert!(
            !err.contains(&good_sha),
            "SHA-256 error must never echo actual file digest"
        );

        let link = dir.join("rc-link.dmg");
        symlink(&real, &link).unwrap();
        assert!(verify_regular_file_sha256(&link, &good_sha, MAX_UPDATE_PACKAGE_BYTES).is_err());

        let _ = fs::remove_dir_all(&dir);
    }
}

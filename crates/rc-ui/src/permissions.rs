//! macOS 系统权限预检与引导（屏幕录制 / 辅助功能 / 输入监控）。
//!
//! 全模块 `#![deny(unsafe_code)]`：macOS 上委托 [`rc_capture_macos::PermissionStatus`]
//! 完成只查不弹的 preflight 检测、首次系统授权请求与系统设置跳转，并在进程首次检测时
//! 冻结启动期权限快照以生成 `restart_hint`；非 macOS 平台返回 `platform_supported: false`
//! 的全放行占位快照。

#![deny(unsafe_code)]
#![deny(missing_docs)]

use rc_client_core::ui_error::{codes, resolve_ui_error, ErrorPlatform, UiError};

/// 供前端渲染与启动前门禁使用的系统权限状态快照（IPC `camelCase`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionSnapshot {
    /// 当前平台是否需要系统权限预检（macOS 为 `true`，其余平台为 `false`）。
    pub platform_supported: bool,
    /// 屏幕录制权限状态（macOS: `CGPreflightScreenCaptureAccess`）。
    pub screen_recording: bool,
    /// 辅助功能权限状态（macOS: `AXIsProcessTrusted`）。
    pub accessibility: bool,
    /// 输入监控权限状态（macOS: `IOHIDCheckAccess(ListenEvent)`）。
    pub input_monitoring: bool,
    /// 本机作为被控端所需必需权限是否齐备（`screen_recording && accessibility`）。
    pub host_ready: bool,
    /// 三项系统权限是否全部授予（`screen_recording && accessibility && input_monitoring`）。
    pub all_granted: bool,
    /// 缺失的必需权限中文名称列表（例如 `["屏幕录制", "辅助功能"]`）。
    pub missing_required: Vec<String>,
    /// 若较进程启动时授权状态发生变化，提示用户重启进程使授权生效。
    pub restart_hint: Option<String>,
    /// 当 macOS 缺少 TCC 权限时的结构化引导错误（`platform.macos.*`；权限齐全或非 macOS 为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_error: Option<UiError>,
}

/// 根据三项权限布尔状态映射最优先的 macOS TCC 结构化 [`UiError`]（`platform.macos.*`）。
pub fn permission_ui_error_from_flags(
    platform_supported: bool,
    screen_recording: bool,
    accessibility: bool,
    input_monitoring: bool,
    detail: Option<String>,
) -> Option<UiError> {
    if !platform_supported {
        return None;
    }
    let code = if !screen_recording {
        codes::PLATFORM_MACOS_SCREEN_RECORDING_DENIED
    } else if !accessibility {
        codes::PLATFORM_MACOS_ACCESSIBILITY_DENIED
    } else if !input_monitoring {
        codes::PLATFORM_MACOS_INPUT_MONITORING_DENIED
    } else {
        return None;
    };
    Some(resolve_ui_error(
        code,
        ErrorPlatform::Macos,
        &[],
        None,
        detail,
    ))
}

impl PermissionSnapshot {
    /// 由平台支持标记、三项权限布尔状态与可选重启提示构造快照（纯逻辑，便于离线单测）。
    pub fn from_flags(
        platform_supported: bool,
        screen_recording: bool,
        accessibility: bool,
        input_monitoring: bool,
        restart_hint: Option<String>,
    ) -> Self {
        let mut missing_required = Vec::new();
        if !screen_recording {
            missing_required.push("屏幕录制".to_string());
        }
        if !accessibility {
            missing_required.push("辅助功能".to_string());
        }
        let ui_error = permission_ui_error_from_flags(
            platform_supported,
            screen_recording,
            accessibility,
            input_monitoring,
            restart_hint.clone(),
        );
        Self {
            platform_supported,
            screen_recording,
            accessibility,
            input_monitoring,
            host_ready: screen_recording && accessibility,
            all_granted: screen_recording && accessibility && input_monitoring,
            missing_required,
            restart_hint,
            ui_error,
        }
    }
}

/// 跨平台权限类型枚举（解析前端传入的 `kind` 字符串参数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedPermissionKind {
    ScreenRecording,
    Accessibility,
    InputMonitoring,
}

fn parse_permission_kind(kind: &str) -> Result<ParsedPermissionKind, String> {
    match kind {
        "screen_recording" => Ok(ParsedPermissionKind::ScreenRecording),
        "accessibility" => Ok(ParsedPermissionKind::Accessibility),
        "input_monitoring" => Ok(ParsedPermissionKind::InputMonitoring),
        other => Err(format!(
            "不支持的权限类型 \"{other}\"，仅支持 screen_recording / accessibility / input_monitoring"
        )),
    }
}

/// 将指定 macOS 权限类型（`"screen_recording"` / `"accessibility"` / `"input_monitoring"`）
/// 映射为结构化 [`UiError`]（未知类型回退 `validation.invalid_input`）。
pub fn permission_kind_ui_error(kind: &str, detail: Option<String>) -> UiError {
    let code = match kind {
        "screen_recording" => codes::PLATFORM_MACOS_SCREEN_RECORDING_DENIED,
        "accessibility" => codes::PLATFORM_MACOS_ACCESSIBILITY_DENIED,
        "input_monitoring" => codes::PLATFORM_MACOS_INPUT_MONITORING_DENIED,
        _ => codes::VALIDATION_INVALID_INPUT,
    };
    resolve_ui_error(code, ErrorPlatform::Macos, &[], None, detail)
}

/// 校验开启本机被控端服务前的必需系统权限；在不支持权限预检的平台直接放行。
pub fn ensure_host_ready(snapshot: &PermissionSnapshot) -> Result<(), String> {
    if !snapshot.platform_supported || snapshot.host_ready {
        return Ok(());
    }
    let missing = if snapshot.missing_required.is_empty() {
        "屏幕录制、辅助功能".to_string()
    } else {
        snapshot.missing_required.join("、")
    };
    Err(format!(
        "开启远程协助前请先在 macOS 系统设置中授予「{missing}」权限，授权后若未立即生效请重启 RabbitControl"
    ))
}

/// 校验开启本机被控端服务前的必需系统权限，缺失时返回结构化 `platform.macos.*` [`UiError`]。
pub fn ensure_host_ready_ui_error(snapshot: &PermissionSnapshot) -> Result<(), Box<UiError>> {
    if !snapshot.platform_supported || snapshot.host_ready {
        return Ok(());
    }
    let detail = ensure_host_ready(snapshot).err();
    let code = if !snapshot.screen_recording {
        codes::PLATFORM_MACOS_SCREEN_RECORDING_DENIED
    } else {
        codes::PLATFORM_MACOS_ACCESSIBILITY_DENIED
    };
    Err(Box::new(resolve_ui_error(
        code,
        ErrorPlatform::Macos,
        &[],
        None,
        detail,
    )))
}

#[cfg(target_os = "macos")]
fn boot_permission_status() -> rc_capture_macos::PermissionStatus {
    use std::sync::OnceLock;
    static BOOT_STATUS: OnceLock<rc_capture_macos::PermissionStatus> = OnceLock::new();
    *BOOT_STATUS.get_or_init(rc_capture_macos::PermissionStatus::check)
}

/// 非阻塞检测当前系统权限状态（macOS 只查不弹；非 macOS 返回全放行占位快照）。
pub fn check_permissions() -> PermissionSnapshot {
    #[cfg(target_os = "macos")]
    {
        let boot = boot_permission_status();
        let current = rc_capture_macos::PermissionStatus::check();
        let restart_hint = rc_capture_macos::PermissionStatus::restart_hint(&boot, &current);
        PermissionSnapshot::from_flags(
            true,
            current.screen_recording,
            current.accessibility,
            current.input_monitoring,
            restart_hint,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        PermissionSnapshot::from_flags(false, true, true, true, None)
    }
}

/// 请求屏幕录制权限（macOS 首次会弹出系统授权对话框），并返回最新的权限状态快照。
pub fn request_screen_recording() -> PermissionSnapshot {
    #[cfg(target_os = "macos")]
    {
        let _ = boot_permission_status();
        let _ = rc_capture_macos::PermissionStatus::request_screen_recording();
        check_permissions()
    }
    #[cfg(not(target_os = "macos"))]
    {
        check_permissions()
    }
}

/// 打开指定权限对应的系统设置隐私面板。
///
/// 支持的 `kind` 取值：`"screen_recording"`、`"accessibility"`、`"input_monitoring"`。
pub fn open_permission_settings(kind: &str) -> Result<(), String> {
    let parsed = parse_permission_kind(kind)?;
    #[cfg(target_os = "macos")]
    {
        let mapped = match parsed {
            ParsedPermissionKind::ScreenRecording => {
                rc_capture_macos::PermissionKind::ScreenRecording
            }
            ParsedPermissionKind::Accessibility => rc_capture_macos::PermissionKind::Accessibility,
            ParsedPermissionKind::InputMonitoring => {
                rc_capture_macos::PermissionKind::InputMonitoring
            }
        };
        rc_capture_macos::PermissionStatus::default()
            .open_settings(mapped)
            .map_err(|e| format!("打开系统设置失败：{e}"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = parsed;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_from_flags_computes_host_ready_all_granted_and_missing_required() {
        let none = PermissionSnapshot::from_flags(true, false, false, false, None);
        assert!(none.platform_supported);
        assert!(!none.host_ready);
        assert!(!none.all_granted);
        assert_eq!(none.missing_required, vec!["屏幕录制", "辅助功能"]);
        assert_eq!(none.restart_hint, None);

        let only_screen = PermissionSnapshot::from_flags(true, true, false, false, None);
        assert!(!only_screen.host_ready);
        assert!(!only_screen.all_granted);
        assert_eq!(only_screen.missing_required, vec!["辅助功能"]);

        let only_ax = PermissionSnapshot::from_flags(true, false, true, true, None);
        assert!(!only_ax.host_ready);
        assert!(!only_ax.all_granted);
        assert_eq!(only_ax.missing_required, vec!["屏幕录制"]);

        let host_ready_without_input = PermissionSnapshot::from_flags(
            true,
            true,
            true,
            false,
            Some("屏幕录制授权已变更，需要重启本进程后才能生效（macOS 限制）".to_string()),
        );
        assert!(host_ready_without_input.host_ready);
        assert!(!host_ready_without_input.all_granted);
        assert!(host_ready_without_input.missing_required.is_empty());
        assert!(host_ready_without_input.restart_hint.is_some());

        let all = PermissionSnapshot::from_flags(true, true, true, true, None);
        assert!(all.host_ready);
        assert!(all.all_granted);
        assert!(all.missing_required.is_empty());
    }

    #[test]
    fn snapshot_serializes_with_camel_case_fields() {
        let snap = PermissionSnapshot::from_flags(
            true,
            true,
            false,
            false,
            Some("需重启生效".to_string()),
        );
        let json = serde_json::to_value(&snap).expect("serialize PermissionSnapshot");
        assert_eq!(json["platformSupported"], true);
        assert_eq!(json["screenRecording"], true);
        assert_eq!(json["accessibility"], false);
        assert_eq!(json["inputMonitoring"], false);
        assert_eq!(json["hostReady"], false);
        assert_eq!(json["allGranted"], false);
        assert_eq!(json["missingRequired"], serde_json::json!(["辅助功能"]));
        assert_eq!(json["restartHint"], "需重启生效");
    }

    #[test]
    fn ensure_host_ready_enforces_required_permissions_only_when_supported() {
        let missing_both = PermissionSnapshot::from_flags(true, false, false, false, None);
        let err = ensure_host_ready(&missing_both).unwrap_err();
        assert!(err.contains("屏幕录制"), "错误信息应包含屏幕录制: {err}");
        assert!(err.contains("辅助功能"), "错误信息应包含辅助功能: {err}");

        let ready = PermissionSnapshot::from_flags(true, true, true, false, None);
        assert!(ensure_host_ready(&ready).is_ok());

        let unsupported = PermissionSnapshot::from_flags(false, false, false, false, None);
        assert!(ensure_host_ready(&unsupported).is_ok());
    }

    #[test]
    fn open_permission_settings_rejects_invalid_kind_without_spawning() {
        assert_eq!(
            parse_permission_kind("screen_recording"),
            Ok(ParsedPermissionKind::ScreenRecording)
        );
        assert_eq!(
            parse_permission_kind("accessibility"),
            Ok(ParsedPermissionKind::Accessibility)
        );
        assert_eq!(
            parse_permission_kind("input_monitoring"),
            Ok(ParsedPermissionKind::InputMonitoring)
        );

        let err = open_permission_settings("camera").unwrap_err();
        assert!(err.contains("camera"), "错误信息应指明非法类型: {err}");
        assert!(open_permission_settings("").is_err());
    }

    #[test]
    fn permission_snapshot_maps_macos_tcc_ui_errors_and_camel_case_json() {
        let missing_screen = PermissionSnapshot::from_flags(true, false, true, true, None);
        let ui_screen = missing_screen
            .ui_error
            .as_ref()
            .expect("missing screen recording must populate ui_error");
        assert_eq!(
            ui_screen.code,
            codes::PLATFORM_MACOS_SCREEN_RECORDING_DENIED
        );
        assert_eq!(
            ui_screen.action,
            rc_client_core::ui_error::ErrorAction::OpenPermissions
        );
        let err_screen = ensure_host_ready_ui_error(&missing_screen).unwrap_err();
        assert_eq!(
            err_screen.code,
            codes::PLATFORM_MACOS_SCREEN_RECORDING_DENIED
        );

        let missing_ax = PermissionSnapshot::from_flags(true, true, false, true, None);
        assert_eq!(
            missing_ax.ui_error.as_ref().map(|e| e.code.as_str()),
            Some(codes::PLATFORM_MACOS_ACCESSIBILITY_DENIED)
        );
        assert_eq!(
            ensure_host_ready_ui_error(&missing_ax).unwrap_err().code,
            codes::PLATFORM_MACOS_ACCESSIBILITY_DENIED
        );

        let missing_input = PermissionSnapshot::from_flags(true, true, true, false, None);
        assert_eq!(
            missing_input.ui_error.as_ref().map(|e| e.code.as_str()),
            Some(codes::PLATFORM_MACOS_INPUT_MONITORING_DENIED)
        );
        assert!(ensure_host_ready_ui_error(&missing_input).is_ok());

        let all_ok = PermissionSnapshot::from_flags(true, true, true, true, None);
        assert!(all_ok.ui_error.is_none());
        let ok_json = serde_json::to_value(&all_ok).unwrap();
        assert!(ok_json.get("uiError").is_none());

        assert_eq!(
            permission_kind_ui_error("screen_recording", None).code,
            codes::PLATFORM_MACOS_SCREEN_RECORDING_DENIED
        );
        assert_eq!(
            permission_kind_ui_error("accessibility", None).code,
            codes::PLATFORM_MACOS_ACCESSIBILITY_DENIED
        );
        assert_eq!(
            permission_kind_ui_error("input_monitoring", None).code,
            codes::PLATFORM_MACOS_INPUT_MONITORING_DENIED
        );
        assert_eq!(
            permission_kind_ui_error("unknown", None).code,
            codes::VALIDATION_INVALID_INPUT
        );
    }
}

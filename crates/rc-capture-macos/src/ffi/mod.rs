//! ScreenCaptureKit / 系统权限的 FFI 边界。
//!
//! 本 crate 的**全部 `unsafe` 都集中在这个模块**：
//! - 权限相关的 C 函数（`CGPreflightScreenCaptureAccess` 等）在这里声明与包装；
//! - ScreenCaptureKit 的 ObjC 调用链（`SCShareableContent` 枚举 → `SCContentFilter`
//!   → `SCStream` 构建与启动）在这里完成；
//! - 帧回调类（`RcSckStreamHandler`，实现 `SCStreamOutput` + `SCStreamDelegate`）
//!   在这里定义，回调里只做轻量提取并把事件投递进最新帧优先单槽邮箱（坑 6）。
//!
//! 对外（`sck.rs` / `permission.rs`）只暴露**安全**的包装函数与类型，
//! 每个 `unsafe` 块都带 `// SAFETY:` 注释。
//!
//! 本模块的存在使 crate 其余部分可以 `deny(unsafe_code)`。

// 见模块文档：本模块是 crate 内唯一允许 unsafe 的地方。
#![allow(unsafe_code)]

pub(crate) mod privacy_and_virtual;

use std::collections::BTreeMap;
use std::ffi::{c_void, CStr};

const fn cstr_from_bytes(bytes: &'static [u8]) -> &'static CStr {
    match CStr::from_bytes_with_nul(bytes) {
        Ok(s) => s,
        Err(_) => panic!("C string literal must be NUL-terminated with no interior NULs"),
    }
}

macro_rules! cstr {
    ($lit:literal) => {{
        const C: &'static CStr = cstr_from_bytes(concat!($lit, "\0").as_bytes());
        C
    }};
}
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFArray, CFBoolean,
    CFDictionary, CFNumber, CFRetained, CFString, CFType, CGRect,
};
use objc2_core_media::{CMClock, CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol, NSValue};
#[cfg(feature = "bench-metrics")]
use objc2_screen_capture_kit::SCRunningApplication;
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamFrameInfoDirtyRects, SCStreamFrameInfoStatus, SCStreamOutput,
    SCStreamOutputType, SCWindow,
};

use rc_types::{Rect, Resolution};

/// 单帧及邮箱合并保留的真实脏区矩形上限；超过上限时自动收敛为精确包围盒，
/// 兼顾 Tile 级静帧无损补帧精度与回调零膨胀约束。
pub(crate) const MAX_DIRTY_RECTS: usize = 16;

// ─────────────────────────────────────────────────────────────────────────────
// C 函数声明（权限三件套，M1/M2/M3）
// ─────────────────────────────────────────────────────────────────────────────

mod raw {
    use super::c_void;
    use objc2_core_foundation::{CFArray, CFDictionary, CFString};

    // CGWindow.h / CGDirectDisplay.h / CGDisplayConfiguration.h
    // 与 core-graphics crate 链接同一个 CoreGraphics 框架。
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        pub static kCGDisplayShowDuplicateLowResolutionModes: Option<&'static CFString>;
        pub static kCGDisplayStreamYCbCrMatrix_ITU_R_709_2: Option<&'static CFString>;

        pub fn CGPreflightScreenCaptureAccess() -> bool;
        pub fn CGRequestScreenCaptureAccess() -> bool;
        pub fn CGGetOnlineDisplayList(
            max_displays: u32,
            online_displays: *mut u32,
            display_count: *mut u32,
        ) -> i32;
        pub fn CGDisplayIsAsleep(display: u32) -> i32;
        pub fn CGSessionCopyCurrentDictionary() -> *const CFDictionary;

        pub fn CGDisplayCopyAllDisplayModes(
            display: u32,
            options: *const CFDictionary,
        ) -> *mut CFArray;
        pub fn CGDisplayCopyDisplayMode(display: u32) -> *mut c_void;
        pub fn CGDisplayModeGetWidth(mode: *const c_void) -> usize;
        pub fn CGDisplayModeGetHeight(mode: *const c_void) -> usize;
        pub fn CGDisplayModeGetPixelWidth(mode: *const c_void) -> usize;
        pub fn CGDisplayModeGetPixelHeight(mode: *const c_void) -> usize;
        pub fn CGDisplayModeGetRefreshRate(mode: *const c_void) -> f64;
        pub fn CGDisplayModeIsUsableForDesktopGUI(mode: *const c_void) -> bool;

        pub fn CGBeginDisplayConfiguration(config: *mut *mut c_void) -> i32;
        pub fn CGConfigureDisplayWithDisplayMode(
            config: *mut c_void,
            display: u32,
            mode: *const c_void,
            options: *const c_void,
        ) -> i32;
        pub fn CGCompleteDisplayConfiguration(config: *mut c_void, option: u32) -> i32;
        pub fn CGCancelDisplayConfiguration(config: *mut c_void) -> i32;
        pub fn CGRectMakeWithDictionaryRepresentation(
            dict: *const CFDictionary,
            rect: *mut objc2_core_foundation::CGRect,
        ) -> bool;
        #[cfg(test)]
        pub fn CGRectCreateDictionaryRepresentation(
            rect: objc2_core_foundation::CGRect,
        ) -> *const CFDictionary;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub fn CFRelease(cf: *const c_void);
        pub fn CFDataCreateMutable(allocator: *const c_void, capacity: isize) -> *mut c_void;
        pub fn CFDataGetLength(the_data: *const c_void) -> isize;
        pub fn CFDataGetBytePtr(the_data: *const c_void) -> *const u8;
    }

    extern "C" {
        pub fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
        pub fn dlopen(path: *const std::ffi::c_char, mode: std::ffi::c_int) -> *mut c_void;
        pub fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;
        pub fn dlclose(handle: *mut c_void) -> std::ffi::c_int;
    }

    #[link(name = "ImageIO", kind = "framework")]
    extern "C" {
        pub static kCGImageSourceCreateThumbnailFromImageAlways: Option<&'static CFString>;
        pub static kCGImageSourceThumbnailMaxPixelSize: Option<&'static CFString>;
        pub static kCGImageSourceCreateThumbnailWithTransform: Option<&'static CFString>;
        pub static kCGImageDestinationLossyCompressionQuality: Option<&'static CFString>;

        pub fn CGImageSourceCreateWithURL(
            url: *const c_void,
            options: *const c_void,
        ) -> *mut c_void;
        pub fn CGImageSourceCreateThumbnailAtIndex(
            isrc: *const c_void,
            index: usize,
            options: *const c_void,
        ) -> *mut c_void;
        pub fn CGImageDestinationCreateWithData(
            data: *mut c_void,
            image_type: *const CFString,
            count: usize,
            options: *const c_void,
        ) -> *mut c_void;
        pub fn CGImageDestinationAddImage(
            idst: *mut c_void,
            image: *const c_void,
            properties: *const c_void,
        );
        pub fn CGImageDestinationFinalize(idst: *mut c_void) -> bool;
    }

    #[link(name = "AppKit", kind = "framework")]
    extern "C" {}

    // AXUIElement.h：`Boolean AXIsProcessTrusted(void)`（10.4+）。
    // Boolean == unsigned char == u8。注意这个变体**没有 options 参数、绝不弹窗**。
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        pub fn AXIsProcessTrusted() -> u8;
    }

    // IOHIDLib.h（10.15+）：
    //   IOHIDAccessType IOHIDCheckAccess(IOHIDRequestType requestType)
    // IOHIDRequestType: PostEvent=0, ListenEvent=1（输入监控是 ListenEvent）。
    // IOHIDAccessType:  Granted=0, Denied=1, Unknown=2。C enum 按 int 传。
    //
    // IOPMLib.h：
    //   IOReturn IOPMAssertionDeclareUserActivity(CFStringRef AssertionName,
    //                                             IOPMUserActiveType userType,
    //                                             IOPMAssertionID *AssertionID)
    //   IOReturn IOPMAssertionRelease(IOPMAssertionID AssertionID)
    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        pub fn IOHIDCheckAccess(request_type: i32) -> i32;
        pub fn IOPMAssertionDeclareUserActivity(
            assertion_name: *const CFString,
            user_type: u32,
            assertion_id: *mut u32,
        ) -> i32;
        pub fn IOPMAssertionRelease(assertion_id: u32) -> i32;
    }
}

const K_IOHID_REQUEST_TYPE_LISTEN_EVENT: i32 = 1;
const K_IOHID_ACCESS_TYPE_GRANTED: i32 = 0;
const K_CG_ERROR_SUCCESS: i32 = 0;
const K_IO_RETURN_SUCCESS: i32 = 0;
const K_IOPM_USER_ACTIVE_LOCAL: u32 = 0;
const MAX_ONLINE_DISPLAYS: usize = 32;
const MAX_DISPLAY_WAKE_WAIT: Duration = Duration::from_secs(5);
const DISPLAY_WAKE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const DEFAULT_WAKE_REASON: &str = "RabbitControl Display Wake";
const MAX_WAKE_REASON_UTF16_LEN: usize = 128;

/// M1：检测屏幕录制权限，**不弹窗**（`CGPreflightScreenCaptureAccess`）。
pub(crate) fn preflight_screen_capture_access() -> bool {
    // SAFETY: 纯查询函数，无前置条件、无副作用（不会触发 TCC 弹窗）。
    unsafe { raw::CGPreflightScreenCaptureAccess() }
}

/// 请求屏幕录制权限：首次调用会弹系统授权对话框，**之后不再弹**（坑 3）。
pub(crate) fn request_screen_capture_access() -> bool {
    // SAFETY: 纯权限请求；返回值表示当前是否已授权。
    unsafe { raw::CGRequestScreenCaptureAccess() }
}

/// M2：检测辅助功能权限，**不弹窗**（`AXIsProcessTrusted()` 无 options 变体）。
pub(crate) fn ax_is_process_trusted() -> bool {
    // SAFETY: 无参纯查询；Boolean(0/1) 转 bool。
    unsafe { raw::AXIsProcessTrusted() != 0 }
}

/// M3：检测输入监控权限，**不弹窗**（`IOHIDCheckAccess(kIOHIDRequestTypeListenEvent)`）。
pub(crate) fn iohid_check_input_monitoring() -> bool {
    // SAFETY: 纯查询；参数是合法枚举值 ListenEvent。
    unsafe {
        raw::IOHIDCheckAccess(K_IOHID_REQUEST_TYPE_LISTEN_EVENT) == K_IOHID_ACCESS_TYPE_GRANTED
    }
}

/// 主显示器 CGDirectDisplayID（用于把配置里的 display=0 映射到主屏）。
pub(crate) fn main_display_id() -> u32 {
    // SAFETY: 纯查询，无前置条件。
    unsafe { core_graphics::display::CGMainDisplayID() }
}

/// 查询主显示器逻辑分辨率（单位：点，向下取偶以匹配 YUV 4:2:0 / 编码器要求）。
///
/// 使用 CoreGraphics `CGDisplay::main().bounds()` 同步查询，不触发任何 TCC 权限弹窗，
/// 返回值与 ScreenCaptureKit `display = 0`（主显示器）的逻辑坐标系完全一致。
pub(crate) fn main_display_logical_resolution() -> Option<Resolution> {
    let bounds = core_graphics::display::CGDisplay::main().bounds();
    let w = bounds.size.width;
    let h = bounds.size.height;
    if !w.is_finite() || !h.is_finite() || w < 2.0 || h < 2.0 {
        return None;
    }
    let res = Resolution::new(w.round() as u32, h.round() as u32).to_even();
    (res.width >= 2 && res.height >= 2).then_some(res)
}

/// 查询主显示器物理像素分辨率（backing pixels，向下取偶以匹配 YUV 4:2:0 / 编码器要求）。
///
/// 优先使用 `CGDisplay::main().display_mode()` 的 `pixel_width/pixel_height` 获取真实 Retina
/// 物理像素（例如 14" MBP `3024×1964`），不可用时回退至 `pixels_wide/pixels_high`；
/// 全程同步查询 CoreGraphics，绝不触发任何 TCC 权限弹窗。
pub(crate) fn main_display_backing_resolution() -> Option<Resolution> {
    let display = core_graphics::display::CGDisplay::main();
    let (w, h) = match display.display_mode() {
        Some(mode) if mode.pixel_width() >= 2 && mode.pixel_height() >= 2 => {
            (mode.pixel_width(), mode.pixel_height())
        }
        _ => (display.pixels_wide(), display.pixels_high()),
    };
    let w = u32::try_from(w).ok()?;
    let h = u32::try_from(h).ok()?;
    if w < 2 || h < 2 {
        return None;
    }
    let res = Resolution::new(w, h).to_even();
    (res.width >= 2 && res.height >= 2).then_some(res)
}

// ─────────────────────────────────────────────────────────────────────────────
// 显示模式枚举与动态切换（Item #4）+ 桌面壁纸缩略图（Item #6）
// ─────────────────────────────────────────────────────────────────────────────

/// `kCGConfigureForSession`（CGDisplayConfiguration.h = 1）：仅当前登录会话生效。
const K_CG_CONFIGURE_FOR_SESSION: u32 = 1;

/// 通用 +1 所有权 CF 对象 RAII 包装，确保任何分支退出都释放 CFRelease。
struct CfOwned(NonNull<c_void>);

impl CfOwned {
    fn from_raw(ptr: *mut c_void) -> Option<Self> {
        NonNull::new(ptr).map(Self)
    }

    fn as_ptr(&self) -> *mut c_void {
        self.0.as_ptr()
    }
}

impl Drop for CfOwned {
    fn drop(&mut self) {
        // SAFETY: self.0 由 Create/Copy 规则拥有 +1 引用，此处配对 CFRelease。
        unsafe { raw::CFRelease(self.0.as_ptr().cast_const()) };
    }
}

/// 构造 `{ kCGDisplayShowDuplicateLowResolutionModes: kCFBooleanTrue }` 选项字典。
fn duplicate_low_res_modes_options() -> Option<CFRetained<CFDictionary>> {
    // SAFETY: 读取框架导出的只读 extern static CFString 常量指针。
    let key = unsafe { raw::kCGDisplayShowDuplicateLowResolutionModes }?;
    let val = CFBoolean::new(true);
    let keys = [key as *const CFString as *const c_void];
    let values = [val as *const CFBoolean as *const c_void];
    // SAFETY: keys/values 为 1 个元素长的有效指针数组，key/val 均为 CF 对象。
    unsafe {
        CFDictionary::new(
            None,
            keys.as_ptr() as *mut *const c_void,
            values.as_ptr() as *mut *const c_void,
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }
}

fn copy_all_display_modes(
    display_id: u32,
    opts: Option<&CFDictionary>,
) -> Option<CFRetained<CFArray>> {
    let opts_ptr = opts
        .map(|d| d as *const CFDictionary)
        .unwrap_or_else(std::ptr::null);
    // SAFETY: opts_ptr 为 null 或有效 CFDictionary；返回值为 +1 retained CFArray（可为 null）。
    let ptr = unsafe { raw::CGDisplayCopyAllDisplayModes(display_id, opts_ptr) };
    // SAFETY: CGDisplayCopyAllDisplayModes 遵循 Copy Rule，非空时返回 +1 所有权引用。
    NonNull::new(ptr).map(|nn| unsafe { CFRetained::from_raw(nn) })
}

fn mode_metrics(mode: *const c_void) -> Option<(u32, u32, u32, u32, u32, bool)> {
    // SAFETY: mode 为有效 CGDisplayModeRef，以下均为纯只读属性查询。
    let usable = unsafe { raw::CGDisplayModeIsUsableForDesktopGUI(mode) };
    if !usable {
        return None;
    }
    // SAFETY: 同上，mode 有效。
    let (w, h, pw, ph, hz_raw) = unsafe {
        (
            raw::CGDisplayModeGetWidth(mode),
            raw::CGDisplayModeGetHeight(mode),
            raw::CGDisplayModeGetPixelWidth(mode),
            raw::CGDisplayModeGetPixelHeight(mode),
            raw::CGDisplayModeGetRefreshRate(mode),
        )
    };
    let w = u32::try_from(w).ok()?;
    let h = u32::try_from(h).ok()?;
    let pw = u32::try_from(pw).ok()?;
    let ph = u32::try_from(ph).ok()?;
    let refresh_hz = if hz_raw.is_finite() && hz_raw > 0.0 {
        hz_raw.round() as u32
    } else {
        0
    };
    let is_hidpi = pw > w;
    Some((w, h, pw, ph, refresh_hz, is_hidpi))
}

/// 查询指定显示器的真实物理像素分辨率（backing pixels，优先读取 `CGDisplayCopyDisplayMode`）。
fn display_backing_resolution(display_id: u32) -> Option<Resolution> {
    // SAFETY: 纯只读查询当前显示模式，返回 +1 所有权 CF 对象或 null，交由 CfOwned 自动释放。
    let mode = CfOwned::from_raw(unsafe { raw::CGDisplayCopyDisplayMode(display_id) })?;
    // SAFETY: mode 为有效 CGDisplayModeRef。
    let (pw, ph) = unsafe {
        (
            raw::CGDisplayModeGetPixelWidth(mode.as_ptr()),
            raw::CGDisplayModeGetPixelHeight(mode.as_ptr()),
        )
    };
    let w = u32::try_from(pw).ok()?;
    let h = u32::try_from(ph).ok()?;
    if w < 2 || h < 2 {
        return None;
    }
    Some(Resolution::new(w, h).to_even())
}

/// 枚举主显示器可用桌面显示模式（960..=3840 × 600..=2160，去重偏好 HiDPI 与高刷）。
pub(crate) fn list_main_display_modes() -> Vec<crate::DisplayModePreset> {
    let display_id = main_display_id();
    let opts = duplicate_low_res_modes_options();
    let Some(modes) = copy_all_display_modes(display_id, opts.as_deref()) else {
        return Vec::new();
    };

    // SAFETY: 纯查询当前显示模式，返回值遵循 Copy Rule（+1 或 null）。
    let cur_mode = CfOwned::from_raw(unsafe { raw::CGDisplayCopyDisplayMode(display_id) });
    let cur_dims = cur_mode.as_ref().and_then(|m| {
        // SAFETY: m.as_ptr() 为有效 CGDisplayModeRef。
        let (w, h) = unsafe {
            (
                raw::CGDisplayModeGetWidth(m.as_ptr()),
                raw::CGDisplayModeGetHeight(m.as_ptr()),
            )
        };
        Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?))
    });

    let mut dedup: BTreeMap<(u32, u32), crate::DisplayModePreset> = BTreeMap::new();
    let count = modes.count();
    for i in 0..count {
        // SAFETY: i < count 在合法范围内；modes 在整个循环内保活。
        let raw_mode = unsafe { modes.value_at_index(i) };
        if raw_mode.is_null() {
            continue;
        }
        let Some((w, h, pw, ph, refresh_hz, is_hidpi)) = mode_metrics(raw_mode) else {
            continue;
        };
        if !(960..=3840).contains(&w) || !(600..=2160).contains(&h) {
            continue;
        }
        let candidate = crate::DisplayModePreset {
            width: w,
            height: h,
            pixel_width: pw,
            pixel_height: ph,
            refresh_hz,
            is_hidpi,
            is_current: cur_dims == Some((w, h)),
        };
        match dedup.get(&(w, h)) {
            Some(existing)
                if (existing.is_hidpi, existing.refresh_hz)
                    >= (candidate.is_hidpi, candidate.refresh_hz) => {}
            _ => {
                dedup.insert((w, h), candidate);
            }
        }
    }

    let mut result: Vec<crate::DisplayModePreset> = dedup.into_values().collect();
    result.sort_by(|a, b| b.width.cmp(&a.width).then_with(|| b.height.cmp(&a.height)));
    result
}

/// 主显示器显示模式 RAII 守卫：切换分辨率并在退出/Drop 时自动恢复原始模式。
pub(crate) struct DisplayModeGuard {
    display_id: u32,
    original_mode: NonNull<c_void>,
    original_res: Resolution,
    modified: bool,
}

// SAFETY: CGDisplayModeRef 是 CoreGraphics 不可变 CF 对象（CFRetain/CFRelease 线程安全），
// DisplayModeGuard 独占其 +1 所有权，且所有可变操作仅经 `&mut self` 进行。
unsafe impl Send for DisplayModeGuard {}

impl DisplayModeGuard {
    /// 捕获主显示器当前显示模式（失败返回 `None`）。
    pub(crate) fn capture_current() -> Option<Self> {
        let display_id = main_display_id();
        // SAFETY: CGDisplayCopyDisplayMode 返回 +1 所有权引用或 null。
        let mode_ptr = NonNull::new(unsafe { raw::CGDisplayCopyDisplayMode(display_id) })?;
        // SAFETY: mode_ptr 为有效 CGDisplayModeRef。
        let (w, h) = unsafe {
            (
                raw::CGDisplayModeGetWidth(mode_ptr.as_ptr()),
                raw::CGDisplayModeGetHeight(mode_ptr.as_ptr()),
            )
        };
        let original_res = main_display_logical_resolution().or_else(|| {
            let w = u32::try_from(w).ok()?;
            let h = u32::try_from(h).ok()?;
            let r = Resolution::new(w, h).to_even();
            (r.width >= 2 && r.height >= 2).then_some(r)
        });
        let Some(original_res) = original_res else {
            // SAFETY: mode_ptr 拥有 +1 引用，失败分支立即释放。
            unsafe { raw::CFRelease(mode_ptr.as_ptr().cast_const()) };
            return None;
        };
        Some(Self {
            display_id,
            original_mode: mode_ptr,
            original_res,
            modified: false,
        })
    }

    /// 捕获时记录的主显示器原始逻辑分辨率。
    pub(crate) fn original_resolution(&self) -> Resolution {
        self.original_res
    }

    /// 切换主显示器到匹配 `(target_w, target_h)` 的最佳可用模式（偏好 HiDPI 与高刷）。
    pub(crate) fn switch_to(&mut self, target_w: u32, target_h: u32) -> Result<Resolution, String> {
        let opts = duplicate_low_res_modes_options();
        let modes = copy_all_display_modes(self.display_id, opts.as_deref())
            .ok_or_else(|| "CGDisplayCopyAllDisplayModes 返回空".to_string())?;

        let mut best: Option<(*const c_void, bool, u32)> = None;
        let count = modes.count();
        for i in 0..count {
            // SAFETY: i < count，modes 在整个函数期间保活。
            let raw_mode = unsafe { modes.value_at_index(i) };
            if raw_mode.is_null() {
                continue;
            }
            let Some((w, h, _pw, _ph, refresh_hz, is_hidpi)) = mode_metrics(raw_mode) else {
                continue;
            };
            if w != target_w || h != target_h {
                continue;
            }
            match best {
                Some((_, best_hidpi, best_hz))
                    if (best_hidpi, best_hz) >= (is_hidpi, refresh_hz) => {}
                _ => best = Some((raw_mode, is_hidpi, refresh_hz)),
            }
        }
        let (target_mode, _, _) =
            best.ok_or_else(|| format!("未找到可用的显示模式 {target_w}x{target_h}"))?;

        let mut config: *mut c_void = std::ptr::null_mut();
        // SAFETY: config 是合法出参。
        let begin_status = unsafe { raw::CGBeginDisplayConfiguration(&mut config) };
        if begin_status != 0 || config.is_null() {
            return Err(format!(
                "CGBeginDisplayConfiguration 失败: CGError {begin_status}"
            ));
        }

        // SAFETY: config 为有效事务句柄；target_mode 在 modes CFArray 存活期内有效。
        let cfg_status = unsafe {
            raw::CGConfigureDisplayWithDisplayMode(
                config,
                self.display_id,
                target_mode,
                std::ptr::null(),
            )
        };
        if cfg_status != 0 {
            // SAFETY: 配置失败必须取消事务释放 CGDisplayConfigRef。
            let _ = unsafe { raw::CGCancelDisplayConfiguration(config) };
            return Err(format!(
                "CGConfigureDisplayWithDisplayMode({target_w}x{target_h}) 失败: CGError {cfg_status}"
            ));
        }

        // SAFETY: 事务已配置，1 = kCGConfigureForSession（仅本会话生效）。
        let complete_status =
            unsafe { raw::CGCompleteDisplayConfiguration(config, K_CG_CONFIGURE_FOR_SESSION) };
        if complete_status != 0 {
            return Err(format!(
                "CGCompleteDisplayConfiguration 失败: CGError {complete_status}"
            ));
        }
        self.modified = true;

        // 给 WindowServer 短暂过渡时间刷新显示边界。
        std::thread::sleep(Duration::from_millis(120));
        main_display_logical_resolution()
            .ok_or_else(|| "切换显示模式后无法读取主显示器逻辑分辨率".to_string())
    }

    /// 恢复为捕获时的原始显示模式（幂等）。
    pub(crate) fn restore(&mut self) {
        if !self.modified {
            return;
        }
        let mut config: *mut c_void = std::ptr::null_mut();
        // SAFETY: config 是合法出参。
        let begin_status = unsafe { raw::CGBeginDisplayConfiguration(&mut config) };
        if begin_status == 0 && !config.is_null() {
            // SAFETY: config 与 self.original_mode 均有效。
            let cfg_status = unsafe {
                raw::CGConfigureDisplayWithDisplayMode(
                    config,
                    self.display_id,
                    self.original_mode.as_ptr(),
                    std::ptr::null(),
                )
            };
            if cfg_status == 0 {
                // SAFETY: 事务已配置，1 = kCGConfigureForSession。
                let _ = unsafe {
                    raw::CGCompleteDisplayConfiguration(config, K_CG_CONFIGURE_FOR_SESSION)
                };
            } else {
                // SAFETY: 配置失败取消事务释放句柄。
                let _ = unsafe { raw::CGCancelDisplayConfiguration(config) };
            }
        }
        self.modified = false;
    }
}

impl Drop for DisplayModeGuard {
    fn drop(&mut self) {
        self.restore();
        // SAFETY: original_mode 由 CGDisplayCopyDisplayMode 返回 +1 所有权，此处配对释放。
        unsafe { raw::CFRelease(self.original_mode.as_ptr().cast_const()) };
    }
}

/// 编码一张 CGImageSource 缩略图到 JPEG 字节流。
fn encode_thumbnail_jpeg(
    image_source: *const c_void,
    max_px: u32,
    quality: f64,
) -> Option<Vec<u8>> {
    // SAFETY: 读取 ImageIO 只读 extern static CFString 键常量。
    let (always_key, max_px_key, transform_key, quality_key) = unsafe {
        (
            raw::kCGImageSourceCreateThumbnailFromImageAlways?,
            raw::kCGImageSourceThumbnailMaxPixelSize?,
            raw::kCGImageSourceCreateThumbnailWithTransform?,
            raw::kCGImageDestinationLossyCompressionQuality?,
        )
    };
    let bool_true = CFBoolean::new(true);
    let max_px_num = CFNumber::new_i32(i32::try_from(max_px.max(1)).unwrap_or(i32::MAX));
    let thumb_keys = [
        always_key as *const CFString as *const c_void,
        max_px_key as *const CFString as *const c_void,
        transform_key as *const CFString as *const c_void,
    ];
    let thumb_vals = [
        bool_true as *const CFBoolean as *const c_void,
        &*max_px_num as *const CFNumber as *const c_void,
        bool_true as *const CFBoolean as *const c_void,
    ];
    // SAFETY: thumb_keys/thumb_vals 为 3 个合法 CF 对象指针。
    let thumb_opts = unsafe {
        CFDictionary::new(
            None,
            thumb_keys.as_ptr() as *mut *const c_void,
            thumb_vals.as_ptr() as *mut *const c_void,
            3,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }?;
    // SAFETY: image_source 与 thumb_opts 均合法，返回 +1 CGImageRef 或 null。
    let thumb_img = CfOwned::from_raw(unsafe {
        raw::CGImageSourceCreateThumbnailAtIndex(
            image_source,
            0,
            &*thumb_opts as *const CFDictionary as *const c_void,
        )
    })?;

    // SAFETY: 创建可变 CFData（容量 0 = 按需增长，返回 +1 所有权）。
    let cf_data = CfOwned::from_raw(unsafe { raw::CFDataCreateMutable(std::ptr::null(), 0) })?;
    let jpeg_uti = CFString::from_str("public.jpeg");
    // SAFETY: cf_data 与 jpeg_uti 均有效，count = 1。
    let dest = CfOwned::from_raw(unsafe {
        raw::CGImageDestinationCreateWithData(cf_data.as_ptr(), &*jpeg_uti, 1, std::ptr::null())
    })?;

    let quality_num = CFNumber::new_f64(quality);
    let dest_keys = [quality_key as *const CFString as *const c_void];
    let dest_vals = [&*quality_num as *const CFNumber as *const c_void];
    // SAFETY: dest_keys/dest_vals 为 1 个合法 CF 对象指针。
    let dest_props = unsafe {
        CFDictionary::new(
            None,
            dest_keys.as_ptr() as *mut *const c_void,
            dest_vals.as_ptr() as *mut *const c_void,
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }?;

    // SAFETY: dest、thumb_img、dest_props 均有效。
    unsafe {
        raw::CGImageDestinationAddImage(
            dest.as_ptr(),
            thumb_img.as_ptr().cast_const(),
            &*dest_props as *const CFDictionary as *const c_void,
        );
        if !raw::CGImageDestinationFinalize(dest.as_ptr()) {
            return None;
        }
    }

    // SAFETY: finalize 成功后从 cf_data 读取字节切片（存活期覆盖本次拷贝）。
    let len = unsafe { raw::CFDataGetLength(cf_data.as_ptr().cast_const()) };
    if len <= 0 {
        return None;
    }
    // SAFETY: len > 0 时 CFDataGetBytePtr 返回至少 len 字节的可读连续内存。
    let ptr = unsafe { raw::CFDataGetBytePtr(cf_data.as_ptr().cast_const()) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: ptr 非空且长度为 len。
    let slice = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    Some(slice.to_vec())
}

/// 读取当前主显示器桌面壁纸并生成低体积 JPEG 缩略图（零 TCC 弹窗）。
pub(crate) fn main_display_wallpaper_jpeg(max_width: u32, max_height: u32) -> Option<Vec<u8>> {
    if max_width == 0 || max_height == 0 {
        return None;
    }
    let image_source = autoreleasepool(|_| -> Option<CfOwned> {
        let ws_cls = AnyClass::get(cstr!("NSWorkspace"))?;
        let screen_cls = AnyClass::get(cstr!("NSScreen"))?;
        // SAFETY: NSWorkspace.sharedWorkspace 是标准类方法，返回共享实例。
        let ws: Option<Retained<AnyObject>> = unsafe { msg_send![ws_cls, sharedWorkspace] };
        let ws = ws?;

        // SAFETY: NSScreen.mainScreen 是标准类方法，无参可返回 nil。
        let main_screen: Option<Retained<AnyObject>> = unsafe { msg_send![screen_cls, mainScreen] };
        let screen = match main_screen {
            Some(s) => s,
            None => {
                // SAFETY: NSScreen.screens 是标准类方法，返回 NSArray<NSScreen *>。
                let screens: Option<Retained<AnyObject>> =
                    unsafe { msg_send![screen_cls, screens] };
                let screens = screens?;
                // SAFETY: NSArray.firstObject 返回首个元素或 nil。
                let first: Option<Retained<AnyObject>> =
                    unsafe { msg_send![&*screens, firstObject] };
                first?
            }
        };

        // SAFETY: desktopImageURLForScreen: 接收有效 NSScreen*，返回 NSURL*（toll-free CFURLRef）或 nil。
        let url: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*ws, desktopImageURLForScreen: &*screen] };
        let url = url?;

        // SAFETY: url 为 toll-free bridged CFURLRef，返回 +1 CGImageSourceRef 或 null。
        CfOwned::from_raw(unsafe {
            raw::CGImageSourceCreateWithURL(
                (&*url as *const AnyObject).cast::<c_void>(),
                std::ptr::null(),
            )
        })
    })?;

    let initial_max_px = max_width.max(max_height).max(1);
    let mut jpeg = encode_thumbnail_jpeg(image_source.as_ptr().cast_const(), initial_max_px, 0.72)?;
    if jpeg.len() > 48 * 1024 {
        let retry_max_px = initial_max_px.min(480);
        jpeg = encode_thumbnail_jpeg(image_source.as_ptr().cast_const(), retry_max_px, 0.50)?;
    }
    if jpeg.len() > 60 * 1024 || jpeg.is_empty() {
        return None;
    }
    Some(jpeg)
}

// ─────────────────────────────────────────────────────────────────────────────
// App Nap 抑制与实时线程 QoS 提升（维持 120fps 不被后台节流）
// ─────────────────────────────────────────────────────────────────────────────

const NS_ACTIVITY_IDLE_DISPLAY_SLEEP_DISABLED: u64 = 1 << 40;
const NS_ACTIVITY_IDLE_SYSTEM_SLEEP_DISABLED: u64 = 1 << 20;
const NS_ACTIVITY_USER_INITIATED: u64 = 0x00FF_FFFF | NS_ACTIVITY_IDLE_SYSTEM_SLEEP_DISABLED;
const NS_ACTIVITY_LATENCY_CRITICAL: u64 = 0x00FF_0000_0000;
const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

/// RAII 声明高优先级实时会话活动，防止 macOS App Nap 在长时 120fps 运行期间
/// 降低定时器与采集/编解码线程调度优先级。
pub(crate) struct LatencyCriticalActivity {
    retained: Option<(Retained<AnyObject>, Retained<AnyObject>)>,
}

// SAFETY: `NSProcessInfo` 是线程安全的，`beginActivityWithOptions:reason:` 返回的
// activity token 仅用于最终配对调用 `endActivity:`，可在创建或工作线程安全持有并 Drop。
unsafe impl Send for LatencyCriticalActivity {}
// SAFETY: 仅持有不可变 Foundation 对象句柄且无内部可变共享状态，Drop 需要独占 `&mut self`。
unsafe impl Sync for LatencyCriticalActivity {}

impl LatencyCriticalActivity {
    pub(crate) fn begin(reason: &str, keep_display_awake: bool) -> Self {
        let mut options = NS_ACTIVITY_USER_INITIATED | NS_ACTIVITY_LATENCY_CRITICAL;
        if keep_display_awake {
            options |= NS_ACTIVITY_IDLE_DISPLAY_SLEEP_DISABLED;
        }
        let sanitized = reason.replace('\0', " ");
        let Ok(c_reason) = std::ffi::CString::new(sanitized) else {
            return Self { retained: None };
        };
        let retained = autoreleasepool(|_| -> Option<(Retained<AnyObject>, Retained<AnyObject>)> {
            let pi_cls = AnyClass::get(cstr!("NSProcessInfo"))?;
            let ns_cls = AnyClass::get(cstr!("NSString"))?;
            // SAFETY: `NSProcessInfo.processInfo` 返回进程级单例。
            let info: Option<Retained<AnyObject>> = unsafe { msg_send![pi_cls, processInfo] };
            let info = info?;
            // SAFETY: `c_reason` 为合法 NUL 结尾 UTF-8 字符串，返回自动释放 NSString。
            let ns_reason: Option<Retained<AnyObject>> =
                unsafe { msg_send![ns_cls, stringWithUTF8String: c_reason.as_ptr()] };
            let ns_reason = ns_reason?;
            // SAFETY: `options` 为 `NSActivityOptions` (u64)，`ns_reason` 为非空 NSString*。
            let token: Option<Retained<AnyObject>> = unsafe {
                msg_send![&*info, beginActivityWithOptions: options, reason: &*ns_reason]
            };
            let token = token?;
            Some((info, token))
        });
        Self { retained }
    }
}

impl Drop for LatencyCriticalActivity {
    fn drop(&mut self) {
        if let Some((info, token)) = self.retained.take() {
            autoreleasepool(|_| {
                // SAFETY: `info` 与 `token` 来自成对的 `beginActivityWithOptions:reason:`。
                unsafe {
                    let _: () = msg_send![&*info, endActivity: &*token];
                }
            });
        }
    }
}

/// 将当前操作系统线程提升至 `QOS_CLASS_USER_INTERACTIVE`（`0x21`）。
pub(crate) fn promote_current_thread_qos() {
    // SAFETY: `pthread_set_qos_class_self_np` 对任意用户态 pthread 均为安全系统调用，
    // 传入合法 `QOS_CLASS_USER_INTERACTIVE`（0x21）与相对优先级 0。
    let _ = unsafe { raw::pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
}

const RTLD_LAZY: std::ffi::c_int = 0x1;
const LOGIN_FRAMEWORK_PATH: &std::ffi::CStr =
    cstr_from_bytes(b"/System/Library/PrivateFrameworks/login.framework/Versions/Current/login\0");

/// 检查系统 `login.framework` 是否导出 `SACLockScreenImmediate`（仅解析符号，不锁屏）。
#[cfg(test)]
pub(crate) fn has_lock_screen_symbol() -> bool {
    // SAFETY: `LOGIN_FRAMEWORK_PATH` 与 `cstr!("SACLockScreenImmediate")` 为静态 NUL 结尾 C 字符串；
    // 若 `dlopen` 返回非空句柄则配对调用 `dlclose`，绝不解引用或调用目标函数指针。
    unsafe {
        let handle = raw::dlopen(LOGIN_FRAMEWORK_PATH.as_ptr(), RTLD_LAZY);
        if handle.is_null() {
            return false;
        }
        let sym = raw::dlsym(handle, cstr!("SACLockScreenImmediate").as_ptr());
        let ok = !sym.is_null();
        let _ = raw::dlclose(handle);
        ok
    }
}

/// 立即锁定当前 macOS 登录会话屏幕（优先调用 `SACLockScreenImmediate`，失败回退 `pmset displaysleepnow`）。
pub(crate) fn lock_screen_now() -> bool {
    // SAFETY: `dlopen`/`dlsym` 解析 Apple `login.framework` 的零参无返回值函数
    // `SACLockScreenImmediate`（`extern "C" fn()`），调用后配对 `dlclose` 释放句柄。
    let locked = unsafe {
        let handle = raw::dlopen(LOGIN_FRAMEWORK_PATH.as_ptr(), RTLD_LAZY);
        if handle.is_null() {
            false
        } else {
            let sym = raw::dlsym(handle, cstr!("SACLockScreenImmediate").as_ptr());
            let invoked = if !sym.is_null() {
                let lock_fn: extern "C" fn() = std::mem::transmute(sym);
                lock_fn();
                true
            } else {
                false
            };
            let _ = raw::dlclose(handle);
            invoked
        }
    };
    if locked {
        return true;
    }
    std::process::Command::new("/usr/bin/pmset")
        .arg("displaysleepnow")
        .status()
        .is_ok_and(|s| s.success())
}

// ─────────────────────────────────────────────────────────────────────────────
// 显示器休眠检测、锁屏检测与 IOKit 亮屏唤醒（TokenBoom/rabbitcontrol-macos#2）
// ─────────────────────────────────────────────────────────────────────────────

/// 根据在线显示器休眠表、主屏 ID 与主屏兜底状态综合判定当前是否处于熄屏休眠。
fn evaluate_displays_asleep(
    online: &[(u32, bool)],
    main_id: u32,
    main_fallback_asleep: bool,
) -> bool {
    if online.is_empty() {
        return main_fallback_asleep;
    }
    let main_asleep = online
        .iter()
        .find(|&&(id, _)| id == main_id)
        .map(|&(_, asleep)| asleep)
        .unwrap_or(false);
    let all_asleep = online.iter().all(|&(_, asleep)| asleep);
    main_asleep || all_asleep
}

/// 查询指定显示器或当前在线主显示器是否处于休眠/熄屏状态。
pub(crate) fn is_display_asleep(display_id: Option<u32>) -> bool {
    if let Some(id) = display_id {
        // SAFETY: `CGDisplayIsAsleep` 为 CoreGraphics 纯只读查询函数，无副作用。
        return unsafe { raw::CGDisplayIsAsleep(id) != 0 };
    }

    let main_id = main_display_id();
    let mut ids = [0u32; MAX_ONLINE_DISPLAYS];
    let mut count = 0u32;
    // SAFETY: `ids` 容量为 `MAX_ONLINE_DISPLAYS`，`count` 为有效栈出参。
    let status = unsafe {
        raw::CGGetOnlineDisplayList(MAX_ONLINE_DISPLAYS as u32, ids.as_mut_ptr(), &mut count)
    };
    if status != K_CG_ERROR_SUCCESS || count == 0 {
        // SAFETY: `CGDisplayIsAsleep` 为 CoreGraphics 纯只读查询函数。
        return unsafe { raw::CGDisplayIsAsleep(main_id) != 0 };
    }

    let n = (count as usize).min(MAX_ONLINE_DISPLAYS);
    let mut states = [(0u32, false); MAX_ONLINE_DISPLAYS];
    for i in 0..n {
        let id = ids[i];
        // SAFETY: `id` 来自系统返回的在线显示器列表，`CGDisplayIsAsleep` 为纯只读查询。
        let asleep = unsafe { raw::CGDisplayIsAsleep(id) != 0 };
        states[i] = (id, asleep);
    }
    // SAFETY: `main_id` 为主显示器 ID，`CGDisplayIsAsleep` 为纯只读查询。
    let main_fallback = unsafe { raw::CGDisplayIsAsleep(main_id) != 0 };
    evaluate_displays_asleep(&states[..n], main_id, main_fallback)
}

/// 从 `CFDictionary` 安全读取布尔标志键（兼容 `CFBoolean` 与 `CFNumber != 0`）。
fn dict_bool_flag(dict: &CFDictionary, key_name: &str) -> bool {
    let key = CFString::from_str(key_name);
    let key_ptr: *const c_void = CFRetained::as_ptr(&key).as_ptr().cast();
    // SAFETY: `dict` 为有效 `CFDictionary`，`key_ptr` 为有效 `CFString` 键，返回值由 `dict` 持有。
    let raw_val = unsafe { dict.value(key_ptr) };
    let Some(val_ptr) = NonNull::new(raw_val.cast_mut()) else {
        return false;
    };
    // SAFETY: 非空字典值是合法 CF 对象且存活期受 `dict` 保护；以下 downcast 经 CFGetTypeID 校验。
    let cf_val: &CFType = unsafe { val_ptr.cast::<CFType>().as_ref() };
    if let Some(b) = cf_val.downcast_ref::<CFBoolean>() {
        return b.as_bool();
    }
    if let Some(n) = cf_val.downcast_ref::<CFNumber>() {
        return n
            .as_i64()
            .map(|v| v != 0)
            .or_else(|| n.as_f64().map(|v| v != 0.0))
            .unwrap_or(false);
    }
    false
}

/// 查询当前 macOS 图形会话是否处于锁屏状态（`CGSessionCopyCurrentDictionary` → `CGSSessionScreenIsLocked`）。
pub(crate) fn is_screen_locked() -> bool {
    // SAFETY: 无参纯查询函数，遵循 CoreFoundation Copy Rule（非空时拥有 +1 引用）。
    let raw_dict = unsafe { raw::CGSessionCopyCurrentDictionary() };
    let Some(dict_ptr) = NonNull::new(raw_dict.cast_mut()) else {
        return false;
    };
    // SAFETY: `raw_dict` 非空且遵循 Copy Rule，交由 `CFRetained` 保证任何路径退出均执行 `CFRelease`。
    let dict: CFRetained<CFDictionary> = unsafe { CFRetained::from_raw(dict_ptr) };
    dict_bool_flag(&dict, "CGSSessionScreenIsLocked")
}

/// 清洗并截断唤醒声明原因字符串（替换 NUL/控制字符，按 UTF-16 长度 ≤ 128 截断，空串回退默认值）。
pub(crate) fn sanitize_wake_reason(reason: &str) -> String {
    let cleaned: String = reason
        .chars()
        .map(|c| if c == '\0' || c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return DEFAULT_WAKE_REASON.to_string();
    }
    let mut out = String::new();
    let mut utf16_len = 0usize;
    for ch in trimmed.chars() {
        let ch_len = ch.len_utf16();
        if utf16_len + ch_len > MAX_WAKE_REASON_UTF16_LEN {
            break;
        }
        out.push(ch);
        utf16_len += ch_len;
    }
    if out.is_empty() {
        DEFAULT_WAKE_REASON.to_string()
    } else {
        out
    }
}

/// IOKit `IOPMAssertionDeclareUserActivity` RAII 守卫（`Send`，不可复制）。
///
/// `Drop` 时若 `assertion_id != 0` 自动调用 `IOPMAssertionRelease` 并将 ID 置零。
#[derive(Debug)]
pub(crate) struct DisplayWakeAssertion {
    assertion_id: u32,
}

impl DisplayWakeAssertion {
    /// 构造内部 RAII 守卫（`assertion_id == 0` 表示空操作）。
    fn new(assertion_id: u32) -> Self {
        Self { assertion_id }
    }
}

impl Drop for DisplayWakeAssertion {
    fn drop(&mut self) {
        let id = std::mem::replace(&mut self.assertion_id, 0);
        if id != 0 {
            // SAFETY: `id` 来自 `IOPMAssertionDeclareUserActivity` 成功返回的有效 `IOPMAssertionID`，
            // 本结构独占其所有权并在 Drop 时至多释放一次。
            let _ = unsafe { raw::IOPMAssertionRelease(id) };
        }
    }
}

/// 若显示器处于休眠态，通过 IOKit `IOPMAssertionDeclareUserActivity` 触发亮屏并按 50ms 步进有界等待。
pub(crate) fn wake_display_if_asleep(
    reason: &str,
    max_wait: Duration,
) -> Result<Option<DisplayWakeAssertion>, String> {
    if !is_display_asleep(None) {
        return Ok(None);
    }

    let sanitized = sanitize_wake_reason(reason);
    let cf_reason = CFString::from_str(&sanitized);
    let mut assertion_id: u32 = 0;

    // SAFETY: `cf_reason` 为有效非空 `CFString`（长度 ≤ 128），`K_IOPM_USER_ACTIVE_LOCAL = 0` 为合法枚举，
    // `assertion_id` 为有效栈出参；成功时由下方 `DisplayWakeAssertion` 立即接管释放责任。
    let ret = unsafe {
        raw::IOPMAssertionDeclareUserActivity(
            &*cf_reason as *const CFString,
            K_IOPM_USER_ACTIVE_LOCAL,
            &mut assertion_id,
        )
    };
    if ret != K_IO_RETURN_SUCCESS {
        tracing::warn!(
            io_return = ret,
            reason = %sanitized,
            "IOPMAssertionDeclareUserActivity 唤醒显示器失败"
        );
        return Err(format!(
            "IOPMAssertionDeclareUserActivity 失败: IOReturn 0x{:08x} ({ret})",
            ret as u32
        ));
    }

    // 立即包装进 RAII 守卫，保证即便后续提前返回或 panic 也能正确释放 assertion。
    let assertion = DisplayWakeAssertion::new(assertion_id);
    let bounded_wait = max_wait.min(MAX_DISPLAY_WAKE_WAIT);
    let start = Instant::now();
    let mut awake = !is_display_asleep(None);
    while !awake {
        let Some(remaining) = bounded_wait.checked_sub(start.elapsed()) else {
            break;
        };
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(remaining.min(DISPLAY_WAKE_POLL_INTERVAL));
        awake = !is_display_asleep(None);
    }

    let elapsed_ms = start.elapsed().as_millis();
    let locked = is_screen_locked();
    if awake {
        tracing::info!(
            elapsed_ms,
            screen_locked = locked,
            assertion_id = assertion.assertion_id,
            "macOS 显示器已唤醒"
        );
    } else {
        tracing::warn!(
            elapsed_ms,
            max_wait_ms = bounded_wait.as_millis(),
            screen_locked = locked,
            assertion_id = assertion.assertion_id,
            "等待 macOS 显示器唤醒超时（可能仍处于熄屏/合盖或锁屏状态）"
        );
    }
    Ok(Some(assertion))
}

// ─────────────────────────────────────────────────────────────────────────────
// 最新帧优先单槽邮箱、事件与像素缓冲句柄
// ─────────────────────────────────────────────────────────────────────────────

/// 单帧视频载荷。
///
/// 泛型参数 `B` 默认为 [`PixelBufferHandle`]，仅在纯 Rust 单测中替换为轻量桩类型，
/// 用来验证「丢旧保新」与旧帧立刻 Drop 归还缓冲池的语义。
pub(crate) struct FramePayload<B = PixelBufferHandle> {
    /// 已 +1 retain 的像素缓冲，Drop 时立即归还 SCK IOSurface 池。
    pub(crate) buffer: B,
    /// CMSampleBuffer 的 presentation timestamp，微秒（契约 C2 的原始来源）。
    pub(crate) pts_us: u64,
    /// 本帧 `SCStreamFrameInfoDirtyRects` 累计脏区面积占比（`0.0..=1.0`，无法解析时为 `None` 按全帧处理）。
    pub(crate) dirty_ratio: Option<f32>,
    /// 本帧经坐标映射与边界裁剪后的真实脏区列表（上限 [`MAX_DIRTY_RECTS`]，超限时收敛为精确包围盒；
    /// 缺失或存在无法解析元素时为 `None`，供上层保守按全帧失效处理）。
    pub(crate) dirty_rects: Option<Vec<Rect>>,
    /// 帧回调入口采样时刻（微秒，与 PTS 同域；仅 `bench-metrics` 启用）。
    #[cfg(feature = "bench-metrics")]
    pub(crate) callback_us: u64,
}

/// 控制事件（与最新视频帧分槽保存，绝不被新视频帧覆写）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ControlEvent {
    /// SCFrameStatus == Suspended（锁屏 / 会话挂起）。
    Suspended,
    /// `stream:didStopWithError:`（权限被撤销等，M10）。绝不 panic，只上报。
    Stopped {
        /// NSError 的可读描述。
        message: String,
    },
}

/// 帧回调 → `next_frame` 的事件。
pub(crate) enum FrameEvent<B = PixelBufferHandle> {
    /// 一帧最新内容：retain 过的 CVPixelBuffer + 内容变化时刻（PTS，微秒）。
    Frame(FramePayload<B>),
    /// SCFrameStatus == Suspended（锁屏 / 会话挂起）。
    Suspended,
    /// `stream:didStopWithError:`（权限被撤销等，M10）。绝不 panic，只上报。
    Stopped {
        /// NSError 的可读描述。
        message: String,
    },
}

/// [`FrameMailbox`] 的内部双槽状态：控制事件槽与最新视频帧槽完全分离。
struct MailboxInner<B> {
    control: Option<ControlEvent>,
    latest_frame: Option<FramePayload<B>>,
    #[cfg(feature = "bench-metrics")]
    overwrites: u64,
}

/// 线程安全的「最新帧优先单槽邮箱」。
///
/// 替代 FIFO `sync_channel`：
/// - 消费端短暂滞后时，新帧直接覆写 `latest_frame` 并在锁外立刻 Drop 被覆盖的旧
///   `PixelBufferHandle`（即刻 `CFRelease` 归还 SCK IOSurface 池），避免旧帧排队
///   导致的采集交接延迟上升；
/// - 控制事件（`Suspended` / `Stopped`）存入独立控制槽，优先级高于视频帧，绝不被
///   新到达的视频帧覆写；
/// - 双槽均空时消费端通过 `Condvar::wait_timeout` 零自旋等待。
pub(crate) struct FrameMailbox<B = PixelBufferHandle>(Arc<(Mutex<MailboxInner<B>>, Condvar)>);

impl<B> Clone for FrameMailbox<B> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<B> FrameMailbox<B> {
    /// 创建空邮箱。
    pub(crate) fn new() -> Self {
        Self(Arc::new((
            Mutex::new(MailboxInner {
                control: None,
                latest_frame: None,
                #[cfg(feature = "bench-metrics")]
                overwrites: 0,
            }),
            Condvar::new(),
        )))
    }

    /// 投递最新视频帧：合并被覆写旧帧的脏区占比（防止大场景切换帧被后续微小闪烁帧覆盖导致漏升码率）
    /// 与真实脏区坐标列表（防止首帧局部变化被覆盖丢失），随后直接覆写 `latest_frame`、
    /// 在锁外立即 Drop 旧帧并唤醒等待者。
    pub(crate) fn push_frame(&self, mut frame: FramePayload<B>) {
        let (lock, cvar) = &*self.0;
        let displaced = {
            let mut inner = lock.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(prev) = inner.latest_frame.as_mut() {
                frame.dirty_ratio = match (prev.dirty_ratio, frame.dirty_ratio) {
                    (Some(a), Some(b)) => Some((a + b).min(1.0)),
                    _ => None,
                };
                frame.dirty_rects = merge_dirty_rects(prev.dirty_rects.take(), frame.dirty_rects);
            }
            let old = inner.latest_frame.replace(frame);
            #[cfg(feature = "bench-metrics")]
            if old.is_some() {
                inner.overwrites = inner.overwrites.saturating_add(1);
            }
            old
        };
        // 在锁外唤醒消费者并立即释放被覆盖的旧帧，避免唤醒后立即在互斥锁上碰撞，
        // 同时第一时间归还 SCK IOSurface 池。
        cvar.notify_one();
        drop(displaced);
    }

    /// 已被新帧覆写丢弃的旧帧累计次数（仅 `bench-metrics` 启用）。
    #[cfg(feature = "bench-metrics")]
    pub(crate) fn overwrites(&self) -> u64 {
        let (lock, _) = &*self.0;
        lock.lock().unwrap_or_else(|e| e.into_inner()).overwrites
    }

    /// 投递控制事件（`Suspended` / `Stopped`）：写入独立控制槽，绝不被新视频帧覆写，并在锁外唤醒等待者。
    ///
    /// 若控制槽已有更致命的 `Stopped`，不会被后续 `Suspended` 降级。
    pub(crate) fn push_control(&self, event: ControlEvent) {
        let (lock, cvar) = &*self.0;
        {
            let mut inner = lock.lock().unwrap_or_else(|e| e.into_inner());
            let should_write = !matches!(
                (&inner.control, &event),
                (Some(ControlEvent::Stopped { .. }), ControlEvent::Suspended)
            );
            if should_write {
                inner.control = Some(event);
            }
        }
        cvar.notify_one();
    }

    /// 取出下一条就绪事件：有控制事件优先返回控制事件，否则 `take()` 最新视频帧。
    fn take_ready(inner: &mut MailboxInner<B>) -> Option<FrameEvent<B>> {
        if let Some(ctrl) = inner.control.take() {
            return Some(match ctrl {
                ControlEvent::Suspended => FrameEvent::Suspended,
                ControlEvent::Stopped { message } => FrameEvent::Stopped { message },
            });
        }
        inner.latest_frame.take().map(FrameEvent::Frame)
    }

    /// 等待下一条事件（控制事件优先，否则取最新视频帧）；超时返回 `None`。
    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Option<FrameEvent<B>> {
        let (lock, cvar) = &*self.0;
        let mut inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(event) = Self::take_ready(&mut inner) {
            return Some(event);
        }
        if timeout.is_zero() {
            return None;
        }
        let deadline = Instant::now().checked_add(timeout);
        loop {
            let (guard, timed_out) = match deadline {
                Some(dl) => {
                    let now = Instant::now();
                    if now >= dl {
                        return None;
                    }
                    let (g, res) = cvar
                        .wait_timeout(inner, dl - now)
                        .unwrap_or_else(|e| e.into_inner());
                    (g, res.timed_out())
                }
                None => (cvar.wait(inner).unwrap_or_else(|e| e.into_inner()), false),
            };
            inner = guard;
            if let Some(event) = Self::take_ready(&mut inner) {
                return Some(event);
            }
            if timed_out {
                return None;
            }
        }
    }
}

/// 拥有型 `CVPixelBuffer` 引用（CFRetained，+1）。
///
/// 这是 [`rc_capture::FrameTexture::CVPixelBuffer`] 背后的真身：
/// 抽象层拿到的 `*mut c_void` 指向这里持有的对象。
pub(crate) struct PixelBufferHandle(CFRetained<CVPixelBuffer>);

// SAFETY: CVPixelBuffer 是 CoreVideo 的线程安全对象（文档只要求访问 base
// address 时加锁，我们从不锁着读 CPU 数据——零拷贝路径不碰像素）。
// 这里持有的是 +1 retain，跨线程移动、在任意线程 Drop 释放都是安全的。
unsafe impl Send for PixelBufferHandle {}

// SAFETY: 本包装仅在 Drop 时调用原子引用计数 CFRelease，且不提供可变内部状态，
// 多线程通过 Arc 共享保活守卫并在最后一个引用 Drop 时释放完全线程安全。
unsafe impl Sync for PixelBufferHandle {}

impl PixelBufferHandle {
    /// 暴露给抽象层的**不透明**句柄（不转移所有权；存活期由本 wrapper 决定）。
    pub(crate) fn as_opaque_ptr(&self) -> *mut c_void {
        CFRetained::as_ptr(&self.0).as_ptr().cast()
    }

    /// `(width, height, bytes_per_row, pixel_format)`。
    ///
    /// 供首帧校验（格式必须是 '420v'）与真机测试核对逻辑分辨率（M9）。
    pub(crate) fn info(&self) -> (u32, u32, u32, u32) {
        // 这些是 objc2-core-video 生成的安全包装（内部已完成 unsafe 调用），
        // 传入的引用由 CFRetained 保证有效存活。
        (
            CVPixelBufferGetWidth(&self.0) as u32,
            CVPixelBufferGetHeight(&self.0) as u32,
            CVPixelBufferGetBytesPerRow(&self.0) as u32,
            CVPixelBufferGetPixelFormatType(&self.0),
        )
    }
}

/// SCK 内部 IOSurface 缓冲池深度（`SCStreamConfiguration::queueDepth`，Apple 文档上限为 8）。
///
/// 注意：它不是会增加排队延迟的发送缓冲（实际排队深度由最新帧优先单槽邮箱
/// [`FrameMailbox`]（深度 1，丢旧保新）与 `HostPipeline` 的 `cap_tx(1)` +
/// `max_in_flight(1)` 严格封顶），而是 SCK 分配 `CVPixelBuffer` 的底层对象池容量。
/// 必须严格大于「调用方保活环 + 邮箱单槽」之和（当前 `3 + 1 = 4 < 8`，至少留 4 块
/// 空闲表面供 SCK 持续产出），否则池耗尽会卡死出帧。
pub(crate) const SCK_SURFACE_POOL_DEPTH: isize = 8;

/// `'420v'` 四字符码（`kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`，
/// 即 NV12 video range）。M5：SCK 配置与首帧校验共用的期望值。
pub(crate) const PIXEL_FORMAT_420V: u32 = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;

// ─────────────────────────────────────────────────────────────────────────────
// CMSampleBuffer 提取（帧回调里的轻量路径）
// ─────────────────────────────────────────────────────────────────────────────

/// 读取 `CMSampleBuffer` 第 0 个样本附件字典并执行只读提取。
///
/// 在 ScreenCaptureKit 中，`SCStreamFrameInfoStatus` / `SCStreamFrameInfoDirtyRects`
/// 等均存放在 `CMSampleBufferGetSampleAttachmentsArray(sb, false)` 返回数组的第 0 个
/// `CFDictionary` 里，而不是 sample buffer 顶层附件。
fn with_sample_attachment_dict<T>(
    sb: &CMSampleBuffer,
    f: impl FnOnce(&CFDictionary) -> T,
) -> Option<T> {
    // SAFETY: sb 是有效的 CMSampleBuffer；create_if_necessary = false 只查询
    // 已有附件数组而不修改 sample buffer，返回值已被绑定层 retain。
    let attachments = unsafe { sb.sample_attachments_array(false) }?;
    if attachments.count() <= 0 {
        return None;
    }
    // SAFETY: 已确认 count > 0，索引 0 合法；取出的指针在 attachments 存活期内有效。
    let raw_dict = unsafe { attachments.value_at_index(0) };
    let dict_ptr = NonNull::new(raw_dict.cast_mut())?;
    // SAFETY: sample attachments array 的非空元素为 CFType 对象，存活期受
    // attachments 保护；随后 downcast_ref 通过 CFGetTypeID 在运行时校验其为 CFDictionary。
    let dict: &CFDictionary = unsafe { dict_ptr.cast::<CFType>().as_ref() }.downcast_ref()?;
    Some(f(dict))
}

/// 从样本附件字典读取帧状态（`SCStreamFrameInfoStatus`）。
fn frame_status_from_dict(dict: &CFDictionary) -> Option<SCFrameStatus> {
    // SAFETY: SCStreamFrameInfoStatus 是 ScreenCaptureKit 导出的 &'static NSString，
    // NSString 与 CFString toll-free bridged，指针可直接作为 CFDictionary 的键。
    let key: *const c_void = unsafe { std::ptr::addr_of!(*SCStreamFrameInfoStatus) }.cast();
    // SAFETY: dict 是有效的 CFDictionary，key 为有效的 CFString/NSString 键；
    // 返回值由 dict 持有，在 attachments 存活期内有效。
    let raw_value = unsafe { dict.value(key) };
    let value_ptr = NonNull::new(raw_value.cast_mut())?;
    // SAFETY: 字典内非空附件值是 CFType 对象，存活期受 dict 保护；
    // 随后 downcast_ref 通过 CFGetTypeID 在运行时校验其为 CFNumber。
    let number: &CFNumber = unsafe { value_ptr.cast::<CFType>().as_ref() }.downcast_ref()?;

    // SCFrameStatus 的底层就是 NSInteger 枚举值（Complete=0, Idle=1, Blank=2,
    // Suspended=3, Started=4, Stopped=5）。
    let status = number.as_isize()?;
    Some(SCFrameStatus(status))
}

/// 读取 SCK 附加在 sample buffer 上的帧状态（`SCStreamFrameInfoStatus`）。
#[cfg_attr(not(test), allow(dead_code))]
fn frame_status(sb: &CMSampleBuffer) -> Option<SCFrameStatus> {
    with_sample_attachment_dict(sb, frame_status_from_dict).flatten()
}

/// 从 `SCStreamFrameInfoDirtyRects` 数组元素中解码一个 `CGRect`（兼容 `NSValue` 与 `CFDictionary`）。
fn decode_dirty_rect_element(elem_ptr: NonNull<c_void>) -> Option<CGRect> {
    // SAFETY: `elem_ptr` 来自存活中的 `CFArray`，指向合法 CF/ObjC 对象。
    let cf_elem: &CFType = unsafe { elem_ptr.cast::<CFType>().as_ref() };
    if let Some(rect_dict) = cf_elem.downcast_ref::<CFDictionary>() {
        let mut rect = CGRect::ZERO;
        // SAFETY: `rect_dict` 为有效 CFDictionary，`rect` 为有效栈指针。
        if unsafe { raw::CGRectMakeWithDictionaryRepresentation(rect_dict, &mut rect) } {
            return Some(rect);
        }
        return None;
    }
    // SAFETY: 所有 CoreFoundation 对象均是合法 ObjC 对象（toll-free bridged），
    // `downcast_ref::<NSValue>()` 通过运行时 `isKindOfClass:` 安全验证。
    let obj_elem: &AnyObject = unsafe { elem_ptr.cast::<AnyObject>().as_ref() };
    let ns_val = obj_elem.downcast_ref::<NSValue>()?;
    if let Some(rect) = ns_val.get_rect() {
        return Some(rect);
    }
    // 若系统以不同 struct tag（例如 CGRect vs _NSRect）封装，检查编码后再按 CGRect 读取。
    if ns_val
        .encoding()
        .is_some_and(|enc| enc.starts_with("{CGRect=") || enc.starts_with("{_NSRect="))
    {
        // SAFETY: 已在上面验证 objCType 编码为 `{CGRect=...}` 或 `{_NSRect=...}`，
        // 与 64 位 macOS 上 `CGRect` 内存布局完全一致。
        return Some(unsafe { ns_val.get::<CGRect>() });
    }
    None
}

/// 判断一个 `CGRect` 是否具有正面积（有限且宽高均大于 0）。
fn rect_has_positive_area(rect: CGRect) -> bool {
    rect.size.width.is_finite()
        && rect.size.height.is_finite()
        && rect.size.width > 0.0
        && rect.size.height > 0.0
}

/// `SCStreamFrameInfoDirtyRects` 汇总结果（零堆分配）。
#[derive(Debug, Clone, Copy, PartialEq)]
struct DirtyRectStats {
    /// 至少存在一个正面积脏区（或存在无法解析的元素需要保守放行）。
    has_change: bool,
    /// 所有可解析正面积脏区的累计面积（若存在无法解析元素则为 `None`，保守按全帧处理）。
    total_area: Option<f64>,
    /// 所有可解析正面积脏区的最大右/下边界（用于自动识别脏区坐标是输出像素还是 Retina backing 像素）。
    max_extent: (f64, f64),
    /// 所有可解析正面积脏区的精确包围盒 `(min_x, min_y, max_x, max_y)`。
    union_bbox: Option<(f64, f64, f64, f64)>,
    /// 有界原始脏区坐标缓存（`[0..raw_rect_count]` 有效；`overflowed == true` 时收敛为 `union_bbox`）。
    raw_rects: [(f64, f64, f64, f64); MAX_DIRTY_RECTS],
    /// 已记录的有效原始脏区数量（`0..=MAX_DIRTY_RECTS`）。
    raw_rect_count: usize,
    /// 是否超出 [`MAX_DIRTY_RECTS`] 上限（为 `true` 时收敛为精确包围盒）。
    overflowed: bool,
}

/// 从样本附件字典解析 `SCStreamFrameInfoDirtyRects` 统计信息与有界真实脏区列表：
/// - `None`：附件缺失或不是数组（无法证明无变化，调用方保守放行并按全帧处理）；
/// - `Some(stats)`：`stats.has_change == false` 表示空数组或全零面积（静屏微扰动，可跳过）。
fn dirty_rect_stats_from_dict(dict: &CFDictionary) -> Option<DirtyRectStats> {
    // SAFETY: SCStreamFrameInfoDirtyRects 是 ScreenCaptureKit 导出的 &'static NSString，
    // NSString 与 CFString toll-free bridged，指针可直接作为 CFDictionary 的键。
    let key: *const c_void = unsafe { std::ptr::addr_of!(*SCStreamFrameInfoDirtyRects) }.cast();
    // SAFETY: `dict` 是有效的 `CFDictionary`，`key` 为有效键，返回值在 `dict` 存活期内有效。
    let raw_value = unsafe { dict.value(key) };
    let value_ptr = NonNull::new(raw_value.cast_mut())?;
    // SAFETY: 字典内非空附件值是 CFType 对象，存活期受 `dict` 保护；
    // `downcast_ref::<CFArray>()` 覆盖 `NSArray`（toll-free bridged）。
    let rects: &CFArray = unsafe { value_ptr.cast::<CFType>().as_ref() }.downcast_ref()?;
    let count = rects.count();
    if count <= 0 {
        return Some(DirtyRectStats {
            has_change: false,
            total_area: Some(0.0),
            max_extent: (0.0, 0.0),
            union_bbox: None,
            raw_rects: [(0.0, 0.0, 0.0, 0.0); MAX_DIRTY_RECTS],
            raw_rect_count: 0,
            overflowed: false,
        });
    }
    let mut total_area = 0.0f64;
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = 0.0f64;
    let mut max_y = 0.0f64;
    let mut has_change = false;
    let mut has_unknown = false;
    let mut raw_rects = [(0.0, 0.0, 0.0, 0.0); MAX_DIRTY_RECTS];
    let mut raw_rect_count = 0usize;
    let mut overflowed = false;
    for i in 0..count {
        // SAFETY: `0 <= i < count`，元素指针在 `rects` 存活期内有效。
        let raw_elem = unsafe { rects.value_at_index(i) };
        let Some(elem_ptr) = NonNull::new(raw_elem.cast_mut()) else {
            has_change = true;
            has_unknown = true;
            continue;
        };
        let Some(rect) = decode_dirty_rect_element(elem_ptr) else {
            has_change = true;
            has_unknown = true;
            continue;
        };
        if !rect.origin.x.is_finite() || !rect.origin.y.is_finite() {
            has_change = true;
            has_unknown = true;
            continue;
        }
        if rect_has_positive_area(rect) {
            has_change = true;
            let x0 = rect.origin.x.max(0.0);
            let y0 = rect.origin.y.max(0.0);
            let x1 = (rect.origin.x + rect.size.width).max(0.0);
            let y1 = (rect.origin.y + rect.size.height).max(0.0);
            if x1.is_finite() && y1.is_finite() && x1 > x0 && y1 > y0 {
                total_area += (x1 - x0) * (y1 - y0);
                min_x = min_x.min(x0);
                min_y = min_y.min(y0);
                max_x = max_x.max(x1);
                max_y = max_y.max(y1);
                if !overflowed {
                    let candidate = (x0, y0, x1, y1);
                    if !raw_rects[..raw_rect_count].contains(&candidate) {
                        if raw_rect_count < MAX_DIRTY_RECTS {
                            raw_rects[raw_rect_count] = candidate;
                            raw_rect_count += 1;
                        } else {
                            overflowed = true;
                        }
                    }
                }
            }
        }
    }
    let union_bbox = if min_x.is_finite() && min_y.is_finite() && max_x > min_x && max_y > min_y {
        let union_bbox_area = (max_x - min_x) * (max_y - min_y);
        if union_bbox_area.is_finite() {
            total_area = total_area.min(union_bbox_area);
        }
        Some((min_x, min_y, max_x, max_y))
    } else {
        None
    };
    Some(DirtyRectStats {
        has_change,
        total_area: (!has_unknown).then_some(total_area),
        max_extent: (max_x, max_y),
        union_bbox,
        raw_rects,
        raw_rect_count,
        overflowed,
    })
}

/// 将单个原始浮点脏区 `(x0, y0, x1, y1)` 从源坐标系 `src_res` 映射到输出像素坐标系 `out_res`，
/// 采用保守外扩取整（左/上 `floor`、右/下 `ceil`）并严格裁剪到 `[0, out_res]` 边界内。
///
/// 映射后宽高任一为 0（如完全越界）或输入非法时返回 `None`。
pub(crate) fn map_raw_rect_to_output(
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    src_res: Resolution,
    out_res: Resolution,
) -> Option<Rect> {
    if src_res.width == 0
        || src_res.height == 0
        || out_res.width == 0
        || out_res.height == 0
        || !x0.is_finite()
        || !y0.is_finite()
        || !x1.is_finite()
        || !y1.is_finite()
        || x1 <= x0
        || y1 <= y0
    {
        return None;
    }
    let sx = f64::from(out_res.width) / f64::from(src_res.width);
    let sy = f64::from(out_res.height) / f64::from(src_res.height);
    let out_w = f64::from(out_res.width);
    let out_h = f64::from(out_res.height);
    let ox0 = (x0 * sx).floor().clamp(0.0, out_w) as u32;
    let oy0 = (y0 * sy).floor().clamp(0.0, out_h) as u32;
    let ox1 = (x1 * sx).ceil().clamp(0.0, out_w) as u32;
    let oy1 = (y1 * sy).ceil().clamp(0.0, out_h) as u32;
    if ox1 > ox0 && oy1 > oy0 {
        Some(Rect::new(ox0, oy0, ox1 - ox0, oy1 - oy0))
    } else {
        None
    }
}

/// 邮箱覆写旧帧时合并两帧的真实脏区列表（不丢失首帧脏区）：
/// - 任一帧脏区未知（`None` 或为空）时保守退回 `None`（按全帧失效处理）；
/// - 两帧均有精确脏区时去重并集；若总数不超过 [`MAX_DIRTY_RECTS`] 则逐项保留，
///   超限时收敛为包含全部脏区的精确最小包围盒，绝不退回 `(0, 0)` 假矩形。
pub(crate) fn merge_dirty_rects(
    prev: Option<Vec<Rect>>,
    next: Option<Vec<Rect>>,
) -> Option<Vec<Rect>> {
    let (prev, next) = match (prev, next) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => (a, b),
        _ => return None,
    };
    let mut merged = Vec::with_capacity(prev.len().saturating_add(next.len()).min(MAX_DIRTY_RECTS));
    let mut overflowed = false;
    let mut bbox: Option<Rect> = None;
    for r in prev.into_iter().chain(next) {
        if r.is_empty() {
            continue;
        }
        bbox = Some(match bbox {
            Some(acc) => acc.union(&r),
            None => r,
        });
        if !overflowed && !merged.contains(&r) {
            if merged.len() < MAX_DIRTY_RECTS {
                merged.push(r);
            } else {
                overflowed = true;
            }
        }
    }
    if overflowed {
        bbox.map(|b| vec![b])
    } else if merged.is_empty() {
        None
    } else {
        Some(merged)
    }
}

/// 将 `DirtyRectStats` 统一换算为：
/// 1. 供码控使用的脏区面积占比 `dirty_ratio`（`0.0..=1.0`，保持原有算法零改动）；
/// 2. 经 backing-space / 输出分辨率统一坐标映射与裁剪后的有界真实脏区列表（上限 [`MAX_DIRTY_RECTS`]，
///    超限时收敛为精确最小包围盒）。
///
/// 通过流级锁存标志 `dirty_in_backing_space` 记住本流是否以 Retina 物理像素坐标上报脏区
/// （一旦首帧或任意帧观测到 `max_extent` 超出输出尺寸即锁存为 `true`），消除左上象限 4× 高估与中线抖动；
/// 若分母不可用或存在未知元素，返回 `(None, None)`（保守按全帧处理）。
fn compute_dirty_info(
    stats: Option<DirtyRectStats>,
    output_res: Resolution,
    backing_res: Resolution,
    dirty_in_backing_space: &AtomicBool,
) -> (Option<f32>, Option<Vec<Rect>>) {
    let Some(stats) = stats else {
        return (None, None);
    };
    let Some(area) = stats.total_area else {
        return (None, None);
    };
    if !area.is_finite() || area <= 0.0 || output_res.width == 0 || output_res.height == 0 {
        return (None, None);
    }
    let out_w = f64::from(output_res.width);
    let out_h = f64::from(output_res.height);
    let back_w_u32 = backing_res.width.max(output_res.width).max(1);
    let back_h_u32 = backing_res.height.max(output_res.height).max(1);
    let back_w = f64::from(back_w_u32);
    let back_h = f64::from(back_h_u32);
    let has_larger_backing = back_w > out_w || back_h > out_h;
    if has_larger_backing
        && (stats.max_extent.0 > out_w * 1.05 || stats.max_extent.1 > out_h * 1.05)
    {
        dirty_in_backing_space.store(true, Ordering::Relaxed);
    }
    let in_backing_space = has_larger_backing && dirty_in_backing_space.load(Ordering::Relaxed);
    let denom = if in_backing_space {
        back_w * back_h
    } else {
        out_w * out_h
    };
    if denom <= 0.0 {
        return (None, None);
    }
    let ratio = Some((area / denom).clamp(0.0, 1.0) as f32);

    let src_res = if in_backing_space {
        Resolution::new(back_w_u32, back_h_u32)
    } else {
        output_res
    };
    let dirty_rects = if stats.overflowed {
        stats
            .union_bbox
            .and_then(|(x0, y0, x1, y1)| {
                map_raw_rect_to_output(x0, y0, x1, y1, src_res, output_res)
            })
            .map(|bbox| vec![bbox])
    } else {
        let mut rects = Vec::with_capacity(stats.raw_rect_count);
        for &(x0, y0, x1, y1) in &stats.raw_rects[..stats.raw_rect_count] {
            if let Some(mapped) = map_raw_rect_to_output(x0, y0, x1, y1, src_res, output_res) {
                if !rects.contains(&mapped) {
                    rects.push(mapped);
                }
            }
        }
        (!rects.is_empty()).then_some(rects)
    };
    (ratio, dirty_rects)
}

/// 兼容既有单测：仅返回 `dirty_ratio`。
#[cfg(test)]
fn compute_dirty_ratio(
    stats: Option<DirtyRectStats>,
    output_res: Resolution,
    backing_res: Resolution,
    dirty_in_backing_space: &AtomicBool,
) -> Option<f32> {
    compute_dirty_info(stats, output_res, backing_res, dirty_in_backing_space).0
}

/// 检查样本附件字典中的 `SCStreamFrameInfoDirtyRects` 是否表明有真实画面变动：
/// - `None`：附件缺失或不是数组（无法证明无变化，调用方应保守放行）；
/// - `Some(false)`：附件存在且为空数组，或全部矩形面积均为 0（静态微扰动，可跳过）；
/// - `Some(true)`：至少存在一个正面积脏区（或存在无法解析的元素），必须放行。
#[cfg_attr(not(test), allow(dead_code))]
fn dirty_rects_from_dict(dict: &CFDictionary) -> Option<bool> {
    dirty_rect_stats_from_dict(dict).map(|s| s.has_change)
}

/// 读取 `CMSampleBuffer` 的 `SCStreamFrameInfoDirtyRects` 判定是否应放行（仅非空/缺失时放行）。
#[cfg_attr(not(test), allow(dead_code))]
fn dirty_rects_indicate_change(sb: &CMSampleBuffer) -> bool {
    with_sample_attachment_dict(sb, dirty_rects_from_dict)
        .flatten()
        .unwrap_or(true)
}

#[cfg(feature = "bench-metrics")]
fn bench_debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("RC_BENCH_DEBUG").is_some())
}

/// 在 `bench-metrics` + `RC_BENCH_DEBUG` 下打印 `Complete` 帧的 `SCStreamFrameInfoDirtyRects`
/// 矩形数量与总面积，用于排查静屏微扰动来源。
#[cfg(feature = "bench-metrics")]
fn log_dirty_rects_debug(dict: &CFDictionary, has_dirty_change: bool) {
    // SAFETY: SCStreamFrameInfoDirtyRects 是 ScreenCaptureKit 导出的 &'static NSString。
    let key: *const c_void = unsafe { std::ptr::addr_of!(*SCStreamFrameInfoDirtyRects) }.cast();
    // SAFETY: `dict` 是有效的 `CFDictionary`，`key` 为有效键。
    let raw_value = unsafe { dict.value(key) };
    let Some(value_ptr) = NonNull::new(raw_value.cast_mut()) else {
        eprintln!("[rc-capture-macos] dirty_rects=None passed={has_dirty_change}");
        return;
    };
    // SAFETY: 字典内非空附件值是 CFType 对象，存活期受 `dict` 保护。
    let Some(rects): Option<&CFArray> =
        unsafe { value_ptr.cast::<CFType>().as_ref() }.downcast_ref()
    else {
        eprintln!("[rc-capture-macos] dirty_rects=NonArray passed={has_dirty_change}");
        return;
    };
    let count = rects.count();
    let mut total_area = 0.0f64;
    let mut unknown = 0isize;
    for i in 0..count {
        // SAFETY: `0 <= i < count`，元素指针在 `rects` 存活期内有效。
        let raw_elem = unsafe { rects.value_at_index(i) };
        let Some(elem_ptr) = NonNull::new(raw_elem.cast_mut()) else {
            unknown += 1;
            continue;
        };
        let Some(rect) = decode_dirty_rect_element(elem_ptr) else {
            unknown += 1;
            continue;
        };
        if rect_has_positive_area(rect) {
            total_area += rect.size.width * rect.size.height;
        }
    }
    eprintln!(
        "[rc-capture-macos] dirty_rects count={count} total_area={total_area:.1} unknown={unknown} passed={has_dirty_change}"
    );
}

/// `CMTime` 换算成微秒。无效时间（flags 无 Valid 或 timescale 为 0）返回 None。
fn cmtime_to_us(t: CMTime) -> Option<u64> {
    if !t.flags.contains(CMTimeFlags::Valid) || t.timescale == 0 {
        return None;
    }
    // i128 中间量避免 value * 1e6 溢出（mach 时间戳的 value 可以很大）。
    let us = (t.value as i128) * 1_000_000 / (t.timescale as i128);
    u64::try_from(us).ok()
}

/// PTS 换算成微秒。无效时间（flags 无 Valid 或 timescale 为 0）返回 None。
fn pts_us(sb: &CMSampleBuffer) -> Option<u64> {
    // SAFETY: 纯 getter，无副作用。
    cmtime_to_us(unsafe { sb.presentation_time_stamp() })
}

/// 读取 `CMClockGetHostTimeClock()` 当前时刻（微秒），与 ScreenCaptureKit
/// `CMSampleBuffer` 的 PTS 处于**完全相同的单调时钟域**。
pub(crate) fn host_time_us() -> u64 {
    // SAFETY: CMClockGetHostTimeClock 返回系统全局单调主机时钟；time() 为纯只读查询。
    let t = unsafe {
        let clock = CMClock::host_time_clock();
        clock.time()
    };
    cmtime_to_us(t).unwrap_or(0)
}

/// 计算传给 `SCStreamConfiguration::setMinimumFrameInterval` 的帧间隔（微秒）。
///
/// - `fps >= 120`：返回 `0`（对应 Apple 文档 `kCMTimeZero`），按显示器原生高刷节拍交付，
///   避免 120Hz ProMotion 下轻微抖动触发 SCK 节流丢帧（`skip_src`）；
/// - `fps < 120`：取标称间隔的 90%（`900_000 / fps`）：
///   - 60fps → `15_000µs`（严格落在 120Hz `8.33ms` / 80Hz `12.5ms` 与 60Hz `16.67ms` 之间，
///     既挡掉 ProMotion 8.33ms/12.5ms 倍频/微扰帧，又保留 `1.67ms` 抖动容差，实测 `S2` 锁定 ~61fps）；
///   - 30fps → `30_000µs`（保留 `3.33ms` 抖动容差）。
pub(crate) fn minimum_frame_interval_us(fps: u32) -> i64 {
    if fps >= 120 {
        return 0;
    }
    let fps = i64::from(fps.max(1));
    (900_000 / fps).max(1)
}

/// 取出 sample buffer 里的像素缓冲（retain +1，交给 [`PixelBufferHandle`]）。
fn image_buffer(sb: &CMSampleBuffer) -> Option<PixelBufferHandle> {
    // SAFETY: 纯 getter；返回的 CVImageBuffer 已被绑定层 retain。
    let buffer = unsafe { sb.image_buffer() }?;
    Some(PixelBufferHandle(buffer))
}

/// NSError → 可读字符串。
fn nserror_message(err: &NSError) -> String {
    // localizedDescription 在绑定层是安全方法（纯属性 getter）。
    // NSString 实现了 Display，to_string() 是安全方法。
    err.localizedDescription().to_string()
}

// ─────────────────────────────────────────────────────────────────────────────
// 帧回调类：实现 SCStreamOutput（帧）+ SCStreamDelegate（M10 错误停止）
// ─────────────────────────────────────────────────────────────────────────────

// 回调侧不做 PTS 节拍过滤。
//
// 曾按「距上一放行帧 ≥ 标称间隔 87.5%」过滤 ProMotion 中间拍，真机实测
// 反而把采集打到 48fps：SCK 在 60fps 内容下的实际到达节拍是 8.33/16.67ms
// **交替**，两个 8.33ms 连拍后再来一帧时，距上一放行帧已累计 25ms 的
// 「假间隔」——过滤器把它当 60Hz 双拍放行，节拍被拉长到 25ms 周期。
// 多余供给（~70fps）交给调度器的预算门（R1 丢到达帧）在真机全链里
// 实测更优：0 预算丢帧时 displayed 72fps、P50 14.3ms，双达标。
// （过滤函数 `should_emit_pts` / `callback_min_spacing_us` 已删除；
// 如需重加，必须先修交替拍累计间隔的判定基准。）

struct HandlerIvars {
    mailbox: FrameMailbox,
    output_res: Resolution,
    backing_res: Resolution,
    dirty_in_backing_space: AtomicBool,
}

define_class!(
    // SAFETY:
    // - 超类 NSObject 没有子类化要求。
    // - 本类不实现 Drop。
    // - ivars（FrameMailbox 与不可变分辨率）只在构造线程写入，在 SCStream 的串行 sample handler
    //   queue 与 delegate queue 上通过内部 Mutex + Condvar 并发访问；
    //   FrameMailbox 本身是 Send + Sync。
    #[unsafe(super(NSObject))]
    #[name = "RcSckStreamHandler"]
    #[ivars = HandlerIvars]
    struct RcSckStreamHandler;

    unsafe impl NSObjectProtocol for RcSckStreamHandler {}

    unsafe impl SCStreamOutput for RcSckStreamHandler {
        // 方法名必须与协议声明的选择器部分一致（non_snake_case 是刻意的）。
        #[allow(non_snake_case)]
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            // 坑 6：本回调运行在 SCStream 的独立串行 dispatch queue 上。
            // 只做指针/整数级提取并投递单槽邮箱，绝不做重活。
            if r#type != SCStreamOutputType::Screen {
                return;
            }
            let Some((status, dirty_stats)) = with_sample_attachment_dict(sample_buffer, |dict| {
                let status = frame_status_from_dict(dict)?;
                let stats = match status {
                    SCFrameStatus::Complete => {
                        let s = dirty_rect_stats_from_dict(dict);
                        #[cfg(feature = "bench-metrics")]
                        if bench_debug_enabled() {
                            log_dirty_rects_debug(dict, s.map_or(true, |st| st.has_change));
                        }
                        s
                    }
                    _ => None,
                };
                Some((status, stats))
            })
            .flatten() else {
                return;
            };
            match status {
                SCFrameStatus::Complete => {
                    if dirty_stats.is_some_and(|s| !s.has_change) {
                        // SCStreamFrameInfoDirtyRects 存在且为空 / 零面积：
                        // 静屏微扰动，不接触像素直接按 Idle 跳过（契约 C1）。
                        return;
                    }
                }
                SCFrameStatus::Suspended => {
                    // 锁屏/挂起：上报 SessionLocked 语义，入独立控制槽，由上层决定重建。
                    self.ivars().mailbox.push_control(ControlEvent::Suspended);
                    return;
                }
                // Idle/Blank/Started/Stopped：没有新内容 —— 不产帧（契约 C1，
                // 静止画面下采集侧 CPU 占用必须接近 0）。
                _ => return,
            }
            let pts = pts_us(sample_buffer).unwrap_or(0);
            #[cfg(feature = "bench-metrics")]
            let callback_us = host_time_us();
            let Some(buffer) = image_buffer(sample_buffer) else {
                return;
            };
            let ivars = self.ivars();
            let (dirty_ratio, dirty_rects) = compute_dirty_info(
                dirty_stats,
                ivars.output_res,
                ivars.backing_res,
                &ivars.dirty_in_backing_space,
            );
            // 最新帧优先：直接覆写单槽，被覆盖的旧 CVPixelBuffer 立刻 Drop 归还 SCK 池，
            // 绝不阻塞回调队列。
            ivars.mailbox.push_frame(FramePayload {
                buffer,
                pts_us: pts,
                dirty_ratio,
                dirty_rects,
                #[cfg(feature = "bench-metrics")]
                callback_us,
            });
        }
    }

    unsafe impl SCStreamDelegate for RcSckStreamHandler {
        #[allow(non_snake_case)]
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_didStopWithError(&self, _stream: &SCStream, error: &NSError) {
            // M10：权限被撤销等致命停止在这里上报（AccessLost 语义），
            // 绝不 panic —— 回调线程 panic 会直接杀掉进程。
            let message = nserror_message(error);
            self.ivars()
                .mailbox
                .push_control(ControlEvent::Stopped { message });
        }
    }
);

// 构造方法放在 define_class! 之外：纯 Rust 辅助函数，不暴露给 ObjC。
impl RcSckStreamHandler {
    fn new(
        mailbox: FrameMailbox,
        output_res: Resolution,
        backing_res: Resolution,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(HandlerIvars {
            mailbox,
            output_res,
            backing_res,
            dirty_in_backing_space: AtomicBool::new(false),
        });
        // SAFETY: this 是 alloc 出来的本类实例，调用超类 init 完成 NSObject
        // 初始化并移交所有权（返回 +1 的 Retained）。
        unsafe { msg_send![super(this), init] }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 显示器枚举
// ─────────────────────────────────────────────────────────────────────────────

/// 一个可采集显示器的快照（plain 数据 + 建 filter 用的 SCDisplay 强引用）。
///
/// 只在建流调用栈上存活，不进入 [`SckBackend`](crate::sck::SckBackend)，
/// 因此无需 Send。
pub(crate) struct DisplaySnapshot {
    /// CGDirectDisplayID。
    pub display_id: u32,
    /// 物理分辨率（像素）。
    pub backing: Resolution,
    /// 物理分辨率 / 逻辑分辨率（Retina 为 2.0）。
    pub scale: f32,
    /// 建 SCContentFilter 用的强引用。
    display: Retained<SCDisplay>,
    /// 需要从采集中排除的隐私幕布 `SCWindow` 列表（被控端防窥模式激活时生效）。
    privacy_windows: Vec<Retained<SCWindow>>,
    /// 当前进程对应的 `SCRunningApplication`（仅 `bench-metrics` 排除自渲染窗用）。
    #[cfg(feature = "bench-metrics")]
    self_app: Option<Retained<SCRunningApplication>>,
    /// 当前进程所属的 `SCWindow` 列表（`self_app` 缺失时的窗口级排除兜底）。
    #[cfg(feature = "bench-metrics")]
    self_windows: Vec<Retained<SCWindow>>,
}

/// 枚举所有可采集显示器（异步获取 `SCShareableContent`，坑 5）。
pub(crate) fn enumerate_displays() -> Result<Vec<DisplaySnapshot>, String> {
    let content = fetch_shareable_content(Duration::from_secs(3))?;
    // SAFETY: displays 是纯属性 getter。
    let displays = unsafe { content.displays() };
    let excluded_privacy_ids = privacy_and_virtual::privacy_excluded_window_ids();
    let privacy_windows: Vec<Retained<SCWindow>> = if excluded_privacy_ids.is_empty() {
        Vec::new()
    } else {
        // SAFETY: windows/windowID 均为只读属性 getter。
        unsafe { content.windows() }
            .to_vec()
            .into_iter()
            .filter(|w| excluded_privacy_ids.contains(&unsafe { w.windowID() }))
            .collect()
    };
    #[cfg(feature = "bench-metrics")]
    let (self_app, self_windows) = {
        let self_pid = std::process::id() as i32;
        // SAFETY: applications 是纯属性 getter；processID 为只读 pid_t getter。
        let app = unsafe { content.applications() }
            .to_vec()
            .into_iter()
            .find(|app| unsafe { app.processID() } == self_pid);
        // SAFETY: windows/owningApplication/processID 均为只读属性 getter。
        let wins: Vec<Retained<SCWindow>> = unsafe { content.windows() }
            .to_vec()
            .into_iter()
            .filter(|w| unsafe {
                w.owningApplication()
                    .is_some_and(|owner| owner.processID() == self_pid)
            })
            .collect();
        (app, wins)
    };
    // to_vec 是 objc2-foundation 的安全方法（逐个 retain 后返回 Vec）。
    Ok(displays
        .to_vec()
        .into_iter()
        .map(|display| {
            // SAFETY: width/height/frame/displayID 均为纯 getter。
            unsafe {
                let backing_w = display.width() as u32;
                let backing_h = display.height() as u32;
                let frame = display.frame();
                // 逻辑宽度只用来推 scale（Retina 为 2.0）；请求的输出分辨率
                // 由 sck.rs 按 backing/scale 下采样计算（M9）。
                let logical_w = frame.size.width.round().max(1.0) as u32;
                let scale = if logical_w > 0 {
                    backing_w as f32 / logical_w as f32
                } else {
                    1.0
                };
                DisplaySnapshot {
                    display_id: display.displayID(),
                    backing: Resolution::new(backing_w, backing_h),
                    scale,
                    display,
                    privacy_windows: privacy_windows.clone(),
                    #[cfg(feature = "bench-metrics")]
                    self_app: self_app.clone(),
                    #[cfg(feature = "bench-metrics")]
                    self_windows: self_windows.clone(),
                }
            }
        })
        .collect())
}

/// 异步获取 `SCShareableContent`，转成同步等待（带超时）。
fn fetch_shareable_content(timeout: Duration) -> Result<Retained<SCShareableContent>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let block = block2::RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = if !error.is_null() {
                // SAFETY: 完成回调保证 error 在回调期间有效；只做同步读取。
                Err(nserror_message(unsafe { &*error }))
            } else {
                match NonNull::new(content) {
                    Some(ptr) => {
                        // SAFETY: 回调期间指针有效；retain 把 +0 借用升级为 +1
                        // 强引用（无论调用方是否移交所有权，这都是正确语义）。
                        // 返回 None 仅当指针为 null（上面已排除）。
                        unsafe { Retained::retain(ptr.as_ptr()) }
                            .ok_or_else(|| "retain SCShareableContent 失败".to_string())
                    }
                    None => Err("SCShareableContent 为空".to_string()),
                }
            };
            // 捕获的 Sender 是 Send + Sync：block 被 ObjC 拷贝后在任意系统队列
            // 上恰好调用一次，线程上是安全的。
            let _ = tx.send(result);
        },
    );
    // SAFETY: block 签名与 completionHandler:（content, error）匹配。
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true,
            false,
            &block,
        );
    }
    rx.recv_timeout(timeout)
        .map_err(|_| "获取 SCShareableContent 超时".to_string())?
}

// ─────────────────────────────────────────────────────────────────────────────
// 建流：filter + configuration + stream + start
// ─────────────────────────────────────────────────────────────────────────────

/// 拥有 SCStream 全套对象（stream、handler、queue）。
///
/// 字段顺序即 Drop 顺序：stream 先释放（释放过程中回调仍可能进来，此时
/// handler 还活着），handler 与 queue 随后释放。
pub(crate) struct StreamHandles {
    stream: Retained<SCStream>,
    /// SCStream 的 delegate 是 unretained 的 —— 这个强引用让它活到 stream 之后。
    _handler: Retained<RcSckStreamHandler>,
    _queue: dispatch2::DispatchRetained<dispatch2::DispatchQueue>,
}

// SAFETY: 这组对象随 SckBackend 一起被移动（Send = 跨线程移交所有权）。
// 访问始终发生在持有 &mut self 的单一线程上（CaptureBackend: Send 的使用
// 约定），不存在并发访问；释放（Drop）发生在最终 owning 线程，ObjC 的
// release 是线程安全的。
unsafe impl Send for StreamHandles {}

impl Drop for StreamHandles {
    fn drop(&mut self) {
        // M8：SCStream 停止后不可复用 —— 丢弃前显式 stop（异步，不等待），
        // 随后字段按声明顺序释放 stream 本体。
        // SAFETY: completion 传 None 合法（只是不要回调）。
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}

/// 组装 `SCStreamConfiguration`（M5/M7/M9 全部在这里落实）。
fn make_configuration(
    target: Resolution,
    fps: u32,
    shows_cursor: bool,
) -> Retained<SCStreamConfiguration> {
    // SAFETY: new 是无参构造。
    let config = unsafe { SCStreamConfiguration::new() };
    // M5：'420v'（NV12 video range）—— VideoToolbox 的原生输入，零拷贝直达
    // 编码器，无需 CPU 色彩转换；显式指定 BT.709 矩阵，与渲染端 Metal shader
    // (`fs_nv12_video` BT.709) 及 VideoToolbox BT.709 VUI 严格对齐，消除默认 BT.601 色偏与发灰。
    // SAFETY: 纯属性 setter，kCGDisplayStreamYCbCrMatrix_ITU_R_709_2 为 CoreGraphics 静态 CFString 常量。
    unsafe {
        config.setPixelFormat(PIXEL_FORMAT_420V);
        if let Some(bt709) = raw::kCGDisplayStreamYCbCrMatrix_ITU_R_709_2 {
            config.setColorMatrix(bt709);
        }
    }
    // M7：默认关闭光标（远端由输入层单独绘制光标）；当控制端选择「使用被控端鼠标」时开启。
    // SAFETY: 纯属性 setter。
    unsafe { config.setShowsCursor(shows_cursor) };
    // 显式关闭音频采集，避免 SCK 维护额外音频管线与回调开销。
    // SAFETY: 纯属性 setter。
    unsafe { config.setCapturesAudio(false) };
    // M9：输出逻辑分辨率 —— Retina 2x 屏上等价于把 2x 帧下采样到 1x，
    // 不把 2x 分辨率喂给编码器（带宽翻倍）；显式启用 scalesToFit 确保
    // 目标分辨率小于物理分辨率时由 GPU 等比缩放填满画幅而非左上角 1:1 裁切。
    // SAFETY: 纯属性 setter。
    unsafe {
        config.setWidth(target.width as usize);
        config.setHeight(target.height as usize);
        config.setScalesToFit(true);
    }
    // minimumFrameInterval：60Hz 带 10% vsync 抖动容差（15_000us），120Hz+ 用 kCMTimeZero
    // 按显示器原生高刷率采集（见 minimum_frame_interval_us）；静止画面下回调仍不触发（契约 C1）。
    let interval_us = minimum_frame_interval_us(fps);
    let min_interval = if interval_us == 0 {
        CMTime {
            epoch: 0,
            flags: CMTimeFlags::Valid,
            value: 0,
            timescale: 1,
        }
    } else {
        CMTime {
            epoch: 0,
            flags: CMTimeFlags::Valid,
            value: interval_us,
            timescale: 1_000_000,
        }
    };
    // SAFETY: 纯属性 setter。
    unsafe { config.setMinimumFrameInterval(min_interval) };
    // SCK 底层 IOSurface 池容量（见 SCK_SURFACE_POOL_DEPTH 注释，必须严格大于
    // 消费侧保活环 + 回调通道之和，避免池耗尽卡死出帧）。
    // SAFETY: 纯属性 setter。
    unsafe { config.setQueueDepth(SCK_SURFACE_POOL_DEPTH) };
    config
}

/// 组装 `SCContentFilter`（捕获整块显示器，含桌面与 Dock，并自动排除防窥隐私幕布窗口）。
fn make_filter(display: &DisplaySnapshot) -> Retained<SCContentFilter> {
    let excluded: Retained<NSArray<SCWindow>> = if display.privacy_windows.is_empty() {
        NSArray::new()
    } else {
        NSArray::from_retained_slice(&display.privacy_windows)
    };
    // SAFETY: alloc + initWithDisplay:excludingWindows: 标准构造；
    // display 与 excluded 均为有效强引用。
    unsafe {
        let allocated = SCContentFilter::alloc();
        SCContentFilter::initWithDisplay_excludingWindows(allocated, &display.display, &excluded)
    }
}

/// 组装排除本进程窗口的 `SCContentFilter`（仅 `bench-metrics` 用，避免自渲染呈现场导致桌面反馈）。
///
/// 优先使用 `excludingWindows` 显式剔除同 PID 窗口（即便无 Dock 图标或窗口刚创建也精确生效），
/// 若暂无可见 `SCWindow` 再回退到 `excludingApplications`。
#[cfg(feature = "bench-metrics")]
fn make_filter_excluding_self(display: &DisplaySnapshot) -> Retained<SCContentFilter> {
    if std::env::var_os("RC_BENCH_DEBUG").is_some() {
        eprintln!(
            "[rc-capture-macos] bench filter: self_windows={} self_app={}",
            display.self_windows.len(),
            display.self_app.is_some()
        );
    }
    if !display.self_windows.is_empty() {
        let excluded_wins: Retained<NSArray<SCWindow>> =
            NSArray::from_retained_slice(&display.self_windows);
        // SAFETY: alloc + initWithDisplay:excludingWindows: 标准构造，display 与 excluded_wins 有效。
        unsafe {
            let allocated = SCContentFilter::alloc();
            SCContentFilter::initWithDisplay_excludingWindows(
                allocated,
                &display.display,
                &excluded_wins,
            )
        }
    } else if let Some(app) = &display.self_app {
        let excluded_apps: Retained<NSArray<SCRunningApplication>> = NSArray::from_slice(&[&**app]);
        let empty_wins: Retained<NSArray<SCWindow>> = NSArray::new();
        // SAFETY: alloc + initWithDisplay:excludingApplications:exceptingWindows: 标准构造，
        // display、excluded_apps、empty_wins 均为有效的强引用 ObjC 对象。
        unsafe {
            let allocated = SCContentFilter::alloc();
            SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                allocated,
                &display.display,
                &excluded_apps,
                &empty_wins,
            )
        }
    } else {
        eprintln!(
            "[rc-capture-macos] bench warning: self_windows and self_app are both empty; capturing unfiltered display"
        );
        make_filter(display)
    }
}

/// 创建 SCK 帧回调专用的高优先级串行队列（`QOS_CLASS_USER_INTERACTIVE` + 每帧自动释放池）。
fn create_sck_callback_queue() -> dispatch2::DispatchRetained<dispatch2::DispatchQueue> {
    let attr = dispatch2::DispatchQueueAttr::with_autorelease_frequency(
        dispatch2::DispatchQueueAttr::SERIAL,
        dispatch2::DispatchAutoReleaseFrequency::WORK_ITEM,
    );
    let attr = dispatch2::DispatchQueueAttr::with_qos_class(
        Some(&attr),
        dispatch2::DispatchQoS::UserInteractive,
        0,
    );
    dispatch2::DispatchQueue::new("rc.capture.sck.frames", Some(&attr))
}

/// 带 `exclude_self` 选项构建 SCStream（`exclude_self` 仅在 `bench-metrics` 下生效）。
///
/// 调用方（`sck.rs`）保证 `display` 是本次调用栈上刚枚举出来的快照。
pub(crate) fn build_stream_with_options(
    display: &DisplaySnapshot,
    target: Resolution,
    fps: u32,
    exclude_self: bool,
    shows_cursor: bool,
) -> Result<(StreamHandles, FrameMailbox), String> {
    #[cfg(feature = "bench-metrics")]
    let filter = if exclude_self {
        make_filter_excluding_self(display)
    } else {
        make_filter(display)
    };
    #[cfg(not(feature = "bench-metrics"))]
    let filter = {
        let _ = exclude_self;
        make_filter(display)
    };
    let config = make_configuration(target, fps, shows_cursor);
    let mailbox = FrameMailbox::new();
    let real_backing = display_backing_resolution(display.display_id).unwrap_or(display.backing);
    let handler = RcSckStreamHandler::new(mailbox.clone(), target, real_backing);

    // SAFETY: initWithFilter:configuration:delegate: 标准构造。delegate 参数是
    // unretained 的 —— StreamHandles 持有 handler 强引用且比 stream 活得久
    // （见字段顺序注释），生命周期安全。
    let stream = unsafe {
        let allocated = SCStream::alloc();
        SCStream::initWithFilter_configuration_delegate(
            allocated,
            &filter,
            &config,
            Some(ProtocolObject::from_ref(&*handler)),
        )
    };

    // 坑 6：帧回调走独立串行高优先级（UserInteractive）queue，不占用主队列 / 全局队列。
    let queue = create_sck_callback_queue();
    // SAFETY: handler 实现了 SCStreamOutput；sample handler queue 为我们创建的
    // 串行队列，回调串行到达，满足协议的线程要求。
    unsafe {
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*handler),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
            .map_err(|e| nserror_message(&e))?;
    }

    // 启动并等待完成回调（拿 NSError 描述，例如权限被拒）。
    let (start_tx, start_rx) = std::sync::mpsc::channel();
    let start_block = block2::RcBlock::new(move |error: *mut NSError| {
        let message = if error.is_null() {
            None
        } else {
            // SAFETY: 完成回调保证 error 在回调期间有效；只做同步读取。
            Some(nserror_message(unsafe { &*error }))
        };
        // 捕获的 Sender 是 Send + Sync，回调恰好在某个系统队列上调用一次。
        let _ = start_tx.send(message);
    });
    // SAFETY: block 签名与 startCaptureWithCompletionHandler:（*mut NSError）匹配。
    unsafe { stream.startCaptureWithCompletionHandler(Some(&start_block)) };
    match start_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(None) => {}
        Ok(Some(message)) => {
            return Err(format!("startCapture 失败: {message}"));
        }
        Err(_) => return Err("等待 startCapture 完成回调超时".to_string()),
    }

    Ok((
        StreamHandles {
            stream,
            _handler: handler,
            _queue: queue,
        },
        mailbox,
    ))
}

/// 只读锁定 `'420v'` `CVPixelBuffer` 并拷贝左上角最多 `max_w × max_h` 像素的 Y 平面亮度数据（仅 `bench-metrics`）。
#[cfg(feature = "bench-metrics")]
pub(crate) fn read_pixel_buffer_y_region(
    buf_ptr: *const c_void,
    max_w: usize,
    max_h: usize,
) -> Result<(usize, usize, Vec<u8>), String> {
    use objc2_core_video::{
        kCVReturnSuccess, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
        CVPixelBufferGetHeightOfPlane, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress,
        CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    if buf_ptr.is_null() {
        return Err("CVPixelBuffer 指针为空".to_string());
    }
    if max_w == 0 || max_h == 0 {
        return Err("读取区域宽高必须大于 0".to_string());
    }
    // SAFETY: 调用方传入的指针来自存活中的 `CapturedFrame`（`resource_guard` 保活），
    // 指向有效 `CVPixelBuffer`。采用 `ReadOnly` 锁定并在退出前配对解锁。
    let pb: &CVPixelBuffer = unsafe { &*(buf_ptr.cast::<CVPixelBuffer>()) };
    // SAFETY: `pb` 指向已保活的有效 `CVPixelBuffer`，`ReadOnly` 标志合法且下方配对解锁。
    let rv = unsafe { CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    if rv != kCVReturnSuccess {
        return Err(format!("CVPixelBufferLockBaseAddress(ReadOnly) 失败: {rv}"));
    }
    let plane_w = CVPixelBufferGetWidthOfPlane(pb, 0);
    let plane_h = CVPixelBufferGetHeightOfPlane(pb, 0);
    let stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
    let base = CVPixelBufferGetBaseAddressOfPlane(pb, 0) as *const u8;
    if base.is_null() || plane_w == 0 || plane_h == 0 || stride < plane_w {
        // SAFETY: 与上方 ReadOnly 加锁配对解锁。
        unsafe { CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
        return Err("CVPixelBuffer Y 平面无效或未就绪".to_string());
    }
    let w = plane_w.min(max_w);
    let h = plane_h.min(max_h);
    let mut out = vec![0u8; w * h];
    for row in 0..h {
        // SAFETY: row < plane_h 且 w <= plane_w <= stride，源切片位于已锁定的平面 0 内存内。
        let src = unsafe { std::slice::from_raw_parts(base.add(row * stride), w) };
        out[row * w..(row + 1) * w].copy_from_slice(src);
    }
    // SAFETY: 与上方 ReadOnly 加锁配对解锁。
    unsafe { CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    Ok((w, h, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_foundation::CFMutableDictionary;

    fn make_empty_sample_buffer() -> CFRetained<CMSampleBuffer> {
        let mut raw_sb: *mut CMSampleBuffer = std::ptr::null_mut();
        // SAFETY: sample_buffer_out 指向有效的栈变量；创建 1 个无负载的
        // sample buffer 仅用于测试 sample attachments array 的读写。
        let status = unsafe {
            CMSampleBuffer::create(
                None,
                None,
                true,
                None,
                std::ptr::null_mut(),
                None,
                1,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                NonNull::from(&mut raw_sb),
            )
        };
        assert_eq!(status, 0, "CMSampleBufferCreate 失败: OSStatus={status}");
        let ptr = NonNull::new(raw_sb).expect("CMSampleBufferCreate 返回空指针");
        // SAFETY: CMSampleBufferCreate 遵循 Create Rule，返回 +1 所有权。
        unsafe { CFRetained::from_raw(ptr) }
    }

    #[test]
    fn frame_status_reads_sample_attachments_array() {
        let sb = make_empty_sample_buffer();
        // 1. 尚未创建 sample attachments 时返回 None。
        assert_eq!(frame_status(&sb), None);

        // 2. 创建空的 sample attachments 字典后仍应返回 None。
        // SAFETY: sb 有效，create_if_necessary = true 创建并保留附件数组。
        let attachments = unsafe { sb.sample_attachments_array(true) }
            .expect("create_if_necessary=true 必须返回 CFArray");
        assert_eq!(attachments.count(), 1);
        assert_eq!(frame_status(&sb), None);

        // SAFETY: count == 1，索引 0 为 CMSampleBuffer 提供的可变 CFDictionary。
        let raw_dict = unsafe { attachments.value_at_index(0) };
        let dict_ptr = NonNull::new(raw_dict.cast_mut()).expect("附件字典非空");
        // SAFETY: CMSampleBufferGetSampleAttachmentsArray 的元素保证是 CFMutableDictionary，
        // 存活期受 attachments 保护。
        let dict: &CFMutableDictionary = unsafe { dict_ptr.cast::<CFMutableDictionary>().as_ref() };
        // SAFETY: SCStreamFrameInfoStatus 是系统框架导出的 &'static NSString，
        // 与 CFString toll-free bridged。
        let key: *const c_void = unsafe { std::ptr::addr_of!(*SCStreamFrameInfoStatus) }.cast();

        // 3. 写入各类 SCFrameStatus 枚举值，frame_status 必须准确还原。
        for expected in [
            SCFrameStatus::Complete,
            SCFrameStatus::Idle,
            SCFrameStatus::Blank,
            SCFrameStatus::Suspended,
            SCFrameStatus::Started,
            SCFrameStatus::Stopped,
        ] {
            let num = CFNumber::new_isize(expected.0);
            let val_ptr: *const c_void = CFRetained::as_ptr(&num).as_ptr().cast();
            // SAFETY: dict 是可变附件字典，key/val_ptr 均为有效 CFType 指针，
            // CFDictionarySetValue 会 retain 它们。
            unsafe { CFMutableDictionary::set_value(Some(dict), key, val_ptr) };
            assert_eq!(
                frame_status(&sb),
                Some(expected),
                "应解析出 SCFrameStatus({})",
                expected.0
            );
        }

        // 4. 值为非 CFNumber（例如 CFString）时，运行时 CFGetTypeID 校验应返回 None。
        // SAFETY: key 本身是有效的 CFString 指针，写入后验证类型不匹配会安全返回 None。
        unsafe { CFMutableDictionary::set_value(Some(dict), key, key) };
        assert_eq!(frame_status(&sb), None);
    }

    #[test]
    fn dirty_rects_filter_handles_empty_zero_area_positive_and_missing() {
        use objc2_core_foundation::{CGPoint, CGSize};

        let sb = make_empty_sample_buffer();
        // 1. 无样本附件字典 -> 保守放行。
        assert!(dirty_rects_indicate_change(&sb));

        // SAFETY: sb 有效，create_if_necessary = true 创建附件数组与字典。
        let attachments = unsafe { sb.sample_attachments_array(true) }
            .expect("create_if_necessary=true 必须返回 CFArray");
        // SAFETY: count == 1，索引 0 为可变字典。
        let raw_dict = unsafe { attachments.value_at_index(0) };
        let dict_ptr = NonNull::new(raw_dict.cast_mut()).expect("附件字典非空");
        // SAFETY: 附件数组元素为 CFMutableDictionary，存活期受 attachments 保护。
        let dict: &CFMutableDictionary = unsafe { dict_ptr.cast::<CFMutableDictionary>().as_ref() };
        // SAFETY: SCStreamFrameInfoDirtyRects 为系统导出的 &'static NSString。
        let dirty_key: *const c_void =
            unsafe { std::ptr::addr_of!(*SCStreamFrameInfoDirtyRects) }.cast();

        // 2. 字典内无 dirtyRects 键 -> None（保守放行）。
        assert_eq!(dirty_rects_from_dict(dict.as_ref()), None);
        assert!(dirty_rects_indicate_change(&sb));

        // 3. 空 NSArray -> Some(false)（静屏跳过）。
        let empty_arr: Retained<NSArray<NSValue>> = NSArray::new();
        let empty_ptr: *const c_void = Retained::as_ptr(&empty_arr).cast();
        // SAFETY: dict 与 empty_ptr 均为有效 CF/ObjC 对象，set_value 会 retain。
        unsafe { CFMutableDictionary::set_value(Some(dict), dirty_key, empty_ptr) };
        assert_eq!(dirty_rects_from_dict(dict.as_ref()), Some(false));
        assert!(!dirty_rects_indicate_change(&sb));

        // 4. 全部为零面积的 NSValue(CGRect)（0x0 / 宽 0 / 高 0 / 负边）-> Some(false)（静屏跳过）。
        let zero_rects = [
            NSValue::new(CGRect::ZERO),
            NSValue::new(CGRect::new(
                CGPoint::new(10.0, 20.0),
                CGSize::new(0.0, 40.0),
            )),
            NSValue::new(CGRect::new(
                CGPoint::new(10.0, 20.0),
                CGSize::new(40.0, 0.0),
            )),
            NSValue::new(CGRect::new(
                CGPoint::new(10.0, 20.0),
                CGSize::new(-5.0, 10.0),
            )),
        ];
        let zero_arr = NSArray::from_retained_slice(&zero_rects);
        let zero_ptr: *const c_void = Retained::as_ptr(&zero_arr).cast();
        // SAFETY: dict 与 zero_ptr 均有效。
        unsafe { CFMutableDictionary::set_value(Some(dict), dirty_key, zero_ptr) };
        assert_eq!(dirty_rects_from_dict(dict.as_ref()), Some(false));
        assert!(!dirty_rects_indicate_change(&sb));

        // 5. 包含正面积矩形的 NSValue(CGRect)（即便前面有零面积矩形）-> Some(true)（放行）。
        let mixed_rects = [
            NSValue::new(CGRect::ZERO),
            NSValue::new(CGRect::new(CGPoint::new(4.0, 8.0), CGSize::new(12.0, 6.0))),
        ];
        let mixed_arr = NSArray::from_retained_slice(&mixed_rects);
        let mixed_ptr: *const c_void = Retained::as_ptr(&mixed_arr).cast();
        // SAFETY: dict 与 mixed_ptr 均有效。
        unsafe { CFMutableDictionary::set_value(Some(dict), dirty_key, mixed_ptr) };
        assert_eq!(dirty_rects_from_dict(dict.as_ref()), Some(true));
        assert!(dirty_rects_indicate_change(&sb));

        // 6. CFDictionary 表示的 CGRect：零面积跳过、正面积放行。
        // SAFETY: CGRectCreateDictionaryRepresentation 返回 +1 CFDictionary。
        let cf_zero_ptr = unsafe { raw::CGRectCreateDictionaryRepresentation(CGRect::ZERO) };
        // SAFETY: `cf_zero_ptr` 非空且遵循 Create Rule，拥有 +1 引用。
        let cf_zero_dict =
            unsafe { CFRetained::from_raw(NonNull::new(cf_zero_ptr.cast_mut()).unwrap()) };
        let cf_zero_elems = [CFRetained::as_ptr(&cf_zero_dict).as_ptr().cast::<c_void>()];
        // SAFETY: 1 个有效 CFDictionary 指针的数组。
        let cf_zero_arr = unsafe {
            CFArray::new(
                None,
                cf_zero_elems.as_ptr() as *mut *const c_void,
                1,
                &objc2_core_foundation::kCFTypeArrayCallBacks,
            )
        }
        .unwrap();
        let cf_zero_arr_ptr: *const c_void = CFRetained::as_ptr(&cf_zero_arr).as_ptr().cast();
        // SAFETY: dict 与 cf_zero_arr_ptr 均有效。
        unsafe { CFMutableDictionary::set_value(Some(dict), dirty_key, cf_zero_arr_ptr) };
        assert_eq!(dirty_rects_from_dict(dict.as_ref()), Some(false));

        let pos_rect = CGRect::new(CGPoint::new(1.0, 2.0), CGSize::new(64.0, 32.0));
        // SAFETY: CGRectCreateDictionaryRepresentation 返回 +1 CFDictionary。
        let cf_pos_ptr = unsafe { raw::CGRectCreateDictionaryRepresentation(pos_rect) };
        // SAFETY: `cf_pos_ptr` 非空且遵循 Create Rule，拥有 +1 引用。
        let cf_pos_dict =
            unsafe { CFRetained::from_raw(NonNull::new(cf_pos_ptr.cast_mut()).unwrap()) };
        let cf_pos_elems = [CFRetained::as_ptr(&cf_pos_dict).as_ptr().cast::<c_void>()];
        // SAFETY: 1 个有效 CFDictionary 指针的数组。
        let cf_pos_arr = unsafe {
            CFArray::new(
                None,
                cf_pos_elems.as_ptr() as *mut *const c_void,
                1,
                &objc2_core_foundation::kCFTypeArrayCallBacks,
            )
        }
        .unwrap();
        let cf_pos_arr_ptr: *const c_void = CFRetained::as_ptr(&cf_pos_arr).as_ptr().cast();
        // SAFETY: dict 与 cf_pos_arr_ptr 均有效。
        unsafe { CFMutableDictionary::set_value(Some(dict), dirty_key, cf_pos_arr_ptr) };
        assert_eq!(dirty_rects_from_dict(dict.as_ref()), Some(true));

        // 7. 未知数组元素（如 CFNumber）-> 保守放行 Some(true)。
        let num = CFNumber::new_isize(42);
        let unknown_elems = [CFRetained::as_ptr(&num).as_ptr().cast::<c_void>()];
        // SAFETY: 1 个有效 CFNumber 指针的数组。
        let unknown_arr = unsafe {
            CFArray::new(
                None,
                unknown_elems.as_ptr() as *mut *const c_void,
                1,
                &objc2_core_foundation::kCFTypeArrayCallBacks,
            )
        }
        .unwrap();
        let unknown_arr_ptr: *const c_void = CFRetained::as_ptr(&unknown_arr).as_ptr().cast();
        // SAFETY: dict 与 unknown_arr_ptr 均有效。
        unsafe { CFMutableDictionary::set_value(Some(dict), dirty_key, unknown_arr_ptr) };
        assert_eq!(dirty_rects_from_dict(dict.as_ref()), Some(true));

        // 8. 非数组值（如 CFNumber）-> None（保守放行）。
        let num_ptr: *const c_void = CFRetained::as_ptr(&num).as_ptr().cast();
        // SAFETY: dict 与 num_ptr 均有效。
        unsafe { CFMutableDictionary::set_value(Some(dict), dirty_key, num_ptr) };
        assert_eq!(dirty_rects_from_dict(dict.as_ref()), None);
        assert!(dirty_rects_indicate_change(&sb));
    }

    #[test]
    fn sck_callback_queue_uses_user_interactive_qos() {
        let queue = create_sck_callback_queue();
        // SAFETY: `queue` 为有效 DispatchQueue，查询其 QoS class 为只读操作。
        let qos = unsafe { queue.qos_class(std::ptr::null_mut()) };
        assert_eq!(qos, dispatch2::DispatchQoS::UserInteractive);
    }

    #[test]
    fn minimum_frame_interval_leaves_vsync_jitter_margin() {
        // 60fps: 15.0ms 严格大于 120Hz (8.33ms) 与 80Hz (12.5ms)，同时相对 16.67ms 留 1.67ms 容差。
        assert_eq!(minimum_frame_interval_us(60), 15_000);
        // 30fps: 30.0ms 相对 33.33ms 留 3.33ms 容差。
        assert_eq!(minimum_frame_interval_us(30), 30_000);
        // 120fps+: 0 表示 kCMTimeZero（原生高刷不节流，消除 SCK skip_src 饥饿）。
        assert_eq!(minimum_frame_interval_us(120), 0);
        assert_eq!(minimum_frame_interval_us(240), 0);
        // 0fps 防御性钳位到 1fps。
        assert_eq!(minimum_frame_interval_us(0), 900_000);
        // host_time_us 返回单调非零微秒。
        let t1 = host_time_us();
        let t2 = host_time_us();
        assert!(t1 > 0 && t2 >= t1);
    }

    #[test]
    fn main_display_backing_resolution_is_even_and_at_least_logical() {
        if let (Some(logical), Some(backing)) = (
            main_display_logical_resolution(),
            main_display_backing_resolution(),
        ) {
            assert_eq!(backing.width % 2, 0);
            assert_eq!(backing.height % 2, 0);
            assert!(
                backing.width >= logical.width && backing.height >= logical.height,
                "backing {backing:?} 应不小于逻辑分辨率 {logical:?}"
            );
        }
    }

    #[test]
    fn frame_mailbox_overwrites_old_frames_and_times_out_when_empty() {
        struct TrackedBuffer {
            id: u64,
            dropped: Arc<Mutex<Vec<u64>>>,
        }
        impl Drop for TrackedBuffer {
            fn drop(&mut self) {
                self.dropped.lock().unwrap().push(self.id);
            }
        }

        let dropped = Arc::new(Mutex::new(Vec::new()));
        let mailbox = FrameMailbox::<TrackedBuffer>::new();

        // 1. 连续写入 3 帧不消费：旧帧 1、2 必须被立即覆写并 Drop，仅保留最新帧 3。
        for (id, pts_us) in [(1, 100), (2, 200), (3, 300)] {
            mailbox.push_frame(FramePayload {
                buffer: TrackedBuffer {
                    id,
                    dropped: Arc::clone(&dropped),
                },
                pts_us,
                dirty_ratio: Some(0.25),
                dirty_rects: None,
                #[cfg(feature = "bench-metrics")]
                callback_us: pts_us + 50,
            });
        }
        #[cfg(feature = "bench-metrics")]
        assert_eq!(mailbox.overwrites(), 2);
        assert_eq!(
            *dropped.lock().unwrap(),
            vec![1, 2],
            "被新帧覆写的旧帧必须立刻 Drop 以归还缓冲池"
        );

        let received = mailbox
            .recv_timeout(Duration::from_millis(50))
            .expect("应立即取到最新帧");
        match received {
            FrameEvent::Frame(payload) => {
                assert_eq!(payload.buffer.id, 3, "必须丢旧保新，返回最新帧");
                assert_eq!(payload.pts_us, 300);
                // 消费端持有期间帧 3 仍存活，Drop 后归还。
                assert_eq!(*dropped.lock().unwrap(), vec![1, 2]);
            }
            _ => panic!("期望收到 FrameEvent::Frame"),
        }
        assert_eq!(*dropped.lock().unwrap(), vec![1, 2, 3]);

        // 2. 邮箱为空时，零超时与有限超时均返回 None。
        assert!(mailbox.recv_timeout(Duration::ZERO).is_none());
        let start = Instant::now();
        assert!(mailbox.recv_timeout(Duration::from_millis(15)).is_none());
        assert!(
            start.elapsed() >= Duration::from_millis(10),
            "空邮箱应通过 Condvar 等待至超时而非忙等立即返回"
        );

        // 3. 跨线程唤醒：生产者推入新帧后应立刻唤醒阻塞等待中的消费端。
        let producer = mailbox.clone();
        let dropped_clone = Arc::clone(&dropped);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            producer.push_frame(FramePayload {
                buffer: TrackedBuffer {
                    id: 4,
                    dropped: dropped_clone,
                },
                pts_us: 400,
                dirty_ratio: None,
                dirty_rects: None,
                #[cfg(feature = "bench-metrics")]
                callback_us: 450,
            });
        });
        let wake_start = Instant::now();
        let woke = mailbox
            .recv_timeout(Duration::from_secs(1))
            .expect("生产者唤醒后应收到帧 4");
        assert!(
            wake_start.elapsed() < Duration::from_millis(500),
            "应被 notify_one 及时唤醒而非等到超时"
        );
        match woke {
            FrameEvent::Frame(payload) => {
                assert_eq!(payload.buffer.id, 4);
                assert_eq!(payload.pts_us, 400);
            }
            _ => panic!("期望收到 FrameEvent::Frame(4)"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn frame_mailbox_preserves_control_events_over_new_frames() {
        let mailbox = FrameMailbox::<u64>::new();

        // 1. 先来视频帧、再进控制事件、再覆盖多帧视频：控制事件绝不被覆写且优先返回。
        mailbox.push_frame(FramePayload {
            buffer: 1,
            pts_us: 100,
            dirty_ratio: None,
            dirty_rects: None,
            #[cfg(feature = "bench-metrics")]
            callback_us: 110,
        });
        mailbox.push_control(ControlEvent::Suspended);
        mailbox.push_frame(FramePayload {
            buffer: 2,
            pts_us: 200,
            dirty_ratio: None,
            dirty_rects: None,
            #[cfg(feature = "bench-metrics")]
            callback_us: 210,
        });
        mailbox.push_frame(FramePayload {
            buffer: 3,
            pts_us: 300,
            dirty_ratio: Some(0.1),
            dirty_rects: None,
            #[cfg(feature = "bench-metrics")]
            callback_us: 310,
        });

        assert!(
            matches!(
                mailbox.recv_timeout(Duration::from_millis(50)),
                Some(FrameEvent::Suspended)
            ),
            "控制事件 Suspended 必须优先于视频帧且不被后续帧覆写"
        );

        // 取出控制事件后，帧槽中保留的仍是最新帧 3。
        match mailbox.recv_timeout(Duration::from_millis(50)) {
            Some(FrameEvent::Frame(payload)) => {
                assert_eq!(payload.buffer, 3);
                assert_eq!(payload.pts_us, 300);
            }
            _ => panic!("控制事件取出后，帧槽应保留最新视频帧 3"),
        }

        // 2. 致命 Stopped 优先级高于 Suspended，不会被后续 Suspended 降级，也不被新帧覆写。
        mailbox.push_control(ControlEvent::Suspended);
        mailbox.push_control(ControlEvent::Stopped {
            message: "permission revoked".to_string(),
        });
        mailbox.push_control(ControlEvent::Suspended);
        mailbox.push_frame(FramePayload {
            buffer: 4,
            pts_us: 400,
            dirty_ratio: None,
            dirty_rects: None,
            #[cfg(feature = "bench-metrics")]
            callback_us: 410,
        });

        match mailbox.recv_timeout(Duration::from_millis(50)) {
            Some(FrameEvent::Stopped { message }) => assert_eq!(message, "permission revoked"),
            _ => panic!("致命 Stopped 不应被后续 Suspended 或视频帧覆写"),
        }
        match mailbox.recv_timeout(Duration::from_millis(50)) {
            Some(FrameEvent::Frame(payload)) => {
                assert_eq!(payload.buffer, 4);
                assert_eq!(payload.pts_us, 400);
            }
            _ => panic!("应取出剩余最新视频帧 4"),
        }
        assert!(mailbox.recv_timeout(Duration::from_millis(10)).is_none());
    }

    fn make_dirty_stats(
        has_change: bool,
        total_area: Option<f64>,
        max_extent: (f64, f64),
    ) -> DirtyRectStats {
        DirtyRectStats {
            has_change,
            total_area,
            max_extent,
            union_bbox: None,
            raw_rects: [(0.0, 0.0, 0.0, 0.0); MAX_DIRTY_RECTS],
            raw_rect_count: 0,
            overflowed: false,
        }
    }

    #[test]
    fn compute_dirty_ratio_handles_output_and_backing_coordinates() {
        let out = Resolution::new(1512, 982);
        let backing = Resolution::new(3024, 1964);
        let latched = AtomicBool::new(false);
        // 1) 输出像素空间下 151x98 ≈ 1% 脏区（未锁存 backing 前按输出尺寸）
        let small = compute_dirty_ratio(
            Some(make_dirty_stats(true, Some(151.0 * 98.0), (200.0, 200.0))),
            out,
            backing,
            &latched,
        )
        .expect("small ratio");
        assert!((small - 0.01).abs() < 0.002, "expected ~1%, got {small}");
        assert!(!latched.load(Ordering::Relaxed));

        // 2) 首帧或大帧出现 Retina backing 坐标（超出输出宽高）自动锁存 backing 坐标系
        let back_half = compute_dirty_ratio(
            Some(make_dirty_stats(
                true,
                Some(1512.0 * 1964.0),
                (3024.0, 1964.0),
            )),
            out,
            backing,
            &latched,
        )
        .expect("backing half ratio");
        assert!(
            (back_half - 0.5).abs() < 0.01,
            "expected ~50%, got {back_half}"
        );
        assert!(latched.load(Ordering::Relaxed));

        // 3) 锁存后，即便后续局部脏区位于左上象限（max_extent <= out），仍稳定使用 backing 分母（无 4x 误判）
        let top_left_after_latch = compute_dirty_ratio(
            Some(make_dirty_stats(
                true,
                Some(1200.0 * 600.0),
                (1400.0, 800.0),
            )),
            out,
            backing,
            &latched,
        )
        .expect("top-left ratio after latch");
        assert!(
            (top_left_after_latch - 0.1212).abs() < 0.01,
            "expected ~12.1% after latch, got {top_left_after_latch}"
        );

        // 4) 缺失或无法解析元素返回 None（保守按全帧处理）
        assert_eq!(compute_dirty_ratio(None, out, backing, &latched), None);
        assert_eq!(
            compute_dirty_ratio(
                Some(make_dirty_stats(true, None, (0.0, 0.0))),
                out,
                backing,
                &latched,
            ),
            None
        );

        // 5) FrameMailbox 覆写旧帧时合并脏区占比（防止大场景切换帧被后续小闪烁帧覆盖）
        let mb = FrameMailbox::<u64>::new();
        mb.push_frame(FramePayload {
            buffer: 1,
            pts_us: 100,
            dirty_ratio: Some(0.90),
            dirty_rects: None,
            #[cfg(feature = "bench-metrics")]
            callback_us: 100,
        });
        mb.push_frame(FramePayload {
            buffer: 2,
            pts_us: 200,
            dirty_ratio: Some(0.02),
            dirty_rects: None,
            #[cfg(feature = "bench-metrics")]
            callback_us: 200,
        });
        match mb.recv_timeout(Duration::from_millis(20)) {
            Some(FrameEvent::Frame(p)) => {
                assert_eq!(p.buffer, 2);
                assert!(
                    p.dirty_ratio.is_some_and(|r| r >= 0.91),
                    "expected accumulated dirty_ratio >= 0.91, got {:?}",
                    p.dirty_ratio
                );
            }
            _ => panic!("expected frame 2"),
        }
    }

    #[test]
    fn dirty_rect_coordinate_mapping_and_boundary_clipping() {
        let logical = Resolution::new(1512, 982);
        let physical = Resolution::new(3024, 1964);

        // 1) 逻辑坐标 → 物理输出坐标（2× 放大映射）
        assert_eq!(
            map_raw_rect_to_output(120.0, 80.0, 320.0, 180.0, logical, physical),
            Some(Rect::new(240, 160, 400, 200))
        );

        // 2) 物理 backing 坐标 → 逻辑输出坐标（0.5× 下采样，奇数坐标采用保守 floor/ceil 外扩）
        assert_eq!(
            map_raw_rect_to_output(200.0, 400.0, 300.0, 460.0, physical, logical),
            Some(Rect::new(100, 200, 50, 30))
        );
        assert_eq!(
            map_raw_rect_to_output(101.0, 201.0, 111.0, 211.0, physical, logical),
            Some(Rect::new(50, 100, 6, 6))
        );

        // 3) 边界裁剪：负起点、右侧/底部越界截断、完全越界返回 None
        assert_eq!(
            map_raw_rect_to_output(-20.0, -10.0, 80.0, 50.0, logical, logical),
            Some(Rect::new(0, 0, 80, 50))
        );
        assert_eq!(
            map_raw_rect_to_output(1480.0, 960.0, 1600.0, 1050.0, logical, logical),
            Some(Rect::new(1480, 960, 32, 22))
        );
        assert_eq!(
            map_raw_rect_to_output(2000.0, 1200.0, 2100.0, 1300.0, logical, logical),
            None
        );
        assert_eq!(
            map_raw_rect_to_output(-100.0, -100.0, -10.0, -10.0, logical, logical),
            None
        );
        assert_eq!(
            map_raw_rect_to_output(10.0, 10.0, 10.0, 50.0, logical, logical),
            None
        );
        assert_eq!(
            map_raw_rect_to_output(f64::NAN, 0.0, 50.0, 50.0, logical, logical),
            None
        );
    }

    #[test]
    fn dirty_rects_from_dict_maps_backing_coords_and_collapses_over_limit_to_exact_bbox() {
        use objc2_core_foundation::{CGPoint, CGSize};

        let out = Resolution::new(1512, 982);
        let backing = Resolution::new(3024, 1964);
        let latched = AtomicBool::new(false);

        let sb = make_empty_sample_buffer();
        // SAFETY: sb 有效，create_if_necessary = true 创建样本附件数组与字典。
        let attachments = unsafe { sb.sample_attachments_array(true) }
            .expect("create_if_necessary=true 必须返回 CFArray");
        // SAFETY: count == 1，索引 0 为可变字典。
        let raw_dict = unsafe { attachments.value_at_index(0) };
        let dict_ptr = NonNull::new(raw_dict.cast_mut()).expect("附件字典非空");
        // SAFETY: 附件数组元素为 CFMutableDictionary，存活期受 attachments 保护。
        let dict: &CFMutableDictionary = unsafe { dict_ptr.cast::<CFMutableDictionary>().as_ref() };
        // SAFETY: SCStreamFrameInfoDirtyRects 为系统导出的 &'static NSString。
        let dirty_key: *const c_void =
            unsafe { std::ptr::addr_of!(*SCStreamFrameInfoDirtyRects) }.cast();

        // 1) Retina backing 坐标脏区自动锁存并映射至输出像素坐标系（不改变 dirty_ratio）
        let backing_rects = [
            NSValue::new(CGRect::new(
                CGPoint::new(1600.0, 1000.0),
                CGSize::new(400.0, 200.0),
            )),
            NSValue::new(CGRect::new(
                CGPoint::new(200.0, 300.0),
                CGSize::new(100.0, 80.0),
            )),
        ];
        let arr = NSArray::from_retained_slice(&backing_rects);
        let arr_ptr: *const c_void = Retained::as_ptr(&arr).cast();
        // SAFETY: dict 与 arr_ptr 均为有效 CF/ObjC 对象。
        unsafe { CFMutableDictionary::set_value(Some(dict), dirty_key, arr_ptr) };

        let stats = dirty_rect_stats_from_dict(dict.as_ref());
        let (ratio, rects) = compute_dirty_info(stats, out, backing, &latched);
        assert!(latched.load(Ordering::Relaxed));
        assert!(ratio.is_some_and(|r| r > 0.0 && r < 0.05));
        assert_eq!(
            rects,
            Some(vec![
                Rect::new(800, 500, 200, 100),
                Rect::new(100, 150, 50, 40),
            ])
        );

        // 2) 锁存后左上象限局部脏区（max_extent <= out）仍按 backing 映射为输出坐标
        let top_left = [NSValue::new(CGRect::new(
            CGPoint::new(200.0, 300.0),
            CGSize::new(100.0, 80.0),
        ))];
        let arr_tl = NSArray::from_retained_slice(&top_left);
        let arr_tl_ptr: *const c_void = Retained::as_ptr(&arr_tl).cast();
        // SAFETY: dict 与 arr_tl_ptr 均为有效 CF/ObjC 对象。
        unsafe { CFMutableDictionary::set_value(Some(dict), dirty_key, arr_tl_ptr) };
        let (_, rects_tl) = compute_dirty_info(
            dirty_rect_stats_from_dict(dict.as_ref()),
            out,
            backing,
            &latched,
        );
        assert_eq!(rects_tl, Some(vec![Rect::new(100, 150, 50, 40)]));

        // 3) 超过 MAX_DIRTY_RECTS (16) 个不重合脏区且远离 (0,0)：必须收敛为精确包围盒，绝不回落 (0,0) 假矩形
        let many_rects: Vec<Retained<NSValue>> = (0..20)
            .map(|i| {
                let x = 800.0 + f64::from(i) * 20.0;
                let y = 600.0 + f64::from(i) * 10.0;
                NSValue::new(CGRect::new(CGPoint::new(x, y), CGSize::new(20.0, 20.0)))
            })
            .collect();
        let many_arr = NSArray::from_retained_slice(&many_rects);
        let many_ptr: *const c_void = Retained::as_ptr(&many_arr).cast();
        // SAFETY: dict 与 many_ptr 均为有效 CF/ObjC 对象。
        unsafe { CFMutableDictionary::set_value(Some(dict), dirty_key, many_ptr) };
        let many_stats = dirty_rect_stats_from_dict(dict.as_ref()).expect("20 rects stats");
        assert!(many_stats.overflowed);
        let (_, collapsed) = compute_dirty_info(Some(many_stats), out, backing, &latched);
        // backing 包围盒: x in [800, 800+19*20+20 = 1200], y in [600, 600+19*10+20 = 810]
        // 映射到 out (0.5x): x in [400, 600] (w=200), y in [300, 405] (h=105)
        let collapsed = collapsed.expect("over-limit dirty rects must collapse to exact bbox");
        assert_eq!(collapsed, vec![Rect::new(400, 300, 200, 105)]);
        assert!(collapsed[0].x > 0 && collapsed[0].y > 0);
    }

    #[test]
    fn frame_mailbox_merges_real_dirty_rects_and_collapses_over_limit_without_losing_first_frame() {
        let mb = FrameMailbox::<u64>::new();

        // 1) 两帧局部不重合脏区覆写合并：首帧与次帧真实脏区均完整保留
        let r1 = Rect::new(120, 140, 80, 60);
        let r2 = Rect::new(900, 520, 160, 100);
        mb.push_frame(FramePayload {
            buffer: 1,
            pts_us: 100,
            dirty_ratio: Some(0.05),
            dirty_rects: Some(vec![r1]),
            #[cfg(feature = "bench-metrics")]
            callback_us: 100,
        });
        mb.push_frame(FramePayload {
            buffer: 2,
            pts_us: 200,
            dirty_ratio: Some(0.08),
            dirty_rects: Some(vec![r2]),
            #[cfg(feature = "bench-metrics")]
            callback_us: 200,
        });
        match mb.recv_timeout(Duration::from_millis(20)) {
            Some(FrameEvent::Frame(p)) => {
                assert_eq!(p.buffer, 2);
                assert_eq!(p.dirty_rects, Some(vec![r1, r2]));
            }
            _ => panic!("expected merged frame 2"),
        }

        // 2) 两帧合计超过 MAX_DIRTY_RECTS (16) 个远离 (0,0) 的脏区：合并为覆盖两帧全部脏区的精确包围盒
        let frame1_rects: Vec<Rect> = (0..10)
            .map(|i| Rect::new(300 + i * 20, 200 + i * 10, 15, 15))
            .collect();
        let frame2_rects: Vec<Rect> = (0..10)
            .map(|i| Rect::new(600 + i * 20, 400 + i * 10, 25, 20))
            .collect();
        mb.push_frame(FramePayload {
            buffer: 3,
            pts_us: 300,
            dirty_ratio: Some(0.05),
            dirty_rects: Some(frame1_rects),
            #[cfg(feature = "bench-metrics")]
            callback_us: 300,
        });
        mb.push_frame(FramePayload {
            buffer: 4,
            pts_us: 400,
            dirty_ratio: Some(0.05),
            dirty_rects: Some(frame2_rects),
            #[cfg(feature = "bench-metrics")]
            callback_us: 400,
        });
        match mb.recv_timeout(Duration::from_millis(20)) {
            Some(FrameEvent::Frame(p)) => {
                assert_eq!(p.buffer, 4);
                // min=(300,200), max=(600+9*20+25=805, 400+9*10+20=510) => w=505, h=310
                assert_eq!(p.dirty_rects, Some(vec![Rect::new(300, 200, 505, 310)]));
            }
            _ => panic!("expected collapsed frame 4"),
        }

        // 3) 首帧未知/全屏（None）被次帧局部脏区覆写：保守保留 None（全帧失效，绝不丢失首帧变化）
        mb.push_frame(FramePayload {
            buffer: 5,
            pts_us: 500,
            dirty_ratio: None,
            dirty_rects: None,
            #[cfg(feature = "bench-metrics")]
            callback_us: 500,
        });
        mb.push_frame(FramePayload {
            buffer: 6,
            pts_us: 600,
            dirty_ratio: Some(0.02),
            dirty_rects: Some(vec![Rect::new(10, 10, 20, 20)]),
            #[cfg(feature = "bench-metrics")]
            callback_us: 600,
        });
        match mb.recv_timeout(Duration::from_millis(20)) {
            Some(FrameEvent::Frame(p)) => {
                assert_eq!(p.buffer, 6);
                assert_eq!(p.dirty_rects, None);
            }
            _ => panic!("expected conservative full-frame fallback on frame 6"),
        }
    }

    #[test]
    fn display_asleep_evaluation_covers_single_multi_clamshell_and_fallback() {
        // 空列表时回退主屏状态。
        assert!(evaluate_displays_asleep(&[], 1, true));
        assert!(!evaluate_displays_asleep(&[], 1, false));

        // 单屏场景。
        assert!(evaluate_displays_asleep(&[(1, true)], 1, true));
        assert!(!evaluate_displays_asleep(&[(1, false)], 1, false));

        // 多屏全休眠。
        assert!(evaluate_displays_asleep(&[(1, true), (2, true)], 1, true));
        // 多屏全亮屏。
        assert!(!evaluate_displays_asleep(
            &[(1, false), (2, false)],
            1,
            false
        ));
        // 合盖/外接屏场景：主屏（外接屏 id=2）亮屏，内建屏休眠 → 未休眠。
        assert!(!evaluate_displays_asleep(
            &[(1, true), (2, false)],
            2,
            false
        ));
        // 主屏休眠、副屏亮屏 → 判定为休眠以触发唤醒主屏。
        assert!(evaluate_displays_asleep(&[(1, true), (2, false)], 1, true));
        // 主屏 ID 不在在线列表中：全部在线屏休眠 → true；至少一屏亮屏 → false。
        assert!(evaluate_displays_asleep(&[(1, true), (2, true)], 99, false));
        assert!(!evaluate_displays_asleep(
            &[(1, false), (2, true)],
            99,
            true
        ));
    }

    #[test]
    fn dict_bool_flag_handles_cfboolean_cfnumber_missing_and_wrong_type() {
        // SAFETY: 创建空 CFMutableDictionary（使用标准 CFType 回调），返回 +1 所有权。
        let dict = unsafe {
            CFMutableDictionary::new(
                None,
                0,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        }
        .expect("CFMutableDictionary::new 应成功");

        let key_name = "CGSSessionScreenIsLocked";
        // 1. 键缺失 → false。
        assert!(!dict_bool_flag(dict.as_ref(), key_name));

        let key = CFString::from_str(key_name);
        let key_ptr: *const c_void = CFRetained::as_ptr(&key).as_ptr().cast();

        // 2. CFBoolean(true / false)。
        let b_true = CFBoolean::new(true);
        // SAFETY: dict、key_ptr、b_true 均为有效 CF 对象。
        unsafe {
            CFMutableDictionary::set_value(
                Some(&dict),
                key_ptr,
                (b_true as *const CFBoolean).cast::<c_void>(),
            );
        }
        assert!(dict_bool_flag(dict.as_ref(), key_name));

        let b_false = CFBoolean::new(false);
        // SAFETY: dict、key_ptr、b_false 均为有效 CF 对象。
        unsafe {
            CFMutableDictionary::set_value(
                Some(&dict),
                key_ptr,
                (b_false as *const CFBoolean).cast::<c_void>(),
            );
        }
        assert!(!dict_bool_flag(dict.as_ref(), key_name));

        // 3. CFNumber(0 / 1 / 42)。
        for (val, expected) in [(0i32, false), (1i32, true), (42i32, true)] {
            let num = CFNumber::new_i32(val);
            // SAFETY: dict、key_ptr、num 均为有效 CF 对象。
            unsafe {
                CFMutableDictionary::set_value(
                    Some(&dict),
                    key_ptr,
                    CFRetained::as_ptr(&num).as_ptr().cast::<c_void>(),
                );
            }
            assert_eq!(dict_bool_flag(dict.as_ref(), key_name), expected);
        }

        // 4. 非布尔/数值类型（CFString）→ false。
        // SAFETY: key_ptr 本身为有效 CFString，验证类型不匹配安全返回 false。
        unsafe {
            CFMutableDictionary::set_value(Some(&dict), key_ptr, key_ptr);
        }
        assert!(!dict_bool_flag(dict.as_ref(), key_name));
    }

    #[test]
    fn sanitize_wake_reason_truncates_and_defaults_safely() {
        assert_eq!(sanitize_wake_reason(""), DEFAULT_WAKE_REASON);
        assert_eq!(sanitize_wake_reason("   \t\n\0  "), DEFAULT_WAKE_REASON);
        assert_eq!(
            sanitize_wake_reason("  RabbitControl\0Wake\nNow  "),
            "RabbitControl Wake Now"
        );

        let long_ascii = "A".repeat(250);
        let truncated_ascii = sanitize_wake_reason(&long_ascii);
        assert_eq!(truncated_ascii.len(), MAX_WAKE_REASON_UTF16_LEN);
        assert!(truncated_ascii.encode_utf16().count() <= MAX_WAKE_REASON_UTF16_LEN);

        // 多字节中文 + 4 字节 emoji 混合，截断必须在合法 UTF-8 字符边界且 UTF-16 长度 ≤ 128。
        let mixed = "唤醒屏幕😀".repeat(40);
        let truncated_mixed = sanitize_wake_reason(&mixed);
        assert!(!truncated_mixed.is_empty());
        assert!(truncated_mixed.encode_utf16().count() <= MAX_WAKE_REASON_UTF16_LEN);
    }

    #[test]
    fn display_wake_assertion_zero_id_drop_is_noop_and_send() {
        fn assert_send<T: Send>() {}
        assert_send::<DisplayWakeAssertion>();

        let mut guard = DisplayWakeAssertion::new(0);
        assert_eq!(guard.assertion_id, 0);
        // 显式置零与 Drop 均应为空操作，不调用 IOPMAssertionRelease。
        guard.assertion_id = 0;
        drop(guard);
    }

    #[test]
    fn repeated_display_sleep_and_screen_lock_queries_do_not_crash_or_leak() {
        for _ in 0..100 {
            let _ = is_display_asleep(None);
            let _ = is_display_asleep(Some(main_display_id()));
            let _ = is_screen_locked();
        }
    }

    // PTS 节拍过滤（should_emit_pts）已被整段删除：真机实测 SCK 60fps 内容的
    // 到达节拍是 8.33/16.67ms 交替，按「距上一放行帧」累计间隔判定会把节拍
    // 拉长到 25ms 周期（采集 48fps）。多余供给交给调度器预算门处理，见
    // 上方 `HandlerIvars` 的文档注释。
}

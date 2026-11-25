//! macOS 物理防窥隐私幕布（AppKit `NSWindow` + `NSWindowSharingNone` + SCK 窗口排除）
//! 与虚拟显示器管理（CoreGraphics `CGVirtualDisplay` + 多屏枚举）。
//!
//! 所有 `unsafe` FFI 均附 `// SAFETY:` 说明。

use std::ffi::{c_char, c_void, CStr};
use std::sync::{Mutex, OnceLock};

use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::NSString;

/// 在线显示器信息（物理屏或虚拟屏）。
#[derive(Debug, Clone, PartialEq)]
pub struct OnlineDisplayInfo {
    /// macOS `CGDirectDisplayID`。
    pub id: u32,
    /// 可读显示器名称。
    pub name: String,
    /// 逻辑宽度（点）。
    pub width: u32,
    /// 逻辑高度（点）。
    pub height: u32,
    /// 全局坐标原点 X。
    pub origin_x: f64,
    /// 全局坐标原点 Y。
    pub origin_y: f64,
    /// 是否为主显示器。
    pub is_main: bool,
    /// 是否为本进程创建的虚拟显示器。
    pub is_virtual: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// SCK 隐私幕布排除窗口 ID 注册表
// ─────────────────────────────────────────────────────────────────────────────

static PRIVACY_EXCLUDED_WIDS: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();

fn excluded_wids_lock() -> &'static Mutex<Vec<u32>> {
    PRIVACY_EXCLUDED_WIDS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 返回当前需要从 ScreenCaptureKit 采集中排除的隐私幕布 `CGWindowID` 列表。
pub(crate) fn privacy_excluded_window_ids() -> Vec<u32> {
    excluded_wids_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn set_privacy_excluded_window_ids(ids: Vec<u32>) {
    *excluded_wids_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = ids;
}

// ─────────────────────────────────────────────────────────────────────────────
// 主线程安全派发辅助（防同线程死锁）
// ─────────────────────────────────────────────────────────────────────────────

extern "C" {
    fn pthread_main_np() -> i32;
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(
        queue: *const c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

/// 在主线程安全执行 AppKit 闭包：
/// - 已在主线程时直接同步执行；
/// - 非主线程时通过堆分配 `Arc<(Mutex<Option<R>>, Condvar)>` + `dispatch_async_f` 派发并带 2s 有界等待，
///   避免在 `LoginWindow` / `rc-cli` / 停机退出（主队列已停止泵送）时发生永久 ABBA 死锁或栈指针悬空。
fn run_on_main_thread_bounded<R: Send + 'static, F: FnOnce() -> R + Send + 'static>(
    f: F,
) -> Result<R, String> {
    use std::sync::{Arc, Condvar};
    use std::time::Duration;

    // SAFETY: `pthread_main_np` 是 macOS 线程安全的无参查询函数。
    let is_main = unsafe { pthread_main_np() } != 0;
    if is_main {
        return Ok(objc2::rc::autoreleasepool(|_| f()));
    }

    struct AsyncWork<R, F> {
        func: Option<F>,
        done: Arc<(Mutex<Option<R>>, Condvar)>,
    }
    extern "C" fn trampoline<R: Send + 'static, F: FnOnce() -> R + Send + 'static>(
        ctx: *mut c_void,
    ) {
        // SAFETY: `ctx` 是下方 `Box::into_raw` 移交所有权的堆对象，由本回调唯一接管并在结束后安全释放。
        let mut work = unsafe { Box::from_raw(ctx.cast::<AsyncWork<R, F>>()) };
        let res = objc2::rc::autoreleasepool(|_| work.func.take().map(|f| f()));
        let (lock, cvar) = &*work.done;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        *guard = res;
        cvar.notify_all();
    }

    let done: Arc<(Mutex<Option<R>>, Condvar)> = Arc::new((Mutex::new(None), Condvar::new()));
    let boxed = Box::new(AsyncWork {
        func: Some(f),
        done: Arc::clone(&done),
    });
    let ctx = Box::into_raw(boxed).cast::<c_void>();
    // SAFETY: `_dispatch_main_q` 是 libdispatch 全局主队列符号；`ctx` 指向堆分配对象，所有权转移给 `trampoline`。
    unsafe {
        dispatch_async_f(
            std::ptr::addr_of!(_dispatch_main_q),
            ctx,
            trampoline::<R, F>,
        );
    }

    let (lock, cvar) = &*done;
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while guard.is_none() {
        let Some(rem) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return Err("主线程 AppKit 事件循环未响应（可能处于无桌面/停机阶段）".to_string());
        };
        let (g, wait_res) = cvar
            .wait_timeout(guard, rem)
            .unwrap_or_else(|e| e.into_inner());
        guard = g;
        if wait_res.timed_out() && guard.is_none() {
            return Err("主线程 AppKit 事件循环未响应（可能处于无桌面/停机阶段）".to_string());
        }
    }
    Ok(guard.take().expect("guarded result must be Some"))
}

// ─────────────────────────────────────────────────────────────────────────────
// AppKit 物理防窥隐私幕布（NSWindow + NSWindowSharingNone）
// ─────────────────────────────────────────────────────────────────────────────

struct RawObjcPtr(*mut AnyObject);
// SAFETY: 内部指针仅在主线程解引用与释放（通过 `run_on_main_thread_sync` 串行保护）。
unsafe impl Send for RawObjcPtr {}

static CURTAIN_WINDOWS: OnceLock<Mutex<Vec<RawObjcPtr>>> = OnceLock::new();

fn curtain_windows_lock() -> &'static Mutex<Vec<RawObjcPtr>> {
    CURTAIN_WINDOWS.get_or_init(|| Mutex::new(Vec::new()))
}

fn preset_colors(preset: &str) -> (f64, f64, f64, &'static str) {
    match preset {
        "preset-2" => (0.09, 0.11, 0.16, "深邃星空 · 远程隐私保护中"),
        "preset-3" => (0.06, 0.15, 0.13, "静谧森绿 · 远程隐私保护中"),
        "preset-4" => (0.14, 0.10, 0.22, "极光紫夜 · 远程隐私保护中"),
        "preset-5" => (0.13, 0.13, 0.14, "极简石墨 · 远程隐私保护中"),
        "preset-6" => (0.02, 0.02, 0.03, "纯黑防窥 · 远程隐私保护中"),
        _ => (0.07, 0.14, 0.28, "守护深蓝 · 远程隐私保护中"),
    }
}

/// 在主线程清理所有现存隐私幕布窗口。
fn clear_curtain_windows_on_main() {
    let old = {
        let mut guard = curtain_windows_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    };
    set_privacy_excluded_window_ids(Vec::new());
    for RawObjcPtr(ptr) in old {
        if !ptr.is_null() {
            // SAFETY: `ptr` 是之前 retain 持有的有效 `NSWindow*`，当前位于主线程。
            unsafe {
                let _: () = msg_send![ptr, orderOut: std::ptr::null_mut::<AnyObject>()];
                let _: () = msg_send![ptr, close];
                let _: () = msg_send![ptr, release];
            }
        }
    }
}

/// 开启或更新被控端物理防窥隐私幕布（遮挡全部显示器但对 SCK 采集不可见）。
pub(crate) fn set_privacy_curtain(
    mode: &str,
    preset: &str,
    custom_image_bytes: Option<&[u8]>,
) -> Result<(), String> {
    let mode_owned = mode.to_string();
    let preset_owned = preset.to_string();
    let img_owned = custom_image_bytes.map(|b| b.to_vec());

    run_on_main_thread_bounded(move || {
        clear_curtain_windows_on_main();
        if mode_owned == "off" {
            return Ok(());
        }

        let Some(ns_screen_cls) = AnyClass::get(c"NSScreen") else {
            return Err("AppKit NSScreen 不可用".to_string());
        };
        let Some(ns_window_cls) = AnyClass::get(c"NSWindow") else {
            return Err("AppKit NSWindow 不可用".to_string());
        };
        let Some(ns_color_cls) = AnyClass::get(c"NSColor") else {
            return Err("AppKit NSColor 不可用".to_string());
        };

        // SAFETY: `[NSScreen screens]` 返回有效 `NSArray<NSScreen*>`（主线程调用）。
        let screens: *mut AnyObject = unsafe { msg_send![ns_screen_cls, screens] };
        if screens.is_null() {
            return Err("未检测到可用屏幕".to_string());
        }
        // SAFETY: `screens` 非空且为 NSArray。
        let count: usize = unsafe { msg_send![screens, count] };
        if count == 0 {
            return Err("未检测到可用屏幕".to_string());
        }

        let (r, g, b, caption) = if mode_owned == "black" {
            (0.0, 0.0, 0.0, "")
        } else {
            preset_colors(&preset_owned)
        };

        // SAFETY: `colorWithSRGBRed:green:blue:alpha:` 返回有效 autoreleased `NSColor*`。
        let bg_color: *mut AnyObject = unsafe {
            msg_send![
                ns_color_cls,
                colorWithSRGBRed: r,
                green: g,
                blue: b,
                alpha: 1.0f64
            ]
        };

        // 可选加载自定义图片 NSImage（一次构造复用给全部显示器）。
        let custom_ns_image: *mut AnyObject = if mode_owned == "privacy" {
            if let Some(ref bytes) = img_owned {
                if !bytes.is_empty() {
                    if let (Some(ns_data_cls), Some(ns_image_cls)) =
                        (AnyClass::get(c"NSData"), AnyClass::get(c"NSImage"))
                    {
                        // SAFETY: `bytes.as_ptr()` 在 `dataWithBytes:length:` 拷贝期间有效。
                        let ns_data: *mut AnyObject = unsafe {
                            msg_send![
                                ns_data_cls,
                                dataWithBytes: bytes.as_ptr().cast::<c_void>(),
                                length: bytes.len()
                            ]
                        };
                        if !ns_data.is_null() {
                            // SAFETY: `alloc` + `initWithData:` 标准 NSImage 构造；后续在函数末尾 release。
                            unsafe {
                                let alloc_img: *mut AnyObject = msg_send![ns_image_cls, alloc];
                                msg_send![alloc_img, initWithData: ns_data]
                            }
                        } else {
                            std::ptr::null_mut()
                        }
                    } else {
                        std::ptr::null_mut()
                    }
                } else {
                    std::ptr::null_mut()
                }
            } else {
                std::ptr::null_mut()
            }
        } else {
            std::ptr::null_mut()
        };

        let mut created_windows: Vec<RawObjcPtr> = Vec::with_capacity(count);
        let mut excluded_wids: Vec<u32> = Vec::with_capacity(count);

        for i in 0..count {
            // SAFETY: `i < count`，`objectAtIndex:` 返回有效 `NSScreen*`。
            let screen: *mut AnyObject = unsafe { msg_send![screens, objectAtIndex: i] };
            if screen.is_null() {
                continue;
            }
            // SAFETY: `frame` 返回屏幕 CGRect。
            let frame: CGRect = unsafe { msg_send![screen, frame] };

            // NSWindowStyleMaskBorderless = 0, NSBackingStoreBuffered = 2
            // SAFETY: 主线程构造无边框缓冲 NSWindow；返回 +1 强引用归 `created_windows` 所有。
            let win: *mut AnyObject = unsafe {
                let alloc_win: *mut AnyObject = msg_send![ns_window_cls, alloc];
                msg_send![
                    alloc_win,
                    initWithContentRect: frame,
                    styleMask: 0usize,
                    backing: 2usize,
                    defer: Bool::NO
                ]
            };
            if win.is_null() {
                continue;
            }

            // SAFETY: 对有效 `NSWindow*` 设置窗口属性、防截屏标志与全空间置顶行为。
            unsafe {
                let _: () = msg_send![win, setReleasedWhenClosed: Bool::NO];
                let _: () = msg_send![win, setOpaque: Bool::YES];
                let _: () = msg_send![win, setHasShadow: Bool::NO];
                let _: () = msg_send![win, setIgnoresMouseEvents: Bool::YES];
                // NSWindowSharingNone = 0：阻止常规系统截屏捕获幕布。
                let _: () = msg_send![win, setSharingType: 0usize];
                // NSScreenSaverWindowLevel = 1000：覆盖菜单栏、Dock 与所有常规窗口。
                let _: () = msg_send![win, setLevel: 1000isize];
                // CanJoinAllSpaces (1<<0) | Stationary (1<<4) | FullScreenAuxiliary (1<<8)
                let _: () = msg_send![win, setCollectionBehavior: (1usize << 0) | (1usize << 4) | (1usize << 8)];
                if !bg_color.is_null() {
                    let _: () = msg_send![win, setBackgroundColor: bg_color];
                }
                let _: () = msg_send![win, setFrame: frame, display: Bool::YES];

                let content_view: *mut AnyObject = msg_send![win, contentView];
                let bounds = CGRect::new(CGPoint::new(0.0, 0.0), frame.size);
                if !content_view.is_null() {
                    if !custom_ns_image.is_null() {
                        if let Some(iv_cls) = AnyClass::get(c"NSImageView") {
                            let iv_alloc: *mut AnyObject = msg_send![iv_cls, alloc];
                            let iv: *mut AnyObject = msg_send![iv_alloc, initWithFrame: bounds];
                            if !iv.is_null() {
                                let _: () = msg_send![iv, setImage: custom_ns_image];
                                // NSImageScaleProportionallyUpOrDown = 3
                                let _: () = msg_send![iv, setImageScaling: 3usize];
                                let _: () = msg_send![content_view, addSubview: iv];
                                let _: () = msg_send![iv, release];
                            }
                        }
                    } else if !caption.is_empty() {
                        if let (Some(tf_cls), Some(font_cls)) =
                            (AnyClass::get(c"NSTextField"), AnyClass::get(c"NSFont"))
                        {
                            let label_w = (frame.size.width * 0.8).clamp(320.0, 720.0);
                            let label_h = 84.0;
                            let label_rect = CGRect::new(
                                CGPoint::new(
                                    (frame.size.width - label_w) * 0.5,
                                    (frame.size.height - label_h) * 0.5,
                                ),
                                CGSize::new(label_w, label_h),
                            );
                            let tf_alloc: *mut AnyObject = msg_send![tf_cls, alloc];
                            let tf: *mut AnyObject = msg_send![tf_alloc, initWithFrame: label_rect];
                            if !tf.is_null() {
                                let ns_str = NSString::from_str(&format!(
                                    "{caption}\nRabbitControl 正在进行远程控制，屏幕内容已受保护"
                                ));
                                let _: () = msg_send![tf, setStringValue: &*ns_str];
                                let _: () = msg_send![tf, setBezeled: Bool::NO];
                                let _: () = msg_send![tf, setDrawsBackground: Bool::NO];
                                let _: () = msg_send![tf, setEditable: Bool::NO];
                                let _: () = msg_send![tf, setSelectable: Bool::NO];
                                // NSTextAlignmentCenter = 1
                                let _: () = msg_send![tf, setAlignment: 1isize];
                                let white: *mut AnyObject = msg_send![ns_color_cls, whiteColor];
                                if !white.is_null() {
                                    let _: () = msg_send![tf, setTextColor: white];
                                }
                                let font: *mut AnyObject =
                                    msg_send![font_cls, systemFontOfSize: 22.0f64];
                                if !font.is_null() {
                                    let _: () = msg_send![tf, setFont: font];
                                }
                                let _: () = msg_send![content_view, addSubview: tf];
                                let _: () = msg_send![tf, release];
                            }
                        }
                    }
                }

                let _: () = msg_send![win, orderFrontRegardless];
                let wid: isize = msg_send![win, windowNumber];
                if wid > 0 {
                    excluded_wids.push(wid as u32);
                }
            }

            created_windows.push(RawObjcPtr(win));
        }

        if !custom_ns_image.is_null() {
            // SAFETY: 配对释放上面 `alloc`/`initWithData:` 持有的 +1 `NSImage*`（`NSImageView` 已自行 retain）。
            unsafe {
                let _: () = msg_send![custom_ns_image, release];
            }
        }

        set_privacy_excluded_window_ids(excluded_wids);
        *curtain_windows_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = created_windows;
        Ok(())
    })?
}

/// 关闭并销毁全部物理防窥隐私幕布窗口（幂等）。
pub(crate) fn clear_privacy_curtain() {
    let has_any = !curtain_windows_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty()
        || !privacy_excluded_window_ids().is_empty();
    if !has_any {
        return;
    }
    if let Err(err) = run_on_main_thread_bounded(clear_curtain_windows_on_main) {
        eprintln!("[rc-capture-macos] 清理隐私幕布超时/跳过: {err}");
        set_privacy_excluded_window_ids(Vec::new());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CoreGraphics 多屏枚举 + 虚拟显示器（CGVirtualDisplay）
// ─────────────────────────────────────────────────────────────────────────────

struct HeldVirtualDisplay {
    display_id: u32,
    name: String,
    width: u32,
    height: u32,
    obj: *mut AnyObject,
    _queue: dispatch2::DispatchRetained<dispatch2::DispatchQueue>,
}

// SAFETY: `CGVirtualDisplay` ObjC 对象受 `VIRTUAL_DISPLAYS` 互斥锁保护，仅在创建与销毁（`release`）时访问。
unsafe impl Send for HeldVirtualDisplay {}

impl Drop for HeldVirtualDisplay {
    fn drop(&mut self) {
        if !self.obj.is_null() {
            // SAFETY: `self.obj` 为创建时持有的 +1 `CGVirtualDisplay*` 强引用，Drop 时精确配对 release。
            unsafe {
                let _: () = msg_send![self.obj, release];
            }
            self.obj = std::ptr::null_mut();
        }
    }
}

static VIRTUAL_DISPLAYS: OnceLock<Mutex<Vec<HeldVirtualDisplay>>> = OnceLock::new();

fn virtual_displays_lock() -> &'static Mutex<Vec<HeldVirtualDisplay>> {
    VIRTUAL_DISPLAYS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 枚举当前全部在线显示器（含物理屏与本进程创建的虚拟屏）。
pub(crate) fn list_online_displays() -> Vec<OnlineDisplayInfo> {
    let active_ids = match core_graphics::display::CGDisplay::active_displays() {
        Ok(ids) => ids,
        Err(_) => vec![core_graphics::display::CGDisplay::main().id],
    };
    let main_id = core_graphics::display::CGDisplay::main().id;
    let virt_names: std::collections::HashMap<u32, String> = virtual_displays_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|v| (v.display_id, v.name.clone()))
        .collect();

    let mut out = Vec::with_capacity(active_ids.len());
    for (idx, &id) in active_ids.iter().enumerate() {
        let disp = core_graphics::display::CGDisplay::new(id);
        let bounds = disp.bounds();
        let w = (bounds.size.width.round().max(2.0) as u32) & !1;
        let h = (bounds.size.height.round().max(2.0) as u32) & !1;
        let is_main = id == main_id;
        let (is_virtual, name) = if let Some(vname) = virt_names.get(&id) {
            (true, vname.clone())
        } else if is_main {
            (false, format!("显示屏{} (主屏)", idx + 1))
        } else {
            (false, format!("显示屏{}", idx + 1))
        };
        out.push(OnlineDisplayInfo {
            id,
            name,
            width: w,
            height: h,
            origin_x: bounds.origin.x,
            origin_y: bounds.origin.y,
            is_main,
            is_virtual,
        });
    }
    // 主屏置顶，其后保持虚拟屏/副屏顺序。
    out.sort_by_key(|d| (!d.is_main, d.is_virtual, d.id));
    out
}

/// 创建一个新的 macOS 虚拟显示器（借助 CoreGraphics `CGVirtualDisplay`）。
pub(crate) fn create_virtual_display(
    width: u32,
    height: u32,
    hidpi: bool,
) -> Result<OnlineDisplayInfo, String> {
    let w = width.clamp(640, 3840) & !1;
    let h = height.clamp(480, 2160) & !1;

    let Some(desc_cls) = AnyClass::get(c"CGVirtualDisplayDescriptor") else {
        return Err("当前 macOS 系统未导出 CGVirtualDisplayDescriptor".to_string());
    };
    let Some(settings_cls) = AnyClass::get(c"CGVirtualDisplaySettings") else {
        return Err("当前 macOS 系统未导出 CGVirtualDisplaySettings".to_string());
    };
    let Some(mode_cls) = AnyClass::get(c"CGVirtualDisplayMode") else {
        return Err("当前 macOS 系统未导出 CGVirtualDisplayMode".to_string());
    };
    let Some(vdisp_cls) = AnyClass::get(c"CGVirtualDisplay") else {
        return Err("当前 macOS 系统未导出 CGVirtualDisplay".to_string());
    };
    let Some(ns_array_cls) = AnyClass::get(c"NSArray") else {
        return Err("Foundation NSArray 不可用".to_string());
    };

    static NEXT_VDISP_SERIAL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

    let serial_num = {
        let guard = virtual_displays_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if guard.len() >= 4 {
            return Err("最多同时创建 4 个虚拟显示器".to_string());
        }
        NEXT_VDISP_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    };
    let vname = format!("虚拟显示屏 {serial_num}");

    let queue =
        dispatch2::DispatchQueue::new("rc.virtual.display", dispatch2::DispatchQueueAttr::SERIAL);
    let queue_ptr = (&*queue as *const dispatch2::DispatchQueue).cast::<AnyObject>();

    let (max_w, max_h) = if hidpi {
        (w.saturating_mul(2).min(7680), h.saturating_mul(2).min(4320))
    } else {
        (w, h)
    };

    // SAFETY: 以下 ObjC 消息调用均针对 Apple CoreGraphics `CGVirtualDisplay*` 运行时类；
    // 整体包裹在 `autoreleasepool` 中，且所有 alloc/init 返回的 +1 对象均配对 release，绝不泄漏。
    let vdisp_obj: *mut AnyObject = objc2::rc::autoreleasepool(|_| unsafe {
        let ns_name = NSString::from_str(&vname);
        let desc_alloc: *mut AnyObject = msg_send![desc_cls, alloc];
        let desc: *mut AnyObject = msg_send![desc_alloc, init];
        if desc.is_null() {
            return Err("构造 CGVirtualDisplayDescriptor 失败".to_string());
        }
        let _: () = msg_send![desc, setName: &*ns_name];
        let _: () = msg_send![desc, setMaxPixelsWide: max_w];
        let _: () = msg_send![desc, setMaxPixelsHigh: max_h];
        let _: () = msg_send![desc, setSizeInMillimeters: CGSize::new(597.0, 336.0)];
        let _: () = msg_send![desc, setProductID: 0x0100u32.wrapping_add(serial_num)];
        let _: () = msg_send![desc, setVendorID: 0x1E6Du32];
        let _: () = msg_send![desc, setSerialNum: serial_num];
        let _: () = msg_send![desc, setQueue: queue_ptr];

        let vd_alloc: *mut AnyObject = msg_send![vdisp_cls, alloc];
        let vd: *mut AnyObject = msg_send![vd_alloc, initWithDescriptor: desc];
        let _: () = msg_send![desc, release];
        if vd.is_null() {
            return Err("系统拒绝创建 CGVirtualDisplay（可能受当前会话/权限限制）".to_string());
        }

        let mode_alloc: *mut AnyObject = msg_send![mode_cls, alloc];
        let mode: *mut AnyObject = msg_send![
            mode_alloc,
            initWithWidth: w,
            height: h,
            refreshRate: 60.0f64
        ];
        if mode.is_null() {
            let _: () = msg_send![vd, release];
            return Err("构造 CGVirtualDisplayMode 失败".to_string());
        }

        let modes_arr: *mut AnyObject = msg_send![ns_array_cls, arrayWithObject: mode];
        let settings_alloc: *mut AnyObject = msg_send![settings_cls, alloc];
        let settings: *mut AnyObject = msg_send![settings_alloc, init];
        if settings.is_null() {
            let _: () = msg_send![mode, release];
            let _: () = msg_send![vd, release];
            return Err("构造 CGVirtualDisplaySettings 失败".to_string());
        }
        let _: () = msg_send![settings, setHiDPI: if hidpi { 1u32 } else { 0u32 }];
        let _: () = msg_send![settings, setModes: modes_arr];
        let applied: Bool = msg_send![vd, applySettings: settings];
        let _: () = msg_send![settings, release];
        let _: () = msg_send![mode, release];
        if !applied.as_bool() {
            let _: () = msg_send![vd, release];
            return Err("应用虚拟显示器分辨率配置失败".to_string());
        }
        Ok(vd)
    })?;

    // SAFETY: `vdisp_obj` 非空且为有效 `CGVirtualDisplay*`。
    let display_id: u32 = unsafe { msg_send![vdisp_obj, displayID] };
    if display_id == 0 {
        // SAFETY: 释放无效实例。
        unsafe {
            let _: () = msg_send![vdisp_obj, release];
        }
        return Err("虚拟显示器未分配有效 CGDirectDisplayID".to_string());
    }

    let bounds = core_graphics::display::CGDisplay::new(display_id).bounds();
    let info = OnlineDisplayInfo {
        id: display_id,
        name: vname.clone(),
        width: w,
        height: h,
        origin_x: bounds.origin.x,
        origin_y: bounds.origin.y,
        is_main: false,
        is_virtual: true,
    };

    virtual_displays_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(HeldVirtualDisplay {
            display_id,
            name: vname,
            width: w,
            height: h,
            obj: vdisp_obj,
            _queue: queue,
        });

    let _ = (
        CStr::from_bytes_with_nul(b"\0"),
        std::mem::size_of::<c_char>(),
    );
    Ok(info)
}

/// 销毁指定的虚拟显示器；存在并销毁返回 `true`。
pub(crate) fn destroy_virtual_display(display_id: u32) -> bool {
    let mut guard = virtual_displays_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(pos) = guard.iter().position(|v| v.display_id == display_id) {
        let removed = guard.remove(pos);
        drop(guard);
        let _ = (removed.width, removed.height);
        drop(removed);
        true
    } else {
        false
    }
}

/// 销毁本进程创建的全部虚拟显示器（幂等）。
pub(crate) fn destroy_all_virtual_displays() {
    let items = {
        let mut guard = virtual_displays_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    };
    drop(items);
}

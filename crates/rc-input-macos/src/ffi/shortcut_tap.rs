//! macOS 系统快捷键主动拦截器（Active `CGEventTap`）。
//!
//! 当控制端 S1 远控窗口处于前台焦点且「发送系统快捷键到被控端」开关打开时，
//! 拦截 macOS 系统级快捷键（如 `Cmd+Tab`、`Cmd+Space`、`Cmd+H`、`Cmd+M`、`Cmd+Q`、
//! `Cmd+Shift+3/4/5`、`Ctrl+Arrow` 等），避开用户配置的例外列表，
//! 在本地吞掉事件并立即通过回调转发给远端被控端。
//!
//! # 安全与稳定性约束
//! - 仅在 `enabled && window_focused` 同时为 `true` 时拦截；任何时候窗口失焦或开关关闭
//!   均原样放行（`return event`），绝不卡死本机键盘；
//! - 遇到 `kCGEventTapDisabledByTimeout` / `kCGEventTapDisabledByUserInput` 立即恢复启用；
//! - 未授辅助功能/输入监控权限时优雅返回 `Err`，不影响常规 WebView 键盘输入。

#![allow(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use core_foundation::base::TCFType;
use core_foundation::mach_port::{CFMachPort, CFMachPortRef};
use core_foundation::runloop::CFRunLoop;
use core_foundation_sys::mach_port::CFMachPortInvalidate;
use core_foundation_sys::runloop::{
    kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoopSourceInvalidate,
};
use core_graphics::event::{CGEventFlags, CGEventType, EventField};

use crate::keymap::Key;
use crate::scancode::set1_scancode;

type CGEventRef = *mut c_void;
type CGEventTapProxy = *mut c_void;
type CGEventMask = u64;

const K_CG_SESSION_EVENT_TAP: u32 = 1;
const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
const K_CG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
const K_CG_EVENT_KEY_DOWN: u32 = 10;
const K_CG_EVENT_KEY_UP: u32 = 11;
const K_CG_EVENT_FLAGS_CHANGED: u32 = 12;
const K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

const MOD_BIT_SHIFT: u8 = 1 << 0;
const MOD_BIT_CTRL: u8 = 1 << 1;
const MOD_BIT_ALT: u8 = 1 << 2;
const MOD_BIT_META: u8 = 1 << 3;

const MOD_SCANCODE_TABLE: [(u8, u16); 4] = [
    (MOD_BIT_CTRL, 0x001D),
    (MOD_BIT_ALT, 0x0038),
    (MOD_BIT_SHIFT, 0x002A),
    (MOD_BIT_META, 0xE05B),
];

type RawTapCallback = unsafe extern "C" fn(
    proxy: CGEventTapProxy,
    ty: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: CGEventMask,
        callback: RawTapCallback,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: u8);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventGetFlags(event: CGEventRef) -> u64;
}

/// 被拦截的系统快捷键转发载荷（纯值，供上层直发 `rc_input::InputEvent::Key`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterceptedShortcutEvent {
    /// 目标主键的 Set-1 扫描码。
    pub scancode: u16,
    /// `true` = 按下，`false` = 抬起。
    pub pressed: bool,
    /// 修饰键快照位（对齐 `rc_input::batch::modifier_bits`）。
    pub modifiers: u8,
    /// 是否按下 Cmd（左 Meta）。
    pub meta: bool,
    /// 是否按下 Ctrl。
    pub ctrl: bool,
    /// 是否按下 Alt/Option。
    pub alt: bool,
    /// 是否按下 Shift。
    pub shift: bool,
}

/// 将 macOS 虚拟键码映射为逻辑 [`Key`] 与规范短名称（用于匹配例外快捷键列表）。
pub(crate) fn mac_vk_to_key_and_name(vk: u16) -> Option<(Key, &'static str)> {
    match vk {
        0x30 => Some((Key::Tab, "tab")),
        0x31 => Some((Key::Space, "space")),
        0x35 => Some((Key::Escape, "esc")),
        0x32 => Some((Key::Grave, "`")),
        0x0C => Some((Key::Q, "q")),
        0x0D => Some((Key::W, "w")),
        0x04 => Some((Key::H, "h")),
        0x2E => Some((Key::M, "m")),
        0x14 => Some((Key::Digit3, "3")),
        0x15 => Some((Key::Digit4, "4")),
        0x17 => Some((Key::Digit5, "5")),
        0x7B => Some((Key::ArrowLeft, "left")),
        0x7C => Some((Key::ArrowRight, "right")),
        0x7D => Some((Key::ArrowDown, "down")),
        0x7E => Some((Key::ArrowUp, "up")),
        _ => None,
    }
}

/// 判断该按键 + 修饰键组合是否属于需要主动拦截的 macOS 系统快捷键，返回规范化组合键标识（如 `"cmd+tab"`）。
pub(crate) fn classify_system_shortcut(vk: u16, flags: u64) -> Option<(Key, String)> {
    let f = CGEventFlags::from_bits_truncate(flags);
    let cmd = f.contains(CGEventFlags::CGEventFlagCommand);
    let ctrl = f.contains(CGEventFlags::CGEventFlagControl);
    let alt = f.contains(CGEventFlags::CGEventFlagAlternate);
    let shift = f.contains(CGEventFlags::CGEventFlagShift);

    let (key, name) = mac_vk_to_key_and_name(vk)?;
    let is_sys = match (cmd, ctrl, alt, shift, name) {
        // Cmd+Tab / Cmd+Shift+Tab / Cmd+Space / Cmd+` / Cmd+H / Cmd+Opt+H / Cmd+M / Cmd+Q / Cmd+Opt+Esc
        (true, false, false, _, "tab") => true,
        (true, false, false, false, "space") => true,
        (true, false, false, _, "`") => true,
        (true, false, _, false, "h") => true,
        (true, false, false, false, "m") => true,
        (true, false, false, false, "q") => true,
        (true, false, true, false, "esc") => true,
        (true, false, false, true, "3" | "4" | "5") => true,
        // Mission Control / Spaces：Ctrl+Up / Down / Left / Right
        (false, true, false, false, "up" | "down" | "left" | "right") => true,
        _ => false,
    };
    if !is_sys {
        return None;
    }

    let mut parts: Vec<&str> = Vec::with_capacity(4);
    if ctrl {
        parts.push("ctrl");
    }
    if alt {
        parts.push("alt");
    }
    if shift {
        parts.push("shift");
    }
    if cmd {
        parts.push("cmd");
    }
    parts.push(name);
    Some((key, parts.join("+")))
}

/// 将用户输入的例外快捷键字符串规范化为小写（如 `"⌘Tab"` / `"Cmd+Tab"` → `"cmd+tab"`）。
pub fn normalize_shortcut_id(raw: &str) -> String {
    let s = raw
        .trim()
        .to_ascii_lowercase()
        .replace('⌘', "cmd+")
        .replace('⌃', "ctrl+")
        .replace('⌥', "alt+")
        .replace('⇧', "shift+")
        .replace("command", "cmd")
        .replace("meta", "cmd")
        .replace("option", "alt")
        .replace("control", "ctrl")
        .replace("escape", "esc");
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut cmd = false;
    let mut main_key = String::new();
    for tok in s.split('+').map(str::trim).filter(|t| !t.is_empty()) {
        match tok {
            "ctrl" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            "cmd" => cmd = true,
            other => main_key = other.to_string(),
        }
    }
    let mut parts: Vec<&str> = Vec::with_capacity(5);
    if ctrl {
        parts.push("ctrl");
    }
    if alt {
        parts.push("alt");
    }
    if shift {
        parts.push("shift");
    }
    if cmd {
        parts.push("cmd");
    }
    if !main_key.is_empty() {
        parts.push(&main_key);
    }
    parts.join("+")
}

struct TapShared {
    enabled: AtomicBool,
    window_focused: AtomicBool,
    stop: AtomicBool,
    tap_port: AtomicUsize,
    /// 通过快捷键拦截合成发送给远端、当前仍处于按下态的修饰键位掩码（`FlagsChanged` 释放或失焦时自动配对抬起）。
    synthetic_mods_held: AtomicU8,
    exceptions: Mutex<HashSet<String>>,
    /// 记录被拦截按下但尚未抬起的按键（保证 keyup 也同步吞掉并发送抬起）。
    active_swallowed_vks: Mutex<HashSet<u16>>,
    on_event: Arc<dyn Fn(InterceptedShortcutEvent) + Send + Sync>,
}

fn emit_mod_bit(shared: &TapShared, scancode: u16, pressed: bool, modifiers: u8) {
    (shared.on_event)(InterceptedShortcutEvent {
        scancode,
        pressed,
        modifiers,
        meta: (modifiers & MOD_BIT_META) != 0,
        ctrl: (modifiers & MOD_BIT_CTRL) != 0,
        alt: (modifiers & MOD_BIT_ALT) != 0,
        shift: (modifiers & MOD_BIT_SHIFT) != 0,
    });
}

fn release_all_synthetic_state(shared: &TapShared) {
    let stale_vks: Vec<u16> = {
        let mut active = shared
            .active_swallowed_vks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        active.drain().collect()
    };
    let held_mods = shared.synthetic_mods_held.swap(0, Ordering::AcqRel);
    for vk in stale_vks {
        if let Some((key, _)) = mac_vk_to_key_and_name(vk) {
            if let Some(scancode) = set1_scancode(key) {
                emit_mod_bit(shared, scancode, false, held_mods);
            }
        }
    }
    if held_mods != 0 {
        for &(bit, sc) in MOD_SCANCODE_TABLE.iter().rev() {
            if (held_mods & bit) != 0 {
                emit_mod_bit(shared, sc, false, 0);
            }
        }
    }
}

fn modifier_bits_from_flags(flags: u64) -> (u8, bool, bool, bool, bool) {
    let f = CGEventFlags::from_bits_truncate(flags);
    let shift = f.contains(CGEventFlags::CGEventFlagShift);
    let ctrl = f.contains(CGEventFlags::CGEventFlagControl);
    let alt = f.contains(CGEventFlags::CGEventFlagAlternate);
    let meta = f.contains(CGEventFlags::CGEventFlagCommand);
    let mut bits = 0u8;
    if shift {
        bits |= MOD_BIT_SHIFT;
    }
    if ctrl {
        bits |= MOD_BIT_CTRL;
    }
    if alt {
        bits |= MOD_BIT_ALT;
    }
    if meta {
        bits |= MOD_BIT_META;
    }
    (bits, meta, ctrl, alt, shift)
}

unsafe extern "C" fn shortcut_tap_callback(
    _proxy: CGEventTapProxy,
    ty: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    if user_info.is_null() {
        return event;
    }
    // SAFETY: `user_info` 是 `run_shortcut_tap_loop` 栈上活着的 `Arc<TapShared>` 原始借用，
    // 直到 runloop 退出且 tap 被移除后才释放。
    let shared = unsafe { &*(user_info.cast::<TapShared>()) };

    if ty == K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT || ty == K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT {
        let port = shared.tap_port.load(Ordering::Acquire);
        if port != 0 {
            // SAFETY: `port` 为本线程持有的存活 `CFMachPortRef`。
            unsafe { CGEventTapEnable(port as CFMachPortRef, 1) };
        }
        return event;
    }

    if event.is_null() {
        return event;
    }

    // SAFETY: `event` 非空且由 CoreGraphics 回调传入。
    let flags = unsafe { CGEventGetFlags(event) };

    if ty == K_CG_EVENT_FLAGS_CHANGED {
        let held = shared.synthetic_mods_held.load(Ordering::Acquire);
        if held != 0 {
            let (cur_mods, _, _, _, _) = modifier_bits_from_flags(flags);
            let released = held & !cur_mods;
            if released != 0 {
                let remaining = held & cur_mods;
                shared
                    .synthetic_mods_held
                    .store(remaining, Ordering::Release);
                for &(bit, sc) in MOD_SCANCODE_TABLE.iter().rev() {
                    if (released & bit) != 0 {
                        emit_mod_bit(shared, sc, false, remaining);
                    }
                }
            }
        }
        // `FlagsChanged` 本身始终放行给本机系统，绝不破坏本地修饰键状态。
        return event;
    }

    if ty != K_CG_EVENT_KEY_DOWN && ty != K_CG_EVENT_KEY_UP {
        return event;
    }

    // SAFETY: `event` 非空且由 CoreGraphics 回调传入。
    let vk = unsafe { CGEventGetIntegerValueField(event, EventField::KEYBOARD_EVENT_KEYCODE) }
        .clamp(0, u16::MAX as i64) as u16;

    let pressed = ty == K_CG_EVENT_KEY_DOWN;
    if !pressed {
        let was_swallowed = shared
            .active_swallowed_vks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&vk);
        if was_swallowed {
            if let Some((key, _)) = mac_vk_to_key_and_name(vk) {
                if let Some(scancode) = set1_scancode(key) {
                    let (modifiers, meta, ctrl, alt, shift) = modifier_bits_from_flags(flags);
                    (shared.on_event)(InterceptedShortcutEvent {
                        scancode,
                        pressed: false,
                        modifiers,
                        meta,
                        ctrl,
                        alt,
                        shift,
                    });
                }
            }
            return std::ptr::null_mut();
        }
        return event;
    }

    if !shared.enabled.load(Ordering::Acquire) || !shared.window_focused.load(Ordering::Acquire) {
        return event;
    }

    let Some((key, combo_id)) = classify_system_shortcut(vk, flags) else {
        return event;
    };

    let is_excepted = shared
        .exceptions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&combo_id);
    if is_excepted {
        return event;
    }

    let Some(scancode) = set1_scancode(key) else {
        return event;
    };

    shared
        .active_swallowed_vks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(vk);
    let (modifiers, meta, ctrl, alt, shift) = modifier_bits_from_flags(flags);
    let prev_held = shared.synthetic_mods_held.load(Ordering::Acquire);
    let newly_pressed = modifiers & !prev_held;
    if newly_pressed != 0 {
        shared
            .synthetic_mods_held
            .store(prev_held | modifiers, Ordering::Release);
        for &(bit, sc) in &MOD_SCANCODE_TABLE {
            if (newly_pressed & bit) != 0 {
                emit_mod_bit(shared, sc, true, prev_held | modifiers);
            }
        }
    }
    (shared.on_event)(InterceptedShortcutEvent {
        scancode,
        pressed: true,
        modifiers,
        meta,
        ctrl,
        alt,
        shift,
    });
    // 返回 NULL 在本地吞掉该系统快捷键，防止被本机 macOS 抢走焦点。
    std::ptr::null_mut()
}

/// 控制端 macOS 系统快捷键拦截器句柄（`Drop` 自动停止线程并卸载 EventTap）。
pub struct SystemShortcutTap {
    shared: Arc<TapShared>,
    thread: Option<JoinHandle<()>>,
}

impl SystemShortcutTap {
    /// 启动后台系统快捷键拦截器（默认未激活，待同时开启 `set_enabled(true)` 与 `set_window_focused(true)` 后生效）。
    pub fn spawn(
        enabled: bool,
        exceptions: &[String],
        on_event: Arc<dyn Fn(InterceptedShortcutEvent) + Send + Sync>,
    ) -> Result<Self, String> {
        if !crate::ffi::ax_is_process_trusted() {
            return Err("未授予辅助功能权限，无法拦截系统快捷键".to_string());
        }
        let exc_set: HashSet<String> = exceptions
            .iter()
            .map(|s| normalize_shortcut_id(s))
            .filter(|s| !s.is_empty())
            .collect();
        let shared = Arc::new(TapShared {
            enabled: AtomicBool::new(enabled),
            window_focused: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            tap_port: AtomicUsize::new(0),
            synthetic_mods_held: AtomicU8::new(0),
            exceptions: Mutex::new(exc_set),
            active_swallowed_vks: Mutex::new(HashSet::new()),
            on_event,
        });

        let (setup_tx, setup_rx) = std::sync::mpsc::channel();
        let shared_thread = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("rc-shortcut-tap".to_string())
            .spawn(move || {
                let mask: CGEventMask = (1u64 << (CGEventType::KeyDown as u32))
                    | (1u64 << (CGEventType::KeyUp as u32))
                    | (1u64 << (CGEventType::FlagsChanged as u32));
                let user_info = (Arc::as_ptr(&shared_thread) as *mut TapShared).cast::<c_void>();
                // SAFETY: `user_info` 指向本线程持有的 `Arc<TapShared>`，在下方循环退出并移除
                // runloop source 且 drop `CFMachPort` 后才会释放，生命周期严格覆盖所有回调。
                let port_ref = unsafe {
                    CGEventTapCreate(
                        K_CG_SESSION_EVENT_TAP,
                        K_CG_HEAD_INSERT_EVENT_TAP,
                        K_CG_EVENT_TAP_OPTION_DEFAULT,
                        mask,
                        shortcut_tap_callback,
                        user_info,
                    )
                };
                if port_ref.is_null() {
                    let _ = setup_tx.send(Err(
                        "CGEventTapCreate 返回空指针（请检查辅助功能/输入监控权限）".to_string(),
                    ));
                    return;
                }
                // SAFETY: `port_ref` 为 `CGEventTapCreate` 返回的 +1 `CFMachPortRef`，接管所有权。
                let mach_port = unsafe { CFMachPort::wrap_under_create_rule(port_ref) };
                let Ok(source) = mach_port.create_runloop_source(0) else {
                    let _ = setup_tx.send(Err("创建 EventTap RunLoopSource 失败".to_string()));
                    return;
                };
                let runloop = CFRunLoop::get_current();
                // SAFETY: `kCFRunLoopCommonModes` 是 CoreFoundation 全局常量模式。
                unsafe { runloop.add_source(&source, kCFRunLoopCommonModes) };
                shared_thread
                    .tap_port
                    .store(mach_port.as_concrete_TypeRef() as usize, Ordering::Release);
                // SAFETY: `mach_port` 存活且已挂入当前 RunLoop。
                unsafe { CGEventTapEnable(mach_port.as_concrete_TypeRef(), 1) };
                let _ = setup_tx.send(Ok(()));

                while !shared_thread.stop.load(Ordering::Acquire) {
                    // SAFETY: `kCFRunLoopDefaultMode` 是 CoreFoundation 全局常量模式。
                    unsafe {
                        CFRunLoop::run_in_mode(
                            kCFRunLoopDefaultMode,
                            Duration::from_millis(80),
                            false,
                        );
                    }
                }
                shared_thread.tap_port.store(0, Ordering::Release);
                // SAFETY: 禁用、移除并显式 Invalidate `CFRunLoopSource` 与 `CFMachPort`，确保退出后再无回调触发且不泄漏内核端口。
                unsafe {
                    CGEventTapEnable(mach_port.as_concrete_TypeRef(), 0);
                    runloop.remove_source(&source, kCFRunLoopCommonModes);
                    CFRunLoopSourceInvalidate(source.as_concrete_TypeRef());
                    CFMachPortInvalidate(mach_port.as_concrete_TypeRef());
                }
                drop(source);
                drop(mach_port);
            })
            .map_err(|e| format!("启动快捷键拦截线程失败: {e}"))?;

        match setup_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                shared,
                thread: Some(handle),
            }),
            Ok(Err(e)) => {
                shared.stop.store(true, Ordering::Release);
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                shared.stop.store(true, Ordering::Release);
                let _ = handle.join();
                Err("等待快捷键拦截器初始化超时".to_string())
            }
        }
    }

    /// 实时更新是否启用系统快捷键发送；关闭时自动释放所有合成按下的远端修饰键与卡键状态。
    pub fn set_enabled(&self, enabled: bool) {
        self.shared.enabled.store(enabled, Ordering::Release);
        if !enabled {
            release_all_synthetic_state(&self.shared);
        }
    }

    /// 实时更新 S1 远控窗口是否拥有前台输入焦点；失焦时自动释放所有合成按下的远端修饰键与卡键状态。
    pub fn set_window_focused(&self, focused: bool) {
        self.shared.window_focused.store(focused, Ordering::Release);
        if !focused {
            release_all_synthetic_state(&self.shared);
        }
    }

    /// 实时更新本地保留不转发的例外快捷键列表。
    pub fn set_exceptions(&self, exceptions: &[String]) {
        let mut guard = self
            .shared
            .exceptions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.clear();
        for s in exceptions {
            let norm = normalize_shortcut_id(s);
            if !norm.is_empty() {
                guard.insert(norm);
            }
        }
    }
}

impl Drop for SystemShortcutTap {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_and_classify_system_shortcuts() {
        assert_eq!(normalize_shortcut_id("⌘Tab"), "cmd+tab");
        assert_eq!(normalize_shortcut_id("Cmd + Shift + 4"), "shift+cmd+4");
        assert_eq!(normalize_shortcut_id("⌥⌘Esc"), "alt+cmd+esc");
        assert_eq!(normalize_shortcut_id("⌃Up"), "ctrl+up");

        let cmd_flags = CGEventFlags::CGEventFlagCommand.bits();
        let (key, id) = classify_system_shortcut(0x30, cmd_flags).expect("Cmd+Tab");
        assert_eq!(key, Key::Tab);
        assert_eq!(id, "cmd+tab");

        let (_, sp) = classify_system_shortcut(0x31, cmd_flags).expect("Cmd+Space");
        assert_eq!(sp, "cmd+space");

        // 普通按键不应被识别为系统快捷键。
        assert!(classify_system_shortcut(0x31, 0).is_none());
    }
}

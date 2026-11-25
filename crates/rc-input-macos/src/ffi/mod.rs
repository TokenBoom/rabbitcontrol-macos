//! CGEvent 注入与捕获的 FFI 边界。
//!
//! 本 crate 的**全部 `unsafe` 都集中在这个模块**（含子模块 [`tap`]）：
//! - `AXIsProcessTrusted`（ApplicationServices）：注入/捕获前的辅助功能
//!   预检（T12 M2），core-graphics 0.24 不提供，这里自行声明；

//! - [`tap`]（捕获侧）：`CGEventTapEnable` / `CGEventSourceFlagsState`
//!   自行声明；CGEventTap 的创建、CFRunLoop 驱动走 core-graphics /
//!   core-foundation 的安全包装；
//! - `CGEvent` 的构造与投递走 core-graphics crate 的**安全**包装
//!   （`CGEvent::new_mouse_event` / `new_scroll_event` / `new_keyboard_event`
//!   / `post`），不产生裸指针操作。
//!
//! 对外只暴露安全类型 [`CgPoster`]（注入）与 [`TapCtl`] /
//! [`spawn_tap_thread`]（捕获），每个 `unsafe` 块都带 `// SAFETY:`。
//! 本模块的存在使 crate 其余部分可以 `deny(unsafe_code)`。

// 见模块文档：本模块是 crate 内唯一允许 unsafe 的地方。
#![allow(unsafe_code)]

pub(crate) mod tap;

pub(crate) use tap::{spawn_tap_thread, TapCtl};

use std::time::Instant;

use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::{CGPoint, CGRect, CGSize};

use crate::error::InputError;
use crate::event::PointerButtonKind;
use crate::plan::{EventPoster, PlannedEvent, PlannedMouseKind, ScreenBounds};

mod raw {
    // AXUIElement.h：`Boolean AXIsProcessTrusted(void)`（10.4+）。
    // Boolean == unsigned char == u8。注意这个变体**没有 options 参数、
    // 绝不弹窗**，适合做只读预检。
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        pub(super) fn AXIsProcessTrusted() -> u8;
    }

    // CGEventSource.h 的 `CGEventSourceCounterForEventType` 曾在这里自行
    // 声明、供真机测试断言注入进入 HID 系统源。已删除：实测 CGEventPost
    // 合成事件只进 Session 态、不进 HID 态计数（偶尔通过纯属混入真实
    // 硬件移动）。现行断言面是 tap 见证 + Session 态光标位置快照
    //（见 injector.rs 的 `inject_moves_cursor` 文档），不再需要该 extern。
}

/// 权限预检：`AXIsProcessTrusted()`，无 options 变体、绝不弹窗。
pub(crate) fn ax_is_process_trusted() -> bool {
    // SAFETY: 无参纯查询；返回 Boolean(0/1)，只判断非零。
    unsafe { raw::AXIsProcessTrusted() != 0 }
}

/// 当前光标位置（全局坐标）。`#[ignore]` 真机测试用。
///
/// 公开 SDK 没有直接的「查询光标位置」API；`CGEventCreate(NULL)` 快照
/// 事件携带当前光标位置是标准等价物（core-graphics 0.24 已包装）。
/// 测试据此避开「注入零位移移动被系统合并丢弃」的位置依赖缺陷。
#[cfg(test)]
pub(crate) fn current_cursor_location() -> (f64, f64) {
    // core-graphics 0.24 的 CGEvent::new 需要 source（无 None 变体）：创建
    // CombinedSession 态源的快照事件，其 location 即当前光标位置。
    // **必须用 Session 态而不是 HID 态**：CGEventPost 的合成事件只进
    // Session 态（HID 态计数器/位置只反映真实硬件输入——实测合成事件
    // 不涨 HID 计数，早期版本用它断言导致真机测试随机挂）。
    let source = core_graphics::event_source::CGEventSource::new(
        core_graphics::event_source::CGEventSourceStateID::CombinedSessionState,
    )
    .expect("Session 态事件源创建失败");
    let snap = core_graphics::event::CGEvent::new(source).expect("快照事件创建失败");
    let p = snap.location();
    (p.x, p.y)
}

/// 主显示器 `CGDisplayBounds`（全局坐标，点）。
///
/// 与 `rc-capture-macos` 默认采集 `display = 0`（`CGMainDisplayID`）的范围严格一致，
/// 避免多显示器环境下将单屏画面的归一化坐标误映射到全部显示器联合边界导致光标偏移。
pub(crate) fn main_display_bounds() -> ScreenBounds {
    display_rect_to_bounds(CGDisplay::main().bounds())
}

/// 所有在线显示器 `CGDisplayBounds` 的联合边界（全局坐标）。
///
/// 取不到在线列表（极端情况）时退回主显示器边界。
pub(crate) fn screen_bounds() -> ScreenBounds {
    let displays = CGDisplay::active_displays().unwrap_or_default();
    if displays.is_empty() {
        return display_rect_to_bounds(CGDisplay::main().bounds());
    }
    let mut union = CGDisplay::new(displays[0]).bounds();
    for &id in &displays[1..] {
        union = union_rect(union, CGDisplay::new(id).bounds());
    }
    display_rect_to_bounds(union)
}

fn display_rect_to_bounds(r: CGRect) -> ScreenBounds {
    ScreenBounds::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}

fn union_rect(a: CGRect, b: CGRect) -> CGRect {
    let x1 = a.origin.x.min(b.origin.x);
    let y1 = a.origin.y.min(b.origin.y);
    let x2 = (a.origin.x + a.size.width).max(b.origin.x + b.size.width);
    let y2 = (a.origin.y + a.size.height).max(b.origin.y + b.size.height);
    CGRect {
        origin: CGPoint { x: x1, y: y1 },
        size: CGSize {
            width: x2 - x1,
            height: y2 - y1,
        },
    }
}

/// 构造**行单位**滚轮 CGEvent：wheel1 = `dy`（垂直，向上为正），
/// wheel2 = `dx`（水平，向左为正），wheel_count=2（无第三轴）。
///
/// 单位必须是 [`ScrollEventUnit::LINE`]——与捕获侧 tap 读取的
/// `SCROLL_WHEEL_EVENT_DELTA_AXIS_1/2`（行单位）同量纲，macOS↔macOS
/// 回环数值对称；也是 `rc_input::InputEvent::MouseWheel` 的规范单位
/// （Windows 端 WHEEL_DELTA=120/格在 rc-input-win 除以 120 归一为行）。
/// 曾用 [`ScrollEventUnit::PIXEL`]：1..3 行的滚动被注成 1..3 **像素**
/// （系统按 ~10px/行换算后 tap 只能读到 0..1 行），回环严重不对称——
/// 离线测试 `line_scroll_event_roundtrips_tap_fields` 与
/// capture 侧的 `wheel_line_units_survive_capture_to_injection_chain`
/// 锁死不许回归。
pub(crate) fn new_line_scroll_event(
    source: CGEventSource,
    dx: i32,
    dy: i32,
) -> std::result::Result<CGEvent, ()> {
    CGEvent::new_scroll_event(source, ScrollEventUnit::LINE, 2, dy, dx, 0)
}

/// 双击/三击判定时间窗（微秒：500ms）与空间容差（逻辑点：4pt）。
const CLICK_CHAIN_TIMEOUT_US: u64 = 500_000;
const CLICK_CHAIN_RADIUS_PT: f64 = 4.0;

fn button_slot_index(button: PointerButtonKind) -> usize {
    match button {
        PointerButtonKind::Left => 0,
        PointerButtonKind::Right => 1,
        PointerButtonKind::Middle => 2,
    }
}

fn button_bit(button: PointerButtonKind) -> u8 {
    match button {
        PointerButtonKind::Left => 1,
        PointerButtonKind::Right => 2,
        PointerButtonKind::Middle => 4,
    }
}

/// 纯逻辑鼠标连击（单/双/三击）与按压拖拽追踪器。
#[derive(Debug, Default, Clone)]
pub(crate) struct ClickTracker {
    pressed_mask: u8,
    last_downs: [Option<(u64, f64, f64, i64)>; 3],
}

impl ClickTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 记录按键按下并返回 `CGEvent` 应写入的 `MOUSE_EVENT_CLICK_STATE`（1..=3）。
    pub(crate) fn on_down(&mut self, button: PointerButtonKind, t_us: u64, x: f64, y: f64) -> i64 {
        let idx = button_slot_index(button);
        let next_count = match self.last_downs[idx] {
            Some((prev_t, prev_x, prev_y, prev_count))
                if t_us.saturating_sub(prev_t) <= CLICK_CHAIN_TIMEOUT_US
                    && (x - prev_x).hypot(y - prev_y) <= CLICK_CHAIN_RADIUS_PT =>
            {
                (prev_count + 1).min(3)
            }
            _ => 1,
        };
        self.last_downs[idx] = Some((t_us, x, y, next_count));
        self.pressed_mask |= button_bit(button);
        next_count
    }

    /// 记录按键抬起并清除对应按键的按下标志，返回对应按下的连击次数。
    pub(crate) fn on_up(&mut self, button: PointerButtonKind) -> i64 {
        self.pressed_mask &= !button_bit(button);
        let idx = button_slot_index(button);
        self.last_downs[idx].map(|(_, _, _, c)| c).unwrap_or(1)
    }

    /// 若当前有任何鼠标键处于按下状态，按 Left > Right > Middle 优先级返回用于选取 `MouseDragged` 的按键。
    pub(crate) fn drag_button(&self) -> Option<PointerButtonKind> {
        if (self.pressed_mask & 1) != 0 {
            Some(PointerButtonKind::Left)
        } else if (self.pressed_mask & 2) != 0 {
            Some(PointerButtonKind::Right)
        } else if (self.pressed_mask & 4) != 0 {
            Some(PointerButtonKind::Middle)
        } else {
            None
        }
    }
}

/// `IOKit/hidsystem/IOLLEvent.h` 中定义的左右修饰键设备相关掩码位。
///
/// `core-graphics` 0.24 的 [`CGEventFlags`] 仅声明了设备无关位（`Shift`/`Control`/
/// `Alternate`/`Command`）；合成 `FlagsChanged` 及组合键事件同时带上左右设备位，
/// 才能与真实键盘事件布局对齐（例如 LeftShift = `0x0002_0002`、LeftOption = `0x0008_0020`）。
const NX_DEVICELCTLKEYMASK: u64 = 0x0000_0001;
const NX_DEVICELSHIFTKEYMASK: u64 = 0x0000_0002;
const NX_DEVICERSHIFTKEYMASK: u64 = 0x0000_0004;
const NX_DEVICELCMDKEYMASK: u64 = 0x0000_0008;
const NX_DEVICERCMDKEYMASK: u64 = 0x0000_0010;
const NX_DEVICELALTKEYMASK: u64 = 0x0000_0020;
const NX_DEVICERALTKEYMASK: u64 = 0x0000_0040;
const NX_DEVICERCTLKEYMASK: u64 = 0x0000_2000;

/// 受 [`ModifierTracker`] 管理的全部修饰键标志位（4 个设备无关位 + 8 个左右设备位）。
///
/// 写入 `CGEvent` 时只替换这一组位，保留 `NumericPad` / `SecondaryFn` /
/// `AlphaShift` 等非追踪键位（例如 `Shift + ArrowUp` 仍保留方向键的 `NumericPad | SecondaryFn`）。
const TRACKED_MODIFIER_MASK: u64 = CGEventFlags::CGEventFlagShift.bits()
    | CGEventFlags::CGEventFlagControl.bits()
    | CGEventFlags::CGEventFlagAlternate.bits()
    | CGEventFlags::CGEventFlagCommand.bits()
    | NX_DEVICELCTLKEYMASK
    | NX_DEVICELSHIFTKEYMASK
    | NX_DEVICERSHIFTKEYMASK
    | NX_DEVICELCMDKEYMASK
    | NX_DEVICERCMDKEYMASK
    | NX_DEVICELALTKEYMASK
    | NX_DEVICERALTKEYMASK
    | NX_DEVICERCTLKEYMASK;

#[derive(Debug, Clone, Copy)]
struct ModifierSpec {
    keycode: u16,
    slot_bit: u8,
    generic_flag: u64,
    device_flag: u64,
}

const MODIFIER_SPECS: [ModifierSpec; 8] = [
    ModifierSpec {
        keycode: 0x38, // 56: kVK_Shift (LeftShift)
        slot_bit: 1 << 0,
        generic_flag: CGEventFlags::CGEventFlagShift.bits(),
        device_flag: NX_DEVICELSHIFTKEYMASK,
    },
    ModifierSpec {
        keycode: 0x3C, // 60: kVK_RightShift
        slot_bit: 1 << 1,
        generic_flag: CGEventFlags::CGEventFlagShift.bits(),
        device_flag: NX_DEVICERSHIFTKEYMASK,
    },
    ModifierSpec {
        keycode: 0x3B, // 59: kVK_Control (LeftControl)
        slot_bit: 1 << 2,
        generic_flag: CGEventFlags::CGEventFlagControl.bits(),
        device_flag: NX_DEVICELCTLKEYMASK,
    },
    ModifierSpec {
        keycode: 0x3E, // 62: kVK_RightControl
        slot_bit: 1 << 3,
        generic_flag: CGEventFlags::CGEventFlagControl.bits(),
        device_flag: NX_DEVICERCTLKEYMASK,
    },
    ModifierSpec {
        keycode: 0x3A, // 58: kVK_Option (LeftOption)
        slot_bit: 1 << 4,
        generic_flag: CGEventFlags::CGEventFlagAlternate.bits(),
        device_flag: NX_DEVICELALTKEYMASK,
    },
    ModifierSpec {
        keycode: 0x3D, // 61: kVK_RightOption
        slot_bit: 1 << 5,
        generic_flag: CGEventFlags::CGEventFlagAlternate.bits(),
        device_flag: NX_DEVICERALTKEYMASK,
    },
    ModifierSpec {
        keycode: 0x37, // 55: kVK_Command (LeftCommand)
        slot_bit: 1 << 6,
        generic_flag: CGEventFlags::CGEventFlagCommand.bits(),
        device_flag: NX_DEVICELCMDKEYMASK,
    },
    ModifierSpec {
        keycode: 0x36, // 54: kVK_RightCommand
        slot_bit: 1 << 7,
        generic_flag: CGEventFlags::CGEventFlagCommand.bits(),
        device_flag: NX_DEVICERCMDKEYMASK,
    },
];

/// 合成修饰键状态追踪器（支持左右 Shift / Ctrl / Option / Command 独立跟踪）。
///
/// 按键槽位而非扁平掩码记录，避免「同时按住左右 Shift 时松开一侧误清通用 `Shift` 位」；
/// 每次查询 [`Self::cg_flags`] 均从当前按住槽位重新汇总设备无关位与左右设备位。
#[derive(Debug, Default, Clone)]
pub(crate) struct ModifierTracker {
    held_slots: u8,
}

impl ModifierTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn spec_for_keycode(keycode: u16) -> Option<&'static ModifierSpec> {
        MODIFIER_SPECS.iter().find(|spec| spec.keycode == keycode)
    }

    /// 更新修饰键按下状态；若 `keycode` 为 8 个受管修饰键之一则返回更新后的 `CGEventFlags` 掩码。
    pub(crate) fn update(&mut self, keycode: u16, pressed: bool) -> Option<u64> {
        let spec = Self::spec_for_keycode(keycode)?;
        if pressed {
            self.held_slots |= spec.slot_bit;
        } else {
            self.held_slots &= !spec.slot_bit;
        }
        Some(self.cg_flags())
    }

    /// 当前按住的修饰键对应的 `CGEventFlags` 位掩码（含设备无关位与左右设备位）。
    pub(crate) fn cg_flags(&self) -> u64 {
        let mut flags = 0u64;
        for spec in &MODIFIER_SPECS {
            if (self.held_slots & spec.slot_bit) != 0 {
                flags |= spec.generic_flag | spec.device_flag;
            }
        }
        flags
    }

    /// 是否没有任何修饰键处于按住状态。
    pub(crate) fn is_empty(&self) -> bool {
        self.held_slots == 0
    }

    /// 逐个释放仍处于按住状态的修饰键，返回 `(要释放的 keycode, 释放后的残余 flags)`。
    pub(crate) fn pop_held(&mut self) -> Option<(u16, u64)> {
        let spec = MODIFIER_SPECS
            .iter()
            .find(|spec| (self.held_slots & spec.slot_bit) != 0)?;
        self.held_slots &= !spec.slot_bit;
        Some((spec.keycode, self.cg_flags()))
    }
}

/// 将 `tracked_flags` 写入 `CGEvent`，同时保留 `NumericPad` / `SecondaryFn` 等非追踪位。
fn apply_modifier_flags(event: &CGEvent, tracked_flags: u64) {
    let preserved = event.get_flags().bits() & !TRACKED_MODIFIER_MASK;
    event.set_flags(CGEventFlags::from_bits_retain(preserved | tracked_flags));
}

fn new_flags_changed_event(
    source: CGEventSource,
    keycode: u16,
    pressed: bool,
    tracked_flags: u64,
) -> Result<CGEvent, InputError> {
    let event = CGEvent::new_keyboard_event(source, keycode, pressed)
        .map_err(|_| InputError::EventCreateFailed("keyboard"))?;
    event.set_type(CGEventType::FlagsChanged);
    apply_modifier_flags(&event, tracked_flags);
    Ok(event)
}

/// [`EventPoster`] 的 CGEvent 生产实现：持有私有事件源，把 [`PlannedEvent`]
/// 构造成 `CGEvent` 并 `CGEventPost` 到 HID 层事件 tap。
///
/// **前置**：`CGEventPost` 需要辅助功能授权（`AXIsProcessTrusted`），
/// 由上层 [`Injector`](crate::injector::Injector) 预检；未授权时系统静默
/// 丢弃事件（不报错）。
pub struct CgPoster {
    source: CGEventSource,
    clicks: ClickTracker,
    modifiers: ModifierTracker,
    origin: Instant,
}

// SAFETY: core-graphics 0.24 未给 CGEventSource 标 Send，但这里跨线程移动
// 是安全的：CGEventSource 是 Core Foundation 引用计数对象，CFRetain /
// CFRelease 线程安全；Quartz Event Services 的 CGEvent 构造与 CGEventPost
// 按苹果线程安全摘要可从任意线程调用。使用方（Injector / InputBridge）
// 只以 `&mut self` 串行访问，不共享引用（Send 足矣，不需要 Sync）。
// T10 的 `rc_input::InputInjector` trait 以 `Send` 为超特征，没有它生产
// 桥接（`InputBridge<CgPoster>`）无法满足契约。
unsafe impl Send for CgPoster {}

impl CgPoster {
    /// 创建事件源。使用 `Private` 状态表：
    ///
    /// - 遵循 Apple `CGEventSource.h` 对远控程序的建议，维护独立于物理硬件的状态表，
    ///   绝不污染或锁死 `HIDSystemState`（避免 `CGEventSourceFlagsState(HIDSystemState)`
    ///   残余左 Shift 粘连）；
    /// - 不继承宿主 `HIDSystemState` / `CombinedSessionState` 可能残留的修饰位；
    /// - 投递目标仍为 `CGEventTapLocation::HID`，HID / Session `CGEventTap` 与应用窗口均可见。
    pub fn new() -> Result<Self, InputError> {
        let source = CGEventSource::new(CGEventSourceStateID::Private)
            .map_err(|_| InputError::SourceCreateFailed)?;
        Ok(Self {
            source,
            clicks: ClickTracker::new(),
            modifiers: ModifierTracker::new(),
            origin: Instant::now(),
        })
    }

    /// 根据 [`PlannedEvent`] 构造待投递的 `CGEvent` 并同步合成修饰键掩码（不执行 `post`）。
    pub(crate) fn build_cg_event(&mut self, planned: &PlannedEvent) -> Result<CGEvent, InputError> {
        match planned {
            PlannedEvent::Mouse { kind, button, x, y } => {
                let t_us = u64::try_from(self.origin.elapsed().as_micros()).unwrap_or(u64::MAX);
                let (event_type, cg_button, click_state) = match (kind, button) {
                    (PlannedMouseKind::Moved, _) => match self.clicks.drag_button() {
                        Some(PointerButtonKind::Left) => {
                            (CGEventType::LeftMouseDragged, CGMouseButton::Left, 1)
                        }
                        Some(PointerButtonKind::Right) => {
                            (CGEventType::RightMouseDragged, CGMouseButton::Right, 1)
                        }
                        Some(PointerButtonKind::Middle) => {
                            (CGEventType::OtherMouseDragged, CGMouseButton::Center, 1)
                        }
                        None => (CGEventType::MouseMoved, CGMouseButton::Left, 1),
                    },
                    (PlannedMouseKind::Down, PointerButtonKind::Left) => (
                        CGEventType::LeftMouseDown,
                        CGMouseButton::Left,
                        self.clicks.on_down(PointerButtonKind::Left, t_us, *x, *y),
                    ),
                    (PlannedMouseKind::Up, PointerButtonKind::Left) => (
                        CGEventType::LeftMouseUp,
                        CGMouseButton::Left,
                        self.clicks.on_up(PointerButtonKind::Left),
                    ),
                    (PlannedMouseKind::Down, PointerButtonKind::Right) => (
                        CGEventType::RightMouseDown,
                        CGMouseButton::Right,
                        self.clicks.on_down(PointerButtonKind::Right, t_us, *x, *y),
                    ),
                    (PlannedMouseKind::Up, PointerButtonKind::Right) => (
                        CGEventType::RightMouseUp,
                        CGMouseButton::Right,
                        self.clicks.on_up(PointerButtonKind::Right),
                    ),
                    (PlannedMouseKind::Down, PointerButtonKind::Middle) => (
                        CGEventType::OtherMouseDown,
                        CGMouseButton::Center,
                        self.clicks.on_down(PointerButtonKind::Middle, t_us, *x, *y),
                    ),
                    (PlannedMouseKind::Up, PointerButtonKind::Middle) => (
                        CGEventType::OtherMouseUp,
                        CGMouseButton::Center,
                        self.clicks.on_up(PointerButtonKind::Middle),
                    ),
                };
                let event = CGEvent::new_mouse_event(
                    self.source.clone(),
                    event_type,
                    CGPoint { x: *x, y: *y },
                    cg_button,
                )
                .map_err(|_| InputError::EventCreateFailed("mouse"))?;
                event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_state);
                apply_modifier_flags(&event, self.modifiers.cg_flags());
                Ok(event)
            }
            PlannedEvent::Scroll { dx, dy } => {
                let event = new_line_scroll_event(self.source.clone(), *dx, *dy)
                    .map_err(|_| InputError::EventCreateFailed("scroll"))?;
                apply_modifier_flags(&event, self.modifiers.cg_flags());
                Ok(event)
            }
            PlannedEvent::Key { keycode, pressed } => {
                if let Some(tracked_flags) = self.modifiers.update(*keycode, *pressed) {
                    new_flags_changed_event(self.source.clone(), *keycode, *pressed, tracked_flags)
                } else {
                    let event =
                        CGEvent::new_keyboard_event(self.source.clone(), *keycode, *pressed)
                            .map_err(|_| InputError::EventCreateFailed("keyboard"))?;
                    apply_modifier_flags(&event, self.modifiers.cg_flags());
                    Ok(event)
                }
            }
        }
    }

    /// 排空仍处于按住状态的修饰键，依次生成 `FlagsChanged` 释放事件（末条掩码必为 `0`）。
    pub(crate) fn drain_drop_release_events(&mut self) -> Result<Vec<CGEvent>, InputError> {
        let mut events = Vec::new();
        while let Some((keycode, remaining_flags)) = self.modifiers.pop_held() {
            events.push(new_flags_changed_event(
                self.source.clone(),
                keycode,
                false,
                remaining_flags,
            )?);
        }
        Ok(events)
    }
}

impl Drop for CgPoster {
    fn drop(&mut self) {
        if self.modifiers.is_empty() {
            return;
        }
        if let Ok(events) = self.drain_drop_release_events() {
            for event in events {
                event.post(CGEventTapLocation::HID);
            }
        }
    }
}

impl EventPoster for CgPoster {
    fn post(&mut self, planned: &PlannedEvent) -> Result<(), InputError> {
        let event = self.build_cg_event(planned)?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 行单位滚轮事件的**tap 侧读回对称性**（离线，无需授权——构造
    /// CGEvent 不需要辅助功能，只有 post 需要）：
    ///
    /// 注入侧 `new_line_scroll_event` 构造的事件，用捕获侧 tap 实际读取
    /// 的同一对字段（`SCROLL_WHEEL_EVENT_DELTA_AXIS_1/2`，见 `ffi::tap`
    /// 的 `extract_raw`）读回，数值必须**逐值相等**——这是 macOS↔macOS
    /// 滚轮回环对称的机制级锁定。
    ///
    /// 选 delta=3（垂直）/-2 与 2（水平）不是随意值：本机实测 PIXEL 单位
    /// 构造同值事件时系统按 ~10px/行换算，AXIS_1 只能读回 1（10× 缩水），
    /// 因此若有人把单位改回 PIXEL，本测试当场爆。3/±2 的典型值域正来自
    /// 审计问题原文（「典型 ±1..±3」）。
    #[test]
    fn line_scroll_event_roundtrips_tap_fields() {
        let source =
            CGEventSource::new(CGEventSourceStateID::HIDSystemState).expect("构造事件源无需授权");

        // 垂直：wheel1 = dy（向上为正，与 tap 读回的 AXIS_1 同符号同值）
        let v = new_line_scroll_event(source.clone(), 0, 3).expect("line scroll event");
        assert_eq!(
            v.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1),
            3,
            "垂直 3 行注入，tap 侧 AXIS_1 必须读回 3"
        );
        assert_eq!(
            v.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2),
            0
        );

        // 垂直向下 -2 行
        let vd = new_line_scroll_event(source.clone(), 0, -2).expect("line scroll event");
        assert_eq!(
            vd.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1),
            -2
        );

        // 水平：wheel2 = dx（向左为正；桥接层已把 rc 的向右为正翻成 dx 负）
        let h = new_line_scroll_event(source, -2, 0).expect("line scroll event");
        assert_eq!(
            h.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2),
            -2,
            "水平 dx=-2（rc 向右 2 行）注入，tap 侧 AXIS_2 必须读回 -2"
        );
        assert_eq!(
            h.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1),
            0
        );
    }

    #[test]
    fn click_tracker_handles_multi_click_drag_and_independent_right_button() {
        let mut tracker = ClickTracker::new();
        assert_eq!(tracker.drag_button(), None);

        // 单击 -> 双击 -> 三击 -> 封顶 3
        assert_eq!(
            tracker.on_down(PointerButtonKind::Left, 100_000, 50.0, 60.0),
            1
        );
        assert_eq!(tracker.drag_button(), Some(PointerButtonKind::Left));
        assert_eq!(tracker.on_up(PointerButtonKind::Left), 1);
        assert_eq!(tracker.drag_button(), None);

        assert_eq!(
            tracker.on_down(PointerButtonKind::Left, 300_000, 51.0, 61.0),
            2
        );
        assert_eq!(tracker.on_up(PointerButtonKind::Left), 2);

        assert_eq!(
            tracker.on_down(PointerButtonKind::Left, 450_000, 51.5, 61.5),
            3
        );
        assert_eq!(tracker.on_up(PointerButtonKind::Left), 3);

        // 超过 500ms 重置为 1
        assert_eq!(
            tracker.on_down(PointerButtonKind::Left, 1_100_000, 51.5, 61.5),
            1
        );
        assert_eq!(tracker.on_up(PointerButtonKind::Left), 1);

        // 移动超过 4pt (10pt) 重置为 1
        assert_eq!(
            tracker.on_down(PointerButtonKind::Left, 1_200_000, 61.5, 61.5),
            1
        );
        assert_eq!(tracker.on_up(PointerButtonKind::Left), 1);

        // 右键独立连击计数 + 优先级 Left > Right > Middle
        assert_eq!(
            tracker.on_down(PointerButtonKind::Right, 1_250_000, 10.0, 10.0),
            1
        );
        assert_eq!(tracker.drag_button(), Some(PointerButtonKind::Right));
        assert_eq!(
            tracker.on_down(PointerButtonKind::Left, 2_000_000, 10.0, 10.0),
            1
        );
        assert_eq!(tracker.drag_button(), Some(PointerButtonKind::Left));
        assert_eq!(tracker.on_up(PointerButtonKind::Left), 1);
        assert_eq!(tracker.drag_button(), Some(PointerButtonKind::Right));
        assert_eq!(tracker.on_up(PointerButtonKind::Right), 1);
        assert_eq!(tracker.drag_button(), None);
        assert_eq!(
            tracker.on_down(PointerButtonKind::Right, 1_400_000, 11.0, 11.0),
            2
        );
        assert_eq!(tracker.on_up(PointerButtonKind::Right), 2);
    }

    #[test]
    fn modifier_tracker_maps_all_eight_modifier_keycodes() {
        let cases: &[(u16, u64)] = &[
            (
                56, // LeftShift (0x38)
                CGEventFlags::CGEventFlagShift.bits() | NX_DEVICELSHIFTKEYMASK,
            ),
            (
                60, // RightShift (0x3C)
                CGEventFlags::CGEventFlagShift.bits() | NX_DEVICERSHIFTKEYMASK,
            ),
            (
                59, // LeftControl (0x3B)
                CGEventFlags::CGEventFlagControl.bits() | NX_DEVICELCTLKEYMASK,
            ),
            (
                62, // RightControl (0x3E)
                CGEventFlags::CGEventFlagControl.bits() | NX_DEVICERCTLKEYMASK,
            ),
            (
                58, // LeftOption (0x3A)
                CGEventFlags::CGEventFlagAlternate.bits() | NX_DEVICELALTKEYMASK,
            ),
            (
                61, // RightOption (0x3D)
                CGEventFlags::CGEventFlagAlternate.bits() | NX_DEVICERALTKEYMASK,
            ),
            (
                55, // LeftCommand (0x37)
                CGEventFlags::CGEventFlagCommand.bits() | NX_DEVICELCMDKEYMASK,
            ),
            (
                54, // RightCommand (0x36)
                CGEventFlags::CGEventFlagCommand.bits() | NX_DEVICERCMDKEYMASK,
            ),
        ];

        for &(keycode, expected_flags) in cases {
            let mut tracker = ModifierTracker::new();
            assert!(tracker.is_empty());
            assert_eq!(tracker.update(keycode, true), Some(expected_flags));
            assert_eq!(tracker.cg_flags(), expected_flags);
            assert!(!tracker.is_empty());
            assert_eq!(tracker.update(keycode, false), Some(0));
            assert_eq!(tracker.cg_flags(), 0);
            assert!(tracker.is_empty());
        }

        // 非修饰键（A / Escape / CapsLock / Fn）不得被 ModifierTracker 吞为 FlagsChanged
        let mut tracker = ModifierTracker::new();
        for non_mod in [0x00u16, 0x35, 0x39, 0x3F] {
            assert_eq!(tracker.update(non_mod, true), None);
            assert_eq!(tracker.update(non_mod, false), None);
        }
        assert!(tracker.is_empty());
    }

    #[test]
    fn modifier_tracker_handles_left_right_same_family_and_option_shift_chords() {
        let mut tracker = ModifierTracker::new();
        let lshift = CGEventFlags::CGEventFlagShift.bits() | NX_DEVICELSHIFTKEYMASK;
        let rshift = CGEventFlags::CGEventFlagShift.bits() | NX_DEVICERSHIFTKEYMASK;
        let lopt = CGEventFlags::CGEventFlagAlternate.bits() | NX_DEVICELALTKEYMASK;

        // 同时按住左右 Shift：松开左侧时通用 Shift 位必须仍由右侧维持
        assert_eq!(tracker.update(56, true), Some(lshift));
        assert_eq!(tracker.update(60, true), Some(lshift | rshift));
        assert_eq!(tracker.update(56, false), Some(rshift));
        assert!(!tracker.is_empty());
        assert_eq!(tracker.update(60, false), Some(0));
        assert!(tracker.is_empty());

        // Option + Shift 组合（S13 场景）+ 幂等重复按下/释放 + 干净归零
        assert_eq!(tracker.update(58, true), Some(lopt));
        assert_eq!(tracker.update(58, true), Some(lopt));
        assert_eq!(tracker.update(56, true), Some(lopt | lshift));
        assert_eq!(tracker.update(56, false), Some(lopt));
        assert_eq!(tracker.update(56, false), Some(lopt));
        assert_eq!(tracker.update(58, false), Some(0));
        assert!(tracker.is_empty());
    }

    #[test]
    fn cg_poster_builds_flags_changed_chords_and_resets_to_zero() {
        let mut poster = CgPoster::new().expect("Private CGEventSource 无需辅助功能权限");
        let lopt = CGEventFlags::CGEventFlagAlternate.bits() | NX_DEVICELALTKEYMASK;
        let lshift = CGEventFlags::CGEventFlagShift.bits() | NX_DEVICELSHIFTKEYMASK;
        let chord = lopt | lshift;

        // 1. 初始非修饰键无残留修饰位（即使宿主 HIDSystemState 存在历史粘连）
        let plain_a = poster
            .build_cg_event(&PlannedEvent::Key {
                keycode: 0x00,
                pressed: true,
            })
            .unwrap();
        assert_eq!(plain_a.get_type() as u32, CGEventType::KeyDown as u32);
        assert_eq!(plain_a.get_flags().bits() & TRACKED_MODIFIER_MASK, 0);

        // 2. Option + Shift 按下：产生 FlagsChanged 且累积掩码准确
        let opt_down = poster
            .build_cg_event(&PlannedEvent::Key {
                keycode: 58,
                pressed: true,
            })
            .unwrap();
        assert_eq!(opt_down.get_type() as u32, CGEventType::FlagsChanged as u32);
        assert_eq!(
            opt_down.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE),
            58
        );
        assert_eq!(opt_down.get_flags().bits() & TRACKED_MODIFIER_MASK, lopt);

        let shift_down = poster
            .build_cg_event(&PlannedEvent::Key {
                keycode: 56,
                pressed: true,
            })
            .unwrap();
        assert_eq!(
            shift_down.get_type() as u32,
            CGEventType::FlagsChanged as u32
        );
        assert_eq!(shift_down.get_flags().bits() & TRACKED_MODIFIER_MASK, chord);

        // 3. 组合键期间普通键（A）和方向键（ArrowUp）均携带完整 Option+Shift 掩码，
        //    且 ArrowUp 保留 NumericPad / SecondaryFn 标志。
        let chord_a = poster
            .build_cg_event(&PlannedEvent::Key {
                keycode: 0x00,
                pressed: true,
            })
            .unwrap();
        assert_eq!(chord_a.get_type() as u32, CGEventType::KeyDown as u32);
        assert_eq!(chord_a.get_flags().bits() & TRACKED_MODIFIER_MASK, chord);

        let chord_up = poster
            .build_cg_event(&PlannedEvent::Key {
                keycode: 0x7E,
                pressed: true,
            })
            .unwrap();
        assert_eq!(chord_up.get_flags().bits() & TRACKED_MODIFIER_MASK, chord);
        assert!(chord_up
            .get_flags()
            .contains(CGEventFlags::CGEventFlagNumericPad | CGEventFlags::CGEventFlagSecondaryFn));

        // 4. 逐一松开后修饰位必须干净归零
        let shift_up = poster
            .build_cg_event(&PlannedEvent::Key {
                keycode: 56,
                pressed: false,
            })
            .unwrap();
        assert_eq!(shift_up.get_type() as u32, CGEventType::FlagsChanged as u32);
        assert_eq!(shift_up.get_flags().bits() & TRACKED_MODIFIER_MASK, lopt);

        let opt_up = poster
            .build_cg_event(&PlannedEvent::Key {
                keycode: 58,
                pressed: false,
            })
            .unwrap();
        assert_eq!(opt_up.get_type() as u32, CGEventType::FlagsChanged as u32);
        assert_eq!(opt_up.get_flags().bits() & TRACKED_MODIFIER_MASK, 0);

        let after_a = poster
            .build_cg_event(&PlannedEvent::Key {
                keycode: 0x00,
                pressed: true,
            })
            .unwrap();
        assert_eq!(after_a.get_flags().bits() & TRACKED_MODIFIER_MASK, 0);
        assert!(poster.modifiers.is_empty());
    }

    #[test]
    fn cg_poster_carries_modifiers_on_mouse_and_scroll_events() {
        let mut poster = CgPoster::new().expect("Private CGEventSource 无需辅助功能权限");
        let lcmd = CGEventFlags::CGEventFlagCommand.bits() | NX_DEVICELCMDKEYMASK;

        poster
            .build_cg_event(&PlannedEvent::Key {
                keycode: 55, // LeftCommand down
                pressed: true,
            })
            .unwrap();

        let mv = poster
            .build_cg_event(&PlannedEvent::Mouse {
                kind: PlannedMouseKind::Moved,
                button: PointerButtonKind::Left,
                x: 100.0,
                y: 200.0,
            })
            .unwrap();
        assert_eq!(mv.get_flags().bits() & TRACKED_MODIFIER_MASK, lcmd);

        let down = poster
            .build_cg_event(&PlannedEvent::Mouse {
                kind: PlannedMouseKind::Down,
                button: PointerButtonKind::Left,
                x: 100.0,
                y: 200.0,
            })
            .unwrap();
        assert_eq!(down.get_flags().bits() & TRACKED_MODIFIER_MASK, lcmd);

        let drag = poster
            .build_cg_event(&PlannedEvent::Mouse {
                kind: PlannedMouseKind::Moved,
                button: PointerButtonKind::Left,
                x: 120.0,
                y: 220.0,
            })
            .unwrap();
        assert_eq!(drag.get_type() as u32, CGEventType::LeftMouseDragged as u32);
        assert_eq!(drag.get_flags().bits() & TRACKED_MODIFIER_MASK, lcmd);

        let up = poster
            .build_cg_event(&PlannedEvent::Mouse {
                kind: PlannedMouseKind::Up,
                button: PointerButtonKind::Left,
                x: 120.0,
                y: 220.0,
            })
            .unwrap();
        assert_eq!(up.get_flags().bits() & TRACKED_MODIFIER_MASK, lcmd);

        let scroll = poster
            .build_cg_event(&PlannedEvent::Scroll { dx: 0, dy: 2 })
            .unwrap();
        assert_eq!(scroll.get_flags().bits() & TRACKED_MODIFIER_MASK, lcmd);

        // 释放 Command 后鼠标事件不再携带修饰位
        poster
            .build_cg_event(&PlannedEvent::Key {
                keycode: 55,
                pressed: false,
            })
            .unwrap();
        let plain_move = poster
            .build_cg_event(&PlannedEvent::Mouse {
                kind: PlannedMouseKind::Moved,
                button: PointerButtonKind::Left,
                x: 130.0,
                y: 230.0,
            })
            .unwrap();
        assert_eq!(plain_move.get_flags().bits() & TRACKED_MODIFIER_MASK, 0);
        assert!(poster.modifiers.is_empty());
    }

    #[test]
    fn cg_poster_drop_release_events_clears_held_modifiers() {
        let mut poster = CgPoster::new().expect("Private CGEventSource 无需辅助功能权限");
        // 模拟未配对释放即断开连接：LeftShift + LeftOption + RightCommand 仍按住
        for kc in [56u16, 58, 54] {
            poster
                .build_cg_event(&PlannedEvent::Key {
                    keycode: kc,
                    pressed: true,
                })
                .unwrap();
        }
        assert!(!poster.modifiers.is_empty());

        let releases = poster.drain_drop_release_events().unwrap();
        assert_eq!(releases.len(), 3);
        for ev in &releases {
            assert_eq!(ev.get_type() as u32, CGEventType::FlagsChanged as u32);
        }
        // 按 MODIFIER_SPECS 顺序依次释放 LeftShift(56) -> LeftOption(58) -> RightCommand(54)
        assert_eq!(
            releases[0].get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE),
            56
        );
        assert_eq!(
            releases[1].get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE),
            58
        );
        assert_eq!(
            releases[2].get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE),
            54
        );
        assert_eq!(
            releases[2].get_flags().bits() & TRACKED_MODIFIER_MASK,
            0,
            "最后一条释放事件必须将所有修饰位清零"
        );
        assert!(poster.modifiers.is_empty());
        assert!(poster.drain_drop_release_events().unwrap().is_empty());
    }
}

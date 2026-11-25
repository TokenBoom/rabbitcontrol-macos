//! 键盘全链路体验量化基准模块（仅 `bench-metrics` 启用，零 `unsafe`）。
//!
//! 覆盖：
//! - `S12-key-typing-quic`：28 次随机相位单键 I2P / 抬键响应（验证 I7 免批窗直发），及 25 键/秒高速连击丢键率与乱序检查
//! - `S13-key-chords-hold`：`Shift` / `Option+Shift` 组合键修饰状态同步准确率，及长按释放防粘连检查

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rc_client_core::input_bridge::scancode_for;
use rc_input_macos::keymap::virtual_keycode;
use rc_input_macos::scancode::key_from_set1;
use rc_render_macos::media_time_us;

use super::mouse::{MouseBenchState, QuicInputLoop};
use super::runner::{BenchSuite, PresentedObservation, TargetKeyEvent};
use super::stats::{DistStats, KeyboardMetrics};

/// 惰性测试字母表（仅发 A–Z 字母键，绝不触碰 Cmd/Esc/Tab/Space 等全局热键）。
const TEST_KEY_NAMES: [&str; 26] = [
    "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK", "KeyL",
    "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT", "KeyU", "KeyV", "KeyW", "KeyX",
    "KeyY", "KeyZ",
];

#[derive(Debug, Clone, Copy)]
pub(super) struct KeyTapEvent {
    pub emit_down_us: u64,
    pub emit_up_us: u64,
    pub expected_vk: u16,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ChordStepEvent {
    pub emit_down_us: u64,
    pub expected_vk: u16,
    pub required_mod_mask: u8,
}

#[derive(Default)]
pub(super) struct KeyboardBenchState {
    pub tap_steps: std::sync::Mutex<Vec<KeyTapEvent>>,
    pub burst_expected_vks: std::sync::Mutex<Vec<(u64, u16)>>,
    pub chord_steps: std::sync::Mutex<Vec<ChordStepEvent>>,
    pub hold_up_steps: std::sync::Mutex<Vec<u64>>,
    pub burst_keys_sent: std::sync::Mutex<u64>,
}

/// RAII 按键释放守卫：任何退出路径下自动补齐已按下但未释放的 Set-1 扫描码 `KeyUp`，防止系统粘键。
pub(super) struct KeyReleaseGuard {
    pressed_scancodes: Vec<u16>,
}

impl KeyReleaseGuard {
    pub(super) fn new() -> Self {
        Self {
            pressed_scancodes: Vec::new(),
        }
    }

    pub(super) fn press(&mut self, qloop: &mut QuicInputLoop, scancode: u16) {
        if !self.pressed_scancodes.contains(&scancode) {
            self.pressed_scancodes.push(scancode);
        }
        qloop.push_key(scancode, true);
    }

    pub(super) fn release(&mut self, qloop: &mut QuicInputLoop, scancode: u16) {
        qloop.push_key(scancode, false);
        self.pressed_scancodes.retain(|&s| s != scancode);
    }

    pub(super) fn release_all(&mut self, qloop: &mut QuicInputLoop) {
        let pending: Vec<u16> = self.pressed_scancodes.drain(..).rev().collect();
        for sc in pending {
            qloop.push_key(sc, false);
        }
    }
}

impl Drop for KeyReleaseGuard {
    fn drop(&mut self) {
        if !self.pressed_scancodes.is_empty() {
            // 兜底经直连注入器确保全部抬起。
            if let Ok(mut inj) = rc_input_macos::Injector::for_main_display() {
                for sc in self.pressed_scancodes.drain(..).rev() {
                    if let Some(k) = key_from_set1(sc) {
                        let _ = inj.inject(rc_input_macos::InputEvent::Key {
                            key: k,
                            pressed: false,
                        });
                    }
                }
            }
        }
    }
}

pub(super) fn scancode_and_vk(dom_code: &str) -> Option<(u16, u16)> {
    let sc = scancode_for(dom_code)?;
    let key = key_from_set1(sc)?;
    let vk = virtual_keycode(key)?;
    Some((sc, vk))
}

/// 驱动 S12–S13 键盘体验测试场景。
pub(super) fn drive_keyboard_scenario(
    suite: BenchSuite,
    inj_stop: &AtomicBool,
    mouse_state: &MouseBenchState,
    kb_state: &KeyboardBenchState,
) -> Result<(), String> {
    let mut qloop = QuicInputLoop::connect()?;
    let mut guard = KeyReleaseGuard::new();
    std::thread::sleep(Duration::from_millis(120));

    match suite {
        BenchSuite::S12KeyTypingQuic => {
            // Phase A: 28 次随机相位单键按下+释放（每键按住 45ms，间隔 160..260ms）。
            let mut rng = 0x94d0_49bb_1331_11eb_u64;
            for i in 0..28usize {
                if inj_stop.load(Ordering::Relaxed) {
                    break;
                }
                let Some((sc, vk)) = scancode_and_vk(TEST_KEY_NAMES[i % TEST_KEY_NAMES.len()])
                else {
                    continue;
                };
                let emit_down_us = media_time_us();
                guard.press(&mut qloop, sc);
                std::thread::sleep(Duration::from_millis(45));
                let emit_up_us = media_time_us();
                guard.release(&mut qloop, sc);
                kb_state
                    .tap_steps
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(KeyTapEvent {
                        emit_down_us,
                        emit_up_us,
                        expected_vk: vk,
                    });
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                let gap = 155 + ((rng >> 32) % 101);
                std::thread::sleep(Duration::from_millis(gap));
            }

            // 冷却后进入 Phase B：52 次 25 keys/s（40ms 步进）高速连续打字，核查零丢键与保序。
            std::thread::sleep(Duration::from_millis(200));
            let mut sent_vks = Vec::with_capacity(52);
            for i in 0..52usize {
                if inj_stop.load(Ordering::Relaxed) {
                    break;
                }
                let Some((sc, vk)) = scancode_and_vk(TEST_KEY_NAMES[i % TEST_KEY_NAMES.len()])
                else {
                    continue;
                };
                let ts = media_time_us();
                guard.press(&mut qloop, sc);
                sent_vks.push((ts, vk));
                std::thread::sleep(Duration::from_millis(18));
                guard.release(&mut qloop, sc);
                std::thread::sleep(Duration::from_millis(22));
            }
            *kb_state
                .burst_keys_sent
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = sent_vks.len() as u64;
            *kb_state
                .burst_expected_vks
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = sent_vks;
        }
        BenchSuite::S13KeyChordsHold => {
            let (sc_shift, _) = scancode_and_vk("ShiftLeft").ok_or("缺少 ShiftLeft")?;
            let (sc_alt, _) = scancode_and_vk("AltLeft").ok_or("缺少 AltLeft")?;

            // Phase A1: 10 组 Shift + 字母组合。
            for &name in TEST_KEY_NAMES.iter().take(10) {
                if inj_stop.load(Ordering::Relaxed) {
                    break;
                }
                let Some((sc, vk)) = scancode_and_vk(name) else {
                    continue;
                };
                guard.press(&mut qloop, sc_shift);
                std::thread::sleep(Duration::from_millis(16));
                let emit_down_us = media_time_us();
                guard.press(&mut qloop, sc);
                kb_state
                    .chord_steps
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(ChordStepEvent {
                        emit_down_us,
                        expected_vk: vk,
                        required_mod_mask: 1 << 0, // Shift
                    });
                std::thread::sleep(Duration::from_millis(45));
                guard.release(&mut qloop, sc);
                std::thread::sleep(Duration::from_millis(12));
                guard.release(&mut qloop, sc_shift);
                std::thread::sleep(Duration::from_millis(180));
            }

            // Phase A2: 6 组 Option+Shift + 字母组合。
            for &name in TEST_KEY_NAMES.iter().take(16).skip(10) {
                if inj_stop.load(Ordering::Relaxed) {
                    break;
                }
                let Some((sc, vk)) = scancode_and_vk(name) else {
                    continue;
                };
                guard.press(&mut qloop, sc_alt);
                guard.press(&mut qloop, sc_shift);
                std::thread::sleep(Duration::from_millis(16));
                let emit_down_us = media_time_us();
                guard.press(&mut qloop, sc);
                kb_state
                    .chord_steps
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(ChordStepEvent {
                        emit_down_us,
                        expected_vk: vk,
                        required_mod_mask: (1 << 0) | (1 << 2), // Shift | Option
                    });
                std::thread::sleep(Duration::from_millis(45));
                guard.release(&mut qloop, sc);
                std::thread::sleep(Duration::from_millis(12));
                guard.release(&mut qloop, sc_shift);
                guard.release(&mut qloop, sc_alt);
                std::thread::sleep(Duration::from_millis(180));
            }

            // Phase B: 10 组长按 200ms 后释放，测 KeyUp 响应与粘键归零。
            let (sc_a, _) = scancode_and_vk("KeyA").ok_or("缺少 KeyA")?;
            for _ in 0..10u32 {
                if inj_stop.load(Ordering::Relaxed) {
                    break;
                }
                guard.press(&mut qloop, sc_a);
                std::thread::sleep(Duration::from_millis(200));
                let emit_up = media_time_us();
                guard.release(&mut qloop, sc_a);
                kb_state
                    .hold_up_steps
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(emit_up);
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        _ => {}
    }

    guard.release_all(&mut qloop);
    std::thread::sleep(Duration::from_millis(40));
    qloop.finish(mouse_state);
    Ok(())
}

/// 根据无损逐键 UDP 遥测 + 真实 `MTLDrawable.presentedTime` 计算 S12/S13/S16 键盘指标。
pub(super) fn populate_keyboard_report(
    obs_list: &[PresentedObservation],
    key_events: &[TargetKeyEvent],
    kb_state: &KeyboardBenchState,
    keyboard: &mut KeyboardMetrics,
    notes: &mut Vec<String>,
) {
    let taps = kb_state
        .tap_steps
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let burst_expected = kb_state
        .burst_expected_vks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let burst_sent = *kb_state
        .burst_keys_sent
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let chords = kb_state
        .chord_steps
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let hold_ups = kb_state
        .hold_up_steps
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    if !taps.is_empty() {
        let mut down_i2p = Vec::new();
        let mut up_i2p = Vec::new();
        let mut inj_app = Vec::new();
        for tap in &taps {
            let pre_kd = obs_list
                .iter()
                .rev()
                .find(|o| o.presented_us < tap.emit_down_us)
                .map_or(0, |o| o.meta.key_down_seq);
            let pre_ku = obs_list
                .iter()
                .rev()
                .find(|o| o.presented_us < tap.emit_up_us)
                .map_or(0, |o| o.meta.key_up_seq);

            if let Some(hit) = obs_list.iter().find(|o| {
                o.presented_us >= tap.emit_down_us
                    && o.presented_us - tap.emit_down_us <= 300_000
                    && o.meta.key_down_seq > pre_kd
                    && o.meta.key_recv_us.is_some_and(|kr| kr >= tap.emit_down_us)
            }) {
                down_i2p.push(hit.presented_us - tap.emit_down_us);
            }
            if let Some(hit_up) = obs_list.iter().find(|o| {
                o.presented_us >= tap.emit_up_us
                    && o.presented_us - tap.emit_up_us <= 300_000
                    && o.meta.key_up_seq > pre_ku
                    && o.meta.key_recv_us.is_some_and(|kr| kr >= tap.emit_up_us)
            }) {
                up_i2p.push(hit_up.presented_us - tap.emit_up_us);
            }
            if let Some(ev) = key_events.iter().find(|e| {
                e.down
                    && e.keycode == tap.expected_vk
                    && e.recv_us >= tap.emit_down_us
                    && e.recv_us - tap.emit_down_us <= 100_000
            }) {
                inj_app.push(ev.recv_us - tap.emit_down_us);
            }
        }
        if !down_i2p.is_empty() {
            keyboard.key_i2p_ms = Some(DistStats::from_us(&down_i2p));
        } else {
            notes.push("键盘测试未匹配到 KeyDown I2P 样本（请检查辅助功能与输入监控权限）".into());
        }
        if !up_i2p.is_empty() {
            keyboard.key_up_i2p_ms = Some(DistStats::from_us(&up_i2p));
        }
        if !inj_app.is_empty() {
            keyboard.key_inject_to_app_ms = Some(DistStats::from_us(&inj_app));
        }
    }

    if burst_sent > 0 {
        let start_us = burst_expected.first().map_or(0, |x| x.0);
        let received: Vec<u16> = key_events
            .iter()
            .filter(|e| e.down && e.recv_us >= start_us)
            .map(|e| e.keycode)
            .collect();
        // 按发送序列依次推进匹配，统计保序命中数与乱序次数。
        let mut cursor = 0usize;
        let mut delivered = 0u64;
        let mut out_of_order = 0u64;
        for &(_, want_vk) in &burst_expected {
            if let Some(offset) = received[cursor..].iter().position(|&got| got == want_vk) {
                if offset > 0 {
                    out_of_order += 1;
                }
                cursor += offset + 1;
                delivered += 1;
            }
        }
        keyboard.keys_sent = Some(burst_sent);
        keyboard.keys_delivered = Some(delivered);
        let drop_pct = ((burst_sent.saturating_sub(delivered)) as f64 / burst_sent as f64) * 100.0;
        keyboard.key_drop_rate_pct = Some(drop_pct);
        keyboard.key_out_of_order_count = Some(out_of_order);
    }

    if !chords.is_empty() {
        let mut ok_count = 0u64;
        for ch in &chords {
            let matched = key_events.iter().any(|e| {
                e.down
                    && e.keycode == ch.expected_vk
                    && e.recv_us >= ch.emit_down_us
                    && e.recv_us - ch.emit_down_us <= 120_000
                    && (e.mods & ch.required_mod_mask) == ch.required_mod_mask
            });
            if matched {
                ok_count += 1;
            }
        }
        keyboard.chord_attempts = Some(chords.len() as u64);
        keyboard.modifier_sync_accuracy_pct = Some((ok_count as f64 / chords.len() as f64) * 100.0);
    }

    if !hold_ups.is_empty() {
        let mut up_i2p = Vec::new();
        for &emit_up in &hold_ups {
            let pre_ku = obs_list
                .iter()
                .rev()
                .find(|o| o.presented_us < emit_up)
                .map_or(0, |o| o.meta.key_up_seq);
            if let Some(hit) = obs_list.iter().find(|o| {
                o.presented_us >= emit_up
                    && o.presented_us - emit_up <= 300_000
                    && o.meta.key_up_seq > pre_ku
            }) {
                up_i2p.push(hit.presented_us - emit_up);
            }
        }
        if !up_i2p.is_empty() && keyboard.key_up_i2p_ms.is_none() {
            keyboard.key_up_i2p_ms = Some(DistStats::from_us(&up_i2p));
        }
    }

    if !taps.is_empty() || !chords.is_empty() || !hold_ups.is_empty() {
        let last_meta = obs_list.last().map(|o| o.meta);
        let stuck = last_meta.map_or(0, |m| u64::from(m.key_is_down || m.last_key_mods != 0));
        if let Some(m) = last_meta.filter(|_| stuck > 0) {
            notes.push(format!(
                "键盘释放后残留状态：key_is_down={}, last_key_mods={:#x}, kd_seq={}, ku_seq={}",
                m.key_is_down, m.last_key_mods, m.key_down_seq, m.key_up_seq
            ));
        }
        keyboard.stuck_keys_after_release = Some(stuck);
    }
}

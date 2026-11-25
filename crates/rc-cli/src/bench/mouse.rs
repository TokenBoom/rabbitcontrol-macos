//! 鼠标体验全链路量化基准模块（仅 `bench-metrics` 启用，零 `unsafe`）。
//!
//! 覆盖 S7–S11 五个鼠标专项场景：
//! - `S7-mouse-click-drag`：左右键点击/双击识别 + 按住左键拖拽（带 RAII 抬键兜底）
//! - `S8-mouse-wheel`：阶跃滚轮 I2P + 60Hz 连续滚轮事件完整交付率
//! - `S9-mouse-quic-e2e`：完整 `stage -> rc-client-core -> InputBatcher(4ms) -> QUIC urgent -> InputBridge -> CGEventPost`
//! - `S10-mouse-polling-1khz`：1000Hz 高回报率鼠标压测（`InputBatcher` 合并率 / 队列开销 / 帧节拍稳态）
//! - `S11-mouse-cursor-sync`：2400 px/s 甩鼠 / 150 px/s 慢速精瞄与急停收敛 + 落点坐标精度

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rc_client_core::input_bridge::{fit_video_rect, normalize_pointer_in_video};
use rc_input::channel::{inject_message, send_events};
use rc_input::{InputBatcher, InputEvent as WireInputEvent};
use rc_input_macos::{Injector, InputBridge, InputEvent, PointerButtonKind};
use rc_net::quic::{Identity, QuicEndpoint, QuicTransport};
use rc_net::{NetConfig, NetEvent, Role, Transport};
use rc_render_macos::media_time_us;
use rc_session::dev_cert::{dev_identity, DEV_CERT_PEM, DEV_SERVER_NAME};
use rc_types::Resolution;

use super::runner::{interpolate_track, BenchSuite, PresentedObservation, StepEvent};
use super::stats::{DistStats, MouseMetrics, SnappinessSection};

/// 虚拟主控端参考舞台尺寸（对齐 `session_flow_macos.rs` 默认舞台）。
pub(super) const STAGE_W: f64 = 1280.0;
pub(super) const STAGE_H: f64 = 752.0;

/// 点击/双击采样意图。
#[derive(Debug, Clone, Copy)]
pub(super) struct ClickStepEvent {
    pub emit_us: u64,
    pub expect_double: bool,
}

/// 滚轮阶跃采样意图。
#[derive(Debug, Clone, Copy)]
pub(super) struct WheelStepEvent {
    pub emit_us: u64,
}

/// 急停落点收敛与静态落点精度目标。
#[derive(Debug, Clone, Copy)]
pub(super) struct StopTargetEvent {
    pub stop_us: u64,
    pub hold_end_us: u64,
    pub expected_x: f64,
    pub expected_y: f64,
}

/// 鼠标专项采集共享上下文。
#[derive(Default)]
pub(super) struct MouseBenchState {
    pub click_steps: Mutex<Vec<ClickStepEvent>>,
    pub wheel_steps: Mutex<Vec<WheelStepEvent>>,
    pub wheel_sent_count: Mutex<u64>,
    pub hold_drag_pts: Mutex<Vec<(u64, f64, f64)>>,
    pub fast_sync_pts: Mutex<Vec<(u64, f64, f64)>>,
    pub slow_sync_pts: Mutex<Vec<(u64, f64, f64)>>,
    pub stop_targets: Mutex<Vec<StopTargetEvent>>,
    pub quic_oneway_us: Mutex<Vec<u64>>,
    pub injected_events: Mutex<u64>,
    pub batches_sent: Mutex<u64>,
    pub events_after_coalesce: Mutex<u64>,
}

/// RAII 左键释放守卫：确保 `S7` 在正常结束或中途退出时必发 `LeftUp`，防止系统光标卡在按住状态。
struct LeftButtonGuard<'a> {
    injector: &'a mut Injector,
    x: f64,
    y: f64,
    pressed: bool,
}

impl<'a> LeftButtonGuard<'a> {
    fn press(injector: &'a mut Injector, x: f64, y: f64) -> Self {
        let _ = injector.inject(InputEvent::PointerButton {
            button: PointerButtonKind::Left,
            pressed: true,
            x,
            y,
        });
        Self {
            injector,
            x,
            y,
            pressed: true,
        }
    }

    fn update_pos(&mut self, x: f64, y: f64) {
        self.x = x;
        self.y = y;
    }

    fn release(&mut self) {
        if self.pressed {
            let _ = self.injector.inject(InputEvent::PointerButton {
                button: PointerButtonKind::Left,
                pressed: false,
                x: self.x,
                y: self.y,
            });
            self.pressed = false;
        }
    }
}

impl Drop for LeftButtonGuard<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

/// 本机回环真实 QUIC 传输 + `InputBatcher(4ms)` + `InputBridge::for_main_display()` 输入链路。
pub(super) struct QuicInputLoop {
    _host_ep: QuicEndpoint,
    _cli_ep: QuicEndpoint,
    client: QuicTransport,
    batcher: InputBatcher,
    sent_ts: Arc<Mutex<VecDeque<u64>>>,
    oneway_us: Arc<Mutex<Vec<u64>>>,
    stop: Arc<AtomicBool>,
    host_thread: Option<JoinHandle<()>>,
    pub injected_events: u64,
    pub batches_sent: u64,
    pub events_after_coalesce: u64,
}

impl QuicInputLoop {
    pub(super) fn connect() -> Result<Self, String> {
        // 先验证辅助功能权限并创建宿主机主屏 InputBridge，再在子线程持桥接器消费。
        let mut bridge = InputBridge::for_main_display()
            .map_err(|e| format!("初始化主屏 InputBridge 失败（需辅助功能权限）：{e}"))?;
        let identity = dev_identity().map_err(|e| format!("加载开发 QUIC 证书失败：{e}"))?;
        let certs =
            Identity::certs_from_pem(DEV_CERT_PEM).map_err(|e| format!("解析信任证书失败：{e}"))?;

        let host_net = NetConfig::new(
            Role::Host,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        );
        let host_ep = QuicEndpoint::new(&host_net, &identity)
            .map_err(|e| format!("建 QUIC Host 端点失败：{e}"))?;
        let peer_addr = host_ep
            .local_addr()
            .map_err(|e| format!("读取 Host QUIC 端口失败：{e}"))?;

        let mut cli_net = NetConfig::new(
            Role::Controller,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        );
        cli_net.peer_addr = Some(peer_addr);
        let cli_ep = QuicEndpoint::new(&cli_net, &identity)
            .map_err(|e| format!("建 QUIC Client 端点失败：{e}"))?;

        let (tx_cli, rx_cli) =
            std::sync::mpsc::channel::<(QuicEndpoint, Result<QuicTransport, String>)>();
        std::thread::spawn(move || {
            let res = cli_ep
                .connect(peer_addr, &certs, DEV_SERVER_NAME, Duration::from_secs(5))
                .map_err(|e| format!("QUIC connect 失败：{e}"));
            let _ = tx_cli.send((cli_ep, res));
        });

        let mut server = host_ep
            .accept(Duration::from_secs(5))
            .map_err(|e| format!("QUIC accept 失败：{e}"))?;
        let (cli_ep, cli_res) = rx_cli
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "等待 QUIC connect 超时".to_string())?;
        let mut client = cli_res?;

        // 预热输入 urgent 流，避免首个控制事件撞上懒打开流建连抖动。
        let _ = send_events(&mut client, &[WireInputEvent::Sync]);
        let warm_deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < warm_deadline {
            if let Ok(evs) = server.poll(Duration::from_millis(5)) {
                if evs
                    .iter()
                    .any(|e| matches!(e, NetEvent::ReliableUrgent(_) | NetEvent::Reliable(_)))
                {
                    break;
                }
            }
        }

        let sent_ts = Arc::new(Mutex::new(VecDeque::<u64>::new()));
        let oneway_us = Arc::new(Mutex::new(Vec::<u64>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let sent_rx = Arc::clone(&sent_ts);
        let oneway_rx = Arc::clone(&oneway_us);
        let stop_rx = Arc::clone(&stop);

        let host_thread = std::thread::Builder::new()
            .name("rc-bench-quic-input-rx".into())
            .spawn(move || {
                while !stop_rx.load(Ordering::Relaxed) {
                    match server.poll(Duration::from_millis(2)) {
                        Ok(events) => {
                            for ev in events {
                                match ev {
                                    NetEvent::ReliableUrgent(msg) | NetEvent::Reliable(msg) => {
                                        let t_send = sent_rx
                                            .lock()
                                            .unwrap_or_else(|e| e.into_inner())
                                            .pop_front();
                                        let _ = inject_message(&msg, &mut bridge);
                                        if let Some(ts) = t_send {
                                            let dt = media_time_us().saturating_sub(ts);
                                            oneway_rx
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .push(dt);
                                        }
                                    }
                                    NetEvent::Disconnected { .. } => return,
                                    _ => {}
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .map_err(|e| format!("启动 QUIC 输入接收线程失败：{e}"))?;

        Ok(Self {
            _host_ep: host_ep,
            _cli_ep: cli_ep,
            client,
            batcher: InputBatcher::new(rc_input::DEFAULT_WINDOW),
            sent_ts,
            oneway_us,
            stop,
            host_thread: Some(host_thread),
            injected_events: 0,
            batches_sent: 0,
            events_after_coalesce: 0,
        })
    }

    pub(super) fn push_move(&mut self, nx: u16, ny: u16) {
        self.injected_events += 1;
        self.batcher
            .push(WireInputEvent::MouseMove { x: nx, y: ny }, Instant::now());
        self.poll_and_send();
    }

    pub(super) fn push_key(&mut self, scancode: u16, pressed: bool) {
        self.injected_events += 1;
        self.batcher.push(
            WireInputEvent::Key {
                scancode,
                pressed,
                modifiers: 0,
            },
            Instant::now(),
        );
        self.poll_and_send();
    }

    pub(super) fn push_scroll(&mut self, delta: i16, horizontal: bool) {
        self.injected_events += 1;
        self.batcher.push(
            WireInputEvent::MouseWheel { delta, horizontal },
            Instant::now(),
        );
        self.poll_and_send();
    }

    pub(super) fn poll_and_send(&mut self) {
        let batch = self.batcher.poll(Instant::now());
        if !batch.is_empty() {
            self.send_batch(&batch);
        }
    }

    pub(super) fn flush_and_send(&mut self) {
        let batch = self.batcher.flush();
        if !batch.is_empty() {
            self.send_batch(&batch);
        }
    }

    fn send_batch(&mut self, batch: &[WireInputEvent]) {
        self.batches_sent += 1;
        self.events_after_coalesce += batch.len() as u64;
        self.sent_ts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(media_time_us());
        let _ = send_events(&mut self.client, batch);
    }

    pub(super) fn finish(mut self, state: &MouseBenchState) {
        self.flush_and_send();
        std::thread::sleep(Duration::from_millis(15));
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.host_thread.take() {
            let _ = h.join();
        }
        let samples = self
            .oneway_us
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        *state
            .quic_oneway_us
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = samples;
        *state
            .injected_events
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = self.injected_events;
        *state.batches_sent.lock().unwrap_or_else(|e| e.into_inner()) = self.batches_sent;
        *state
            .events_after_coalesce
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = self.events_after_coalesce;
    }
}

impl Drop for QuicInputLoop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.host_thread.take() {
            let _ = h.join();
        }
    }
}

pub(super) fn map_stage_point(res: Resolution, sx: f64, sy: f64) -> (u16, u16, f64, f64) {
    let (nx, ny) =
        normalize_pointer_in_video(sx, sy, STAGE_W, STAGE_H, Some((res.width, res.height)));
    let ex = (f64::from(nx) / 65535.0) * f64::from(res.width.saturating_sub(1));
    let ey = (f64::from(ny) / 65535.0) * f64::from(res.height.saturating_sub(1));
    (nx, ny, ex, ey)
}

pub(super) fn stage_point_from_host(res: Resolution, hx: f64, hy: f64) -> (f64, f64) {
    let (ox, oy, fw, fh) = fit_video_rect(STAGE_W, STAGE_H, res.width, res.height);
    let rx = (hx / f64::from(res.width.saturating_sub(1).max(1))).clamp(0.0, 1.0);
    let ry = (hy / f64::from(res.height.saturating_sub(1).max(1))).clamp(0.0, 1.0);
    (ox + rx * fw, oy + ry * fh)
}

/// 驱动 S7–S11 鼠标体验激励线程（运行在 `std::thread::scope` 内）。
pub(super) fn drive_mouse_scenario(
    suite: BenchSuite,
    res: Resolution,
    duration: Duration,
    inj_stop: &AtomicBool,
    step_tx: &std::sync::mpsc::Sender<StepEvent>,
    drag_points: &Mutex<Vec<(u64, f64, f64)>>,
    mouse_state: &MouseBenchState,
) -> Result<(), String> {
    let cx = f64::from(res.width) * 0.5;
    let cy = f64::from(res.height) * 0.5;

    match suite {
        BenchSuite::S7MouseClickDrag => {
            let mut injector = Injector::for_main_display()
                .map_err(|e| format!("初始化 S7 输入注入器失败：{e}"))?;
            let _ = injector.inject(InputEvent::PointerMove { x: cx, y: cy });
            std::thread::sleep(Duration::from_millis(180));

            // Phase A1: 20 次离散左右摆动单击（>5px 偏移保证每次独立识别为单次点击）。
            for i in 0..20u32 {
                if inj_stop.load(Ordering::Relaxed) {
                    break;
                }
                let tx = if i % 2 == 0 { cx - 120.0 } else { cx + 120.0 };
                let ty = if i % 2 == 0 { cy - 60.0 } else { cy + 60.0 };
                let _ = injector.inject(InputEvent::PointerMove { x: tx, y: ty });
                std::thread::sleep(Duration::from_millis(35));
                let emit_us = media_time_us();
                let _ = injector.inject(InputEvent::PointerButton {
                    button: PointerButtonKind::Left,
                    pressed: true,
                    x: tx,
                    y: ty,
                });
                mouse_state
                    .click_steps
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(ClickStepEvent {
                        emit_us,
                        expect_double: false,
                    });
                std::thread::sleep(Duration::from_millis(35));
                let _ = injector.inject(InputEvent::PointerButton {
                    button: PointerButtonKind::Left,
                    pressed: false,
                    x: tx,
                    y: ty,
                });
                std::thread::sleep(Duration::from_millis(140));
            }

            // Phase A2: 8 次固定坐标双击对（550ms 冷却复位 ClickTracker，80ms 内连击第 2 下）。
            let _ = injector.inject(InputEvent::PointerMove { x: cx, y: cy });
            for _ in 0..8u32 {
                if inj_stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(560));
                if inj_stop.load(Ordering::Relaxed) {
                    break;
                }
                let _ = injector.inject(InputEvent::PointerButton {
                    button: PointerButtonKind::Left,
                    pressed: true,
                    x: cx,
                    y: cy,
                });
                std::thread::sleep(Duration::from_millis(25));
                let _ = injector.inject(InputEvent::PointerButton {
                    button: PointerButtonKind::Left,
                    pressed: false,
                    x: cx,
                    y: cy,
                });
                std::thread::sleep(Duration::from_millis(60));
                let emit2_us = media_time_us();
                let _ = injector.inject(InputEvent::PointerButton {
                    button: PointerButtonKind::Left,
                    pressed: true,
                    x: cx,
                    y: cy,
                });
                mouse_state
                    .click_steps
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(ClickStepEvent {
                        emit_us: emit2_us,
                        expect_double: true,
                    });
                std::thread::sleep(Duration::from_millis(35));
                let _ = injector.inject(InputEvent::PointerButton {
                    button: PointerButtonKind::Left,
                    pressed: false,
                    x: cx,
                    y: cy,
                });
            }

            // Phase B: 按住左键 1200 px/s 连续画圆拖拽，结束/异常时 RAII 自动松键。
            if !inj_stop.load(Ordering::Relaxed) {
                let radius = 160.0f64;
                let omega = 1200.0 / radius;
                let _ = injector.inject(InputEvent::PointerMove {
                    x: cx + radius,
                    y: cy,
                });
                std::thread::sleep(Duration::from_millis(50));
                let mut guard = LeftButtonGuard::press(&mut injector, cx + radius, cy);
                std::thread::sleep(Duration::from_millis(40));
                let start = Instant::now();
                let tick = Duration::from_micros(8_333);
                let mut step = 0u64;
                while !inj_stop.load(Ordering::Relaxed) {
                    let angle = omega * start.elapsed().as_secs_f64();
                    let x = cx + radius * angle.cos();
                    let y = cy + radius * angle.sin();
                    guard.update_pos(x, y);
                    let t_us = media_time_us();
                    let _ = guard.injector.inject(InputEvent::PointerMove { x, y });
                    {
                        let mut pts = mouse_state
                            .hold_drag_pts
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if pts.len() >= 4096 {
                            pts.remove(0);
                        }
                        pts.push((t_us, x, y));
                    }
                    step += 1;
                    let next = start + tick * (step as u32);
                    if let Some(d) = next.checked_duration_since(Instant::now()) {
                        std::thread::sleep(d);
                    }
                }
                guard.release();
            }
        }
        BenchSuite::S8MouseWheel => {
            let mut injector = Injector::for_main_display()
                .map_err(|e| format!("初始化 S8 输入注入器失败：{e}"))?;
            let _ = injector.inject(InputEvent::PointerMove { x: cx, y: cy });
            std::thread::sleep(Duration::from_millis(180));

            let mut total_scrolls = 0u64;
            // Phase A: 18 次随机相位阶跃滚轮脉冲。
            let mut rng = 0x517c_c1b7_2722_0a95u64;
            for i in 0..18u32 {
                if inj_stop.load(Ordering::Relaxed) {
                    break;
                }
                let dy = if i % 2 == 0 { 3.0 } else { -3.0 };
                let emit_us = media_time_us();
                let _ = injector.inject(InputEvent::PointerScroll { dx: 0.0, dy });
                total_scrolls += 1;
                mouse_state
                    .wheel_steps
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(WheelStepEvent { emit_us });
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                let gap = 180 + ((rng >> 32) % 121);
                std::thread::sleep(Duration::from_millis(gap));
            }

            // Phase B: 60Hz 连续滚轮流（每 tick 2 行，交替上下）。
            let start = Instant::now();
            let tick = Duration::from_micros(16_667);
            let mut step = 0u64;
            while !inj_stop.load(Ordering::Relaxed) {
                let dy = if (step / 90) % 2 == 0 { 2.0 } else { -2.0 };
                let _ = injector.inject(InputEvent::PointerScroll { dx: 0.0, dy });
                total_scrolls += 1;
                step += 1;
                let next = start + tick * (step as u32);
                if let Some(d) = next.checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
            }
            *mouse_state
                .wheel_sent_count
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = total_scrolls;
        }
        BenchSuite::S9MouseQuicE2e => {
            let mut qloop = QuicInputLoop::connect()?;
            let (ox, oy, fw, fh) = fit_video_rect(STAGE_W, STAGE_H, res.width, res.height);
            let mut stops = Vec::new();

            // 归位。
            let (nx0, ny0, _, _) = map_stage_point(res, ox + fw * 0.2, oy + fh * 0.5);
            qloop.push_move(nx0, ny0);
            std::thread::sleep(Duration::from_millis(8));
            qloop.poll_and_send();
            std::thread::sleep(Duration::from_millis(180));

            // Phase A (~42% 时长): 随机相位跨舞台阶跃跳点，同时测 I2P、QUIC 单程与坐标落点精度。
            let phase_a_end = Instant::now() + duration.mul_f64(0.42);
            let mut rng = 0xd1b5_4a32_d192_ed03u64;
            let pts_norm = [
                (0.20, 0.25),
                (0.80, 0.75),
                (0.75, 0.22),
                (0.25, 0.78),
                (0.50, 0.50),
            ];
            let mut idx = 0usize;
            while Instant::now() < phase_a_end && !inj_stop.load(Ordering::Relaxed) {
                let (rx, ry) = pts_norm[idx % pts_norm.len()];
                idx += 1;
                let (nx, ny, ex, ey) = map_stage_point(res, ox + rx * fw, oy + ry * fh);
                let emit_us = media_time_us();
                qloop.push_move(nx, ny);
                let _ = step_tx.send(StepEvent {
                    emit_us,
                    target_x: ex.round() as u16,
                    target_y: ey.round() as u16,
                });
                // 在 1ms 循环中等待 ~190ms（确保 4ms InputBatcher 按真实周期准时冲刷）。
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                let dwell_ms = 180 + ((rng >> 32) % 101);
                let step_deadline = Instant::now() + Duration::from_millis(dwell_ms);
                while Instant::now() < step_deadline && !inj_stop.load(Ordering::Relaxed) {
                    qloop.poll_and_send();
                    std::thread::sleep(Duration::from_millis(1));
                }
                let hold_end_us = media_time_us();
                stops.push(StopTargetEvent {
                    stop_us: emit_us,
                    hold_end_us,
                    expected_x: ex,
                    expected_y: ey,
                });
            }

            // Phase B: 经完整 QUIC 链路 1200 px/s 圆周连续拖动。
            let radius = 160.0f64;
            let omega = 1200.0 / radius;
            let start = Instant::now();
            let tick = Duration::from_micros(4_000); // 250Hz 驱动进 4ms 批处理窗
            let mut step = 0u64;
            while !inj_stop.load(Ordering::Relaxed) {
                let angle = omega * start.elapsed().as_secs_f64();
                let hx = cx + radius * angle.cos();
                let hy = cy + radius * angle.sin();
                let (sx, sy) = stage_point_from_host(res, hx, hy);
                let (nx, ny, ex, ey) = map_stage_point(res, sx, sy);
                let t_us = media_time_us();
                qloop.push_move(nx, ny);
                {
                    let mut pts = drag_points.lock().unwrap_or_else(|e| e.into_inner());
                    if pts.len() >= 8192 {
                        pts.remove(0);
                    }
                    pts.push((t_us, ex, ey));
                }
                step += 1;
                let next = start + tick * (step as u32);
                if let Some(d) = next.checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
            }
            *mouse_state
                .stop_targets
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = stops;
            qloop.finish(mouse_state);
        }
        BenchSuite::S10MousePolling1khz => {
            let mut qloop = QuicInputLoop::connect()?;
            let radius = 160.0f64;
            let omega = 1200.0 / radius;
            let start = Instant::now();
            let tick = Duration::from_micros(1_000); // 严格 1000 Hz（每 1ms 发送一笔事件）
            let mut step = 0u64;
            while !inj_stop.load(Ordering::Relaxed) {
                let angle = omega * start.elapsed().as_secs_f64();
                let hx = cx + radius * angle.cos();
                let hy = cy + radius * angle.sin();
                let (sx, sy) = stage_point_from_host(res, hx, hy);
                let (nx, ny, ex, ey) = map_stage_point(res, sx, sy);
                let t_us = media_time_us();
                qloop.push_move(nx, ny);
                {
                    let mut pts = drag_points.lock().unwrap_or_else(|e| e.into_inner());
                    if pts.len() >= 16384 {
                        pts.remove(0);
                    }
                    pts.push((t_us, ex, ey));
                }
                step += 1;
                let next = start + tick * (step as u32);
                if let Some(d) = next.checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
            }
            qloop.finish(mouse_state);
        }
        BenchSuite::S11MouseCursorSync => {
            let mut qloop = QuicInputLoop::connect()?;
            let (ox, oy, fw, fh) = fit_video_rect(STAGE_W, STAGE_H, res.width, res.height);

            // Sub-phase 1 (~25%): 2400 px/s 高速甩鼠（测端侧局部预测光标与视频光标最大分裂距离）。
            let p1_end = Instant::now() + duration.mul_f64(0.26);
            let r_fast = 180.0f64;
            let omega_fast = 2400.0 / r_fast;
            let s1 = Instant::now();
            let tick = Duration::from_micros(4_000);
            let mut k1 = 0u64;
            while Instant::now() < p1_end && !inj_stop.load(Ordering::Relaxed) {
                let a = omega_fast * s1.elapsed().as_secs_f64();
                let (sx, sy) =
                    stage_point_from_host(res, cx + r_fast * a.cos(), cy + r_fast * a.sin());
                let (nx, ny, ex, ey) = map_stage_point(res, sx, sy);
                let t_us = media_time_us();
                qloop.push_move(nx, ny);
                mouse_state
                    .fast_sync_pts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((t_us, ex, ey));
                k1 += 1;
                let next = s1 + tick * (k1 as u32);
                if let Some(d) = next.checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
            }

            // Sub-phase 2 (~25%): 150 px/s 慢速精瞄（测低速微操跟手分裂）。
            let p2_end = Instant::now() + duration.mul_f64(0.26);
            let r_slow = 120.0f64;
            let omega_slow = 150.0 / r_slow;
            let s2 = Instant::now();
            let mut k2 = 0u64;
            while Instant::now() < p2_end && !inj_stop.load(Ordering::Relaxed) {
                let a = omega_slow * s2.elapsed().as_secs_f64();
                let (sx, sy) =
                    stage_point_from_host(res, cx + r_slow * a.cos(), cy + r_slow * a.sin());
                let (nx, ny, ex, ey) = map_stage_point(res, sx, sy);
                let t_us = media_time_us();
                qloop.push_move(nx, ny);
                mouse_state
                    .slow_sync_pts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((t_us, ex, ey));
                k2 += 1;
                let next = s2 + tick * (k2 as u32);
                if let Some(d) = next.checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
            }

            // Sub-phase 3 (~48%): 全屏 16 组「急停落点收敛 + 精度」测量。
            let anchors = [
                (0.18, 0.20),
                (0.82, 0.22),
                (0.80, 0.80),
                (0.20, 0.78),
                (0.50, 0.50),
                (0.32, 0.36),
                (0.68, 0.64),
                (0.65, 0.28),
            ];
            let mut stops = Vec::new();
            let mut cur_x = ox + 0.5 * fw;
            let mut cur_y = oy + 0.5 * fh;
            let mut round = 0usize;
            while !inj_stop.load(Ordering::Relaxed) {
                let (rx, ry) = anchors[round % anchors.len()];
                round += 1;
                let dst_x = ox + rx * fw;
                let dst_y = oy + ry * fh;
                // 100ms 快速划过去（12 步）然后急停保持 240ms；若途中停机信号到达则丢弃未完成轮次，避免半途目标污染落点精度。
                let mut sweep_completed = true;
                for s in 1..=12u32 {
                    if inj_stop.load(Ordering::Relaxed) {
                        sweep_completed = false;
                        break;
                    }
                    let f = f64::from(s) / 12.0;
                    let sx = cur_x + (dst_x - cur_x) * f;
                    let sy = cur_y + (dst_y - cur_y) * f;
                    let (nx, ny, _, _) = map_stage_point(res, sx, sy);
                    qloop.push_move(nx, ny);
                    std::thread::sleep(Duration::from_millis(8));
                }
                if !sweep_completed {
                    break;
                }
                qloop.flush_and_send();
                let (_, _, ex, ey) = map_stage_point(res, dst_x, dst_y);
                let stop_us = media_time_us();
                std::thread::sleep(Duration::from_millis(240));
                let hold_end_us = media_time_us();
                stops.push(StopTargetEvent {
                    stop_us,
                    hold_end_us,
                    expected_x: ex,
                    expected_y: ey,
                });
                cur_x = dst_x;
                cur_y = dst_y;
            }
            *mouse_state
                .stop_targets
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = stops;
            qloop.finish(mouse_state);
        }
        _ => {}
    }

    Ok(())
}

/// 填充 S7–S11 的 `MouseMetrics` 与关联的 `SnappinessSection` 字段。
pub(super) fn populate_mouse_report(
    suite: BenchSuite,
    obs_list: &[PresentedObservation],
    drag_pts: &[(u64, f64, f64)],
    mouse_state: &MouseBenchState,
    snappiness: &mut SnappinessSection,
    mouse: &mut MouseMetrics,
    notes: &mut Vec<String>,
) {
    let quic_oneway = mouse_state
        .quic_oneway_us
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if !quic_oneway.is_empty() {
        mouse.quic_input_oneway_ms = Some(DistStats::from_us(&quic_oneway));
    }
    let inj_total = *mouse_state
        .injected_events
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let batches_total = *mouse_state
        .batches_sent
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let after_coal = *mouse_state
        .events_after_coalesce
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if inj_total > 0 {
        mouse.injected_events = Some(inj_total);
        mouse.batches_sent = Some(batches_total);
        mouse.events_after_coalesce = Some(after_coal);
        let pct = (1.0 - (after_coal as f64 / inj_total as f64)).clamp(0.0, 1.0) * 100.0;
        mouse.coalesce_ratio_pct = Some(pct);
    }

    match suite {
        BenchSuite::S7MouseClickDrag => {
            let click_steps = mouse_state
                .click_steps
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let mut click_i2p = Vec::new();
            let mut dbl_total = 0u64;
            let mut dbl_hit = 0u64;
            for step in &click_steps {
                let pre_seq = obs_list
                    .iter()
                    .rev()
                    .find(|o| o.presented_us < step.emit_us)
                    .map_or(0, |o| o.meta.click_down_seq);
                let mut matched_i2p = false;
                let mut hit_double = false;
                for o in obs_list {
                    if o.presented_us < step.emit_us {
                        continue;
                    }
                    if o.presented_us - step.emit_us > 300_000 {
                        break;
                    }
                    let ev_recv_ok = o.meta.event_recv_us.is_some_and(|er| er >= step.emit_us);
                    if !matched_i2p && ev_recv_ok && o.meta.click_down_seq > pre_seq {
                        click_i2p.push(o.presented_us - step.emit_us);
                        matched_i2p = true;
                    }
                    if step.expect_double
                        && o.meta.click_down_seq > pre_seq
                        && o.meta.click_count >= 2
                    {
                        hit_double = true;
                    }
                }
                if step.expect_double {
                    dbl_total += 1;
                    if hit_double {
                        dbl_hit += 1;
                    }
                }
            }
            if !click_i2p.is_empty() {
                mouse.click_i2p_ms = Some(DistStats::from_us(&click_i2p));
            } else {
                notes.push("S7 未匹配到有效左键点击 I2P 样本".into());
            }
            if dbl_total > 0 {
                mouse.double_click_recognized_pct =
                    Some((dbl_hit as f64 / dbl_total as f64) * 100.0);
            }

            let hold_pts = mouse_state
                .hold_drag_pts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some((d_px, d_ms, jit)) = compute_track_lag(obs_list, &hold_pts, 1200.0) {
                mouse.hold_drag_spatial_lag_px = Some(d_px);
                mouse.hold_drag_equiv_lag_ms = Some(d_ms);
                mouse.hold_drag_lag_jitter_ms = Some(jit);
            } else {
                notes.push("S7 未收集到按住拖拽连续轨迹样本".into());
            }
        }
        BenchSuite::S8MouseWheel => {
            let wheel_steps = mouse_state
                .wheel_steps
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let mut wheel_i2p = Vec::new();
            let mut wheel_inj_app = Vec::new();
            for step in &wheel_steps {
                let pre_seq = obs_list
                    .iter()
                    .rev()
                    .find(|o| o.presented_us < step.emit_us)
                    .map_or(0, |o| o.meta.scroll_seq);
                for o in obs_list {
                    if o.presented_us < step.emit_us {
                        continue;
                    }
                    if o.presented_us - step.emit_us > 300_000 {
                        break;
                    }
                    let ev_recv_ok = o.meta.event_recv_us.is_some_and(|er| er >= step.emit_us);
                    if ev_recv_ok && o.meta.scroll_seq > pre_seq {
                        wheel_i2p.push(o.presented_us - step.emit_us);
                        if let Some(er) = o.meta.event_recv_us {
                            wheel_inj_app.push(er.saturating_sub(step.emit_us));
                        }
                        break;
                    }
                }
            }
            if !wheel_i2p.is_empty() {
                mouse.wheel_i2p_ms = Some(DistStats::from_us(&wheel_i2p));
            } else {
                notes.push("S8 未匹配到有效滚轮阶跃 I2P 样本".into());
            }
            if !wheel_inj_app.is_empty() {
                mouse.wheel_inject_to_app_ms = Some(DistStats::from_us(&wheel_inj_app));
            }
            let sent = *mouse_state
                .wheel_sent_count
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if sent > 0 {
                let min_seq = obs_list.first().map_or(0, |o| o.meta.scroll_seq);
                let max_seq = obs_list
                    .iter()
                    .map(|o| o.meta.scroll_seq)
                    .max()
                    .unwrap_or(0);
                let delivered = max_seq.saturating_sub(min_seq);
                mouse.wheel_delivered_ratio_pct =
                    Some(((f64::from(delivered) / sent as f64) * 100.0).min(100.0));
            }
        }
        BenchSuite::S9MouseQuicE2e => {
            if let Some((d_px, d_ms, jit)) = compute_track_lag(obs_list, drag_pts, 1200.0) {
                snappiness.lag_jitter_ms = Some(jit);
                snappiness.drag_spatial_lag_px = Some(d_px);
                snappiness.drag_equiv_lag_ms = Some(d_ms);
            }
            let stops = mouse_state
                .stop_targets
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let (_, errs) = compute_settle_and_accuracy(obs_list, &stops);
            if !errs.is_empty() {
                mouse.pointer_accuracy_err_px = Some(DistStats::from_ms(&errs));
            }
        }
        BenchSuite::S10MousePolling1khz | BenchSuite::S16MixedWorkflowTtff => {
            if let Some((d_px, d_ms, jit)) = compute_track_lag(obs_list, drag_pts, 1200.0) {
                snappiness.lag_jitter_ms = Some(jit);
                snappiness.drag_spatial_lag_px = Some(d_px);
                snappiness.drag_equiv_lag_ms = Some(d_ms);
            } else {
                notes.push("未收集到并发连续拖动轨迹样本".into());
            }
        }
        BenchSuite::S11MouseCursorSync => {
            let fast_pts = mouse_state
                .fast_sync_pts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let slow_pts = mouse_state
                .slow_sync_pts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let stops = mouse_state
                .stop_targets
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some((d_fast, _, _)) = compute_track_lag(obs_list, &fast_pts, 2400.0) {
                mouse.cursor_split_fast_px = Some(d_fast);
            }
            if let Some((d_slow, _, _)) = compute_track_lag(obs_list, &slow_pts, 150.0) {
                mouse.cursor_split_slow_px = Some(d_slow);
            }
            let (settle_us, errs) = compute_settle_and_accuracy(obs_list, &stops);
            if !settle_us.is_empty() {
                mouse.cursor_settle_ms = Some(DistStats::from_us(&settle_us));
            }
            if !errs.is_empty() {
                mouse.pointer_accuracy_err_px = Some(DistStats::from_ms(&errs));
            }
        }
        _ => {}
    }
}

fn compute_track_lag(
    obs_list: &[PresentedObservation],
    pts: &[(u64, f64, f64)],
    speed_px_per_s: f64,
) -> Option<(DistStats, DistStats, f64)> {
    let t_warmup = pts.first().map_or(0, |p| p.0 + 350_000);
    let t_end = pts.last().map_or(u64::MAX, |p| p.0);
    let mut spatial_px = Vec::new();
    let mut equiv_ms = Vec::new();
    for o in obs_list {
        if o.presented_us < t_warmup || o.presented_us > t_end {
            continue;
        }
        let Some(bc) = o.meta.barcode else {
            continue;
        };
        if bc.event_seq == 0 {
            continue;
        }
        if let Some((lx, ly)) = interpolate_track(pts, o.presented_us) {
            let dx = lx - f64::from(bc.cursor_x);
            let dy = ly - f64::from(bc.cursor_y);
            let dist = (dx * dx + dy * dy).sqrt();
            spatial_px.push(dist);
            equiv_ms.push((dist / speed_px_per_s.max(1.0)) * 1000.0);
        }
    }
    if spatial_px.is_empty() {
        return None;
    }
    let d_px = DistStats::from_ms(&spatial_px);
    let d_ms = DistStats::from_ms(&equiv_ms);
    let jitter = (d_ms.p95 - d_ms.p50).max(0.0);
    Some((d_px, d_ms, jitter))
}

fn compute_settle_and_accuracy(
    obs_list: &[PresentedObservation],
    stops: &[StopTargetEvent],
) -> (Vec<u64>, Vec<f64>) {
    let mut settle_us = Vec::new();
    let mut errs_px = Vec::new();
    for st in stops {
        let mut settled_at_us: Option<u64> = None;
        let mut settled_errs = Vec::new();
        let mut late_fallback_errs = Vec::new();
        let min_margin_us = st.stop_us + 95_000;
        let late_fallback_us = st.stop_us + 140_000;
        for o in obs_list {
            if o.presented_us < st.stop_us {
                continue;
            }
            if o.presented_us > st.hold_end_us {
                break;
            }
            let Some(bc) = o.meta.barcode else {
                continue;
            };
            let dx = st.expected_x - f64::from(bc.cursor_x);
            let dy = st.expected_y - f64::from(bc.cursor_y);
            let err = (dx * dx + dy * dy).sqrt();
            if settled_at_us.is_none() && err <= 2.5 {
                settle_us.push(o.presented_us - st.stop_us);
                settled_at_us = Some(o.presented_us);
            }
            if let Some(s_us) = settled_at_us {
                if o.presented_us >= min_margin_us.max(s_us) {
                    settled_errs.push(err);
                }
            } else if o.presented_us >= late_fallback_us {
                late_fallback_errs.push(err);
            }
        }
        if !settled_errs.is_empty() {
            errs_px.extend(settled_errs);
        } else {
            errs_px.extend(late_fallback_errs);
        }
    }
    (settle_us, errs_px)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::runner::PendingPresentMeta;
    use rc_render_macos::barcode::BarcodePayload;

    fn obs(presented_us: u64, cx: u16, cy: u16) -> PresentedObservation {
        PresentedObservation {
            presented_us,
            meta: PendingPresentMeta {
                barcode: Some(BarcodePayload {
                    frame_id: 1,
                    cursor_x: cx,
                    cursor_y: cy,
                    event_seq: 1,
                }),
                src_presented_us: None,
                event_recv_us: None,
                sck_cb_us: 0,
                encode_done_us: 0,
                decode_done_us: 0,
                click_down_seq: 0,
                click_count: 0,
                scroll_seq: 0,
                key_down_seq: 0,
                key_up_seq: 0,
                last_key_mods: 0,
                key_is_down: false,
                key_recv_us: None,
            },
        }
    }

    #[test]
    fn settle_and_accuracy_excludes_late_settling_transition_frames() {
        let stops = vec![StopTargetEvent {
            stop_us: 1_000_000,
            hold_end_us: 1_240_000,
            expected_x: 500.0,
            expected_y: 400.0,
        }];
        let observations = vec![
            obs(1_050_000, 200, 400), // +50ms: still mid-sweep (300px off)
            obs(1_100_000, 100, 400), // +100ms: past 95ms margin, still settling (400px off)
            obs(1_115_000, 500, 400), // +115ms: settled (0px)
            obs(1_150_000, 501, 400), // +150ms: steady-state (1px)
        ];
        let (settle, errs) = compute_settle_and_accuracy(&observations, &stops);
        assert_eq!(settle, vec![115_000]);
        assert_eq!(errs, vec![0.0, 1.0]);
    }

    #[test]
    fn settle_and_accuracy_still_reports_persistent_steady_state_offset() {
        let stops = vec![StopTargetEvent {
            stop_us: 2_000_000,
            hold_end_us: 2_240_000,
            expected_x: 500.0,
            expected_y: 400.0,
        }];
        let observations = vec![
            obs(2_100_000, 510, 400), // +100ms: <140ms fallback
            obs(2_160_000, 510, 400), // +160ms: persistent 10px bug must surface
        ];
        let (settle, errs) = compute_settle_and_accuracy(&observations, &stops);
        assert!(settle.is_empty());
        assert_eq!(errs, vec![10.0]);
    }
}

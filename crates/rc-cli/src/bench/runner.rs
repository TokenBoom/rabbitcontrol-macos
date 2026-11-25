//! macOS 真机基准运行器（仅 `bench-metrics` 启用，零 `unsafe`）。
//!
//! 在主线程建立右下角小号真 `NSWindow` + `CAMetalLayer`（透传鼠标事件、不抢焦点）
//! 以获取真实 `MTLDrawable.presentedTime()`；工作线程按 `BENCHMARK_MACOS.md` S1–S6
//! 拉起全屏 `bench_target` 探针窗并驱动 `SckBackend -> VtEncoder -> VtDecoder -> MetalRenderer`。

use std::collections::HashMap;
use std::fs;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rc_capture::{CaptureBackend, CapturedFrame};
use rc_capture_macos::{
    host_time_us, main_display_logical_resolution, promote_current_thread_qos,
    read_pixel_buffer_y_region, CaptureFrameTiming, LatencyCriticalActivity, SckBackend, SckConfig,
};
use rc_decode::{DecodeOutput, Decoder};
use rc_decode_vt::{VtDecoder, VtDecoderConfig};
use rc_encode::{Codec, EncodedFrame, Encoder, EncoderConfig, Preset};
use rc_encode_vt::VtEncoder;
use rc_input_macos::{Injector, InputEvent};
use rc_render::Renderer;
use rc_render_macos::barcode::{
    decode_barcode_y, BarcodePayload, BARCODE_HEIGHT, BARCODE_OFFSET_X, BARCODE_OFFSET_Y,
    BARCODE_WIDTH,
};
use rc_render_macos::{
    media_time_us, window_bench, MetalRenderer, MetalRendererConfig, PresentTimingSample,
};
use rc_types::{Resolution, Timestamp};
use serde::Deserialize;

use super::keyboard::{drive_keyboard_scenario, populate_keyboard_report, KeyboardBenchState};
use super::mouse::{drive_mouse_scenario, populate_mouse_report, MouseBenchState};
use super::stats::{
    compute_pacing_metrics, histogram_1ms_from_us, merge_scenario_runs, BenchReport,
    CaptureSection, DistStats, EnvSnapshot, KeyboardMetrics, MouseMetrics, ScenarioReport,
    SnappinessSection, SystemMetrics,
};
use super::system::{
    drive_mixed_workflow, measure_cold_start_ttff, run_standalone_system_scenario,
};

/// 支持运行的基准场景集合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchSuite {
    All,
    Core,
    Mouse,
    Keyboard,
    System,
    S1StaticIdle,
    S2Motion60,
    S3Motion120,
    S4StepI2p,
    S5DragTrack,
    S6IdleWakeup,
    S7MouseClickDrag,
    S8MouseWheel,
    S9MouseQuicE2e,
    S10MousePolling1khz,
    S11MouseCursorSync,
    S12KeyTypingQuic,
    S13KeyChordsHold,
    S14AudioPipeline,
    S15ClipboardSync,
    S16MixedWorkflowTtff,
}

impl BenchSuite {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "all" => Ok(Self::All),
            "core" => Ok(Self::Core),
            "mouse" => Ok(Self::Mouse),
            "keyboard" | "kb" => Ok(Self::Keyboard),
            "system" | "sys" => Ok(Self::System),
            "s1" | "S1" | "static-idle" => Ok(Self::S1StaticIdle),
            "s2" | "S2" | "motion-60" => Ok(Self::S2Motion60),
            "s3" | "S3" | "motion-120" => Ok(Self::S3Motion120),
            "s4" | "S4" | "step-i2p" => Ok(Self::S4StepI2p),
            "s5" | "S5" | "drag-track" => Ok(Self::S5DragTrack),
            "s6" | "S6" | "idle-wakeup" => Ok(Self::S6IdleWakeup),
            "s7" | "S7" | "mouse-click-drag" => Ok(Self::S7MouseClickDrag),
            "s8" | "S8" | "mouse-wheel" => Ok(Self::S8MouseWheel),
            "s9" | "S9" | "mouse-quic-e2e" => Ok(Self::S9MouseQuicE2e),
            "s10" | "S10" | "mouse-polling-1khz" => Ok(Self::S10MousePolling1khz),
            "s11" | "S11" | "mouse-cursor-sync" => Ok(Self::S11MouseCursorSync),
            "s12" | "S12" | "key-typing-quic" => Ok(Self::S12KeyTypingQuic),
            "s13" | "S13" | "key-chords-hold" => Ok(Self::S13KeyChordsHold),
            "s14" | "S14" | "audio-pipeline" => Ok(Self::S14AudioPipeline),
            "s15" | "S15" | "clipboard-sync" => Ok(Self::S15ClipboardSync),
            "s16" | "S16" | "mixed-workflow-ttff" => Ok(Self::S16MixedWorkflowTtff),
            other => Err(format!(
                "无效的 --suite {other:?}：应为 all / core / mouse / keyboard / system / s1..s16"
            )),
        }
    }

    fn expand(self) -> Vec<Self> {
        match self {
            Self::All => vec![
                Self::S1StaticIdle,
                Self::S2Motion60,
                Self::S3Motion120,
                Self::S4StepI2p,
                Self::S5DragTrack,
                Self::S6IdleWakeup,
                Self::S7MouseClickDrag,
                Self::S8MouseWheel,
                Self::S9MouseQuicE2e,
                Self::S10MousePolling1khz,
                Self::S11MouseCursorSync,
                Self::S12KeyTypingQuic,
                Self::S13KeyChordsHold,
                Self::S14AudioPipeline,
                Self::S15ClipboardSync,
                Self::S16MixedWorkflowTtff,
            ],
            Self::Core => vec![
                Self::S1StaticIdle,
                Self::S2Motion60,
                Self::S3Motion120,
                Self::S4StepI2p,
                Self::S5DragTrack,
                Self::S6IdleWakeup,
            ],
            Self::Mouse => vec![
                Self::S4StepI2p,
                Self::S5DragTrack,
                Self::S6IdleWakeup,
                Self::S7MouseClickDrag,
                Self::S8MouseWheel,
                Self::S9MouseQuicE2e,
                Self::S10MousePolling1khz,
                Self::S11MouseCursorSync,
            ],
            Self::Keyboard => vec![Self::S12KeyTypingQuic, Self::S13KeyChordsHold],
            Self::System => vec![
                Self::S14AudioPipeline,
                Self::S15ClipboardSync,
                Self::S16MixedWorkflowTtff,
            ],
            single => vec![single],
        }
    }
}

/// `bench run` 参数。
#[derive(Debug, Clone)]
pub struct BenchRunConfig {
    pub suite: BenchSuite,
    pub runs: u32,
    pub duration_override: Option<Duration>,
    pub bench_target_bin: Option<PathBuf>,
    pub report_path: Option<PathBuf>,
}

/// 探针窗 UDP 回传的真实上屏记录。
#[derive(Debug, Clone, Copy, Deserialize)]
struct TargetPacket {
    frame_id: u16,
    src_presented_us: u64,
    event_seq: u16,
    event_recv_us: u64,
    #[serde(default)]
    click_down_seq: u32,
    #[serde(default)]
    click_count: u16,
    #[serde(default)]
    scroll_seq: u32,
    #[serde(default)]
    key_down_seq: u32,
    #[serde(default)]
    key_up_seq: u32,
    #[serde(default)]
    last_key_mods: u8,
    #[serde(default)]
    key_is_down: bool,
    #[serde(default)]
    key_recv_us: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct TargetKeyUdp {
    kind: String,
    keycode: u16,
    mods: u8,
    down: bool,
    recv_us: u64,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct TargetKeyEvent {
    pub keycode: u16,
    pub mods: u8,
    pub down: bool,
    pub recv_us: u64,
}

#[derive(Default)]
struct TargetTelemetry {
    by_frame_id: HashMap<u16, TargetPacket>,
    by_event_seq: HashMap<u16, TargetPacket>,
    key_events: Vec<TargetKeyEvent>,
    packets_seen: u64,
}

struct BenchTargetGuard {
    child: Child,
}

impl BenchTargetGuard {
    fn spawn(
        bin: &Path,
        mode: &str,
        fps: u32,
        udp_addr: std::net::SocketAddr,
    ) -> Result<Self, String> {
        let child = Command::new(bin)
            .args([
                "--mode",
                mode,
                "--fps",
                &fps.to_string(),
                "--udp-report",
                &udp_addr.to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("启动 bench_target ({}) 失败：{e}", bin.display()))?;
        Ok(Self { child })
    }
}

impl Drop for BenchTargetGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct CapturedBenchFrame {
    frame: CapturedFrame,
    timing: CaptureFrameTiming,
    barcode: Option<BarcodePayload>,
}

struct InflightMeta {
    barcode: Option<BarcodePayload>,
    capture_timing: CaptureFrameTiming,
    encode_start_us: u64,
}

#[derive(Clone, Copy)]
pub(super) struct PendingPresentMeta {
    pub barcode: Option<BarcodePayload>,
    pub src_presented_us: Option<u64>,
    pub event_recv_us: Option<u64>,
    pub sck_cb_us: u64,
    pub encode_done_us: u64,
    pub decode_done_us: u64,
    pub click_down_seq: u32,
    pub click_count: u16,
    pub scroll_seq: u32,
    pub key_down_seq: u32,
    pub key_up_seq: u32,
    pub last_key_mods: u8,
    pub key_is_down: bool,
    pub key_recv_us: Option<u64>,
}

#[derive(Clone, Copy)]
pub(super) struct PresentedObservation {
    pub presented_us: u64,
    pub meta: PendingPresentMeta,
}

#[derive(Clone, Copy)]
pub(super) struct StepEvent {
    pub emit_us: u64,
    pub target_x: u16,
    pub target_y: u16,
}

/// 定位 `bench_target` 测试可执行文件（优先显式路径，其次自动检索 `target/release/deps` 与 `target/debug/deps`）。
fn resolve_bench_target_bin(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
        return Err(format!("找不到指定的 --bench-target: {}", p.display()));
    }
    let mut search_dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            search_dirs.push(dir.to_path_buf());
            search_dirs.push(dir.join("deps"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        search_dirs.push(cwd.join("target/release/deps"));
        search_dirs.push(cwd.join("target/debug/deps"));
    }

    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for dir in search_dirs {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with("bench_target-") && !name.ends_with(".d") && path.is_file() {
                let mtime = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                if newest.as_ref().map_or(true, |(t, _)| mtime > *t) {
                    newest = Some((mtime, path));
                }
            }
        }
    }
    newest.map(|(_, p)| p).ok_or_else(|| {
        "未找到 bench_target 二进制；请先编译：`cargo test -p rc-render-macos --features \"window-bench bench-metrics\" --test bench_target --release --no-run` 或传入 --bench-target <path>"
            .to_string()
    })
}

/// 入口：主线程挂载右下角小号真实 `NSWindow`（透传鼠标、不抢焦点）并运行 `NSApp.run()`，
/// 后台工作线程跑完全部基准场景后落盘并结束进程。
pub fn execute_bench_run(cfg: BenchRunConfig) -> Result<(), String> {
    let target_bin = resolve_bench_target_bin(cfg.bench_target_bin.as_deref())?;
    let res = main_display_logical_resolution().unwrap_or(Resolution::new(1512, 982));

    let mtm = objc2::MainThreadMarker::new()
        .ok_or_else(|| "rc-cli bench run 必须在 macOS 主线程调用".to_string())?;
    // 右下角 320×200 小窗（避开屏幕中央 S4/S5/S6 注入区并透传鼠标），用于拿到真实 MTLDrawable.presentedTime。
    let win_w = 320u32;
    let win_h = 200u32;
    let origin_x = f64::from(res.width.saturating_sub(win_w + 16));
    let origin_y = 16.0;
    let win = window_bench::window_with_metal_layer_at(mtm, (origin_x, origin_y), (win_w, win_h))?;
    // 高于全屏 bench_target (level=25)，确保小号接收窗不被遮挡，从而能获得真实 MTLDrawable.presentedTime。
    win.set_level(26);
    win.set_ignores_mouse_events(true);

    let layer_ptr = win.layer_ptr() as usize;
    let target_clone = target_bin.clone();
    let cfg_clone = cfg.clone();

    std::thread::Builder::new()
        .name("rc-bench-orchestrator".into())
        .spawn(move || {
            let outcome =
                run_all_scenarios(layer_ptr, (win_w, win_h), res, &target_clone, &cfg_clone);
            match outcome {
                Ok(report) => {
                    let json = serde_json::to_string_pretty(&report)
                        .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
                    match cfg_clone.report_path.as_deref() {
                        Some(path) if path != Path::new("-") => {
                            if let Err(e) = fs::write(path, format!("{json}\n")) {
                                eprintln!("错误：写基准报告 {} 失败：{e}", path.display());
                                std::process::exit(1);
                            }
                            eprintln!("基准报告已写入：{}", path.display());
                        }
                        _ => {
                            println!("{json}");
                        }
                    }
                    std::process::exit(0);
                }
                Err(err) => {
                    eprintln!("错误：基准执行失败：{err}");
                    std::process::exit(1);
                }
            }
        })
        .map_err(|e| format!("启动基准协调线程失败：{e}"))?;

    // 保留窗口直到进程退出（由工作线程 `std::process::exit` 回收）。
    win.leak();
    {
        use objc2_app_kit::NSApplication;
        NSApplication::sharedApplication(mtm).run();
    }
    Ok(())
}

fn run_all_scenarios(
    layer_ptr: usize,
    presenter_size: (u32, u32),
    display_res: Resolution,
    target_bin: &Path,
    cfg: &BenchRunConfig,
) -> Result<BenchReport, String> {
    promote_current_thread_qos();
    let _activity = LatencyCriticalActivity::begin("RabbitControl MacOS Benchmark", true);

    let mut renderer = MetalRenderer::from_raw_layer(
        layer_ptr as *mut _,
        MetalRendererConfig {
            size: presenter_size,
            ..MetalRendererConfig::default()
        },
    )
    .map_err(|e| format!("初始化基准 MetalRenderer 失败：{e}"))?;

    // 等待主线程 NSApp.run() 完成接收窗上屏注册，确保 SckBackend 能精确识别并排除本进程窗口。
    std::thread::sleep(Duration::from_millis(150));

    let suites = cfg.suite.expand();
    let runs = cfg.runs.max(1);
    let mut scenarios = Vec::with_capacity(suites.len());

    for suite in suites {
        let mut per_run = Vec::with_capacity(runs as usize);
        for run_idx in 0..runs {
            eprintln!(
                "==> 运行场景 {:?}（第 {}/{} 轮）…",
                suite,
                run_idx + 1,
                runs
            );
            let s = run_single_scenario(
                suite,
                display_res,
                target_bin,
                &mut renderer,
                cfg.duration_override,
            )?;
            print_scenario_summary(&s);
            per_run.push(s);
        }
        if let Some(merged) = merge_scenario_runs(&per_run) {
            scenarios.push(merged);
        }
    }

    let now_s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    Ok(BenchReport {
        schema_version: 1,
        generated_at_unix_s: now_s,
        env: collect_env_snapshot(display_res, runs),
        scenarios,
    })
}

pub(super) fn collect_env_snapshot(res: Resolution, runs: u32) -> EnvSnapshot {
    let chip = command_stdout("sysctl", &["-n", "machdep.cpu.brand_string"])
        .unwrap_or_else(|| "Apple Silicon".to_string());
    let os_version =
        command_stdout("sw_vers", &["-productVersion"]).unwrap_or_else(|| "macOS".to_string());
    let git_commit = option_env!("RC_CLI_GIT_COMMIT")
        .filter(|s| !s.is_empty() && *s != "unknown")
        .map(str::to_string)
        .or_else(|| command_stdout("git", &["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    let power_source = command_stdout("pmset", &["-g", "batt"])
        .and_then(|s| s.lines().next().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    let clock_delta_us = (host_time_us() as i128 - i128::from(media_time_us())) as i64;

    EnvSnapshot {
        chip,
        os_version,
        resolution: format!("{}x{}", res.width, res.height),
        git_commit,
        power_source,
        clock_domain: "CMClockGetHostTimeClock/CACurrentMediaTime".to_string(),
        clock_delta_us,
        runs_per_scenario: runs,
    }
}

fn command_stdout(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn print_scenario_summary(s: &ScenarioReport) {
    eprintln!(
        "  [{}] cap={} bc={} pres={} avg={:.1}fps 1%Low={:.1}fps jitter={:.2}ms hitch={:.1}ms/s A1={:.2}ms A2={:.2}ms A3={:.2}ms i2p_p50={}",
        s.id,
        s.capture.captured_frames,
        s.capture.barcode_decoded_frames,
        s.pacing.presented_frames,
        s.pacing.avg_fps,
        s.pacing.low_1pct_fps,
        s.pacing.ifi_jitter_ms,
        s.pacing.hitch_ms_per_s,
        s.capture.stimulus_to_pts_ms.p50,
        s.capture.pts_to_callback_ms.p50,
        s.capture.mailbox_wait_ms.p50,
        s.snappiness
            .step_i2p_ms
            .as_ref()
            .map_or("-".into(), |d| format!("{:.2}ms", d.p50)),
    );
}

fn run_single_scenario(
    suite: BenchSuite,
    res: Resolution,
    target_bin: &Path,
    renderer: &mut MetalRenderer,
    duration_override: Option<Duration>,
) -> Result<ScenarioReport, String> {
    if matches!(
        suite,
        BenchSuite::S14AudioPipeline | BenchSuite::S15ClipboardSync
    ) {
        return run_standalone_system_scenario(suite, duration_override);
    }

    let ttff_stats = if suite == BenchSuite::S16MixedWorkflowTtff {
        Some(measure_cold_start_ttff(res, target_bin, renderer)?)
    } else {
        None
    };

    let (id, mode, fps, default_dur) = match suite {
        BenchSuite::S1StaticIdle => ("S1-static-idle", "static", 60, Duration::from_secs(12)),
        BenchSuite::S2Motion60 => ("S2-motion-60", "anim", 60, Duration::from_secs(15)),
        BenchSuite::S3Motion120 => ("S3-motion-120", "anim", 120, Duration::from_secs(15)),
        BenchSuite::S4StepI2p => ("S4-step-i2p", "interactive", 120, Duration::from_secs(18)),
        BenchSuite::S5DragTrack => ("S5-drag-track", "interactive", 120, Duration::from_secs(12)),
        BenchSuite::S6IdleWakeup => ("S6-idle-wakeup", "wakeup", 120, Duration::from_secs(24)),
        BenchSuite::S7MouseClickDrag => (
            "S7-mouse-click-drag",
            "interactive",
            120,
            Duration::from_secs(16),
        ),
        BenchSuite::S8MouseWheel => (
            "S8-mouse-wheel",
            "interactive",
            120,
            Duration::from_secs(12),
        ),
        BenchSuite::S9MouseQuicE2e => (
            "S9-mouse-quic-e2e",
            "interactive",
            120,
            Duration::from_secs(14),
        ),
        BenchSuite::S10MousePolling1khz => (
            "S10-mouse-polling-1khz",
            "interactive",
            120,
            Duration::from_secs(8),
        ),
        BenchSuite::S11MouseCursorSync => (
            "S11-mouse-cursor-sync",
            "interactive",
            120,
            Duration::from_secs(14),
        ),
        BenchSuite::S12KeyTypingQuic => (
            "S12-key-typing-quic",
            "interactive",
            120,
            Duration::from_secs(14),
        ),
        BenchSuite::S13KeyChordsHold => (
            "S13-key-chords-hold",
            "interactive",
            120,
            Duration::from_secs(12),
        ),
        BenchSuite::S16MixedWorkflowTtff => (
            "S16-mixed-workflow-ttff",
            "interactive",
            120,
            Duration::from_secs(12),
        ),
        BenchSuite::All
        | BenchSuite::Core
        | BenchSuite::Mouse
        | BenchSuite::Keyboard
        | BenchSuite::System
        | BenchSuite::S14AudioPipeline
        | BenchSuite::S15ClipboardSync => unreachable!(),
    };
    let duration = duration_override.unwrap_or(default_dur);

    // 绑定 UDP 接收来自 bench_target 的真实上屏时间戳。
    let udp_sock =
        UdpSocket::bind("127.0.0.1:0").map_err(|e| format!("绑定 UDP 接收端口失败：{e}"))?;
    udp_sock
        .set_read_timeout(Some(Duration::from_millis(50)))
        .map_err(|e| format!("设置 UDP 超时失败：{e}"))?;
    let udp_addr = udp_sock
        .local_addr()
        .map_err(|e| format!("读取 UDP 地址失败：{e}"))?;

    let telemetry = Arc::new(Mutex::new(TargetTelemetry::default()));
    let stop_udp = Arc::new(AtomicBool::new(false));
    let telemetry_udp = Arc::clone(&telemetry);
    let stop_udp_rx = Arc::clone(&stop_udp);
    let udp_thread = std::thread::Builder::new()
        .name("rc-bench-udp-rx".into())
        .spawn(move || {
            let mut buf = [0u8; 512];
            while !stop_udp_rx.load(Ordering::Relaxed) {
                match udp_sock.recv_from(&mut buf) {
                    Ok((n, _)) => {
                        if let Ok(kev) = serde_json::from_slice::<TargetKeyUdp>(&buf[..n]) {
                            if kev.kind == "key" {
                                let mut t = telemetry_udp.lock().unwrap_or_else(|e| e.into_inner());
                                t.key_events.push(TargetKeyEvent {
                                    keycode: kev.keycode,
                                    mods: kev.mods,
                                    down: kev.down,
                                    recv_us: kev.recv_us,
                                });
                                continue;
                            }
                        }
                        if let Ok(pkt) = serde_json::from_slice::<TargetPacket>(&buf[..n]) {
                            let mut t = telemetry_udp.lock().unwrap_or_else(|e| e.into_inner());
                            t.packets_seen += 1;
                            t.by_frame_id.insert(pkt.frame_id, pkt);
                            if pkt.event_seq > 0 {
                                t.by_event_seq.entry(pkt.event_seq).or_insert(pkt);
                            }
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
        })
        .map_err(|e| format!("启动 UDP 接收线程失败：{e}"))?;

    let _target_guard = BenchTargetGuard::spawn(target_bin, mode, fps, udp_addr)?;

    // 等待探针窗上屏第一帧再启动管线。
    let wait_deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < wait_deadline {
        if telemetry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .packets_seen
            > 0
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if suite == BenchSuite::S1StaticIdle {
        // 等待 macOS 窗口激活/切换动画彻底收敛，再开始统计静屏零编码基线。
        std::thread::sleep(Duration::from_millis(1200));
    }

    let origin_us = media_time_us();
    let mut capture = SckBackend::new_for_bench(
        SckConfig {
            display: 0,
            fps,
            resolution: Some(res),
            origin_us: Some(origin_us),
        },
        true,
    )
    .map_err(|e| format!("初始化基准 SckBackend 失败：{e}"))?;

    let bitrate_bps = if fps >= 90 { 20_000_000 } else { 10_000_000 };
    let mut encoder = VtEncoder::new(EncoderConfig {
        codec: Codec::H264,
        resolution: res,
        fps,
        bitrate_bps,
        b_frames: 0,
        gop_len: 0,
        preset: Preset::Fastest,
        low_latency: true,
    })
    .map_err(|e| format!("初始化基准 VtEncoder 失败：{e}"))?;

    let mut decoder = VtDecoder::new(VtDecoderConfig::default())
        .map_err(|e| format!("初始化 VtDecoder 失败：{e}"))?;

    let pending_presents = Arc::new(Mutex::new(HashMap::<u64, PendingPresentMeta>::new()));
    let presented_obs = Arc::new(Mutex::new(Vec::<PresentedObservation>::new()));
    let pending_cb = Arc::clone(&pending_presents);
    let obs_cb = Arc::clone(&presented_obs);
    renderer.set_present_timing_sink(Some(Arc::new(move |sample: PresentTimingSample| {
        let meta = pending_cb
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&sample.frame_tag);
        if sample.presented_us == 0 {
            return;
        }
        if let Some(meta) = meta {
            obs_cb
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(PresentedObservation {
                    presented_us: sample.presented_us,
                    meta,
                });
        }
    })));

    // 采集统计容器。
    let mut a1_samples = Vec::new();
    let mut a2_samples = Vec::new();
    let mut a3_samples = Vec::new();
    let mut a4_samples = Vec::new();
    let mut captured_count = 0u64;
    let mut decoded_count = 0u64;
    let mut barcode_decoded_frames = 0u64;
    let mut skipped_source_frames = 0u64;
    let mut last_a1_frame_id: Option<u16> = None;

    // 单槽最新帧交接通道，保证采集线程与编解码线程解耦。
    let cap_slot = Arc::new((Mutex::new(None::<CapturedBenchFrame>), Condvar::new()));
    let cap_stop = Arc::new(AtomicBool::new(false));
    let cap_slot_prod = Arc::clone(&cap_slot);
    let cap_stop_prod = Arc::clone(&cap_stop);

    let (step_tx, step_rx) = std::sync::mpsc::channel::<StepEvent>();
    let drag_points = Arc::new(Mutex::new(Vec::<(u64, f64, f64)>::new()));
    let wakeup_triggers = Arc::new(Mutex::new(Vec::<u64>::new()));
    let mouse_state = Arc::new(MouseBenchState::default());
    let kb_state = Arc::new(KeyboardBenchState::default());

    let inj_stop = Arc::new(AtomicBool::new(false));
    let mut injector_opt = if matches!(
        suite,
        BenchSuite::S4StepI2p | BenchSuite::S5DragTrack | BenchSuite::S6IdleWakeup
    ) {
        Some(Injector::for_main_display().map_err(|e| format!("初始化输入注入器失败：{e}"))?)
    } else {
        None
    };

    let is_mouse_suite = matches!(
        suite,
        BenchSuite::S7MouseClickDrag
            | BenchSuite::S8MouseWheel
            | BenchSuite::S9MouseQuicE2e
            | BenchSuite::S10MousePolling1khz
            | BenchSuite::S11MouseCursorSync
    );
    let is_kb_suite = matches!(
        suite,
        BenchSuite::S12KeyTypingQuic | BenchSuite::S13KeyChordsHold
    );
    let is_mixed_suite = suite == BenchSuite::S16MixedWorkflowTtff;

    let run_start = Instant::now();
    let actual_duration = std::thread::scope(|scope| -> Result<f64, String> {
        let cap_handle = scope.spawn(|| -> u64 {
            promote_current_thread_qos();
            while !cap_stop_prod.load(Ordering::Relaxed) {
                match capture.next_frame(Duration::from_millis(20)) {
                    Ok(Some(frame)) => {
                        let timing = capture.last_frame_timing().unwrap_or_default();
                        let read_w = BARCODE_OFFSET_X + BARCODE_WIDTH;
                        let read_h = BARCODE_OFFSET_Y + BARCODE_HEIGHT;
                        let barcode = read_pixel_buffer_y_region(&frame, read_w, read_h)
                            .ok()
                            .and_then(|(w, h, y)| {
                                if w <= BARCODE_OFFSET_X || h <= BARCODE_OFFSET_Y {
                                    return None;
                                }
                                decode_barcode_y(
                                    &y[BARCODE_OFFSET_Y * w + BARCODE_OFFSET_X..],
                                    w,
                                    w - BARCODE_OFFSET_X,
                                    h - BARCODE_OFFSET_Y,
                                )
                            });
                        let (lock, cvar) = &*cap_slot_prod;
                        let mut slot = lock.lock().unwrap_or_else(|e| e.into_inner());
                        *slot = Some(CapturedBenchFrame {
                            frame,
                            timing,
                            barcode,
                        });
                        cvar.notify_one();
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
            capture.mailbox_overwrites()
        });

        let inj_stop_th = Arc::clone(&inj_stop);
        let drag_pts_th = Arc::clone(&drag_points);
        let wakeup_th = Arc::clone(&wakeup_triggers);
        let mouse_state_th = Arc::clone(&mouse_state);
        let mouse_handle = if is_mouse_suite {
            let step_tx_m = step_tx.clone();
            Some(scope.spawn(move || -> Result<(), String> {
                drive_mouse_scenario(
                    suite,
                    res,
                    duration,
                    &inj_stop_th,
                    &step_tx_m,
                    &drag_pts_th,
                    &mouse_state_th,
                )
            }))
        } else {
            None
        };
        let kb_stop_th = Arc::clone(&inj_stop);
        let mouse_state_kb = Arc::clone(&mouse_state);
        let kb_state_th = Arc::clone(&kb_state);
        let kb_handle = if is_kb_suite {
            Some(scope.spawn(move || -> Result<(), String> {
                drive_keyboard_scenario(suite, &kb_stop_th, &mouse_state_kb, &kb_state_th)
            }))
        } else {
            None
        };
        let mixed_stop_th = Arc::clone(&inj_stop);
        let drag_pts_mix = Arc::clone(&drag_points);
        let mouse_state_mix = Arc::clone(&mouse_state);
        let kb_state_mix = Arc::clone(&kb_state);
        let mixed_handle = if is_mixed_suite {
            Some(scope.spawn(move || -> Result<(), String> {
                drive_mixed_workflow(
                    res,
                    &mixed_stop_th,
                    &drag_pts_mix,
                    &mouse_state_mix,
                    &kb_state_mix,
                )
            }))
        } else {
            None
        };
        let inj_stop_inj = Arc::clone(&inj_stop);
        let drag_pts_inj = Arc::clone(&drag_points);
        let inj_handle = if let Some(mut injector) = injector_opt.take() {
            Some(scope.spawn(move || {
                let cx = f64::from(res.width) * 0.5;
                let cy = f64::from(res.height) * 0.5;
                match suite {
                    BenchSuite::S4StepI2p => {
                        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
                        let mut toggle = false;
                        // 先归位，避免首跳重合。
                        let _ = injector.inject(InputEvent::PointerMove {
                            x: cx - 180.0,
                            y: cy,
                        });
                        std::thread::sleep(Duration::from_millis(250));
                        while !inj_stop_inj.load(Ordering::Relaxed) {
                            toggle = !toggle;
                            let tx = if toggle { cx + 160.0 } else { cx - 160.0 };
                            let ty = if toggle { cy + 80.0 } else { cy - 80.0 };
                            let emit_us = media_time_us();
                            let _ = injector.inject(InputEvent::PointerMove { x: tx, y: ty });
                            let _ = step_tx.send(StepEvent {
                                emit_us,
                                target_x: tx.round() as u16,
                                target_y: ty.round() as u16,
                            });
                            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                            let jitter_ms = 150 + ((rng >> 32) % 201);
                            std::thread::sleep(Duration::from_millis(jitter_ms));
                        }
                    }
                    BenchSuite::S5DragTrack => {
                        let radius = 160.0f64;
                        let omega = 1200.0 / radius; // 1200 px/s
                        let start = Instant::now();
                        let tick = Duration::from_micros(8_333);
                        let mut step = 0u64;
                        while !inj_stop_inj.load(Ordering::Relaxed) {
                            let elapsed_s = start.elapsed().as_secs_f64();
                            let angle = omega * elapsed_s;
                            let x = cx + radius * angle.cos();
                            let y = cy + radius * angle.sin();
                            let t_us = media_time_us();
                            let _ = injector.inject(InputEvent::PointerMove { x, y });
                            {
                                let mut pts =
                                    drag_pts_inj.lock().unwrap_or_else(|e| e.into_inner());
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
                    }
                    BenchSuite::S6IdleWakeup => {
                        let mut round = 0u32;
                        while !inj_stop_inj.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(2500));
                            if inj_stop_inj.load(Ordering::Relaxed) {
                                break;
                            }
                            round += 1;
                            let base_x = if round % 2 == 0 {
                                cx - 140.0
                            } else {
                                cx + 140.0
                            };
                            let emit_us = media_time_us();
                            wakeup_th
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push(emit_us);
                            for k in 0..120u32 {
                                if inj_stop_inj.load(Ordering::Relaxed) {
                                    break;
                                }
                                let x =
                                    base_x + f64::from(k) * if round % 2 == 0 { 2.0 } else { -2.0 };
                                let _ = injector.inject(InputEvent::PointerMove { x, y: cy });
                                std::thread::sleep(Duration::from_micros(8_333));
                            }
                        }
                    }
                    _ => {}
                }
            }))
        } else {
            drop(step_tx);
            None
        };

        let mut token_seq = 1u64;
        let mut present_tag = 1u64;
        let mut in_flight: HashMap<u64, InflightMeta> = HashMap::new();
        let mut first_frame = true;
        let mut prev_frame_id: Option<u16> = None;
        let mut carried: Option<(u64, EncodedFrame, u64)> = None;

        while let Some(rem) = duration.checked_sub(run_start.elapsed()) {
            if rem <= Duration::from_millis(250) {
                inj_stop.store(true, Ordering::Release);
            }
            if rem.is_zero() {
                break;
            }
            let mut did_work = false;

            // 1) 非阻塞排空全部已就绪编码帧（含上一拍 VT 唤醒帧），按序解码保参考链；暂缓调用 `present`，
            //    先让 Step 2 把刚采集的新帧提交进 VT 编码硬件并行处理。
            let mut ready_batch: Vec<(u64, EncodedFrame, u64)> = Vec::with_capacity(2);
            if let Some(item) = carried.take() {
                ready_batch.push(item);
            }
            loop {
                match encoder.recv_encoded(Duration::ZERO) {
                    Ok(Some((tok, encoded))) => {
                        ready_batch.push((tok, encoded, media_time_us()));
                    }
                    Ok(None) => break,
                    Err(_) => {
                        in_flight.clear();
                        encoder.request_idr();
                        break;
                    }
                }
            }
            let mut last_present = None;
            if !ready_batch.is_empty() {
                did_work = true;
                for (tok, encoded, encode_done_us) in ready_batch {
                    let Some(meta) = in_flight.remove(&tok) else {
                        continue;
                    };
                    match decoder.decode(&encoded.data, encoded.kind, Timestamp(u32::MAX)) {
                        Ok(DecodeOutput::Frame(decoded)) => {
                            let decode_done_us = media_time_us();
                            decoded_count += 1;
                            let (pkt_opt, ev_recv) = if let Some(bc) = meta.barcode {
                                let t = telemetry.lock().unwrap_or_else(|e| e.into_inner());
                                let pkt = t.by_frame_id.get(&bc.frame_id).copied();
                                let ev_recv =
                                    pkt.map(|p| p.event_recv_us).filter(|&v| v > 0).or_else(|| {
                                        t.by_event_seq
                                            .get(&bc.event_seq)
                                            .map(|p| p.event_recv_us)
                                            .filter(|&v| v > 0)
                                    });
                                (pkt, ev_recv)
                            } else {
                                (None, None)
                            };
                            let src_presented_us = pkt_opt.map(|p| p.src_presented_us);

                            if let Some(src_us) = src_presented_us {
                                let frame_id = meta.barcode.map(|b| b.frame_id);
                                let is_fresh_s6 = suite != BenchSuite::S6IdleWakeup
                                    || (frame_id != last_a1_frame_id
                                        && meta.capture_timing.pts_us.saturating_sub(src_us)
                                            <= 200_000);
                                if is_fresh_s6 {
                                    a1_samples
                                        .push(meta.capture_timing.pts_us.saturating_sub(src_us));
                                    a4_samples.push(meta.encode_start_us.saturating_sub(src_us));
                                }
                                if let Some(fid) = frame_id {
                                    last_a1_frame_id = Some(fid);
                                }
                            }
                            a2_samples.push(meta.capture_timing.pts_to_callback_us());
                            a3_samples.push(meta.capture_timing.mailbox_wait_us());

                            if suite != BenchSuite::S1StaticIdle {
                                let pending_meta = PendingPresentMeta {
                                    barcode: meta.barcode,
                                    src_presented_us,
                                    event_recv_us: ev_recv,
                                    sck_cb_us: meta.capture_timing.callback_us,
                                    encode_done_us,
                                    decode_done_us,
                                    click_down_seq: pkt_opt.map_or(0, |p| p.click_down_seq),
                                    click_count: pkt_opt.map_or(0, |p| p.click_count),
                                    scroll_seq: pkt_opt.map_or(0, |p| p.scroll_seq),
                                    key_down_seq: pkt_opt.map_or(0, |p| p.key_down_seq),
                                    key_up_seq: pkt_opt.map_or(0, |p| p.key_up_seq),
                                    last_key_mods: pkt_opt.map_or(0, |p| p.last_key_mods),
                                    key_is_down: pkt_opt.is_some_and(|p| p.key_is_down),
                                    key_recv_us: pkt_opt.map(|p| p.key_recv_us).filter(|&v| v > 0),
                                };
                                last_present = Some((decoded, pending_meta));
                            }
                        }
                        Ok(DecodeOutput::Nothing) => {}
                        Ok(DecodeOutput::NeedKeyframe) | Err(_) => {
                            encoder.request_idr();
                        }
                    }
                }
            }

            // 2) 每轮最多从采集单槽取 1 帧先提交 VT 编码（在 `renderer.present()` 之前启动硬件并行编码）。
            if in_flight.len() < 2 {
                let next_cap = {
                    let (lock, _) = &*cap_slot;
                    let mut slot = lock.lock().unwrap_or_else(|e| e.into_inner());
                    slot.take()
                };
                if let Some(item) = next_cap {
                    did_work = true;
                    captured_count += 1;
                    if let Some(bc) = item.barcode {
                        barcode_decoded_frames += 1;
                        if let Some(prev) = prev_frame_id {
                            let diff = bc.frame_id.wrapping_sub(prev);
                            if diff > 1 && diff < 1000 {
                                skipped_source_frames += u64::from(diff - 1);
                            }
                        }
                        prev_frame_id = Some(bc.frame_id);
                    }
                    let encode_start_us = media_time_us();
                    let tok = token_seq;
                    token_seq += 1;
                    let deadline = Timestamp(item.frame.timestamp.0.saturating_add(100_000));
                    if encoder
                        .submit_encode(tok, &item.frame, first_frame, deadline)
                        .is_ok()
                    {
                        first_frame = false;
                        in_flight.insert(
                            tok,
                            InflightMeta {
                                barcode: item.barcode,
                                capture_timing: item.timing,
                                encode_start_us,
                            },
                        );
                    }
                }
            }

            // 2.5) 提交编码后再上屏最后一帧已解码输出，避免 `nextDrawable()` 拖慢下一帧编码提交。
            if let Some((decoded, pending_meta)) = last_present {
                let tag = present_tag;
                present_tag = present_tag.wrapping_add(1);
                pending_presents
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(tag, pending_meta);
                renderer.set_next_frame_tag(tag);
                let _ = renderer.present(decoded);
            }

            // 3) 无推进时精确等待：
            //    - in_flight < 2：等 cap_slot Condvar（新采集帧 <0.05ms 唤醒；in_flight==1 时上限 1ms 及时回访 Step 1 收割 VT）
            //    - in_flight >= 2：等 VT recv_encoded(<=1ms)（VT 回调 Condvar 立刻唤醒）
            if !did_work {
                let Some(rem_wait) = duration.checked_sub(run_start.elapsed()) else {
                    break;
                };
                if rem_wait.is_zero() {
                    break;
                }
                if in_flight.len() < 2 {
                    let wait_ms = if in_flight.is_empty() { 10 } else { 1 };
                    let (lock, cvar) = &*cap_slot;
                    let slot = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if slot.is_none() {
                        let _ = cvar
                            .wait_timeout(slot, rem_wait.min(Duration::from_millis(wait_ms)))
                            .unwrap_or_else(|e| e.into_inner());
                    }
                } else {
                    match encoder.recv_encoded(rem_wait.min(Duration::from_millis(1))) {
                        Ok(Some((tok, encoded))) => {
                            carried = Some((tok, encoded, media_time_us()));
                        }
                        Ok(None) => {}
                        Err(_) => {
                            in_flight.clear();
                            encoder.request_idr();
                        }
                    }
                }
            }
        }

        inj_stop.store(true, Ordering::Release);
        cap_stop.store(true, Ordering::Release);
        cap_slot.1.notify_all();
        if let Some(h) = mouse_handle {
            h.join()
                .map_err(|_| "鼠标基准激励线程 panic".to_string())??;
        }
        if let Some(h) = kb_handle {
            h.join()
                .map_err(|_| "键盘基准激励线程 panic".to_string())??;
        }
        if let Some(h) = mixed_handle {
            h.join()
                .map_err(|_| "混合并发激励线程 panic".to_string())??;
        }
        if let Some(h) = inj_handle {
            let _ = h.join();
        }
        let overwrites = cap_handle.join().unwrap_or(0);
        let _ = overwrites;
        Ok(run_start.elapsed().as_secs_f64())
    })?;

    // 等待最后在途 `addPresentedHandler` 回调落袋。
    std::thread::sleep(Duration::from_millis(40));
    renderer.set_present_timing_sink(None);
    stop_udp.store(true, Ordering::Release);
    let _ = udp_thread.join();

    let mailbox_overwrites = capture.mailbox_overwrites();
    drop(capture);

    let obs_list = presented_obs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let steps: Vec<StepEvent> = step_rx.try_iter().collect();
    let drag_pts = drag_points
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let wakeup_ts = wakeup_triggers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let key_events = telemetry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .key_events
        .clone();

    Ok(build_scenario_report(
        suite,
        id,
        fps,
        actual_duration,
        captured_count,
        decoded_count,
        barcode_decoded_frames,
        skipped_source_frames,
        mailbox_overwrites,
        &a1_samples,
        &a2_samples,
        &a3_samples,
        &a4_samples,
        &obs_list,
        &steps,
        &drag_pts,
        &wakeup_ts,
        &key_events,
        &mouse_state,
        &kb_state,
        ttff_stats,
    ))
}

pub(super) fn interpolate_track(drag_pts: &[(u64, f64, f64)], ts: u64) -> Option<(f64, f64)> {
    let idx = drag_pts.partition_point(|p| p.0 <= ts);
    if idx > 0 && idx < drag_pts.len() {
        let (t0, x0, y0) = drag_pts[idx - 1];
        let (t1, x1, y1) = drag_pts[idx];
        if t1 > t0 && ts - t0 <= 40_000 && t1 - ts <= 40_000 {
            let frac = (ts - t0) as f64 / (t1 - t0) as f64;
            Some((x0 + (x1 - x0) * frac, y0 + (y1 - y0) * frac))
        } else {
            None
        }
    } else if idx > 0 {
        let (t0, x0, y0) = drag_pts[idx - 1];
        (ts.saturating_sub(t0) <= 40_000).then_some((x0, y0))
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn build_scenario_report(
    suite: BenchSuite,
    id: &str,
    fps: u32,
    duration_s: f64,
    captured_frames: u64,
    decoded_frames: u64,
    barcode_decoded_frames: u64,
    skipped_source_frames: u64,
    mailbox_overwrites: u64,
    a1_samples: &[u64],
    a2_samples: &[u64],
    a3_samples: &[u64],
    a4_samples: &[u64],
    obs_list: &[PresentedObservation],
    steps: &[StepEvent],
    drag_pts: &[(u64, f64, f64)],
    wakeup_ts: &[u64],
    key_events: &[TargetKeyEvent],
    mouse_state: &MouseBenchState,
    kb_state: &KeyboardBenchState,
    ttff_stats: Option<DistStats>,
) -> ScenarioReport {
    let presented_us: Vec<u64> = obs_list.iter().map(|o| o.presented_us).collect();
    let frame_ids: Vec<Option<u16>> = obs_list
        .iter()
        .map(|o| o.meta.barcode.map(|b| b.frame_id))
        .collect();

    let pacing = compute_pacing_metrics(&presented_us, &frame_ids, fps, duration_s);

    let e2e_samples: Vec<u64> = obs_list
        .iter()
        .filter_map(|o| {
            o.meta
                .src_presented_us
                .and_then(|src| o.presented_us.checked_sub(src))
        })
        .collect();

    let mut snappiness = SnappinessSection {
        e2e_presented_ms: (!e2e_samples.is_empty()).then(|| DistStats::from_us(&e2e_samples)),
        ..Default::default()
    };

    let mut notes = Vec::new();
    let mut mouse = MouseMetrics::default();
    let mut keyboard = KeyboardMetrics::default();
    let system = SystemMetrics {
        ttff_ms: ttff_stats,
        ..Default::default()
    };
    match suite {
        BenchSuite::S4StepI2p | BenchSuite::S9MouseQuicE2e => {
            let mut i2p_us = Vec::new();
            let mut inj_to_app = Vec::new();
            let mut app_to_src = Vec::new();
            for st in steps {
                if let Some(hit) = obs_list.iter().find(|o| {
                    o.presented_us >= st.emit_us
                        && o.meta.barcode.is_some_and(|bc| {
                            bc.cursor_x.abs_diff(st.target_x) <= 3
                                && bc.cursor_y.abs_diff(st.target_y) <= 3
                        })
                }) {
                    i2p_us.push(hit.presented_us - st.emit_us);
                    if let Some(ev_us) = hit.meta.event_recv_us {
                        if ev_us >= st.emit_us {
                            inj_to_app.push(ev_us - st.emit_us);
                        }
                        if let Some(src_us) = hit.meta.src_presented_us {
                            if src_us >= ev_us {
                                app_to_src.push(src_us - ev_us);
                            }
                        }
                    }
                }
            }
            if !i2p_us.is_empty() {
                snappiness.step_i2p_ms = Some(DistStats::from_us(&i2p_us));
            } else {
                notes
                    .push("S4 未匹配到阶跃坐标命中帧（请检查辅助功能权限及前台探针窗状态）".into());
            }
            if !inj_to_app.is_empty() {
                snappiness.inject_to_app_ms = Some(DistStats::from_us(&inj_to_app));
            }
            if !app_to_src.is_empty() {
                snappiness.app_to_src_present_ms = Some(DistStats::from_us(&app_to_src));
            }
        }
        BenchSuite::S5DragTrack => {
            let mut spatial_px = Vec::new();
            let mut equiv_ms = Vec::new();
            let mut outlier_dbg: Vec<(f64, u64, f64, f64, u16, u16, u16)> = Vec::new();
            let t_warmup = drag_pts.first().map_or(0, |p| p.0 + 500_000);
            let t_end = drag_pts.last().map_or(u64::MAX, |p| p.0);
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
                // 在拖动轨迹时间表里线性插值查本地光标理想位置。
                if let Some((lx, ly)) = interpolate_track(drag_pts, o.presented_us) {
                    let dx = lx - f64::from(bc.cursor_x);
                    let dy = ly - f64::from(bc.cursor_y);
                    let dist = (dx * dx + dy * dy).sqrt();
                    spatial_px.push(dist);
                    equiv_ms.push((dist / 1200.0) * 1000.0);
                    outlier_dbg.push((
                        dist,
                        o.presented_us,
                        lx,
                        ly,
                        bc.cursor_x,
                        bc.cursor_y,
                        bc.event_seq,
                    ));
                }
            }
            if !spatial_px.is_empty() {
                let d_lag = DistStats::from_ms(&equiv_ms);
                let d_spatial = DistStats::from_ms(&spatial_px);
                if d_spatial.p95 > 120.0 {
                    outlier_dbg
                        .sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                    for (dist, ts, lx, ly, cx, cy, seq) in outlier_dbg.iter().take(3) {
                        eprintln!(
                            "[bench S5 outlier] dist={dist:.1}px ts={ts} ideal=({lx:.1},{ly:.1}) barcode=({cx},{cy}) seq={seq}"
                        );
                    }
                }
                snappiness.lag_jitter_ms = Some((d_lag.p95 - d_lag.p50).max(0.0));
                snappiness.drag_spatial_lag_px = Some(d_spatial);
                snappiness.drag_equiv_lag_ms = Some(d_lag);
            } else {
                notes.push("S5 未收集到连续拖动有效轨迹样本".into());
            }
        }
        BenchSuite::S6IdleWakeup => {
            let (first_us, ramp_us, breakdown_note) =
                compute_s6_wakeup_breakdown(obs_list, wakeup_ts, fps);
            if !first_us.is_empty() {
                snappiness.wakeup_first_frame_ms = Some(DistStats::from_us(&first_us));
            }
            if !ramp_us.is_empty() {
                snappiness.wakeup_ramp_ms = Some(DistStats::from_us(&ramp_us));
            }
            if let Some(note) = breakdown_note {
                notes.push(note);
            }
        }
        BenchSuite::S1StaticIdle => {
            notes.push(
                "S1 静屏零编码测试：抑制同屏 Metal 上屏，避免 WindowServer 同屏合成自唤醒 SCK"
                    .into(),
            );
        }
        _ => {}
    }
    populate_mouse_report(
        suite,
        obs_list,
        drag_pts,
        mouse_state,
        &mut snappiness,
        &mut mouse,
        &mut notes,
    );
    populate_keyboard_report(obs_list, key_events, kb_state, &mut keyboard, &mut notes);

    let encoded_frames = if suite == BenchSuite::S1StaticIdle {
        decoded_frames
    } else {
        obs_list.len() as u64
    };
    let encoded_fps = encoded_frames as f64 / duration_s.max(0.001);

    ScenarioReport {
        id: id.to_string(),
        target_fps: fps,
        duration_s,
        runs_merged: 1,
        iqr_warning: false,
        encoded_frames,
        encoded_fps,
        capture: CaptureSection {
            captured_frames,
            barcode_decoded_frames,
            skipped_source_frames,
            mailbox_overwrites,
            stimulus_to_pts_ms: DistStats::from_us(a1_samples),
            pts_to_callback_ms: DistStats::from_us(a2_samples),
            mailbox_wait_ms: DistStats::from_us(a3_samples),
            capture_to_encode_ms: DistStats::from_us(a4_samples),
            stimulus_to_pts_hist_1ms: histogram_1ms_from_us(a1_samples),
        },
        snappiness,
        mouse,
        keyboard,
        system,
        pacing,
        notes,
    }
}

fn compute_s6_wakeup_breakdown(
    obs_list: &[PresentedObservation],
    wakeup_ts: &[u64],
    fps: u32,
) -> (Vec<u64>, Vec<u64>, Option<String>) {
    let mut first_us = Vec::new();
    let mut ramp_us = Vec::new();
    let mut trig_to_inj = Vec::new();
    let mut inj_to_src = Vec::new();
    let mut src_to_cb = Vec::new();
    let mut cb_to_enc = Vec::new();
    let mut enc_to_dec = Vec::new();
    let mut dec_to_pres = Vec::new();
    let target_dt_us = (1_000_000 / u64::from(fps.max(1))) * 5 / 4; // 1.25T

    for &t_emit in wakeup_ts {
        let first_pos = obs_list.iter().position(|o| {
            o.presented_us >= t_emit
                && o.presented_us - t_emit <= 1_000_000
                && o.meta.event_recv_us.is_some_and(|er| er >= t_emit)
        });
        if let Some(start_idx) = first_pos {
            let first_obs = &obs_list[start_idx];
            first_us.push(first_obs.presented_us - t_emit);
            if let Some(er) = first_obs.meta.event_recv_us {
                if er >= t_emit {
                    trig_to_inj.push(er - t_emit);
                }
                if let Some(sp) = first_obs.meta.src_presented_us {
                    if sp >= er {
                        inj_to_src.push(sp - er);
                    }
                    if first_obs.meta.sck_cb_us >= sp {
                        src_to_cb.push(first_obs.meta.sck_cb_us - sp);
                    }
                }
            }
            if first_obs.meta.sck_cb_us > 0
                && first_obs.meta.encode_done_us >= first_obs.meta.sck_cb_us
            {
                cb_to_enc.push(first_obs.meta.encode_done_us - first_obs.meta.sck_cb_us);
            }
            if first_obs.meta.encode_done_us > 0
                && first_obs.meta.decode_done_us >= first_obs.meta.encode_done_us
            {
                enc_to_dec.push(first_obs.meta.decode_done_us - first_obs.meta.encode_done_us);
            }
            if first_obs.meta.decode_done_us > 0
                && first_obs.presented_us >= first_obs.meta.decode_done_us
            {
                dec_to_pres.push(first_obs.presented_us - first_obs.meta.decode_done_us);
            }
            // 连续 3 个满足 Δ_i <= 1.25T 的第 1 帧作为爬坡完成点。
            let slice = &obs_list[start_idx..];
            let mut streak = 0u32;
            for w in slice.windows(2) {
                if w[1].presented_us - w[0].presented_us <= target_dt_us {
                    streak += 1;
                    if streak >= 3 {
                        ramp_us.push(w[1].presented_us - t_emit);
                        break;
                    }
                } else {
                    streak = 0;
                }
            }
        }
    }

    let note = if !trig_to_inj.is_empty() {
        let s1 = DistStats::from_us(&trig_to_inj);
        let s2 = DistStats::from_us(&inj_to_src);
        let s3 = DistStats::from_us(&src_to_cb);
        let s4 = DistStats::from_us(&cb_to_enc);
        let s5 = DistStats::from_us(&enc_to_dec);
        let s6 = DistStats::from_us(&dec_to_pres);
        Some(format!(
            "S6 唤醒分段拆解 (n={}): trigger->inj_app P50={:.2}ms/P95={:.2}ms, inj_app->src_pres P50={:.2}ms/P95={:.2}ms, src_pres->sck_cb P50={:.2}ms/P95={:.2}ms, sck_cb->enc_done P50={:.2}ms/P95={:.2}ms, enc->dec P50={:.2}ms/P95={:.2}ms, dec->pres P50={:.2}ms/P95={:.2}ms",
            s1.count,
            s1.p50,
            s1.p95,
            s2.p50,
            s2.p95,
            s3.p50,
            s3.p95,
            s4.p50,
            s4.p95,
            s5.p50,
            s5.p95,
            s6.p50,
            s6.p95
        ))
    } else {
        None
    };

    (first_us, ramp_us, note)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s6_wakeup_breakdown_records_substages_in_note() {
        let wakeup_ts = vec![1_000_000, 3_500_000];
        let obs = vec![
            PresentedObservation {
                presented_us: 1_045_000,
                meta: PendingPresentMeta {
                    barcode: None,
                    src_presented_us: Some(1_020_000),
                    event_recv_us: Some(1_002_000),
                    sck_cb_us: 1_026_000,
                    encode_done_us: 1_030_000,
                    decode_done_us: 1_032_000,
                    click_down_seq: 0,
                    click_count: 0,
                    scroll_seq: 0,
                    key_down_seq: 0,
                    key_up_seq: 0,
                    last_key_mods: 0,
                    key_is_down: false,
                    key_recv_us: None,
                },
            },
            PresentedObservation {
                presented_us: 3_550_000,
                meta: PendingPresentMeta {
                    barcode: None,
                    src_presented_us: Some(3_522_000),
                    event_recv_us: Some(3_503_000),
                    sck_cb_us: 3_528_000,
                    encode_done_us: 3_533_000,
                    decode_done_us: 3_535_000,
                    click_down_seq: 0,
                    click_count: 0,
                    scroll_seq: 0,
                    key_down_seq: 0,
                    key_up_seq: 0,
                    last_key_mods: 0,
                    key_is_down: false,
                    key_recv_us: None,
                },
            },
        ];
        let (first_us, _ramp_us, note) = compute_s6_wakeup_breakdown(&obs, &wakeup_ts, 120);
        assert_eq!(first_us, vec![45_000, 50_000]);
        let note = note.expect("breakdown note");
        assert!(note.contains("trigger->inj_app"));
        assert!(note.contains("inj_app->src_pres"));
        assert!(note.contains("src_pres->sck_cb"));
        assert!(note.contains("sck_cb->enc_done"));
        assert!(note.contains("enc->dec"));
        assert!(note.contains("dec->pres"));
        assert!(note.contains("n=2"));
    }
}

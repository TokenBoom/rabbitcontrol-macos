//! 无高刷新率物理屏幕限制的高熵合成真机管线吞吐基准（Item 5）。
//!
//! 构建由 8 块 IOSurface `'420v'` GPU 帧缓冲组成的高熵动态池，依次经过：
//! 真实 `VtEncoder`（HEVC/H.264）→ `inproc` 或真实本机 `QUIC + Pacer` →
//! 真实 `VtDecoder`（`async_decode`）→ `MetalOffscreenSink`（零拷贝离屏渲染）。

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use rc_capture::{CaptureBackend, CapturedFrame, DisplayInfo, FrameTexture, InvalidationReason};
use rc_decode_vt::{VtDecoder, VtDecoderConfig};
use rc_encode::Codec;
use rc_net::quic::{Identity, QuicEndpoint};
use rc_net::{NetConfig, NetEvent, Role, Transport};
use rc_pipeline::{ControllerPipeline, DiffMode, HostPipeline, PipelineConfig};
use rc_render_macos::MetalOffscreenSink;
use rc_session::dev_cert::{DEV_CERT_PEM, DEV_KEY_PEM, DEV_SERVER_NAME};
use rc_trace::{Mark, Outcome};
use rc_types::{CaptureSeq, DisplayId, Resolution, Result as RcResult, Timestamp};

/// 运行传输模式。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SynthTransportMode {
    /// 同进程直连（测试硬件编解码与管线吞吐极限）。
    Inproc,
    /// 本机回环真实 QUIC + Pacer + 重组。
    Quic,
}

impl SynthTransportMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Inproc => "inproc",
            Self::Quic => "quic",
        }
    }
}

/// `bench synth` 运行配置。
pub struct SynthBenchCfg {
    pub matrix: bool,
    pub resolution: Resolution,
    pub fps: u32,
    pub bitrate_bps: Option<u32>,
    pub codec: Codec,
    pub mode: SynthTransportMode,
    pub duration: Duration,
}

mod cv_pool {
    #![allow(unsafe_code)]

    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::sync::Arc;

    use objc2_core_foundation::{
        kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFBoolean, CFDictionary,
        CFRetained, CFString,
    };
    use objc2_core_video::{
        kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVReturnSuccess, CVPixelBuffer,
        CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
        CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeightOfPlane,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };

    struct OwnedBuffer(#[allow(dead_code)] CFRetained<CVPixelBuffer>);
    // SAFETY: CVPixelBuffer 池构造完成后变为只读共享（VT 与 Metal 仅读取像素与纹理句柄），
    // `CFRetained` 引用计数本身线程安全。
    unsafe impl Send for OwnedBuffer {}
    // SAFETY: 同上，池构造后不做 CPU 可变写入。
    unsafe impl Sync for OwnedBuffer {}

    #[derive(Clone)]
    pub(super) struct PoolFrame {
        pub ptr: *mut c_void,
        pub guard: Arc<dyn std::any::Any + Send + Sync>,
    }

    // SAFETY: `ptr` 指向由 `guard` (`Arc<OwnedBuffer>`) 保活的只读 `CVPixelBuffer`。
    unsafe impl Send for PoolFrame {}

    fn iosurface_metal_attrs() -> Result<CFRetained<CFDictionary>, String> {
        // SAFETY: 只读取 CoreVideo SDK 常量属性键指针，进程生命期内只读有效。
        let (io_key, metal_key) = unsafe {
            (
                kCVPixelBufferIOSurfacePropertiesKey,
                kCVPixelBufferMetalCompatibilityKey,
            )
        };
        // SAFETY: 空 CFDictionary 作为 IOSurfaceProperties 合法传入，表示使用系统默认属性。
        let empty_dict = unsafe {
            CFDictionary::new(
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        }
        .ok_or("创建空 IOSurface 属性字典失败")?;
        let metal_true = CFBoolean::new(true);
        let keys: [*const c_void; 2] = [
            io_key as *const CFString as *const c_void,
            metal_key as *const CFString as *const c_void,
        ];
        let values: [*const c_void; 2] = [
            CFRetained::as_ptr(&empty_dict)
                .as_ptr()
                .cast_const()
                .cast::<c_void>(),
            metal_true as *const CFBoolean as *const c_void,
        ];
        // SAFETY: 2 个键值对指针与回调表在调用期间有效，构造 +1 CFDictionary。
        let dict = unsafe {
            CFDictionary::new(
                None,
                keys.as_ptr() as *mut *const c_void,
                values.as_ptr() as *mut *const c_void,
                2,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        }
        .ok_or("创建 PixelBuffer IOSurface/Metal 属性字典失败")?;
        Ok(dict)
    }

    /// 预分配 `pool_len` 块高熵动态 '420v' IOSurface 帧缓冲，避免 P 帧跳过导致虚高帧率。
    pub(super) fn build_dynamic_pool(
        width: u32,
        height: u32,
        pool_len: usize,
    ) -> Result<Vec<PoolFrame>, String> {
        let attrs = iosurface_metal_attrs()?;
        let mut pool = Vec::with_capacity(pool_len);
        for frame_idx in 0..pool_len {
            let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
            // SAFETY: out 为合法栈出参，attrs 在调用期存活，420v 偶数尺寸由调用方保证。
            let rv = unsafe {
                CVPixelBufferCreate(
                    None,
                    width as usize,
                    height as usize,
                    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                    Some(&attrs),
                    NonNull::from(&mut out),
                )
            };
            if rv != kCVReturnSuccess {
                return Err(format!("CVPixelBufferCreate 失败: CVReturn={rv}"));
            }
            let buf_ptr = NonNull::new(out).ok_or("CVPixelBufferCreate 返回空指针")?;
            // SAFETY: Create 成功时 out 为 +1 所有权引用。
            let buf = unsafe { CFRetained::from_raw(buf_ptr) };

            // SAFETY: Lock/Unlock 成对执行，初始化后此缓冲只读。
            let lock_rv =
                unsafe { CVPixelBufferLockBaseAddress(&buf, CVPixelBufferLockFlags::empty()) };
            if lock_rv != kCVReturnSuccess {
                return Err(format!(
                    "CVPixelBufferLockBaseAddress 失败: CVReturn={lock_rv}"
                ));
            }

            // 写入结构化高动态桌面图案：整屏平稳滚动斜向渐变 + 清晰窗口网格面板 +
            // 类文字高对比字形条纹 + 移动光标块，确保 P 帧不可全零跳过同时保持桌面级空间可压缩性。
            // SAFETY: Plane 0 为 Y 平面，锁定期间 `base` 对 `rows * stride` 合法可写。
            unsafe {
                let y_base = CVPixelBufferGetBaseAddressOfPlane(&buf, 0).cast::<u8>();
                let y_stride = CVPixelBufferGetBytesPerRowOfPlane(&buf, 0);
                let y_rows = CVPixelBufferGetHeightOfPlane(&buf, 0);
                let shift_x = frame_idx * 12;
                let shift_y = frame_idx * 8;
                let cursor_x = ((frame_idx * 73) % (width as usize).saturating_sub(160).max(1))
                    .min((width as usize).saturating_sub(160));
                let cursor_y = ((frame_idx * 47) % y_rows.saturating_sub(120).max(1))
                    .min(y_rows.saturating_sub(120));
                for y in 0..y_rows {
                    let row =
                        std::slice::from_raw_parts_mut(y_base.add(y * y_stride), width as usize);
                    let sy = y + shift_y;
                    let text_band = ((sy / 24) & 3) == 1 && (sy % 24) >= 6 && (sy % 24) <= 14;
                    let in_cursor_y = y >= cursor_y && y < cursor_y + 120;
                    for (x, px) in row.iter_mut().enumerate() {
                        let sx = x + shift_x;
                        let in_cursor = in_cursor_y && x >= cursor_x && x < cursor_x + 160;
                        let grid_line = (sx & 127) < 2 || (sy & 127) < 2;
                        let text_glyph = text_band && ((sx / 10) % 7 != 0) && ((sx & 1) == 0);
                        let grad = (((sx >> 3) + (sy >> 3)) & 0x5f) as u8;
                        let val = if in_cursor {
                            if (x + y) & 16 == 0 {
                                224
                            } else {
                                36
                            }
                        } else if grid_line {
                            200
                        } else if text_glyph {
                            215
                        } else {
                            44 + grad
                        };
                        *px = val;
                    }
                }

                let uv_base = CVPixelBufferGetBaseAddressOfPlane(&buf, 1).cast::<u8>();
                let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(&buf, 1);
                let uv_rows = CVPixelBufferGetHeightOfPlane(&buf, 1);
                let u_shift = frame_idx * 6;
                for y in 0..uv_rows {
                    let row = std::slice::from_raw_parts_mut(
                        uv_base.add(y * uv_stride),
                        (width as usize) & !1,
                    );
                    let band = ((y + u_shift) >> 6) & 3;
                    let (cb, cr) = match band {
                        0 => (110u8, 146u8),
                        1 => (142u8, 118u8),
                        2 => (96u8, 162u8),
                        _ => (128u8, 128u8),
                    };
                    for pair in row.chunks_exact_mut(2) {
                        pair[0] = cb;
                        pair[1] = cr;
                    }
                }
                CVPixelBufferUnlockBaseAddress(&buf, CVPixelBufferLockFlags::empty());
            }

            let raw_ptr = CFRetained::as_ptr(&buf).as_ptr().cast::<c_void>();
            let guard: Arc<dyn std::any::Any + Send + Sync> = Arc::new(OwnedBuffer(buf));
            pool.push(PoolFrame {
                ptr: raw_ptr,
                guard,
            });
        }
        Ok(pool)
    }
}

struct SynthCaptureBackend {
    info: DisplayInfo,
    pool: Vec<cv_pool::PoolFrame>,
    origin: Instant,
    interval: Duration,
    next_due: Instant,
    seq: u64,
}

impl SynthCaptureBackend {
    fn new(width: u32, height: u32, fps: u32, origin: Instant) -> Result<Self, String> {
        let even = Resolution::new(width, height).to_even();
        let pool = cv_pool::build_dynamic_pool(even.width, even.height, 16)?;
        let interval = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
        Ok(Self {
            info: DisplayInfo {
                id: DisplayId(0),
                resolution: even,
                scale: 1.0,
                hdr: false,
            },
            pool,
            origin,
            interval,
            next_due: Instant::now(),
            seq: 0,
        })
    }
}

impl CaptureBackend for SynthCaptureBackend {
    fn next_frame(&mut self, timeout: Duration) -> RcResult<Option<CapturedFrame>> {
        let now = Instant::now();
        let limit = now + timeout;
        if self.next_due > limit {
            if timeout > Duration::from_micros(200) {
                thread::sleep(timeout - Duration::from_micros(150));
            }
            return Ok(None);
        }
        while Instant::now() < self.next_due {
            let rem = self.next_due.saturating_duration_since(Instant::now());
            if rem > Duration::from_millis(2) {
                thread::sleep(rem - Duration::from_micros(1_200));
            } else {
                std::hint::spin_loop();
            }
        }
        let now_fire = Instant::now();
        self.next_due += self.interval;
        if self.next_due + self.interval < now_fire {
            self.next_due = now_fire + self.interval;
        }
        let idx = (self.seq as usize) % self.pool.len();
        let frame = &self.pool[idx];
        let ts_us = self.origin.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
        self.seq = self.seq.saturating_add(1);
        Ok(Some(CapturedFrame {
            texture: FrameTexture::CVPixelBuffer(frame.ptr),
            timestamp: Timestamp(ts_us),
            dirty: None,
            seq: CaptureSeq(self.seq),
            is_repeat: false,
            resource_guard: Some(Arc::clone(&frame.guard)),
        }))
    }

    fn poll_invalidation(&mut self) -> Option<InvalidationReason> {
        None
    }

    fn rebuild(&mut self) -> RcResult<()> {
        Ok(())
    }

    fn display_info(&self) -> DisplayInfo {
        self.info
    }
}

struct ScenarioSpec {
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
}

struct ScenarioStats {
    codec: Codec,
    mode: SynthTransportMode,
    elapsed_s: f64,
    encoded_frames: u64,
    displayed_frames: u64,
    dropped_frames: u64,
    host_dropped: u64,
    ctrl_dropped: u64,
    decode_errors: u64,
    lost_in_network: u64,
    ctrl_frames: u64,
    actual_mbps: f64,
    enc_p50_ms: f64,
    enc_p95_ms: f64,
    dec_p50_ms: f64,
    dec_p95_ms: f64,
    e2e_p50_ms: f64,
    e2e_p95_ms: f64,
}

fn percentile_ms(samples_us: &mut [u32], pct: f64) -> f64 {
    if samples_us.is_empty() {
        return 0.0;
    }
    samples_us.sort_unstable();
    let rank = ((pct / 100.0) * samples_us.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(samples_us.len() - 1);
    f64::from(samples_us[idx]) / 1000.0
}

fn run_single_scenario(
    spec: ScenarioSpec,
    codec: Codec,
    mode: SynthTransportMode,
    duration: Duration,
) -> Result<ScenarioStats, String> {
    let origin = Instant::now();
    let clock: Arc<dyn Fn() -> u32 + Send + Sync> =
        Arc::new(move || origin.elapsed().as_micros().min(u128::from(u32::MAX)) as u32);

    let capture = SynthCaptureBackend::new(spec.width, spec.height, spec.fps, origin)?;
    let even_res = Resolution::new(spec.width, spec.height).to_even();
    let pcfg = PipelineConfig {
        resolution: even_res,
        fps: spec.fps,
        bitrate_bps: spec.bitrate_bps,
        prefer_hardware: true,
        codec,
        target_latency_us: 22_000,
        trace_capacity: 16_384,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: Some(0),
        async_decode: true,
        max_packet_payload: rc_net::quic::max_media_payload(rc_net::quic::DEFAULT_MTU),
        ..PipelineConfig::default()
    };

    let enc = rc_encode_vt::new_encoder(rc_pipeline::default_encoder_config(&pcfg))
        .map_err(|e| format!("VtEncoder 初始化失败: {e}"))?;
    let dec = VtDecoder::new(VtDecoderConfig {
        codec,
        max_resolution: Resolution::new(even_res.width.max(4096), even_res.height.max(4096)),
        ..VtDecoderConfig::default()
    })
    .map_err(|e| format!("VtDecoder 初始化失败: {e}"))?;
    let sink = MetalOffscreenSink::new((even_res.width, even_res.height), Some(origin))
        .map_err(|e| format!("MetalOffscreenSink 初始化失败: {e}"))?;

    let mut host = HostPipeline::with_backends_paced(
        PipelineConfig {
            async_decode: false,
            ..pcfg.clone()
        },
        Box::new(capture),
        Box::new(enc),
        false,
    )
    .map_err(|e| format!("HostPipeline 初始化失败: {e}"))?;
    host.set_clock(Arc::clone(&clock));

    let mut ctrl = ControllerPipeline::with_backends(pcfg.clone(), Box::new(dec), Box::new(sink))
        .map_err(|e| format!("ControllerPipeline 初始化失败: {e}"))?;
    ctrl.set_clock(Arc::clone(&clock));

    let stop = Arc::new(AtomicBool::new(false));
    let mut wire_bytes: u64 = 0;

    let run_start = Instant::now();
    if mode == SynthTransportMode::Inproc {
        let (pkt_tx, pkt_rx) = mpsc::sync_channel::<Vec<Vec<u8>>>(1024);
        let idr_flag = Arc::new(AtomicBool::new(false));
        let stop_rx = Arc::clone(&stop);
        let idr_rx = Arc::clone(&idr_flag);
        let clock_rx = Arc::clone(&clock);

        let rx_thread = thread::Builder::new()
            .name("synth-ctrl-inproc".into())
            .spawn(move || {
                while !stop_rx.load(Ordering::Acquire) {
                    match pkt_rx.recv_timeout(Duration::from_millis(5)) {
                        Ok(batch) => {
                            let now_us = clock_rx();
                            for pkt in batch {
                                if let Err(e) = ctrl.push_datagram(&pkt, now_us) {
                                    eprintln!("[synth inproc] push_datagram err: {e}");
                                    stop_rx.store(true, Ordering::Release);
                                    break;
                                }
                            }
                            while let Ok(more) = pkt_rx.try_recv() {
                                let now_us = clock_rx();
                                for pkt in more {
                                    if let Err(e) = ctrl.push_datagram(&pkt, now_us) {
                                        eprintln!("[synth inproc] push_datagram err: {e}");
                                        stop_rx.store(true, Ordering::Release);
                                        break;
                                    }
                                }
                            }
                            if ctrl.take_keyframe_request() {
                                idr_rx.store(true, Ordering::Release);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                while let Ok(more) = pkt_rx.try_recv() {
                    let now_us = clock_rx();
                    for pkt in more {
                        let _ = ctrl.push_datagram(&pkt, now_us);
                    }
                }
                ctrl.shutdown();
                ctrl
            })
            .map_err(|e| format!("spawn inproc ctrl failed: {e}"))?;

        let deadline = run_start + duration;
        while Instant::now() < deadline {
            if idr_flag.swap(false, Ordering::AcqRel) {
                host.request_idr();
            }
            if let Ok(Some(pkts)) = host.tick(clock()) {
                for p in &pkts {
                    wire_bytes = wire_bytes.saturating_add(p.len() as u64);
                }
                if pkt_tx.send(pkts).is_err() {
                    break;
                }
            }
            if !host.has_pending_work() {
                host.wait_event(Duration::from_micros(500));
            }
        }
        stop.store(true, Ordering::Release);
        drop(pkt_tx);
        host.stop_capture();
        host.shutdown();
        ctrl = rx_thread
            .join()
            .map_err(|_| "inproc ctrl thread panicked".to_string())?;
    } else {
        let ident =
            Identity::from_pem(DEV_CERT_PEM, DEV_KEY_PEM).map_err(|e| format!("dev cert: {e}"))?;
        let certs =
            Identity::certs_from_pem(DEV_CERT_PEM).map_err(|e| format!("dev certs: {e}"))?;
        let loop_addr: SocketAddr = "127.0.0.1:0".parse().expect("valid addr");
        let host_ep = QuicEndpoint::new(&NetConfig::new(Role::Host, loop_addr), &ident)
            .map_err(|e| format!("host ep: {e}"))?;
        let host_addr = host_ep
            .local_addr()
            .map_err(|e| format!("host addr: {e}"))?;
        let client_ep = QuicEndpoint::new(&NetConfig::new(Role::Controller, loop_addr), &ident)
            .map_err(|e| format!("client ep: {e}"))?;

        let accepter = thread::spawn(move || host_ep.accept(Duration::from_secs(5)));
        let mut client_tx = client_ep
            .connect(host_addr, &certs, DEV_SERVER_NAME, Duration::from_secs(5))
            .map_err(|e| format!("quic connect: {e}"))?;
        let mut host_tx = accepter
            .join()
            .map_err(|_| "accept thread panicked".to_string())?
            .map_err(|e| format!("quic accept: {e}"))?;
        let pacing_bps = u64::from(spec.bitrate_bps)
            .saturating_mul(2)
            .max(250_000_000);
        host_tx.set_pacing_rate(pacing_bps);

        let stop_rx = Arc::clone(&stop);
        let clock_rx = Arc::clone(&clock);
        let rx_thread = thread::Builder::new()
            .name("synth-ctrl-quic".into())
            .spawn(move || {
                while !stop_rx.load(Ordering::Acquire) {
                    let events = match client_tx.poll(Duration::from_micros(250)) {
                        Ok(ev) => ev,
                        Err(_) => break,
                    };
                    let now_us = clock_rx();
                    for ev in events {
                        if let NetEvent::Datagram(pkt) = ev {
                            if let Err(e) = ctrl.push_datagram(&pkt, now_us) {
                                eprintln!("[synth quic] push_datagram err: {e}");
                                stop_rx.store(true, Ordering::Release);
                                break;
                            }
                        }
                    }
                    if ctrl.take_keyframe_request() {
                        let _ = client_tx.send_reliable_urgent(&[1u8]);
                    }
                }
                ctrl.shutdown();
                ctrl
            })
            .map_err(|e| format!("spawn quic ctrl failed: {e}"))?;

        let deadline = run_start + duration;
        while Instant::now() < deadline {
            if let Ok(evs) = host_tx.poll(Duration::ZERO) {
                for ev in evs {
                    if matches!(ev, NetEvent::ReliableUrgent(_) | NetEvent::Reliable(_)) {
                        host.request_idr();
                    }
                }
            }
            if let Ok(Some(pkts)) = host.tick(clock()) {
                for p in pkts {
                    wire_bytes = wire_bytes.saturating_add(p.len() as u64);
                    let _ = host_tx.send_datagram(&p);
                }
            }
            if !host.has_pending_work() {
                host.wait_event(Duration::from_micros(300));
            }
        }
        stop.store(true, Ordering::Release);
        host.stop_capture();
        host.shutdown();
        ctrl = rx_thread
            .join()
            .map_err(|_| "quic ctrl thread panicked".to_string())?;
    }

    let elapsed_s = run_start.elapsed().as_secs_f64().max(0.001);
    let mut enc_us = Vec::new();
    let mut host_cap_map = std::collections::HashMap::new();
    for f in host.trace().frames() {
        if let (Some(s), Some(e)) = (f.at(Mark::EncodeStart), f.at(Mark::EncodeDone)) {
            enc_us.push(e.0.saturating_sub(s.0));
        }
        if let Some(c) = f.at(Mark::Captured) {
            host_cap_map.insert(f.seq(), c.0);
        }
    }

    let mut dec_us = Vec::new();
    let mut e2e_us = Vec::new();
    for f in ctrl.trace().frames() {
        if f.outcome() != Some(Outcome::Displayed) {
            continue;
        }
        if let (Some(r), Some(d)) = (f.at(Mark::Received), f.at(Mark::Decoded)) {
            dec_us.push(d.0.saturating_sub(r.0));
        }
        if let Some(pres) = f.at(Mark::Presented) {
            if let Some(&cap) = host_cap_map.get(&f.seq()) {
                e2e_us.push(pres.0.saturating_sub(cap));
            } else if let Some(recv) = f.at(Mark::Received) {
                e2e_us.push(pres.0.saturating_sub(recv.0));
            }
        }
    }

    let host_sum = host.trace().summary();
    let ctrl_sum = ctrl.trace().summary();
    let displayed = ctrl.displayed().max(ctrl_sum.displayed);
    let encoded = host_sum.frames;
    let dropped = host_sum.dropped_total() + ctrl_sum.dropped_total();

    Ok(ScenarioStats {
        codec,
        mode,
        elapsed_s,
        encoded_frames: encoded,
        displayed_frames: displayed,
        dropped_frames: dropped,
        host_dropped: host_sum.dropped_total(),
        ctrl_dropped: ctrl_sum.dropped_total(),
        decode_errors: ctrl_sum.decode_errors,
        lost_in_network: ctrl_sum.lost_in_network,
        ctrl_frames: ctrl_sum.frames,
        actual_mbps: (wire_bytes as f64 * 8.0) / (elapsed_s * 1_000_000.0),
        enc_p50_ms: percentile_ms(&mut enc_us, 50.0),
        enc_p95_ms: percentile_ms(&mut enc_us, 95.0),
        dec_p50_ms: percentile_ms(&mut dec_us, 50.0),
        dec_p95_ms: percentile_ms(&mut dec_us, 95.0),
        e2e_p50_ms: percentile_ms(&mut e2e_us, 50.0),
        e2e_p95_ms: percentile_ms(&mut e2e_us, 95.0),
    })
}

/// 运行 `bench synth` 基准测试（支持 `--matrix` 或单场景）。
pub fn execute_synth_bench(cfg: SynthBenchCfg) -> Result<(), String> {
    let scenarios = if cfg.matrix {
        vec![
            ScenarioSpec {
                width: 3440,
                height: 1440,
                fps: 120,
                bitrate_bps: cfg.bitrate_bps.unwrap_or(60_000_000),
            },
            ScenarioSpec {
                width: 3840,
                height: 2160,
                fps: 120,
                bitrate_bps: cfg.bitrate_bps.unwrap_or(80_000_000),
            },
            ScenarioSpec {
                width: 2560,
                height: 1440,
                fps: 240,
                bitrate_bps: cfg.bitrate_bps.unwrap_or(100_000_000),
            },
            ScenarioSpec {
                width: 1920,
                height: 1080,
                fps: 380,
                bitrate_bps: cfg.bitrate_bps.unwrap_or(100_000_000),
            },
        ]
    } else {
        let bps = cfg.bitrate_bps.unwrap_or_else(|| {
            rc_client_core::app_prefs::recommended_auto_bitrate_bps(
                cfg.resolution.width,
                cfg.resolution.height,
                cfg.fps,
            )
        });
        vec![ScenarioSpec {
            width: cfg.resolution.width,
            height: cfg.resolution.height,
            fps: cfg.fps,
            bitrate_bps: bps,
        }]
    };

    println!(
        "RabbitControl 合成高熵真机管线基准（codec={}, mode={}, duration={:.1}s）",
        cfg.codec.as_str(),
        cfg.mode.as_str(),
        cfg.duration.as_secs_f64()
    );
    println!(
        "{:<16} {:>7} {:>8} {:>9} {:>7} {:>8} {:>12} {:>12} {:>12}",
        "Target",
        "Codec",
        "Mode",
        "Achieved",
        "Disp/Enc",
        "Mbps",
        "Enc P50/95",
        "Dec P50/95",
        "E2E P50/95"
    );
    for spec in scenarios {
        let target_label = format!("{}x{}@{}", spec.width, spec.height, spec.fps);
        let st = run_single_scenario(spec, cfg.codec, cfg.mode, cfg.duration)?;
        let achieved_fps = st.displayed_frames as f64 / st.elapsed_s;
        println!(
            "{:<16} {:>7} {:>8} {:>6.1}fps {:>4}/{:<4} {:>6.1}M {:>5.2}/{:<5.2}ms {:>5.2}/{:<5.2}ms {:>5.2}/{:<5.2}ms (drops={} h={} c={} decerr={} lost={} c_tot={})",
            target_label,
            st.codec.as_str(),
            st.mode.as_str(),
            achieved_fps,
            st.displayed_frames,
            st.encoded_frames,
            st.actual_mbps,
            st.enc_p50_ms,
            st.enc_p95_ms,
            st.dec_p50_ms,
            st.dec_p95_ms,
            st.e2e_p50_ms,
            st.e2e_p95_ms,
            st.dropped_frames,
            st.host_dropped,
            st.ctrl_dropped,
            st.decode_errors,
            st.lost_in_network,
            st.ctrl_frames,
        );
    }
    Ok(())
}

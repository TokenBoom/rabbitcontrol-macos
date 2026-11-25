//! macOS 编解码隔离基准（`rc-encode-vt` + `rc-decode-vt` 真实生产后端直测）。
//!
//! 与 `bench synth`（完整 HostPipeline + QUIC + ControllerPipeline + Metal）互补，
//! 本模块剥离采集/网络/调度/渲染，直接对生产 `VtEncoder`（同步与 2/4-deep 异步流水线）
//! 和 `VtDecoder` 做逐维压测，用于精确定位编解码本身的性能上限与可优化点。

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rc_capture::{CapturedFrame, FrameTexture};
use rc_decode::{DecodeOutput, Decoder};
use rc_decode_vt::{VtDecoder, VtDecoderConfig};
use rc_encode::{Codec, EncodedFrame, Encoder, EncoderConfig, Preset};
use rc_types::{CaptureSeq, Resolution, Timestamp};

/// `bench codec` 运行配置。
pub struct CodecBenchCfg {
    /// 是否运行全矩阵（1080p / 1440p / 3440x1440 / 4K × H.264 & HEVC）。
    pub matrix: bool,
    /// 单场景分辨率（非 `--matrix` 时生效）。
    pub resolution: Resolution,
    /// 单场景目标帧率（60 开启 LLRC，120 关闭 LLRC）。
    pub fps: u32,
    /// 单场景码率（bps），省略时自动按分辨率与帧率计算。
    pub bitrate_bps: Option<u32>,
    /// 单场景编解码器。
    pub codec: Codec,
    /// 每场景编码帧数（含首帧 IDR）。
    pub frames: usize,
    /// JSON 报告输出路径（可选）。
    pub report_path: Option<PathBuf>,
}

mod pool {
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
    // SAFETY: 缓冲构造完成后只读，CFRetained 引用计数线程安全。
    unsafe impl Send for OwnedBuffer {}
    // SAFETY: 同上。
    unsafe impl Sync for OwnedBuffer {}

    #[derive(Clone)]
    pub(super) struct PoolFrame {
        pub ptr: *mut c_void,
        pub guard: Arc<dyn std::any::Any + Send + Sync>,
    }

    // SAFETY: ptr 指向由 guard 保活的只读 CVPixelBuffer。
    unsafe impl Send for PoolFrame {}

    pub(super) fn build_desktop_pool(
        width: u32,
        height: u32,
        count: usize,
    ) -> Result<Vec<PoolFrame>, String> {
        // SAFETY: 读取 CoreVideo 全局属性键常量。
        let (io_key, metal_key) = unsafe {
            (
                kCVPixelBufferIOSurfacePropertiesKey,
                kCVPixelBufferMetalCompatibilityKey,
            )
        };
        // SAFETY: 空字典表示默认 IOSurface 属性。
        let empty = unsafe {
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
        let keys = [
            io_key as *const CFString as *const c_void,
            metal_key as *const CFString as *const c_void,
        ];
        let values = [
            CFRetained::as_ptr(&empty)
                .as_ptr()
                .cast_const()
                .cast::<c_void>(),
            metal_true as *const CFBoolean as *const c_void,
        ];
        // SAFETY: 栈上 2 个有效键值对构造字典。
        let attrs = unsafe {
            CFDictionary::new(
                None,
                keys.as_ptr() as *mut *const c_void,
                values.as_ptr() as *mut *const c_void,
                2,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        }
        .ok_or("创建 PixelBuffer 属性字典失败")?;

        let mut pool = Vec::with_capacity(count);
        for frame_idx in 0..count {
            let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
            // SAFETY: out 为合法栈出参，attrs 有效。
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
                return Err(format!("CVPixelBufferCreate 失败: {rv}"));
            }
            let ptr = NonNull::new(out).ok_or("CVPixelBufferCreate 返回空指针")?;
            // SAFETY: Create 成功返回 +1 所有权引用。
            let buf = unsafe { CFRetained::from_raw(ptr) };
            // SAFETY: Lock/Unlock 成对，初始化后只读。
            let lrv =
                unsafe { CVPixelBufferLockBaseAddress(&buf, CVPixelBufferLockFlags::empty()) };
            if lrv != kCVReturnSuccess {
                return Err(format!("CVPixelBufferLockBaseAddress 失败: {lrv}"));
            }
            // SAFETY: 已锁，Y/UV 平面在各自 stride*rows 内合法可写。
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
                        *px = if in_cursor {
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

#[derive(Clone, Debug)]
struct LatStats {
    min_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
}

impl LatStats {
    fn from_ms(mut xs: Vec<f64>) -> Self {
        if xs.is_empty() {
            return Self {
                min_ms: 0.0,
                p50_ms: 0.0,
                p95_ms: 0.0,
                p99_ms: 0.0,
                max_ms: 0.0,
            };
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = xs.len();
        let pct = |p: f64| {
            let rank = ((p / 100.0) * n as f64).ceil() as usize;
            xs[rank.saturating_sub(1).min(n - 1)]
        };
        Self {
            min_ms: xs[0],
            p50_ms: pct(50.0),
            p95_ms: pct(95.0),
            p99_ms: pct(99.0),
            max_ms: xs[n - 1],
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "min_ms": r2(self.min_ms),
            "p50_ms": r2(self.p50_ms),
            "p95_ms": r2(self.p95_ms),
            "p99_ms": r2(self.p99_ms),
            "max_ms": r2(self.max_ms),
        })
    }
}

fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn make_captured(frame: &pool::PoolFrame, idx: usize, fps: u32) -> CapturedFrame {
    let ts_us = ((idx as u64) * 1_000_000 / u64::from(fps.max(1))) as u32;
    CapturedFrame {
        texture: FrameTexture::CVPixelBuffer(frame.ptr),
        timestamp: Timestamp(ts_us),
        dirty: None,
        seq: CaptureSeq(idx as u64),
        is_repeat: false,
        resource_guard: Some(Arc::clone(&frame.guard)),
    }
}

struct EncodeBenchOut {
    init_ms: f64,
    max_fps: f64,
    dropped: usize,
    lat: LatStats,
    idr_kb: f64,
    p_avg_kb: f64,
    actual_mbps: f64,
    frames: Vec<EncodedFrame>,
}

fn bench_encode_sync(
    cfg: EncoderConfig,
    pool: &[pool::PoolFrame],
    frames_n: usize,
    paced: bool,
) -> Result<EncodeBenchOut, String> {
    let t_init = Instant::now();
    let mut enc =
        rc_encode_vt::new_encoder(cfg).map_err(|e| format!("VtEncoder::new 失败: {e}"))?;
    let init_ms = t_init.elapsed().as_secs_f64() * 1000.0;

    let mut out_frames = Vec::with_capacity(frames_n);
    let mut lat_ms = Vec::with_capacity(frames_n);
    let mut dropped = 0usize;
    let period = Duration::from_secs_f64(1.0 / f64::from(cfg.fps.max(1)));
    let mut next_due = Instant::now() + period;

    let wall_start = Instant::now();
    for i in 0..frames_n {
        let cf = make_captured(&pool[i % pool.len()], i, cfg.fps);
        let deadline = Timestamp(cf.timestamp.0.saturating_add(100_000));
        let t0 = Instant::now();
        match enc.encode(&cf, i == 0, deadline) {
            Ok(ef) => {
                let dt = t0.elapsed().as_secs_f64() * 1000.0;
                if i > 0 {
                    lat_ms.push(dt);
                }
                out_frames.push(ef);
            }
            Err(rc_types::Error::EncoderDroppedFrame { .. }) => {
                dropped += 1;
            }
            Err(e) => return Err(format!("sync encode 第 {i} 帧失败: {e}")),
        }
        if paced {
            if let Some(rem) = next_due.checked_duration_since(Instant::now()) {
                if rem > Duration::from_micros(500) {
                    std::thread::sleep(rem - Duration::from_micros(300));
                }
            }
            next_due += period;
        }
    }
    let wall_s = wall_start.elapsed().as_secs_f64().max(1e-9);
    let idr_kb = out_frames
        .first()
        .map(|f| f.data.len() as f64 / 1024.0)
        .unwrap_or(0.0);
    let p_bytes: usize = out_frames.iter().skip(1).map(|f| f.data.len()).sum();
    let p_cnt = out_frames.len().saturating_sub(1).max(1);
    let p_avg_kb = p_bytes as f64 / (p_cnt as f64 * 1024.0);
    let total_bytes: usize = out_frames.iter().map(|f| f.data.len()).sum();
    let logical_s = out_frames.len() as f64 / f64::from(cfg.fps.max(1));
    let actual_mbps = (total_bytes as f64 * 8.0) / (logical_s.max(1e-6) * 1e6);

    Ok(EncodeBenchOut {
        init_ms,
        max_fps: out_frames.len() as f64 / wall_s,
        dropped,
        lat: LatStats::from_ms(lat_ms),
        idr_kb,
        p_avg_kb,
        actual_mbps,
        frames: out_frames,
    })
}

fn bench_encode_pipelined(
    cfg: EncoderConfig,
    pool: &[pool::PoolFrame],
    frames_n: usize,
    depth: usize,
) -> Result<EncodeBenchOut, String> {
    let t_init = Instant::now();
    let mut enc =
        rc_encode_vt::new_encoder(cfg).map_err(|e| format!("VtEncoder::new 失败: {e}"))?;
    let init_ms = t_init.elapsed().as_secs_f64() * 1000.0;

    let mut out_frames = Vec::with_capacity(frames_n);
    let mut lat_ms = Vec::with_capacity(frames_n);
    let mut submitted: VecDeque<(u64, Instant)> = VecDeque::with_capacity(depth + 1);
    let mut dropped = 0usize;

    let wall_start = Instant::now();
    for i in 0..frames_n {
        let cf = make_captured(&pool[i % pool.len()], i, cfg.fps);
        let deadline = Timestamp(cf.timestamp.0.saturating_add(100_000));
        let token = i as u64;
        let t_sub = Instant::now();
        enc.submit_encode(token, &cf, i == 0, deadline)
            .map_err(|e| format!("submit_encode 第 {i} 帧失败: {e}"))?;
        submitted.push_back((token, t_sub));

        while enc.in_flight_count() >= depth {
            match enc.recv_encoded(Duration::from_millis(200)) {
                Ok(Some((tok, ef))) => {
                    if let Some(pos) = submitted.iter().position(|(t, _)| *t == tok) {
                        let (_, st) = submitted.remove(pos).unwrap();
                        if tok > 0 {
                            lat_ms.push(st.elapsed().as_secs_f64() * 1000.0);
                        }
                    }
                    out_frames.push(ef);
                }
                Ok(None) => return Err("recv_encoded 超时".to_string()),
                Err(rc_types::Error::EncoderDroppedFrame { .. }) => {
                    dropped += 1;
                    let _ = submitted.pop_front();
                }
                Err(e) => return Err(format!("recv_encoded 失败: {e}")),
            }
        }
    }
    while enc.in_flight_count() > 0 {
        match enc.recv_encoded(Duration::from_millis(200)) {
            Ok(Some((tok, ef))) => {
                if let Some(pos) = submitted.iter().position(|(t, _)| *t == tok) {
                    let (_, st) = submitted.remove(pos).unwrap();
                    if tok > 0 {
                        lat_ms.push(st.elapsed().as_secs_f64() * 1000.0);
                    }
                }
                out_frames.push(ef);
            }
            Ok(None) => return Err("recv_encoded 排空超时".to_string()),
            Err(rc_types::Error::EncoderDroppedFrame { .. }) => {
                dropped += 1;
                let _ = submitted.pop_front();
            }
            Err(e) => return Err(format!("recv_encoded 排空失败: {e}")),
        }
    }
    let wall_s = wall_start.elapsed().as_secs_f64().max(1e-9);
    let idr_kb = out_frames
        .first()
        .map(|f| f.data.len() as f64 / 1024.0)
        .unwrap_or(0.0);
    let p_bytes: usize = out_frames.iter().skip(1).map(|f| f.data.len()).sum();
    let p_cnt = out_frames.len().saturating_sub(1).max(1);
    let p_avg_kb = p_bytes as f64 / (p_cnt as f64 * 1024.0);
    let total_bytes: usize = out_frames.iter().map(|f| f.data.len()).sum();
    let logical_s = out_frames.len() as f64 / f64::from(cfg.fps.max(1));
    let actual_mbps = (total_bytes as f64 * 8.0) / (logical_s.max(1e-6) * 1e6);

    Ok(EncodeBenchOut {
        init_ms,
        max_fps: out_frames.len() as f64 / wall_s,
        dropped,
        lat: LatStats::from_ms(lat_ms),
        idr_kb,
        p_avg_kb,
        actual_mbps,
        frames: out_frames,
    })
}

struct DecodeBenchOut {
    cold_idr_ms: f64,
    max_fps: f64,
    lat: LatStats,
}

fn decoder_max_resolution_for(codec: Codec) -> Resolution {
    match codec {
        Codec::Hevc => Resolution::new(8192, 4320),
        _ => Resolution::new(4096, 4096),
    }
}

fn bench_decode_vt(codec: Codec, frames: &[EncodedFrame]) -> Result<DecodeBenchOut, String> {
    let mut dec = VtDecoder::new(VtDecoderConfig {
        codec,
        max_resolution: decoder_max_resolution_for(codec),
        ..VtDecoderConfig::default()
    })
    .map_err(|e| format!("VtDecoder::new 失败: {e}"))?;

    let mut cold_idr_ms = 0.0;
    let mut lat_ms = Vec::with_capacity(frames.len());
    let mut wall_start: Option<Instant> = None;

    for (i, ef) in frames.iter().enumerate() {
        let t0 = Instant::now();
        let out = dec
            .decode(&ef.data, ef.kind, Timestamp(u32::MAX))
            .map_err(|e| format!("VtDecoder::decode 第 {i} 帧失败: {e}"))?;
        let dt = t0.elapsed().as_secs_f64() * 1000.0;
        if !matches!(out, DecodeOutput::Frame(_)) {
            return Err(format!("第 {i} 帧未解出画面: {out:?}"));
        }
        if i == 0 {
            cold_idr_ms = dt;
            wall_start = Some(Instant::now());
        } else {
            lat_ms.push(dt);
        }
    }
    let wall_s = wall_start
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or(1e-6)
        .max(1e-9);
    Ok(DecodeBenchOut {
        cold_idr_ms,
        max_fps: lat_ms.len() as f64 / wall_s,
        lat: LatStats::from_ms(lat_ms),
    })
}

fn bench_decode_vt_pipelined(
    codec: Codec,
    frames: &[EncodedFrame],
    depth: usize,
) -> Result<DecodeBenchOut, String> {
    let mut dec = VtDecoder::new(VtDecoderConfig {
        codec,
        max_resolution: decoder_max_resolution_for(codec),
        ..VtDecoderConfig::default()
    })
    .map_err(|e| format!("VtDecoder::new 失败: {e}"))?;

    let first = frames.first().ok_or("空帧序列")?;
    let t_cold = Instant::now();
    let first_out = dec
        .decode(&first.data, first.kind, Timestamp(u32::MAX))
        .map_err(|e| format!("VtDecoder 首帧 IDR 失败: {e}"))?;
    let cold_idr_ms = t_cold.elapsed().as_secs_f64() * 1000.0;
    if !matches!(first_out, DecodeOutput::Frame(_)) {
        return Err(format!("首帧 IDR 未解出画面: {first_out:?}"));
    }

    let mut lat_ms = Vec::with_capacity(frames.len());
    let mut submitted: VecDeque<(u64, Instant)> = VecDeque::with_capacity(depth + 1);
    let wall_start = Instant::now();
    for (i, ef) in frames.iter().enumerate().skip(1) {
        let tok = i as u64;
        let t_sub = Instant::now();
        let st = dec
            .submit_decode(tok, &ef.data, ef.kind, Timestamp(u32::MAX))
            .map_err(|e| format!("submit_decode 第 {i} 帧失败: {e}"))?;
        if st != rc_decode::DecodeSubmitStatus::Submitted {
            return Err(format!("submit_decode 第 {i} 帧返回 {st:?}"));
        }
        submitted.push_back((tok, t_sub));

        while dec.in_flight_count() >= depth {
            match dec.recv_decoded(Duration::from_millis(200)) {
                Ok(Some((done_tok, Ok(DecodeOutput::Frame(_))))) => {
                    if let Some(pos) = submitted.iter().position(|(t, _)| *t == done_tok) {
                        let (_, st_t) = submitted.remove(pos).unwrap();
                        lat_ms.push(st_t.elapsed().as_secs_f64() * 1000.0);
                    }
                }
                Ok(Some((done_tok, Ok(other)))) => {
                    return Err(format!("recv_decoded 帧 {done_tok} 非 Frame: {other:?}"));
                }
                Ok(Some((done_tok, Err(e)))) => {
                    return Err(format!("recv_decoded 帧 {done_tok} 失败: {e}"));
                }
                Ok(None) => return Err("recv_decoded 超时".to_string()),
                Err(e) => return Err(format!("recv_decoded 失败: {e}")),
            }
        }
    }
    while dec.in_flight_count() > 0 {
        match dec.recv_decoded(Duration::from_millis(200)) {
            Ok(Some((done_tok, Ok(DecodeOutput::Frame(_))))) => {
                if let Some(pos) = submitted.iter().position(|(t, _)| *t == done_tok) {
                    let (_, st_t) = submitted.remove(pos).unwrap();
                    lat_ms.push(st_t.elapsed().as_secs_f64() * 1000.0);
                }
            }
            Ok(Some((done_tok, Ok(other)))) => {
                return Err(format!(
                    "recv_decoded 排空帧 {done_tok} 非 Frame: {other:?}"
                ));
            }
            Ok(Some((done_tok, Err(e)))) => {
                return Err(format!("recv_decoded 排空帧 {done_tok} 失败: {e}"));
            }
            Ok(None) => return Err("recv_decoded 排空超时".to_string()),
            Err(e) => return Err(format!("recv_decoded 排空失败: {e}")),
        }
    }
    let wall_s = wall_start.elapsed().as_secs_f64().max(1e-9);
    Ok(DecodeBenchOut {
        cold_idr_ms,
        max_fps: lat_ms.len() as f64 / wall_s,
        lat: LatStats::from_ms(lat_ms),
    })
}

/// 执行 `rc-cli bench codec`。
pub fn execute_codec_bench(cfg: CodecBenchCfg) -> Result<(), String> {
    let scenarios: Vec<(Resolution, u32, u32, Codec)> = if cfg.matrix {
        let mut v = Vec::new();
        for &(w, h, fps, bps) in &[
            (1920u32, 1080u32, 60u32, 12_000_000u32),
            (1920, 1080, 120, 20_000_000),
            (2560, 1440, 120, 35_000_000),
            (3440, 1440, 120, 60_000_000),
            (3840, 2160, 120, 80_000_000),
        ] {
            for &codec in &[Codec::H264, Codec::Hevc] {
                v.push((Resolution::new(w, h), fps, bps, codec));
            }
        }
        v
    } else {
        let even = cfg.resolution.to_even();
        let bps = cfg.bitrate_bps.unwrap_or_else(|| {
            rc_client_core::app_prefs::recommended_auto_bitrate_bps(
                even.width,
                even.height,
                cfg.fps,
            )
        });
        vec![(even, cfg.fps, bps, cfg.codec)]
    };

    println!(
        "RabbitControl macOS 编解码隔离基准（生产 rc-encode-vt + rc-decode-vt，每场景 {} 帧）",
        cfg.frames
    );
    println!(
        "{:<15} {:<5} {:<4} {:>7} | {:>13} {:>7} | {:>13} {:>7} | {:>13} {:>7} | {:>13} {:>7}",
        "Res@FPS",
        "Codec",
        "LLRC",
        "InitMS",
        "Sync P50/95",
        "SyncFPS",
        "Pipe2 P50/95",
        "Pipe2FPS",
        "DecSync P50/95",
        "DecSync",
        "DecPipe2 P50/95",
        "DecP2FPS"
    );
    println!("{}", "-".repeat(138));

    let mut cached_res = Resolution::new(0, 0);
    let mut cached_pool: Vec<pool::PoolFrame> = Vec::new();
    let mut rows_json = Vec::new();

    for (res, fps, bps, codec) in scenarios {
        let even = res.to_even();
        if even != cached_res {
            cached_pool = pool::build_desktop_pool(even.width, even.height, 16)?;
            cached_res = even;
        }
        let enc_cfg = EncoderConfig {
            codec,
            resolution: even,
            fps,
            bitrate_bps: bps,
            preset: Preset::Balanced,
            low_latency: true,
            gop_len: 0,
            b_frames: 0,
        };
        let sync_res = bench_encode_sync(enc_cfg, &cached_pool, cfg.frames, false)?;
        let pipe2_res = bench_encode_pipelined(enc_cfg, &cached_pool, cfg.frames, 2)?;
        let dec_res = bench_decode_vt(codec, &sync_res.frames)?;
        let dec_pipe2_res = bench_decode_vt_pipelined(codec, &sync_res.frames, 2)?;

        println!(
            "{:<15} {:<5} {:<4} {:>6.1}ms | {:>5.2}/{:<5.2}ms {:>6.1} | {:>5.2}/{:<5.2}ms {:>6.1} | {:>5.2}/{:<5.2}ms {:>6.1} | {:>5.2}/{:<5.2}ms {:>6.1}",
            format!("{}x{}@{}", even.width, even.height, fps),
            codec.as_str(),
            if fps < 90 { "on" } else { "off" },
            sync_res.init_ms,
            sync_res.lat.p50_ms,
            sync_res.lat.p95_ms,
            sync_res.max_fps,
            pipe2_res.lat.p50_ms,
            pipe2_res.lat.p95_ms,
            pipe2_res.max_fps,
            dec_res.lat.p50_ms,
            dec_res.lat.p95_ms,
            dec_res.max_fps,
            dec_pipe2_res.lat.p50_ms,
            dec_pipe2_res.lat.p95_ms,
            dec_pipe2_res.max_fps,
        );

        rows_json.push(serde_json::json!({
            "resolution": format!("{}x{}", even.width, even.height),
            "fps": fps,
            "codec": codec.as_str(),
            "llrc": fps < 90,
            "target_mbps": r2(f64::from(bps) / 1e6),
            "actual_mbps": r2(sync_res.actual_mbps),
            "idr_kb": r2(sync_res.idr_kb),
            "p_avg_kb": r2(sync_res.p_avg_kb),
            "enc_init_ms": r2(sync_res.init_ms),
            "enc_sync": { "fps": r2(sync_res.max_fps), "dropped": sync_res.dropped, "latency": sync_res.lat.to_json() },
            "enc_pipe2": { "fps": r2(pipe2_res.max_fps), "dropped": pipe2_res.dropped, "latency": pipe2_res.lat.to_json() },
            "dec_sync": { "cold_idr_ms": r2(dec_res.cold_idr_ms), "fps": r2(dec_res.max_fps), "latency": dec_res.lat.to_json() },
            "dec_pipe2": { "cold_idr_ms": r2(dec_pipe2_res.cold_idr_ms), "fps": r2(dec_pipe2_res.max_fps), "latency": dec_pipe2_res.lat.to_json() },
        }));
    }

    // 额外 A/B：仅在 --matrix 模式下执行 Preset（speed_over_quality 开关对照）与稳态定频专项对比
    let mut ab_rows = Vec::new();
    if cfg.matrix {
        println!("\n=== 编码器 Preset / 定频稳态专项对照（3840x2160@120 & 1920x1080@60）===");
        for &(w, h, fps, bps, codec, preset, paced) in &[
            (
                1920u32,
                1080u32,
                60u32,
                12_000_000u32,
                Codec::H264,
                Preset::HighQuality,
                true,
            ),
            (
                1920,
                1080,
                60,
                12_000_000,
                Codec::H264,
                Preset::Balanced,
                true,
            ),
            (
                1920,
                1080,
                60,
                12_000_000,
                Codec::Hevc,
                Preset::Balanced,
                true,
            ),
            (
                1920,
                1080,
                60,
                12_000_000,
                Codec::Hevc,
                Preset::Fastest,
                true,
            ),
            (
                3840,
                2160,
                120,
                80_000_000,
                Codec::Hevc,
                Preset::Balanced,
                false,
            ),
            (
                3840,
                2160,
                120,
                80_000_000,
                Codec::Hevc,
                Preset::Fastest,
                false,
            ),
            (
                3840,
                2160,
                120,
                80_000_000,
                Codec::H264,
                Preset::HighQuality,
                false,
            ),
            (
                3840,
                2160,
                120,
                80_000_000,
                Codec::H264,
                Preset::Balanced,
                false,
            ),
        ] {
            let even = Resolution::new(w, h).to_even();
            if even != cached_res {
                cached_pool = pool::build_desktop_pool(even.width, even.height, 16)?;
                cached_res = even;
            }
            let enc_cfg = EncoderConfig {
                codec,
                resolution: even,
                fps,
                bitrate_bps: bps,
                preset,
                low_latency: true,
                gop_len: 0,
                b_frames: 0,
            };
            let n = if paced {
                cfg.frames.min(90)
            } else {
                cfg.frames
            };
            let res = bench_encode_sync(enc_cfg, &cached_pool, n, paced)?;
            let preset_name = match preset {
                Preset::Fastest => "fastest",
                Preset::Balanced => "balanced",
                Preset::HighQuality => "high_quality",
                _ => "other",
            };
            println!(
                "  {w}x{h}@{fps:<3} {:<4} {:<12} {:<7} | P50={:>5.2}ms P95={:>5.2}ms P99={:>5.2}ms | {:>6.1}fps | P_avg={:>5.1}KB ({:>5.1} Mbps) drops={}",
                codec.as_str(),
                preset_name,
                if paced { "paced" } else { "unpaced" },
                res.lat.p50_ms,
                res.lat.p95_ms,
                res.lat.p99_ms,
                res.max_fps,
                res.p_avg_kb,
                res.actual_mbps,
                res.dropped,
            );
            ab_rows.push(serde_json::json!({
                "resolution": format!("{w}x{h}"),
                "fps": fps,
                "codec": codec.as_str(),
                "preset": preset_name,
                "paced": paced,
                "max_fps": r2(res.max_fps),
                "p_avg_kb": r2(res.p_avg_kb),
                "actual_mbps": r2(res.actual_mbps),
                "dropped": res.dropped,
                "latency": res.lat.to_json(),
            }));
        }
    }

    if let Some(path) = cfg.report_path {
        let report = serde_json::json!({
            "frames_per_scenario": cfg.frames,
            "matrix": rows_json,
            "preset_ab": ab_rows,
        });
        let text =
            serde_json::to_string_pretty(&report).map_err(|e| format!("序列化 JSON 失败: {e}"))?;
        fs::write(&path, text).map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
        println!("\nJSON 报告已保存至: {}", path.display());
    }

    Ok(())
}

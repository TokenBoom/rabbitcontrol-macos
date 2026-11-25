//! VideoToolbox 解码真机自测（从原本地 `rc-decode-vt` 迁入 `rc-platform-macos`）。
//!
//! 覆盖原 T05 两条编码 → 解码真机验收测试（纯 VideoToolbox，不依赖屏幕采集权限）：
//! - `h264_decode_1080p_under_3ms`：300 帧 1080p60 编码 → 解码延迟 P50 ≤ 3ms 及进一出一；
//! - `decode_encode_roundtrip_visual_match`：1080p IDR+2P 编解码视觉往返（PSNR > 35dB）。

#![cfg(target_os = "macos")]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::time::{Duration, Instant};

use rc_capture::{CapturedFrame, FrameTexture};
use rc_decode::{DecodeOutput, Decoder};
use rc_decode_vt::{VtDecoder, VtDecoderConfig};
use rc_encode::Encoder as _;
use rc_types::{CaptureSeq, Timestamp};

/// 宽裕的解码截止（注入时间域；解码器逻辑时钟远小于它）。
const RELAXED_DEADLINE: Timestamp = Timestamp(60_000_000);

// ─────────────────────────────────────────────────────────────────────────────
// 测试支撑（仅测试构建）：像素缓冲创建 / 写图案 / 一次性 CPU 回读。
// 生产解码路径绝不 LockBaseAddress（D1 零拷贝）；CPU 访问只发生在测试里。
// ─────────────────────────────────────────────────────────────────────────────

mod test_support {
    use std::ffi::c_void;
    use std::ptr::NonNull;

    use objc2_core_foundation::CFRetained;
    use objc2_core_video::{
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer, CVPixelBufferCreate,
        CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
        CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
        CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };

    /// `'420v'`（NV12 video range）——与采集/编码侧约定一致的输入格式（M5）。
    pub(crate) const PIXEL_FORMAT_420V: u32 = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;

    /// 拥有型 CVPixelBuffer（`CVPixelBufferCreate` +1），供真机测试。
    pub(crate) struct TestPixelBuffer(CFRetained<CVPixelBuffer>);

    impl TestPixelBuffer {
        /// 不透明句柄（与 SCK 交付给 `FrameTexture::CVPixelBuffer` 的形态一致）。
        pub(crate) fn as_opaque_ptr(&self) -> *mut c_void {
            CFRetained::as_ptr(&self.0).as_ptr().cast()
        }
    }

    /// 创建一块 '420v' 像素缓冲（内容未定义，写图案前不要喂给编码器）。
    pub(crate) fn make_pixel_buffer(width: u32, height: u32) -> Result<TestPixelBuffer, String> {
        let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: out 是合法出参；分配器与属性传 None 合法（系统默认分配器、无附加属性）。
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                width as usize,
                height as usize,
                PIXEL_FORMAT_420V,
                None,
                // SAFETY: &mut out 是本栈帧上的唯一可变借用，满足 NonNull 出参要求；
                // CVPixelBufferCreate 同步返回后借用即结束。
                NonNull::from(&mut out),
            )
        };
        if status != 0 {
            return Err(format!(
                "CVPixelBufferCreate({width}x{height}) 失败: CVReturn {status}"
            ));
        }
        match NonNull::new(out) {
            // SAFETY: Create 成功后 out 是 +1 所有权引用（CM_RETURNS_RETAINED_PARAMETER）。
            Some(ptr) => Ok(TestPixelBuffer(unsafe { CFRetained::from_raw(ptr) })),
            None => Err("CVPixelBufferCreate 成功但返回空缓冲".to_string()),
        }
    }

    /// 把测试图案写入 '420v' 缓冲的 Y/C 平面，返回写入的 Y 平面**密集副本**
    /// （无行距填充，供 PSNR 对比）。
    ///
    /// 图案 = 平滑双梯度 + PRNG 生成的实心矩形（逐帧平移）——结构化、可压缩、
    /// 对预测编码友好，同时把参考链真正用起来。亮度值域夹在 [64, 200]，
    /// 避开 video/full range 两端的重映射区。
    pub(crate) fn write_nv12_pattern(
        buffer: &TestPixelBuffer,
        frame: u32,
        seed: u64,
    ) -> Result<Vec<u8>, String> {
        let width = CVPixelBufferGetWidth(&buffer.0) as u32;
        let height = CVPixelBufferGetHeight(&buffer.0) as u32;
        let rects = pattern_rects(seed, width, height);

        // SAFETY: Lock(读写) 与 Unlock 配对；两个 getter 是同步只读安全包装。
        let lock =
            unsafe { CVPixelBufferLockBaseAddress(&buffer.0, CVPixelBufferLockFlags::empty()) };
        if lock != 0 {
            return Err(format!(
                "CVPixelBufferLockBaseAddress 失败: CVReturn {lock}"
            ));
        }
        let result = (|| {
            let base = CVPixelBufferGetBaseAddressOfPlane(&buffer.0, 0);
            let stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer.0, 0);
            if base.is_null() || stride < width as usize {
                return Err("Y 平面基址为空或行距小于宽度".to_string());
            }
            let mut dense = vec![0u8; (width as usize) * (height as usize)];
            // SAFETY: 锁存期间 base 指向 Y 平面可写内存（stride * height 字节），
            // 逐行写入宽度个字节，不越界。
            unsafe {
                let plane =
                    std::slice::from_raw_parts_mut(base as *mut u8, stride * height as usize);
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        let v = pattern_luma(x as u32, y as u32, frame, &rects);
                        plane[y * stride + x] = v;
                        dense[y * width as usize + x] = v;
                    }
                }
            }
            // C 平面（NV12 交织 CbCr）填中性值 128/128。
            let chroma_base = CVPixelBufferGetBaseAddressOfPlane(&buffer.0, 1);
            let chroma_stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer.0, 1);
            if !chroma_base.is_null() && chroma_stride > 0 {
                // SAFETY: 同上——锁存期间 CbCr 平面可写；高度为 Y 的一半。
                unsafe {
                    let plane = std::slice::from_raw_parts_mut(
                        chroma_base as *mut u8,
                        chroma_stride * (height as usize / 2).max(1),
                    );
                    for row in plane.chunks_mut(chroma_stride) {
                        row.fill(128);
                    }
                }
            }
            Ok(dense)
        })();
        // SAFETY: 与上面的 Lock 配对（任何退出路径都解锁）。
        unsafe { CVPixelBufferUnlockBaseAddress(&buffer.0, CVPixelBufferLockFlags::empty()) };
        result
    }

    /// 模拟真实办公/终端高对比文本桌面：静态深色 IDE 背景 + 局部区域高速滚动高熵终端字符（每帧变化），
    /// 返回完整密集 Y 平面供解码后 Y-PSNR 校验。
    pub(crate) fn write_nv12_office_terminal(
        buffer: &TestPixelBuffer,
        frame: u32,
        active_rect: rc_types::Rect,
    ) -> Result<Vec<u8>, String> {
        let width = CVPixelBufferGetWidth(&buffer.0) as u32;
        let height = CVPixelBufferGetHeight(&buffer.0) as u32;
        // SAFETY: Lock/Unlock 成对。
        let lock =
            unsafe { CVPixelBufferLockBaseAddress(&buffer.0, CVPixelBufferLockFlags::empty()) };
        if lock != 0 {
            return Err(format!(
                "CVPixelBufferLockBaseAddress 失败: CVReturn {lock}"
            ));
        }
        let result = (|| {
            let base = CVPixelBufferGetBaseAddressOfPlane(&buffer.0, 0);
            let stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer.0, 0);
            if base.is_null() || stride < width as usize {
                return Err("Y 平面基址为空或行距小于宽度".to_string());
            }
            let mut dense = vec![0u8; (width as usize) * (height as usize)];
            let rx0 = active_rect.x as usize;
            let ry0 = active_rect.y as usize;
            let rx1 = (active_rect.x.saturating_add(active_rect.width)).min(width) as usize;
            let ry1 = (active_rect.y.saturating_add(active_rect.height)).min(height) as usize;
            // SAFETY: 锁存期间 base 指向 stride * height 字节合法可写 Y 平面。
            unsafe {
                let plane =
                    std::slice::from_raw_parts_mut(base as *mut u8, stride * height as usize);
                for y in 0..height as usize {
                    let row = &mut plane[y * stride..y * stride + width as usize];
                    let drow = &mut dense[y * width as usize..(y + 1) * width as usize];
                    let in_y = y >= ry0 && y < ry1;
                    for x in 0..width as usize {
                        let v = if in_y && x >= rx0 && x < rx1 {
                            // 局部高熵滚动终端文本/日志（2×2 笔画密度，抗平滑预测但符合真实代码/日志流）
                            let gx = (x - rx0) / 2;
                            let gy = (y - ry0) / 2;
                            let cell_hash = (gx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
                                ^ (gy.wrapping_add((frame as usize) * 5) as u64)
                                    .wrapping_mul(0xBF58_476D_1CE4_E5B9)
                                ^ (frame as u64).wrapping_mul(0x94D0_49BB_1331_11EB);
                            if (cell_hash >> 61) == 0 {
                                225u8
                            } else {
                                26u8
                            }
                        } else {
                            // 静态深色 IDE 边框与只读代码区
                            let text_row = ((y / 22) & 1) == 0 && (y % 22) >= 5 && (y % 22) <= 13;
                            if text_row && ((x / 9) % 5 != 0) && ((x & 1) == 0) {
                                200u8
                            } else {
                                32u8
                            }
                        };
                        row[x] = v;
                        drow[x] = v;
                    }
                }
            }
            let chroma_base = CVPixelBufferGetBaseAddressOfPlane(&buffer.0, 1);
            let chroma_stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer.0, 1);
            if !chroma_base.is_null() && chroma_stride > 0 {
                // SAFETY: 锁存期间 CbCr 平面可写。
                unsafe {
                    let plane = std::slice::from_raw_parts_mut(
                        chroma_base as *mut u8,
                        chroma_stride * (height as usize / 2).max(1),
                    );
                    for row in plane.chunks_mut(chroma_stride) {
                        row.fill(128);
                    }
                }
            }
            Ok(dense)
        })();
        // SAFETY: 与上面的 Lock 配对。
        unsafe { CVPixelBufferUnlockBaseAddress(&buffer.0, CVPixelBufferLockFlags::empty()) };
        result
    }

    /// 一次性 CPU 回读指定像素缓冲的 Y 平面（**仅测试**，D1 红线不适用于此）。
    /// 返回 `(width, height, 密集 Y 平面)`。
    pub(crate) fn read_luma_plane(ptr: *mut c_void) -> Result<(u32, u32, Vec<u8>), String> {
        if ptr.is_null() {
            return Err("像素缓冲句柄为空".to_string());
        }
        // SAFETY: 调用方（真机测试）在解码器的保活窗口内（下一次 decode 前）
        // 立即回读，句柄有效；借用只在本函数内存活。
        let buffer = unsafe { &*(ptr as *const CVPixelBuffer) };
        let width = CVPixelBufferGetWidth(buffer) as u32;
        let height = CVPixelBufferGetHeight(buffer) as u32;
        // SAFETY: 只读 Lock 与 Unlock 配对。
        let lock = unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::empty()) };
        if lock != 0 {
            return Err(format!(
                "CVPixelBufferLockBaseAddress(读) 失败: CVReturn {lock}"
            ));
        }
        let result = (|| {
            let base = CVPixelBufferGetBaseAddressOfPlane(buffer, 0);
            let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0);
            if base.is_null() || stride < width as usize {
                return Err("Y 平面基址为空或行距小于宽度".to_string());
            }
            let mut dense = vec![0u8; (width as usize) * (height as usize)];
            // SAFETY: 锁存期间 base 指向 Y 平面可读内存；逐行拷贝宽度个字节。
            unsafe {
                let plane = std::slice::from_raw_parts(base as *const u8, stride * height as usize);
                for y in 0..height as usize {
                    dense[y * width as usize..(y + 1) * width as usize]
                        .copy_from_slice(&plane[y * stride..y * stride + width as usize]);
                }
            }
            Ok((width, height, dense))
        })();
        // SAFETY: 与上面的 Lock 配对。
        unsafe { CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::empty()) };
        result
    }

    // —— 图案生成（确定性 PRNG，与像素格式无关的纯函数）——

    /// xorshift64 PRNG（种子非零）。
    struct XorShift64(u64);

    impl XorShift64 {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }

    /// 一个实心矩形（左上角 + 宽高 + 亮度）。
    struct PatternRect {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        shade: u8,
    }

    /// 从种子生成 6 个互不重叠区间的矩形（位置留在边界内，留出逐帧平移余量）。
    fn pattern_rects(seed: u64, width: u32, height: u32) -> Vec<PatternRect> {
        let mut rng = XorShift64(seed | 1);
        let mut rects = Vec::with_capacity(6);
        for _ in 0..6 {
            let w = 160 + rng.below(160) as u32;
            let h = 120 + rng.below(120) as u32;
            // 位置留 64px 边距（逐帧平移 ±52px 不越界）。
            let x = 64 + rng.below((width.saturating_sub(w + 128)) as u64) as u32;
            let y = 64 + rng.below((height.saturating_sub(h + 128)) as u64) as u32;
            let shade = (90 + rng.below(100)) as u8;
            rects.push(PatternRect { x, y, w, h, shade });
        }
        rects
    }

    /// 图案亮度：平滑梯度打底 + 命中矩形取矩形亮度；逐帧平移（+13, +7 px）。
    fn pattern_luma(x: u32, y: u32, frame: u32, rects: &[PatternRect]) -> u8 {
        let mut v = 64u32 + (x * 96) / 1920 + (y * 64) / 1080; // 梯度 64..224
        for r in rects {
            let rx = r.x + (frame * 13) % 52;
            let ry = r.y + (frame * 7) % 52;
            if x >= rx && x < rx + r.w && y >= ry && y < ry + r.h {
                v = u32::from(r.shade);
            }
        }
        v.min(200) as u8
    }
}

/// T05 真机验收：1080p 解码段延迟。
///
/// 方法（按任务卡）：用 rc-encode-vt 先按 60fps 节奏编码 300 帧（PRNG 图案，
/// 码流形态与真实管线一致），再逐帧解码，统计每次 `decode()` 全程
/// （Annex-B 解析 → AVCC 转换 → 装帧 → VT 提交 → 回调等待 → 输出组装）
/// 的耗时分布。断言 **P50 ≤ 3ms**（T05 验收）。顺带断言进一出一（300 进 300 出）
/// 与首帧尺寸上报。
#[test]
#[ignore = "requires macOS + GPU session"]
fn h264_decode_1080p_under_3ms() {
    // —— 编码段：300 帧 1080p（rc-encode-vt，Annex-B 输出）——
    let mut encoder = rc_encode_vt::new_encoder(rc_encode::EncoderConfig::default())
        .expect("构造编码器（含 E6 自检）");
    let buffer = test_support::make_pixel_buffer(1920, 1080).expect("创建测试像素缓冲");
    let mut streams: Vec<(Vec<u8>, rc_types::FrameKind)> = Vec::with_capacity(300);
    // 按 60fps 节奏提交：与编码侧真机测试一致（突发提交会让码控误判，
    // 产出的码流不代表稳态）。
    let frame_period = Duration::from_nanos(16_666_667);
    let mut next_submit = Instant::now() + frame_period;
    for i in 0u32..300 {
        test_support::write_nv12_pattern(&buffer, i, 0x5EED_0000_0000_0001).expect("写入测试图案");
        let ts = Timestamp(i * 16_667);
        let frame = CapturedFrame {
            texture: FrameTexture::CVPixelBuffer(buffer.as_opaque_ptr()),
            timestamp: ts,
            dirty: None,
            seq: CaptureSeq(u64::from(i)),
            is_repeat: false,
            resource_guard: None,
        };
        let out = encoder
            .encode(&frame, false, Timestamp(ts.0.saturating_add(50_000)))
            .unwrap_or_else(|e| panic!("第 {i} 帧编码失败: {e}"));
        if i == 0 {
            assert!(out.is_keyframe, "编码器首个正式帧必须是 IDR");
        }
        streams.push((out.data, out.kind));
        if let Some(sleep) = next_submit.checked_duration_since(Instant::now()) {
            std::thread::sleep(sleep);
        }
        next_submit += frame_period;
    }

    // —— 解码段：进一出一逐帧解码，测 decode() 全程 ——
    let mut decoder = VtDecoder::new(VtDecoderConfig::default()).expect("默认配置合法");
    let mut samples_ms: Vec<f64> = Vec::with_capacity(300);
    let mut outputs = 0usize;
    let decode_started = Instant::now();
    for (i, (data, kind)) in streams.iter().enumerate() {
        let started = Instant::now();
        let out = decoder
            .decode(data, *kind, RELAXED_DEADLINE)
            .unwrap_or_else(|e| panic!("第 {i} 帧解码失败: {e}"));
        samples_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        match out {
            DecodeOutput::Frame(f) => {
                outputs += 1;
                assert_eq!(
                    (f.width, f.height),
                    (1920, 1080),
                    "帧尺寸上报必须与码流一致（帧 {i}）"
                );
                assert!(f.decode_done_ts.0 > 0);
            }
            other => panic!("第 {i} 帧应进一出一，实际 {other:?}"),
        }
    }
    let wall_ms = decode_started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(outputs, 300, "300 帧进必须 300 帧出（D5）");
    assert_eq!(decoder.submitted_frames(), 300);

    samples_ms.sort_by(|a, b| a.partial_cmp(b).expect("延迟样本均为有限值"));
    let p50 = samples_ms[samples_ms.len() / 2];
    let p95 = samples_ms[(samples_ms.len() * 95) / 100];
    eprintln!(
        "VideoToolbox 1080p 解码段（300 帧，含 Annex-B→AVCC + 装帧 + VT 提交 + 回调等待）：\
         P50 = {p50:.2}ms, P95 = {p95:.2}ms, min = {:.2}ms, max = {:.2}ms（总墙钟 {wall_ms:.0}ms）",
        samples_ms[0],
        samples_ms[samples_ms.len() - 1]
    );
    assert!(
        p50 <= 3.0,
        "解码段 P50 = {p50:.2}ms 超过 3ms 预算（P95 = {p95:.2}ms）"
    );
}

/// T05 真机验收：编码 → 解码视觉往返。
///
/// 方法（按任务卡）：PRNG 图案（梯度 + 6 个 PRNG 实心矩形，逐帧平移）
/// → rc-encode-vt 编码（IDR + 2 个 P 帧）→ 解码 → **测试内 CPU 回读
/// 一次**（仅测试允许；生产路径零 CPU 回读）→ 对比最后一帧（P 帧，
/// 真正走到参考链）的 Y 平面算 PSNR。断言 **PSNR > 35dB 且尺寸一致**。
#[test]
#[ignore = "requires macOS + GPU session"]
fn decode_encode_roundtrip_visual_match() {
    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;
    const FRAMES: u32 = 3;

    // —— 编码段（24Mbps：视觉测试要质量余量，延迟测试在另一条）——
    let enc_cfg = rc_encode::EncoderConfig {
        bitrate_bps: 24_000_000,
        ..rc_encode::EncoderConfig::default()
    };
    let mut encoder = rc_encode_vt::new_encoder(enc_cfg).expect("构造编码器");
    let buffer = test_support::make_pixel_buffer(WIDTH, HEIGHT).expect("创建测试像素缓冲");
    let mut src_luma: Vec<Vec<u8>> = Vec::with_capacity(FRAMES as usize);
    let mut streams: Vec<(Vec<u8>, rc_types::FrameKind)> = Vec::with_capacity(FRAMES as usize);
    for i in 0..FRAMES {
        let dense = test_support::write_nv12_pattern(&buffer, i, 0x00C0_FFEE_0000_0001)
            .expect("写入测试图案");
        src_luma.push(dense);
        let ts = Timestamp(i * 16_667);
        let frame = CapturedFrame {
            texture: FrameTexture::CVPixelBuffer(buffer.as_opaque_ptr()),
            timestamp: ts,
            dirty: None,
            seq: CaptureSeq(u64::from(i)),
            is_repeat: false,
            resource_guard: None,
        };
        let out = encoder
            .encode(&frame, false, Timestamp(ts.0.saturating_add(50_000)))
            .unwrap_or_else(|e| panic!("第 {i} 帧编码失败: {e}"));
        streams.push((out.data, out.kind));
    }

    // —— 解码段：全部帧进一出一，末帧句柄仍在保活窗口内 ——
    let mut decoder = VtDecoder::new(VtDecoderConfig::default()).expect("默认配置合法");
    let mut last_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    for (i, (data, kind)) in streams.iter().enumerate() {
        match decoder
            .decode(data, *kind, RELAXED_DEADLINE)
            .unwrap_or_else(|e| panic!("第 {i} 帧解码失败: {e}"))
        {
            DecodeOutput::Frame(f) => {
                assert_eq!((f.width, f.height), (WIDTH, HEIGHT), "尺寸一致（帧 {i}）");
                last_ptr = match f.texture {
                    FrameTexture::CVPixelBuffer(ptr) => ptr,
                    ref other => panic!("应输出 CVPixelBuffer 零拷贝句柄，实际 {other:?}"),
                };
            }
            other => panic!("第 {i} 帧应进一出一，实际 {other:?}"),
        }
    }

    // —— 一次性 CPU 回读（仅测试允许）+ Y 平面 PSNR ——
    let (w, h, decoded_luma) = test_support::read_luma_plane(last_ptr).expect("回读末帧 Y 平面");
    assert_eq!((w, h), (WIDTH, HEIGHT), "解码输出尺寸与编码输入一致");
    let reference = &src_luma[usize::try_from(FRAMES - 1).expect("末帧索引")];
    let mse = decoded_luma
        .iter()
        .zip(reference)
        .map(|(a, b)| {
            let d = i32::from(*a) - i32::from(*b);
            (d * d) as f64
        })
        .sum::<f64>()
        / decoded_luma.len() as f64;
    let psnr = 10.0 * (255.0f64 * 255.0 / mse).log10();
    eprintln!(
        "编码→解码往返（{WIDTH}x{HEIGHT}，{FRAMES} 帧，末帧为 P 帧）：Y 平面 PSNR = {psnr:.2}dB（MSE = {mse:.2}）"
    );
    assert!(
        psnr > 35.0,
        "视觉往返 PSNR = {psnr:.2}dB ≤ 35dB（结构不匹配或参考链断裂）"
    );
}

#[test]
#[ignore = "requires macOS + GPU VideoToolbox"]
fn hevc_encode_decode_roundtrip_1080p_and_4k() {
    for (width, height, fps, bitrate) in [
        (1920u32, 1080u32, 60u32, 24_000_000u32),
        (3840, 2160, 120, 60_000_000),
    ] {
        let enc_cfg = rc_encode::EncoderConfig {
            codec: rc_encode::Codec::Hevc,
            resolution: rc_types::Resolution::new(width, height),
            fps,
            bitrate_bps: bitrate,
            ..rc_encode::EncoderConfig::default()
        };
        let mut encoder = rc_encode_vt::new_encoder(enc_cfg).expect("构造 HEVC 编码器");
        let mut decoder = VtDecoder::new(VtDecoderConfig {
            codec: rc_encode::Codec::Hevc,
            max_resolution: rc_types::Resolution::new(4096, 4096),
            ..VtDecoderConfig::default()
        })
        .expect("构造 HEVC 解码器");

        let buffer = test_support::make_pixel_buffer(width, height).expect("创建测试像素缓冲");
        let mut decode_ms = Vec::with_capacity(20);
        let mut last_ref_luma = Vec::new();
        let mut last_ptr = std::ptr::null_mut();

        for i in 0..20u32 {
            let dense = test_support::write_nv12_pattern(
                &buffer,
                i,
                0x4845_5643_0000_0001u64.wrapping_add(u64::from(width)),
            )
            .expect("写入测试图案");
            last_ref_luma = dense;
            let ts = Timestamp(i * (1_000_000 / fps));
            let frame = CapturedFrame {
                texture: FrameTexture::CVPixelBuffer(buffer.as_opaque_ptr()),
                timestamp: ts,
                dirty: None,
                seq: CaptureSeq(u64::from(i)),
                is_repeat: false,
                resource_guard: None,
            };
            let out = encoder
                .encode(&frame, false, Timestamp(ts.0.saturating_add(50_000)))
                .unwrap_or_else(|e| panic!("HEVC {width}x{height} 帧 {i} 编码失败: {e}"));
            let t0 = Instant::now();
            match decoder
                .decode(&out.data, out.kind, RELAXED_DEADLINE)
                .unwrap_or_else(|e| panic!("HEVC {width}x{height} 帧 {i} 解码失败: {e}"))
            {
                DecodeOutput::Frame(f) => {
                    assert_eq!((f.width, f.height), (width, height));
                    decode_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
                    last_ptr = match f.texture {
                        FrameTexture::CVPixelBuffer(ptr) => ptr,
                        other => panic!("unexpected texture {other:?}"),
                    };
                }
                other => panic!("HEVC 帧 {i} 期望 Frame，实际 {other:?}"),
            }
        }
        decode_ms.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
        let p50 = decode_ms[decode_ms.len() / 2];
        let (w, h, decoded_luma) =
            test_support::read_luma_plane(last_ptr).expect("回读 HEVC Y 平面");
        assert_eq!((w, h), (width, height));
        let mse = decoded_luma
            .iter()
            .zip(&last_ref_luma)
            .map(|(a, b)| {
                let d = i32::from(*a) - i32::from(*b);
                (d * d) as f64
            })
            .sum::<f64>()
            / decoded_luma.len() as f64;
        let psnr = 10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10();
        eprintln!(
            "HEVC 真机编解码往返 {width}x{height}@{fps}: 解码 P50={p50:.2}ms, PSNR={psnr:.2}dB"
        );
        assert!(psnr > 35.0, "HEVC {width}x{height} PSNR={psnr:.2}dB 过低");
        if width <= 1920 {
            assert!(p50 <= 3.0, "HEVC 1080p 解码 P50={p50:.2}ms > 3ms");
        }
    }
}

/// 真机 A/B 实测：2560×1662 HiDPI HEVC 轻度办公桌面（每帧仅 ~2% 局部区域变动），
/// 对比关闭脏区感知（dirty=None）与开启内容自适应码控（dirty=2%）的实际码率与解码 Y-PSNR。
#[test]
#[ignore = "requires macOS + 真实 VideoToolbox 编解码器（内容自适应码控与文本清晰度 A/B 实测）"]
fn desktop_office_adaptive_bitrate_saves_bandwidth_with_high_psnr() {
    let width = 2560u32;
    let height = 1662u32;
    let fps = 60u32;
    let ceiling_bps = 14_000_000u32;
    let total_frames = 150u32; // 2.5s @ 60fps（前 1.5s 含 250ms 防抖与平滑衰减，后 1.0s 测稳态码率）
    let active_rect = rc_types::Rect::new(120, 120, 640, 260); // ~3.9% of 2560x1662 (终端输出区)

    let run_case = |label: &str, dirty: Option<rc_types::Rect>| -> (f64, f64, f64) {
        let enc_cfg = rc_encode::EncoderConfig {
            codec: rc_encode::Codec::Hevc,
            resolution: rc_types::Resolution::new(width, height),
            fps,
            bitrate_bps: ceiling_bps,
            preset: rc_encode::Preset::Balanced,
            ..rc_encode::EncoderConfig::default()
        };
        let mut encoder = rc_encode_vt::new_encoder(enc_cfg).expect("构造 HEVC 编码器");
        let mut decoder = VtDecoder::new(VtDecoderConfig {
            codec: rc_encode::Codec::Hevc,
            max_resolution: rc_types::Resolution::new(4096, 4096),
            ..VtDecoderConfig::default()
        })
        .expect("构造 HEVC 解码器");

        let buffer = test_support::make_pixel_buffer(width, height).expect("创建像素缓冲");
        let mut total_bytes_tail = 0usize; // 统计后 60 帧稳态码率（1.0s）
        let mut enc_ms = Vec::with_capacity(total_frames as usize);
        let mut last_ptr = std::ptr::null_mut();

        for i in 0..total_frames {
            let ref_luma = test_support::write_nv12_office_terminal(&buffer, i, active_rect)
                .expect("写入办公终端图案");
            let ts = Timestamp(i * (1_000_000 / fps));
            let frame = CapturedFrame {
                texture: FrameTexture::CVPixelBuffer(buffer.as_opaque_ptr()),
                timestamp: ts,
                dirty,
                seq: CaptureSeq(u64::from(i)),
                is_repeat: false,
                resource_guard: None,
            };
            let t0 = Instant::now();
            let out = encoder
                .encode(&frame, false, Timestamp(ts.0.saturating_add(50_000)))
                .unwrap_or_else(|e| panic!("{label} 第 {i} 帧编码失败: {e}"));
            enc_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
            if i >= 90 {
                total_bytes_tail += out.data.len();
            }
            if let DecodeOutput::Frame(f) = decoder
                .decode(&out.data, out.kind, RELAXED_DEADLINE)
                .unwrap_or_else(|e| panic!("{label} 第 {i} 帧解码失败: {e}"))
            {
                if let FrameTexture::CVPixelBuffer(ptr) = f.texture {
                    last_ptr = ptr;
                }
            }
            if i + 1 == total_frames {
                let (w, h, dec_luma) =
                    test_support::read_luma_plane(last_ptr).expect("回读解码 Y 平面");
                assert_eq!((w, h), (width, height));
                let mse = dec_luma
                    .iter()
                    .zip(&ref_luma)
                    .map(|(a, b)| {
                        let d = i32::from(*a) - i32::from(*b);
                        (d * d) as f64
                    })
                    .sum::<f64>()
                    / dec_luma.len() as f64;
                let psnr = 10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10();
                enc_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let p50 = enc_ms[enc_ms.len() / 2];
                let mbps = (total_bytes_tail as f64 * 8.0) / 1e6;
                eprintln!(
                    "[{label}] 2560x1662@60 HEVC: tail_bitrate={mbps:.2} Mbps, enc_P50={p50:.2} ms, Y-PSNR={psnr:.2} dB"
                );
                return (mbps, p50, psnr);
            }
        }
        unreachable!()
    };

    let (base_mbps, base_p50, base_psnr) = run_case("baseline_flat_14m", None);
    let (adapt_mbps, adapt_p50, adapt_psnr) = run_case("adaptive_dirty_4pct", Some(active_rect));
    let saving_pct = (1.0 - adapt_mbps / base_mbps.max(0.01)) * 100.0;
    eprintln!(
        "=== 2K HEVC 办公终端场景 A/B：码率 {base_mbps:.2} -> {adapt_mbps:.2} Mbps (-{saving_pct:.1}%) | 编码 P50 {base_p50:.2} -> {adapt_p50:.2} ms | Y-PSNR {base_psnr:.2} -> {adapt_psnr:.2} dB ==="
    );
    assert!(
        adapt_mbps < base_mbps * 0.45,
        "期望自适应码控降低至少 55% 码率，实际 {base_mbps:.2} -> {adapt_mbps:.2} Mbps"
    );
    assert!(
        adapt_psnr >= 32.0,
        "期望自适应码控下高熵局部文本整体 Y-PSNR >= 32 dB，实际 {adapt_psnr:.2} dB"
    );
}

//! RabbitControl 的 VideoToolbox 硬件编码后端（任务卡 T12）。
//!
//! # 职责
//! 用 `VTCompressionSession` 实现 [`rc_encode::Encoder`]（H.264），
//! 吃 ScreenCaptureKit 交付的 [`FrameTexture::CVPixelBuffer`](rc_capture::FrameTexture::CVPixelBuffer)，
//! 产出 Annex-B 码流。
//!
//! # 零拷贝
//! 输入的 `CVPixelBuffer` **句柄直通** VideoToolbox：不 LockBaseAddress、
//! 不读 CPU 像素（坑 7：锁了才有 CPU 视图，零拷贝的前提就是不锁）。
//! 与 `rc-capture-macos` 同卡交付正是因为共享这条 GPU 路径。
//!
//! # 延迟红线（契约 E1–E6 的落点）
//! | 契约 | 落点 |
//! |---|---|
//! | E1（b_frames = 0） | 构造期 `EncoderConfig::validate` 拒绝 + `AllowFrameReordering = false` |
//! | E2（关 look-ahead / 2-pass） | `low_latency` 构造期强制 + `RealTime = true`（M6，未设时 VT 按文件转码优化，延迟 +20–40ms；VT 实时路径不存在 2-pass） |
//! | E3（输出零延迟） | `MaxFrameDelayCount = 0`；`flush` 恒返回空 |
//! | E4（输出序 == 输入序） | 会话内帧号单调，同步等待本帧输出，一进一出 |
//! | E5（超时不阻塞） | 预算 = `deadline_ts - 帧时间戳`（注入时间域），到点返回 `Error::Timeout`；迟到输出丢弃 + 下一帧强制 IDR 恢复（低延迟优先于完整） |
//! | E6（启动自检） | `new` 时编一帧假数据（内容未定义的 '420v' 缓冲）：结构校验（Annex-B / SPS+PPS / IDR）**并用 `VTDecompressionSession` 解码验证**，失败返回 `Err` |
//!
//! # 输出格式
//! VideoToolbox 的输出是 AVCC 风格（长度前缀 NAL、参数集在格式描述里，
//! SDK 无 Annex-B 开关），本 crate 在输出侧重排为 Annex-B（纯逻辑在
//! [`annexb`]，含 3/4 字节起始码直通防御），并在
//! `capabilities().bitstream_format` 里如实声明。
//!
//! 非 macOS 目标上本 crate 编译为空壳（仅保留纯逻辑模块），保证 workspace
//! 在任何平台都能构建与测试。

#![deny(missing_docs)]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
// 每个 unsafe 块必须有 // SAFETY: 注释 —— 只对 ffi 模块有实际约束。
#![deny(clippy::undocumented_unsafe_blocks)]

/// AVCC → Annex-B 重排与 NAL 结构解析（纯函数，跨平台可测）。
///
/// 调用方只有 macOS 的 ffi 模块；非 macOS 空壳构建里它没有调用者但仍然
/// 编译 + 跑单测（保持纯逻辑在任何目标上可验证），因此容忍空壳的
/// dead_code 提示。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod annexb;

#[cfg(target_os = "macos")]
pub(crate) mod ffi;

use rc_encode::{BitstreamFormat, EncoderCaps, EncoderConfig};
use rc_types::Resolution;

#[cfg(target_os = "macos")]
use std::fmt;
#[cfg(target_os = "macos")]
use std::sync::{Mutex, OnceLock};
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use rc_capture::{CapturedFrame, FrameTexture};
#[cfg(target_os = "macos")]
use rc_encode::{EncodedFrame, Encoder, Preset};
#[cfg(target_os = "macos")]
use rc_types::{Error, FrameKind, Result, Timestamp};

/// 能力声明是配置的纯函数：任何平台（含无 GPU 的 CI）都可校验。
/// 调用方在 macOS 的 `VtEncoder` 里；空壳构建同样容忍无调用者。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn caps_for(cfg: &EncoderConfig) -> EncoderCaps {
    let profiles: &'static [u8] = match cfg.codec {
        rc_encode::Codec::Hevc => &[1],   // HEVC Main
        rc_encode::Codec::H264 => &[100], // H.264 High Profile (CABAC + 8x8 DCT)
        rc_encode::Codec::Av1 => &[],
    };
    let max_resolution = match cfg.codec {
        // Apple Silicon HEVC 硬件编码支持 5K (5120x2880) / 8K；H.264 硬件编码上限为 4096x4096。
        rc_encode::Codec::Hevc => Resolution::new(8192, 4320),
        rc_encode::Codec::H264 => Resolution::new(4096, 4096),
        // Apple VideoToolbox 无 AV1 硬件编码器，绝不虚报硬件能力或分辨率上限。
        rc_encode::Codec::Av1 => Resolution::new(0, 0),
    };
    let hardware = matches!(cfg.codec, rc_encode::Codec::H264 | rc_encode::Codec::Hevc);
    let bitstream_format = match cfg.codec {
        // ffi 侧重排为 Annex-B（见模块文档「输出格式」）。
        rc_encode::Codec::H264 | rc_encode::Codec::Hevc => BitstreamFormat::AnnexB,
        rc_encode::Codec::Av1 => BitstreamFormat::Obu,
    };
    EncoderCaps {
        codec: cfg.codec,
        // 会话创建时显式要求硬件编码器（RequireHardwareAccelerated = true，
        // 见 ffi 模块），无硬件可用会在构造期失败而非静默落回软编。
        hardware,
        max_resolution,
        bitstream_format,
        profiles,
    }
}

/// 工厂：按配置创建 VideoToolbox 编码器（[`VtEncoder::new`] 的别名入口）。
///
/// ```no_run
/// # #[cfg(target_os = "macos")]
/// # fn main() -> rc_types::Result<()> {
/// use rc_encode::Encoder as _;
/// let cfg = rc_encode::EncoderConfig::default(); // 1080p60 低延迟 H.264
/// let mut enc = rc_encode_vt::new_encoder(cfg)?; // 含 E6 启动自检
/// assert!(enc.capabilities().hardware);
/// # Ok(())
/// # }
/// # #[cfg(not(target_os = "macos"))]
/// # fn main() {}
/// ```
#[cfg(target_os = "macos")]
pub fn new_encoder(config: EncoderConfig) -> Result<VtEncoder> {
    VtEncoder::new(config)
}

/// 内容自适应码率控制器（纯 Rust，零堆分配）。
///
/// 结合 SCK `dirty` 脏区面积占比与真实帧到达间隔（`frame.timestamp`），在用户/网络设定的
/// `ceiling_bps` 上限内动态调整 VideoToolbox 目标码率：
/// - 剧烈运动、大面积更新（脏区 ≥ 35%）或关键帧 IDR：**单帧即时拉满**至 `ceiling_bps`，零延迟、零模糊；
/// - 轻度办公/打字/终端微更新（脏区 < 15%）：经过防抖帧数与冷却窗口后平滑回落至文本清晰度下限，
///   消除 60fps 局部小动画/光标闪烁浪费 14–30 Mbps 带宽的问题；
/// - 当调用方未提供脏区（`dirty == None`，如全帧压测/未知脏区平台）时保持 100% `ceiling_bps`，不做误限流。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) struct ContentAdaptiveRateController {
    codec: rc_encode::Codec,
    frame_pixels: u64,
    ceiling_bps: u32,
    active_bps: u32,
    last_frame_ts_us: Option<u32>,
    low_demand_streak: u32,
    low_demand_since_ts_us: Option<u32>,
    last_decay_ts_us: Option<u32>,
    frames_since_decay: u32,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl ContentAdaptiveRateController {
    /// 连续低变动帧最小帧数防抖阈值。
    const DECAY_STREAK_MIN: u32 = 8;
    /// 连续低变动最小持续时间（250ms，避免 120–380fps 高刷下 20–67ms 的短暂停顿触发误降码率）。
    const DECAY_HOLD_US: u32 = 250_000;
    /// 向下衰减最小冷却间隔（200ms，减少动态属性写入频率并平滑过渡）。
    const DECAY_COOLDOWN_US: u32 = 200_000;
    /// 当 `Timestamp` 饱和于 `u32::MAX`（>71.6 分钟长会话）时的帧数冷却兜底。
    const SATURATED_DECAY_COOLDOWN_FRAMES: u32 = 12;

    pub(crate) fn new(cfg: &EncoderConfig) -> Self {
        let ceiling = cfg.bitrate_bps.max(1);
        Self {
            codec: cfg.codec,
            frame_pixels: u64::from(cfg.resolution.width.max(1))
                .saturating_mul(u64::from(cfg.resolution.height.max(1))),
            ceiling_bps: ceiling,
            active_bps: ceiling,
            last_frame_ts_us: None,
            low_demand_streak: 0,
            low_demand_since_ts_us: None,
            last_decay_ts_us: None,
            frames_since_decay: 0,
        }
    }

    /// 规划外部更新码率上限后的目标 `active_bps` 与是否需要调用 VideoToolbox `set_bitrate`：
    /// - 新上限低于当前 `active_bps`：立即钳制到新上限并下发；
    /// - 当前处于满额态（未衰减）：直接跟随新上限并下发；
    /// - 当前处于低变动衰减态且新上限 ≥ 当前 `active_bps`（如中继拥塞把 14M 降至 10.5M 时当前已在 2.5M）：
    ///   绝不向上暴冲码率，仅更新上限待下次大变动时再抬升。
    pub(crate) fn plan_ceiling_change(&self, ceiling_bps: u32) -> (u32, bool) {
        let c = ceiling_bps.max(1);
        let next_active = if c < self.active_bps || self.active_bps == self.ceiling_bps {
            c
        } else {
            self.active_bps
        };
        (next_active, next_active != self.active_bps)
    }

    /// 在 FFI 下发成功（或无需下发）后原子提交新上限与活动码率。
    pub(crate) fn commit_ceiling_change(&mut self, ceiling_bps: u32, next_active_bps: u32) {
        let c = ceiling_bps.max(1);
        let clamped_active = next_active_bps.min(c).max(1);
        let changed_active = clamped_active != self.active_bps;
        self.ceiling_bps = c;
        self.active_bps = clamped_active;
        if changed_active {
            self.low_demand_streak = 0;
            self.low_demand_since_ts_us = None;
            self.last_decay_ts_us = None;
            self.frames_since_decay = 0;
        }
    }

    /// 保证局部文本锐利度的最低码率底线（永不超过 `ceiling_bps`）。
    fn crisp_text_floor_bps(&self) -> u32 {
        let (per_pixel_num, per_pixel_den, min_bps, max_bps) = match self.codec {
            // HEVC 同画质效率更高：1080p ≈ 1.35 Mbps，2560×1662 ≈ 2.5 Mbps（在 <5% 脏区上等效 >50 Mbps 密度）
            rc_encode::Codec::Hevc => (65u64, 100u64, 800_000u64, 2_500_000u64),
            // AV1 压缩效率更高（防御性详尽分支；VideoToolbox 实际不支持 AV1 硬件编码）
            rc_encode::Codec::Av1 => (55u64, 100u64, 650_000u64, 2_200_000u64),
            // H.264 High Profile 文本锐利底线：1080p ≈ 2.0 Mbps，2560×1662 ≈ 3.8 Mbps
            rc_encode::Codec::H264 => (100u64, 100u64, 1_200_000u64, 3_800_000u64),
        };
        let scaled = self
            .frame_pixels
            .saturating_mul(per_pixel_num)
            .saturating_div(per_pixel_den)
            .clamp(min_bps, max_bps);
        (scaled as u32).min(self.ceiling_bps).max(1)
    }

    /// 编码前评估本帧目标码率：仅当需要调整 VideoToolbox `AverageBitRate` 时返回 `Some(next_bps)`。
    pub(crate) fn on_frame(
        &mut self,
        dirty: Option<rc_types::Rect>,
        frame_ts_us: u32,
        force_idr: bool,
    ) -> Option<u32> {
        let prev_ts = self.last_frame_ts_us;
        let dt_us = prev_ts
            .map(|prev| frame_ts_us.wrapping_sub(prev))
            .unwrap_or(0);
        let ts_saturated = prev_ts == Some(u32::MAX) && frame_ts_us == u32::MAX;
        self.last_frame_ts_us = Some(frame_ts_us);

        // 无脏区信息时保守保持 100% ceiling（不误伤全帧压测/未知脏区场景）。
        let dirty_ratio = match dirty {
            Some(rect) => {
                let area = u64::from(rect.width).saturating_mul(u64::from(rect.height));
                (area as f64 / self.frame_pixels.max(1) as f64).clamp(0.0, 1.0)
            }
            None => 1.0,
        };

        let mut desired_scale: f64 = if force_idr || dirty_ratio >= 0.35 {
            1.0
        } else if dirty_ratio >= 0.15 {
            0.60
        } else if dirty_ratio >= 0.04 {
            0.35
        } else {
            0.18
        };

        // 低频稀疏更新（<= 12.5 fps，例如文档阅读/慢速打字）：适当再降一档避免合成 PTS 过量分配码率。
        if !force_idr && dirty_ratio < 0.35 && (80_000..10_000_000).contains(&dt_us) {
            desired_scale = (desired_scale * 0.75).max(0.15);
        }

        let floor = self.crisp_text_floor_bps();
        let desired_bps = (((f64::from(self.ceiling_bps) * desired_scale).round()) as u32)
            .clamp(floor, self.ceiling_bps);

        // 1) 即时抬升（1 帧攻击）：大变动/IDR 立即恢复目标码率，避免任何模糊拖影。
        if force_idr || desired_bps > self.active_bps {
            self.low_demand_streak = 0;
            self.low_demand_since_ts_us = None;
            self.frames_since_decay = 0;
            if u64::from(desired_bps).saturating_mul(100)
                >= u64::from(self.active_bps).saturating_mul(112)
                || (desired_bps == self.ceiling_bps && self.active_bps < self.ceiling_bps)
            {
                self.active_bps = desired_bps;
                self.last_decay_ts_us = Some(frame_ts_us);
                return Some(self.active_bps);
            }
            return None;
        }

        // 2) 平滑衰减：连续低变动帧达标、持续时间 >= 250ms 且经过冷却窗口后逐步回落。
        if desired_bps < self.active_bps {
            if self.low_demand_streak == 0 {
                self.low_demand_since_ts_us = Some(frame_ts_us);
            }
            self.low_demand_streak = self.low_demand_streak.saturating_add(1);
            self.frames_since_decay = self.frames_since_decay.saturating_add(1);
            if self.low_demand_streak < Self::DECAY_STREAK_MIN {
                return None;
            }
            let hold_ok = if ts_saturated {
                self.low_demand_streak >= Self::SATURATED_DECAY_COOLDOWN_FRAMES
            } else {
                self.low_demand_since_ts_us.map_or(true, |since| {
                    frame_ts_us.wrapping_sub(since) >= Self::DECAY_HOLD_US
                })
            };
            if !hold_ok {
                return None;
            }
            let cooldown_ok = if ts_saturated {
                self.frames_since_decay >= Self::SATURATED_DECAY_COOLDOWN_FRAMES
            } else {
                self.last_decay_ts_us.map_or(true, |last| {
                    frame_ts_us.wrapping_sub(last) >= Self::DECAY_COOLDOWN_US
                })
            };
            if !cooldown_ok {
                return None;
            }
            // 每拍向 desired_bps 靠拢 max(40% 差值, 12% 当前码率)，差值不足 8% 时不再重复写属性，
            // 保证既不会在距目标 ~30% 处停滞，也不频繁触发 VTSessionSetProperty。
            let gap = self.active_bps.saturating_sub(desired_bps);
            let min_gap = ((u64::from(self.active_bps) * 8) / 100).max(1) as u32;
            if gap < min_gap {
                return None;
            }
            let min_step = ((u64::from(self.active_bps) * 12) / 100).max(1);
            let step = ((u64::from(gap) * 40) / 100).max(min_step) as u32;
            let candidate = self.active_bps.saturating_sub(step).max(desired_bps);
            self.active_bps = candidate;
            self.last_decay_ts_us = Some(frame_ts_us);
            self.frames_since_decay = 0;
            return Some(self.active_bps);
        } else {
            self.low_demand_streak = 0;
            self.low_demand_since_ts_us = None;
            self.frames_since_decay = 0;
        }
        None
    }

    #[cfg(test)]
    pub(crate) fn active_bps(&self) -> u32 {
        self.active_bps
    }
}

/// VideoToolbox 硬件视频编码器（H.264 High / HEVC Main，实现 [`Encoder`](rc_encode::Encoder)）。
///
/// - 只接受 [`FrameTexture::CVPixelBuffer`](rc_capture::FrameTexture::CVPixelBuffer)
///   输入（SCK 零拷贝路径）；CPU 帧请交给软件编码器（对称于
///   `FakeEncoder` 只接受 CPU 帧）；
/// - 会话属性在构造期逐条落死（M6/E1/E2/E3/无限 GOP/BT.709 VUI，见 crate 文档），
///   并通过 E6 启动自检；
/// - 关键帧按需产生：`force_idr` / `request_idr` / 首个正式帧；
///   无限 GOP 下不自动插关键帧。
#[cfg(target_os = "macos")]
pub struct VtEncoder {
    cfg: EncoderConfig,
    session: ffi::CompressionSession,
    rate_ctrl: ContentAdaptiveRateController,
    /// `request_idr` 置位，下一次成功编码消费。
    idr_requested: bool,
    /// 首个正式帧强制 IDR：E6 自检帧的输出不进管线（被丢弃），解码端
    /// 仍需要一个带 SPS/PPS 的 IDR 开场。
    first_frame_pending: bool,
}

#[cfg(target_os = "macos")]
impl fmt::Debug for VtEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // session 是不透明 FFI 句柄，不展开（打印指针地址无诊断价值）。
        f.debug_struct("VtEncoder")
            .field("cfg", &self.cfg)
            .field("idr_requested", &self.idr_requested)
            .field("first_frame_pending", &self.first_frame_pending)
            .finish_non_exhaustive()
    }
}

/// 真机基准实证（Apple M1 Pro 1080p@60 定频）：H.264 开启 PrioritizeEncodingSpeedOverQuality
/// 在同码率（12.2 Mbps）下把 P95/P99 从 10.55/11.08ms 压低至 6.33/6.84ms（-40%）；
/// HEVC 在 Balanced 默认档位表现更优，故仅在 Fastest/Fast 显式开启。
#[cfg(target_os = "macos")]
fn speed_over_quality_for(cfg: &EncoderConfig) -> bool {
    match cfg.codec {
        rc_encode::Codec::H264 => {
            matches!(
                cfg.preset,
                Preset::Fastest | Preset::Fast | Preset::Balanced
            )
        }
        rc_encode::Codec::Hevc | rc_encode::Codec::Av1 => {
            matches!(cfg.preset, Preset::Fastest | Preset::Fast)
        }
    }
}

/// 进程级 E6 自检缓存键：按 `(codec, high_profile, llrc, speed_over_quality, width, height)`
/// 精确区分不同编码器规格与分辨率，消除同配置热重连时重复创建临时解码会话带来的 30–70ms 开销。
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct E6CacheKey {
    codec: rc_encode::Codec,
    high_profile: bool,
    llrc: bool,
    speed_over_quality: bool,
    width: u32,
    height: u32,
}

#[cfg(target_os = "macos")]
const MAX_E6_CACHE_ENTRIES: usize = 64;

#[cfg(target_os = "macos")]
static E6_VERIFIED_KEYS: OnceLock<Mutex<Vec<E6CacheKey>>> = OnceLock::new();

#[cfg(target_os = "macos")]
fn e6_cache_key(cfg: &EncoderConfig) -> E6CacheKey {
    let high_profile = cfg.codec == rc_encode::Codec::H264;
    E6CacheKey {
        codec: cfg.codec,
        high_profile,
        llrc: ffi::use_low_latency_rate_control(cfg.fps),
        speed_over_quality: speed_over_quality_for(cfg),
        width: cfg.resolution.width,
        height: cfg.resolution.height,
    }
}

#[cfg(target_os = "macos")]
fn e6_cache_contains(key: E6CacheKey) -> bool {
    let cache = E6_VERIFIED_KEYS.get_or_init(|| Mutex::new(Vec::new()));
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&key)
}

#[cfg(target_os = "macos")]
fn e6_cache_insert(key: E6CacheKey) {
    let cache = E6_VERIFIED_KEYS.get_or_init(|| Mutex::new(Vec::new()));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if !guard.contains(&key) {
        if guard.len() >= MAX_E6_CACHE_ENTRIES {
            guard.remove(0);
        }
        guard.push(key);
    }
}

#[cfg(target_os = "macos")]
impl VtEncoder {
    /// 创建编码器：配置校验 → 建会话（属性逐条落死）→ E6 启动自检（同进程按配置键缓存）。
    ///
    /// 任何一步失败都返回 `Err`（花屏问题在构造期暴露，而不是运行期）。
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        validate_supported(&cfg)?;
        let speed_over_quality = speed_over_quality_for(&cfg);
        let session = ffi::CompressionSession::new(&ffi::SessionParams {
            codec: cfg.codec,
            width: cfg.resolution.width,
            height: cfg.resolution.height,
            bitrate_bps: cfg.bitrate_bps,
            fps: cfg.fps,
            gop_len: cfg.gop_len,
            speed_over_quality,
        })
        .map_err(Error::Encoder)?;
        let rate_ctrl = ContentAdaptiveRateController::new(&cfg);
        let mut encoder = Self {
            cfg,
            session,
            rate_ctrl,
            idr_requested: false,
            first_frame_pending: true,
        };
        let cache_key = e6_cache_key(&cfg);
        if !e6_cache_contains(cache_key) {
            encoder.self_check()?;
            e6_cache_insert(cache_key);
        }
        Ok(encoder)
    }

    /// E6 启动自检：编一帧假数据（内容未定义的 '420v' 缓冲，无需初始化
    /// 像素——会话可用性不依赖画面内容），先验证输出结构，再用
    /// `VTDecompressionSession` 把这帧**真解码回来**（契约 §7.2 E6 的
    /// 原文：「编一帧假数据并解码验证」）。
    ///
    /// 两层校验各挡一类故障：结构校验（Annex-B 可解析、含 SPS/PPS、是
    /// IDR）暴露「会话建立但产出不可解析码流」；解码验证暴露「码流形态
    /// 对但内容解不回来」（参数集与 slice 不匹配、码流损坏）——花屏类
    /// 问题因此在构造期报 `Err`，而不是上线后由远端用户看到。
    fn self_check(&mut self) -> Result<()> {
        let buffer = ffi::make_pixel_buffer(self.cfg.resolution.width, self.cfg.resolution.height)
            .map_err(Error::Encoder)?;
        let output = self
            .session
            .encode(buffer.as_opaque_ptr(), true, Duration::from_millis(500))
            .map_err(|e| Error::Encoder(format!("E6 启动自检（编码一帧假数据）失败: {e}")))?;
        let is_hevc = match self.cfg.codec {
            rc_encode::Codec::Hevc => true,
            rc_encode::Codec::H264 => false,
            rc_encode::Codec::Av1 => {
                return Err(Error::InvalidConfig(
                    "rc-encode-vt 不支持 AV1 硬件编码（Apple VideoToolbox 仅支持 H.264 与 HEVC/H.265）"
                        .to_string(),
                ));
            }
        };
        if !output.keyframe
            || !annexb::has_parameter_sets_for(is_hevc, &output.data)
            || annexb::nal_unit_types_for(is_hevc, &output.data).is_empty()
        {
            return Err(Error::Encoder(format!(
                "E6 启动自检失败（{:?}）：输出（{} 字节）不是带参数集的 Annex-B IDR",
                self.cfg.codec,
                output.data.len()
            )));
        }
        ffi::decode_verify_annexb_for(
            self.cfg.codec,
            &output.data,
            self.cfg.resolution.width,
            self.cfg.resolution.height,
            Duration::from_secs(2),
        )
        .map_err(|e| Error::Encoder(format!("E6 启动自检（解码验证）失败: {e}")))?;
        Ok(())
    }
}

/// 构造期配置校验：先跑抽象层校验（E1/E2 与基本项），再补本后端的
/// 支持范围。全部在触碰任何 FFI 之前完成。
#[cfg(target_os = "macos")]
fn validate_supported(cfg: &EncoderConfig) -> Result<()> {
    cfg.validate()?;
    match cfg.codec {
        rc_encode::Codec::H264 | rc_encode::Codec::Hevc => {}
        rc_encode::Codec::Av1 => {
            return Err(Error::InvalidConfig(format!(
                "rc-encode-vt 不支持 AV1 硬件编码（Apple VideoToolbox 仅支持 H.264 与 HEVC/H.265，配置的是 {}）",
                cfg.codec.as_str()
            )));
        }
    }
    if !cfg.resolution.is_even() {
        return Err(Error::InvalidConfig(format!(
            "rc-encode-vt: 分辨率必须为偶数（YUV 4:2:0），当前 {}x{}；请先 Resolution::to_even",
            cfg.resolution.width, cfg.resolution.height
        )));
    }
    let max = caps_for(cfg).max_resolution;
    if cfg.resolution.width > max.width || cfg.resolution.height > max.height {
        return Err(Error::InvalidConfig(format!(
            "rc-encode-vt: 分辨率 {}x{} 超过硬件编码上限 {}x{}",
            cfg.resolution.width, cfg.resolution.height, max.width, max.height
        )));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
impl VtEncoder {
    /// 校验输入帧携带有效的 `CVPixelBuffer` 且尺寸与配置匹配（M9 防线）。
    fn validate_cv_pixel_buffer(&self, frame: &CapturedFrame) -> Result<*mut std::ffi::c_void> {
        let buffer = match &frame.texture {
            FrameTexture::CVPixelBuffer(ptr) => *ptr,
            _ => {
                return Err(Error::InvalidFrame(
                    "rc-encode-vt 只接受 FrameTexture::CVPixelBuffer（SCK → VT 零拷贝路径）；CPU 帧请交给软件编码器"
                        .to_string(),
                ))
            }
        };
        if buffer.is_null() {
            return Err(Error::InvalidFrame("CVPixelBuffer 句柄为空".to_string()));
        }
        let (width, height) = ffi::pixel_buffer_dimensions(buffer);
        if width != self.cfg.resolution.width || height != self.cfg.resolution.height {
            return Err(Error::InvalidFrame(format!(
                "CVPixelBuffer {}x{} != 编码配置 {}x{}（M9：Retina 应先下采样到逻辑分辨率）",
                width, height, self.cfg.resolution.width, self.cfg.resolution.height
            )));
        }
        Ok(buffer)
    }
}

#[cfg(target_os = "macos")]
impl Encoder for VtEncoder {
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        force_idr: bool,
        deadline_ts: Timestamp,
    ) -> Result<EncodedFrame> {
        // 零拷贝路径：只接受 CVPixelBuffer 句柄（SCK 交付形态）。
        let buffer = self.validate_cv_pixel_buffer(frame)?;

        // E5：预算 = deadline_ts - 帧时间戳（同一注入时间域），从进入本函数
        // 起算。帧时间戳是内容变化时刻，早于调用时刻的部分计入预算，因此
        // 实际等待可能略长于调用方预期的剩余时间——完成时刻按注入时间域
        // 估算（encode_done_ts），这是契约允许的模型（见 rc-encode 文档）。
        let budget_us = deadline_ts.0.saturating_sub(frame.timestamp.0);
        if budget_us == 0 {
            // 已到截止：立即超时，不进 FFI。
            return Err(Error::Timeout(Duration::ZERO));
        }

        let force = force_idr || self.idr_requested || self.first_frame_pending;
        if let Some(next_bps) = self
            .rate_ctrl
            .on_frame(frame.dirty, frame.timestamp.0, force)
        {
            let _ = self.session.set_bitrate(next_bps);
        }
        let started = Instant::now();
        let output =
            match self
                .session
                .encode(buffer, force, Duration::from_micros(u64::from(budget_us)))
            {
                Ok(output) => output,
                Err(ffi::EncodeError::Timeout) => {
                    // 丢帧恢复：迟到输出即弃（不缓冲，E3），请求下一帧强制 IDR
                    // 重建参考链——低延迟优先于完整。
                    self.idr_requested = true;
                    return Err(Error::Timeout(started.elapsed()));
                }
                Err(ffi::EncodeError::Dropped) => {
                    // 参考链语义（与 Timeout 分支的刻意对比）：
                    // - Dropped：编码器**没有编这帧**——输入被丢，编码器内部
                    //   与解码端的参考链均无损，下一帧照常 P 帧即可，因此
                    //   **不**置 idr_requested（置了会让下一帧无谓地变 IDR）。
                    // - Timeout：输出已产生但迟到被弃 → 解码端缺这一帧、
                    //   参考链真断 → 必须置 idr_requested 重建参考（现状不动）。
                    // RealTime 过载丢帧在高帧率逼近硬编吞吐上限时是预期瞬态
                    // （1080p×120 实测 VT 吞吐 ≈105–108fps < 120fps），
                    // 用非致命变体上报：管线丢帧继续（P7），不拆会话。
                    return Err(Error::EncoderDroppedFrame {
                        msg: "VideoToolbox 丢弃了本帧（RealTime 过载）".to_string(),
                    });
                }
                Err(ffi::EncodeError::Failed(msg)) => return Err(Error::Encoder(msg)),
            };
        self.first_frame_pending = false;
        // force 已随本帧提交，IDR 请求已消费。
        self.idr_requested = false;
        let elapsed_us = u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX);
        Ok(EncodedFrame {
            data: output.data,
            kind: if output.keyframe {
                FrameKind::Idr
            } else {
                FrameKind::P
            },
            is_keyframe: output.keyframe,
            encode_done_ts: Timestamp(frame.timestamp.0.saturating_add(elapsed_us)),
        })
    }

    fn capabilities(&self) -> EncoderCaps {
        caps_for(&self.cfg)
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        if bps == 0 {
            return Err(Error::InvalidConfig(
                "rc-encode-vt: bitrate must be > 0".to_string(),
            ));
        }
        let (next_active, need_vt_update) = self.rate_ctrl.plan_ceiling_change(bps);
        if need_vt_update {
            self.session
                .set_bitrate(next_active)
                .map_err(Error::Encoder)?;
        }
        self.cfg.bitrate_bps = bps;
        self.rate_ctrl.commit_ceiling_change(bps, next_active);
        Ok(())
    }

    fn request_idr(&mut self) {
        self.idr_requested = true;
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        // E3：零输出延迟——会话内没有待取的帧；超时被丢弃的帧不在此复活
        //（丢弃帧的恢复手段是下一帧强制 IDR，不是补发）。
        Ok(Vec::new())
    }

    fn supports_pipelined_encode(&self) -> bool {
        true
    }

    fn submit_encode(
        &mut self,
        token: u64,
        frame: &CapturedFrame,
        force_idr: bool,
        deadline_ts: Timestamp,
    ) -> Result<()> {
        let buffer = self.validate_cv_pixel_buffer(frame)?;
        let budget_us = deadline_ts.0.saturating_sub(frame.timestamp.0);
        if budget_us == 0 {
            return Err(Error::Timeout(Duration::ZERO));
        }
        if !self.session.can_submit() {
            return Err(Error::Encoder(
                "pipelined in_flight limit reached".to_string(),
            ));
        }
        let force = force_idr || self.idr_requested || self.first_frame_pending;
        if let Some(next_bps) = self
            .rate_ctrl
            .on_frame(frame.dirty, frame.timestamp.0, force)
        {
            let _ = self.session.set_bitrate(next_bps);
        }
        self.session
            .submit_encode(
                token,
                frame.timestamp.0,
                buffer,
                force,
                frame.resource_guard.clone(),
            )
            .map_err(|e| match e {
                ffi::EncodeError::Timeout => Error::Timeout(Duration::ZERO),
                ffi::EncodeError::Dropped => Error::EncoderDroppedFrame {
                    msg: "VideoToolbox 丢弃了本帧（RealTime 过载）".to_string(),
                },
                ffi::EncodeError::Failed(msg) => Error::Encoder(msg),
            })?;
        self.first_frame_pending = false;
        self.idr_requested = false;
        Ok(())
    }

    fn recv_encoded(&mut self, timeout: Duration) -> Result<Option<(u64, EncodedFrame)>> {
        let Some(out) = self.session.recv_encoded(timeout) else {
            return Ok(None);
        };
        match out.result {
            Ok(encoded) => {
                let elapsed_us =
                    u32::try_from(out.started.elapsed().as_micros()).unwrap_or(u32::MAX);
                Ok(Some((
                    out.token,
                    EncodedFrame {
                        data: encoded.data,
                        kind: if encoded.keyframe {
                            FrameKind::Idr
                        } else {
                            FrameKind::P
                        },
                        is_keyframe: encoded.keyframe,
                        encode_done_ts: Timestamp(out.frame_ts.saturating_add(elapsed_us)),
                    },
                )))
            }
            // RealTime 过载丢帧：不置 idr_requested，避免无谓插关键帧。
            Err(ffi::EncodeError::Dropped) => Err(Error::EncoderDroppedFrame {
                msg: "VideoToolbox 丢弃了本帧（RealTime 过载）".to_string(),
            }),
            Err(ffi::EncodeError::Failed(msg)) => Err(Error::Encoder(msg)),
            Err(ffi::EncodeError::Timeout) => {
                self.idr_requested = true;
                Err(Error::Timeout(out.started.elapsed()))
            }
        }
    }

    fn in_flight_count(&self) -> usize {
        self.session.in_flight_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_encode::{Codec, FakeEncoder};

    /// E1/E2：违反延迟红线的配置在构造期被拒，错误信息指明字段——
    /// 校验发生在触碰任何 FFI 之前，无 GPU 也能跑。
    #[cfg(target_os = "macos")]
    #[test]
    fn new_rejects_latency_violations_e1_e2() {
        let b_frames = VtEncoder::new(EncoderConfig {
            b_frames: 2,
            ..EncoderConfig::default()
        })
        .unwrap_err();
        assert!(
            matches!(&b_frames, Error::InvalidConfig(msg) if msg.contains("b_frames")),
            "E1：b_frames != 0 必须被拒，实际 {b_frames:?}"
        );

        let low_latency = VtEncoder::new(EncoderConfig {
            low_latency: false,
            ..EncoderConfig::default()
        })
        .unwrap_err();
        assert!(
            matches!(&low_latency, Error::InvalidConfig(msg) if msg.contains("low_latency")),
            "E2：low_latency = false 必须被拒，实际 {low_latency:?}"
        );

        let zero_fps = VtEncoder::new(EncoderConfig {
            fps: 0,
            ..EncoderConfig::default()
        })
        .unwrap_err();
        assert!(
            matches!(&zero_fps, Error::InvalidConfig(msg) if msg.contains("fps")),
            "fps = 0 必须被拒，实际 {zero_fps:?}"
        );
    }

    /// 本后端的支持范围：AV1（非 H.264/HEVC）与奇数分辨率在构造期被拒（同样不碰 FFI）。
    #[cfg(target_os = "macos")]
    #[test]
    fn new_rejects_unsupported_codec_and_odd_resolution() {
        let av1 = VtEncoder::new(EncoderConfig {
            codec: Codec::Av1,
            ..EncoderConfig::default()
        })
        .unwrap_err();
        assert!(
            matches!(&av1, Error::InvalidConfig(msg) if msg.contains("av1") || msg.contains("H.264")),
            "AV1 超出本后端支持范围，实际 {av1:?}"
        );

        let odd = VtEncoder::new(EncoderConfig {
            resolution: rc_types::Resolution::new(1921, 1081),
            ..EncoderConfig::default()
        })
        .unwrap_err();
        assert!(
            matches!(&odd, Error::InvalidConfig(msg) if msg.contains("偶数")),
            "奇数分辨率（YUV 4:2:0 不支持）必须被拒，实际 {odd:?}"
        );
    }

    /// caps 声明与实现严格一致：硬件 H.264、Annex-B（输出侧确实做了
    /// AVCC → Annex-B 重排）、High Profile（CABAC + 8×8 DCT，与会话 ProfileLevel 一致）。
    #[test]
    fn caps_declare_hardware_h264_annexb_high() {
        let caps = caps_for(&EncoderConfig::default());
        assert_eq!(caps.codec, Codec::H264);
        assert!(caps.hardware, "VideoToolbox 是硬件编码器");
        assert_eq!(
            caps.bitstream_format,
            BitstreamFormat::AnnexB,
            "caps 声明什么，解码端就按什么解析——输出侧重排后必须声明 Annex-B"
        );
        assert_eq!(caps.profiles, &[100], "profile idc 100 = High Profile");
    }

    /// AV1 非本后端硬件编码能力：绝不虚报 hardware=true 或 Annex-B，且分辨率上限为 0×0。
    #[test]
    fn caps_for_av1_does_not_claim_hardware_support() {
        let caps = caps_for(&EncoderConfig {
            codec: Codec::Av1,
            ..EncoderConfig::default()
        });
        assert_eq!(caps.codec, Codec::Av1);
        assert!(!caps.hardware, "VideoToolbox 不得虚报 AV1 硬件编码");
        assert_eq!(caps.bitstream_format, BitstreamFormat::Obu);
        assert_eq!(caps.max_resolution, Resolution::new(0, 0));
        assert!(caps.profiles.is_empty());
    }

    #[test]
    fn content_adaptive_rate_controller_backs_off_on_small_dirty_and_attacks_instantly() {
        let cfg = EncoderConfig {
            codec: Codec::Hevc,
            resolution: Resolution::new(2560, 1662),
            fps: 60,
            bitrate_bps: 14_000_000,
            ..EncoderConfig::default()
        };
        let mut ctrl = ContentAdaptiveRateController::new(&cfg);
        assert_eq!(ctrl.active_bps(), 14_000_000);

        // 1) 无脏区信息（None）保持 100% ceiling，不误限流全帧压测
        for i in 0..30u32 {
            assert_eq!(ctrl.on_frame(None, i * 16_667, false), None);
        }
        assert_eq!(ctrl.active_bps(), 14_000_000);

        // 2) 连续小脏区（~2% 屏幕，如终端打字/光标）经过防抖与冷却后平滑回落至文本清晰度底线附近（<= 4 Mbps）
        let small_dirty = Some(rc_types::Rect::new(0, 0, 360, 240));
        let mut ts = 500_000u32;
        for _ in 0..180 {
            ts = ts.wrapping_add(16_667);
            let _ = ctrl.on_frame(small_dirty, ts, false);
        }
        assert!(
            ctrl.active_bps() <= 3_500_000 && ctrl.active_bps() >= 2_000_000,
            "expected smooth backoff near HEVC crisp-text floor, got {}",
            ctrl.active_bps()
        );

        // 3) 突然大幅拖动/滚动（脏区 > 35%）：单帧即时拉满至 14 Mbps
        ts = ts.wrapping_add(16_667);
        let full_dirty = Some(rc_types::Rect::new(0, 0, 2000, 1400));
        assert_eq!(ctrl.on_frame(full_dirty, ts, false), Some(14_000_000));
        assert_eq!(ctrl.active_bps(), 14_000_000);

        // 4) 衰减后外部拥塞下调上限（14M -> 10.5M）绝不把已衰减的 2.5M 暴冲回 10.5M
        for _ in 0..180 {
            ts = ts.wrapping_add(16_667);
            let _ = ctrl.on_frame(small_dirty, ts, false);
        }
        let decayed = ctrl.active_bps();
        assert!(decayed <= 3_500_000);
        let (planned, need_vt) = ctrl.plan_ceiling_change(10_500_000);
        assert_eq!(planned, decayed);
        assert!(!need_vt);
        ctrl.commit_ceiling_change(10_500_000, planned);
        assert_eq!(ctrl.active_bps(), decayed);

        // 5) 外部上限压到比当前 active_bps 还低（1.5M）时立即钳制并下发
        let (clamped, need_vt_clamp) = ctrl.plan_ceiling_change(1_500_000);
        assert_eq!(clamped, 1_500_000);
        assert!(need_vt_clamp);
        ctrl.commit_ceiling_change(1_500_000, clamped);
        assert_eq!(ctrl.active_bps(), 1_500_000);

        // 6) 高刷 240fps 下 10 帧（41.7ms < 250ms hold）短暂停顿不得误触发衰减
        let hi_fps_cfg = EncoderConfig {
            codec: Codec::Hevc,
            resolution: Resolution::new(2464, 1600),
            fps: 240,
            bitrate_bps: 14_000_000,
            ..EncoderConfig::default()
        };
        let mut hi_ctrl = ContentAdaptiveRateController::new(&hi_fps_cfg);
        let mut hts = 0u32;
        for _ in 0..10 {
            hts = hts.wrapping_add(4_167);
            assert_eq!(hi_ctrl.on_frame(small_dirty, hts, false), None);
        }
        assert_eq!(hi_ctrl.active_bps(), 14_000_000);

        // 7) u32::MAX 时间戳饱和长会话下仍可通过帧数冷却兜底正常衰减
        for _ in 0..80 {
            let _ = hi_ctrl.on_frame(small_dirty, u32::MAX, false);
        }
        assert!(
            hi_ctrl.active_bps() <= 3_500_000,
            "expected saturated-ts fallback decay, got {}",
            hi_ctrl.active_bps()
        );
    }

    /// 与 FakeEncoder 的 trait 一致性对照：两条管线的码流形态一致
    /// （Annex-B / H.264 / baseline），调用方可以无差别消费；
    /// 唯一刻意的差别是硬件标志（软件假路径 vs VT 硬件路径）。
    #[test]
    fn caps_shape_matches_fake_encoder() {
        // 显式经 trait 调用：不依赖父模块的（平台门控）导入。
        let fake = rc_encode::Encoder::capabilities(
            &FakeEncoder::new(EncoderConfig::default()).expect("假编码器配置合法"),
        );
        let vt = caps_for(&EncoderConfig::default());
        assert_eq!(
            vt.bitstream_format, fake.bitstream_format,
            "两条管线必须同为 Annex-B（Annex-B 与 AVCC 不能混用）"
        );
        assert_eq!(vt.codec, fake.codec);
        assert!(fake.profiles.contains(&66));
        assert!(!fake.hardware, "假编码器是软件路径");
        assert!(vt.hardware, "VT 是硬件路径");
    }
}

/// 真机测试：需要 macOS + 可用的 VideoToolbox 编码器（不依赖屏幕采集
/// 权限——输入是自建的像素缓冲）。
#[cfg(all(test, target_os = "macos"))]
mod real_machine {
    use super::*;
    use rc_capture::{CapturedFrame, FrameTexture};
    use rc_types::{CaptureSeq, Timestamp};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// 串行化真机测试：避免多个硬件编码会话并发争用媒体引擎、污染
    /// 全局 `latency_diag` 与延迟基线。
    static REAL_MACHINE_LOCK: Mutex<()> = Mutex::new(());

    /// T12 真机验收：验证 `kVTCompressionPropertyKey_RealTime` 确实生效。
    ///
    /// 方法（按任务卡注释）：编码 300 帧，统计从输入到输出的延迟分布；
    /// 未设标志时 VT 按文件转码优化，P50 会 > 30ms；设了应 < 12ms。
    /// 顺带抽检动态行为：首帧 IDR、`request_idr` 下一帧生效、码率可调。
    #[test]
    #[ignore = "requires macOS + 真实 VideoToolbox 编码器（真机延迟验收，T12）"]
    fn videotoolbox_realtime_flag_is_set() {
        let _serial = REAL_MACHINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = crate::ffi::latency_diag::take();
        let mut encoder =
            VtEncoder::new(EncoderConfig::default()).expect("构造（含 E6 启动自检）必须成功");
        let buffer = crate::ffi::make_pixel_buffer(1920, 1080).expect("创建测试像素缓冲");

        let mut samples_ms: Vec<f64> = Vec::with_capacity(300);
        let mut keyframe_count = 0usize;
        // 按 60fps 节奏提交：与帧时间戳（16_667µs/帧）和会话
        // ExpectedFrameRate=60 一致——真实管线（SCK 采集 → 编码）就是
        // 这条节奏。背靠背 ~107fps 提交会让 PTS 谎报节奏、码控按 60fps
        // 误判，测得的是突发吞吐延迟而非稳态单帧延迟。
        let frame_period = Duration::from_nanos(16_666_667);
        let mut next_submit = Instant::now() + frame_period;
        for i in 0u32..300 {
            let ts = Timestamp(i * 16_667);
            let frame = CapturedFrame {
                texture: FrameTexture::CVPixelBuffer(buffer.as_opaque_ptr()),
                timestamp: ts,
                dirty: None,
                seq: CaptureSeq(u64::from(i)),
                is_repeat: false,
                resource_guard: None,
            };
            let started = Instant::now();
            let out = encoder
                .encode(&frame, false, Timestamp(ts.0.saturating_add(50_000)))
                .unwrap_or_else(|e| panic!("第 {i} 帧编码失败（预算 50ms）: {e}"));
            samples_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            if out.is_keyframe {
                keyframe_count += 1;
            }
            // 睡到下一个 60fps 提交点（睡眠在采样之后，不污染延迟测量；
            // 偶发超睡由 next_submit 累计对齐吸收，超时则立即提交）。
            if let Some(sleep) = next_submit.checked_duration_since(Instant::now()) {
                std::thread::sleep(sleep);
            }
            next_submit += frame_period;

            if i == 0 {
                assert!(
                    out.is_keyframe,
                    "首个正式帧必须是 IDR（解码端依赖它建参考）"
                );
            }
            if i == 150 {
                encoder.request_idr();
                encoder
                    .set_bitrate(6_000_000)
                    .expect("动态码率调整必须成功");
            }
            if i == 151 {
                assert!(out.is_keyframe, "request_idr 后下一帧必须是关键帧");
            }
        }

        samples_ms.sort_by(|a, b| a.partial_cmp(b).expect("延迟样本均为有限值"));
        let p50 = samples_ms[samples_ms.len() / 2];
        let p95 = samples_ms[(samples_ms.len() * 95) / 100];
        // 真机诊断输出：验收数字直接可见（--ignored 运行时才打印）。
        eprintln!(
            "VideoToolbox 1080p 输入→输出延迟（300 帧）：P50 = {p50:.2}ms, P95 = {p95:.2}ms, min = {:.2}ms, max = {:.2}ms",
            samples_ms[0],
            samples_ms[samples_ms.len() - 1]
        );

        // —— 延迟分解（cfg(test) 探针，ffi::latency_diag）——
        // 若本次构造执行了 E6 自检（记录数多于正式帧数），跳过第 0 条 E6 帧；四段按最短对齐。
        let marks = crate::ffi::latency_diag::take();
        let total_marks = marks
            .submitted
            .len()
            .min(marks.callback.len())
            .min(marks.woken.len())
            .min(marks.returned.len());
        let start_idx = usize::from(total_marks > 300);
        let n = total_marks.saturating_sub(start_idx);
        let seg_ms = |a: std::time::Instant, b: std::time::Instant| {
            b.checked_duration_since(a)
                .unwrap_or_default()
                .as_secs_f64()
                * 1000.0
        };
        let mut vt_ms: Vec<f64> = Vec::with_capacity(n);
        let mut wake_ms: Vec<f64> = Vec::with_capacity(n);
        let mut extract_ms: Vec<f64> = Vec::with_capacity(n);
        for r in start_idx..(start_idx + n) {
            vt_ms.push(seg_ms(marks.submitted[r], marks.callback[r]));
            wake_ms.push(seg_ms(marks.callback[r], marks.woken[r]));
            extract_ms.push(seg_ms(marks.woken[r], marks.returned[r]));
        }
        let report = |name: &str, xs: &mut Vec<f64>| {
            if xs.is_empty() {
                return;
            }
            xs.sort_by(|a, b| a.partial_cmp(b).expect("延迟样本均为有限值"));
            eprintln!(
                "  {name}: P50 = {:.2}ms, P95 = {:.2}ms, min = {:.2}ms, max = {:.2}ms",
                xs[xs.len() / 2],
                xs[xs.len() * 95 / 100],
                xs[0],
                xs[xs.len() - 1]
            );
        };
        eprintln!(
            "  300 帧中关键帧 = {keyframe_count}（首帧 + request_idr@150 各 1，超出即语义异常）"
        );
        report("submit→callback（VT 内部）", &mut vt_ms);
        report("callback→唤醒（线程切换）", &mut wake_ms);
        report("唤醒→返回（提取+重排）", &mut extract_ms);
        // 相邻回调到达间隔：若 VT 以固定节拍投递输出，会在这里显形。
        let mut cadence_ms: Vec<f64> = Vec::with_capacity(n.saturating_sub(1));
        for r in (start_idx + 1)..(start_idx + n) {
            cadence_ms.push(seg_ms(marks.callback[r - 1], marks.callback[r]));
        }
        report("相邻 callback 间隔（投递节拍）", &mut cadence_ms);
        // 总延迟直方图（1ms 桶，>20ms 归并）：分布形态（单峰/双峰）可见。
        let mut hist: std::collections::BTreeMap<u64, usize> = std::collections::BTreeMap::new();
        for s in &samples_ms {
            *hist.entry((*s as u64).min(20)).or_default() += 1;
        }
        eprintln!("  总延迟直方图（1ms 桶，20 = ≥20ms 归并）：{hist:?}");
        assert!(
            p50 < 12.0,
            "输入→输出延迟 P50 = {p50:.2}ms ≥ 12ms：kVTCompressionPropertyKey_RealTime 可能未生效（未设时 P50 通常 > 30ms；本次 P95 = {p95:.2}ms）"
        );
    }

    /// E3 实证：`kVTCompressionPropertyKey_MaxFrameDelayCount = 0` 在本机
    /// 编码器上的原始设置结果。
    ///
    /// 构造路径只容忍「成功(0)」与 `kVTPropertyNotSupportedErr(-12900)`两种
    /// 结果（其余状态码 = 构造失败）。Apple Silicon 硬件 H.264 编码器对该
    /// 属性一律不支持的声明过去只有代码注释佐证——本测试把它变成真机上的
    /// 可观测事实：跑出任何第三种状态码都说明容忍范围写错了。零输出延迟
    /// 本身不依赖该属性设置成功：由结构保证（严格单帧在途 + 每次提交前
    /// 清空邮箱，见 ffi 模块 E3 注释），300 帧逐帧同步等待的延迟测试
    ///（`videotoolbox_realtime_flag_is_set`）隐式验证了这一点。
    #[test]
    #[ignore = "requires macOS + 真实 VideoToolbox 编码器（E3 属性实证，T12）"]
    fn max_frame_delay_property_status_on_this_device() {
        let _serial = REAL_MACHINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut encoder = VtEncoder::new(EncoderConfig::default())
            .expect("构造（含 E6 自检：编码 + 解码验证）必须成功");
        let status = encoder
            .session
            .probe_max_frame_delay_zero_status()
            .expect("属性键常量自 macOS 10.8 起存在，不可能缺失");
        // 真机诊断输出：本机编码器对该属性的态度直接可见（--ignored 运行时打印）。
        eprintln!("MaxFrameDelayCount=0 在本机的原始 OSStatus = {status}");
        assert!(
            status == 0 || status == -12900,
            "E3 容忍范围外的结果：OSStatus {status}（0 = 设置成功，-12900 = \
             kVTPropertyNotSupportedErr；其它状态码必须让构造失败，不该被容忍）"
        );
    }

    /// 无限 GOP 真机回归：`gop_len = 0` 下连续 300 帧（首帧后全部
    /// `force_idr = false`）必须只产生 1 个 IDR（仅首帧），不自动周期性插
    /// 关键帧；随后显式 `force_idr = true` 仍可立即触发 IDR。
    #[test]
    #[ignore = "requires macOS + 真实 VideoToolbox 编码器（无限 GOP 真机回归）"]
    fn infinite_gop_emits_only_initial_and_forced_idr() {
        let _serial = REAL_MACHINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = EncoderConfig {
            gop_len: 0,
            ..EncoderConfig::default()
        };
        let mut encoder = VtEncoder::new(cfg).expect("构造（含 E6 启动自检）必须成功");
        let buffer = crate::ffi::make_pixel_buffer(1920, 1080).expect("创建测试像素缓冲");

        let mut idr_indices: Vec<u32> = Vec::new();
        for i in 0u32..300 {
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
            if out.is_keyframe {
                idr_indices.push(i);
            }
        }

        let ts_forced = Timestamp(300 * 16_667);
        let forced_frame = CapturedFrame {
            texture: FrameTexture::CVPixelBuffer(buffer.as_opaque_ptr()),
            timestamp: ts_forced,
            dirty: None,
            seq: CaptureSeq(300),
            is_repeat: false,
            resource_guard: None,
        };
        let forced_out = encoder
            .encode(
                &forced_frame,
                true,
                Timestamp(ts_forced.0.saturating_add(50_000)),
            )
            .expect("第 300 帧显式 force_idr 编码必须成功");

        eprintln!(
            "无限 GOP 300 帧真机回归：IDR 数量 = {}（帧号 {idr_indices:?}），第 300 帧 force_idr = {}",
            idr_indices.len(),
            forced_out.is_keyframe
        );
        assert_eq!(
            idr_indices,
            vec![0],
            "gop_len=0（无限 GOP）300 帧内不应自动插入关键帧，仅首帧应为 IDR"
        );
        assert!(
            forced_out.is_keyframe,
            "显式 force_idr=true 必须立即触发 IDR"
        );
    }

    /// 2-deep 流水线编码真机回归：验证 `supports_pipelined_encode`、双帧在途、
    /// `resource_guard` 在提取后释放、以及 FIFO 输出顺序与首帧 IDR。
    #[test]
    #[ignore = "requires macOS + 真实 VideoToolbox 编码器（2-deep 流水线真机验证）"]
    fn pipelined_encode_2deep_preserves_order_and_drops_guards() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct GuardDropCounter(Arc<AtomicUsize>);
        impl Drop for GuardDropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let _serial = REAL_MACHINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut encoder = VtEncoder::new(EncoderConfig::default()).expect("构造 VtEncoder");
        assert!(encoder.supports_pipelined_encode());
        assert_eq!(encoder.in_flight_count(), 0);

        let buffer = crate::ffi::make_pixel_buffer(1920, 1080).expect("创建测试像素缓冲");
        let drops = Arc::new(AtomicUsize::new(0));

        for seq in 0u64..2 {
            let ts = Timestamp((seq as u32) * 16_667);
            let frame = CapturedFrame {
                texture: FrameTexture::CVPixelBuffer(buffer.as_opaque_ptr()),
                timestamp: ts,
                dirty: None,
                seq: CaptureSeq(seq),
                is_repeat: false,
                resource_guard: Some(Arc::new(GuardDropCounter(Arc::clone(&drops)))),
            };
            encoder
                .submit_encode(seq + 100, &frame, false, Timestamp(ts.0 + 100_000))
                .expect("submit_encode 应成功");
            drop(frame);
        }
        assert_eq!(encoder.in_flight_count(), 2);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "在途期间 resource_guard 必须保持存活"
        );

        // 第 3 帧在途满 2 时必须被拒（铁律 1）。
        let extra = CapturedFrame {
            texture: FrameTexture::CVPixelBuffer(buffer.as_opaque_ptr()),
            timestamp: Timestamp(50_000),
            dirty: None,
            seq: CaptureSeq(2),
            is_repeat: false,
            resource_guard: None,
        };
        assert!(
            encoder
                .submit_encode(102, &extra, false, Timestamp(150_000))
                .is_err(),
            "超过 2-deep 上限必须拒绝提交"
        );

        let first = encoder
            .recv_encoded(Duration::from_secs(1))
            .expect("recv 1")
            .expect("first frame ready");
        assert_eq!(first.0, 100);
        assert!(first.1.is_keyframe, "首个正式帧必须是 IDR");
        assert_eq!(encoder.in_flight_count(), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);

        let second = encoder
            .recv_encoded(Duration::from_secs(1))
            .expect("recv 2")
            .expect("second frame ready");
        assert_eq!(second.0, 101);
        assert!(!second.1.is_keyframe, "第二帧应为 P 帧");
        assert_eq!(encoder.in_flight_count(), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod e6_cache_tests {
    use super::*;

    #[test]
    fn e6_cache_key_distinguishes_llrc_profile_and_resolution() {
        let h264_60 = EncoderConfig {
            codec: rc_encode::Codec::H264,
            resolution: Resolution::new(1920, 1080),
            fps: 60,
            ..EncoderConfig::default()
        };
        let h264_120 = EncoderConfig {
            fps: 120,
            ..h264_60
        };
        let hevc_60 = EncoderConfig {
            codec: rc_encode::Codec::Hevc,
            ..h264_60
        };
        let hevc_120 = EncoderConfig {
            fps: 120,
            ..hevc_60
        };
        let hevc_4k_120 = EncoderConfig {
            resolution: Resolution::new(3840, 2160),
            ..hevc_120
        };

        let h264_hq = EncoderConfig {
            preset: Preset::HighQuality,
            ..h264_60
        };
        let hevc_fastest = EncoderConfig {
            preset: Preset::Fastest,
            ..hevc_120
        };

        assert_ne!(e6_cache_key(&h264_60), e6_cache_key(&h264_120));
        assert_ne!(e6_cache_key(&h264_60), e6_cache_key(&h264_hq));
        assert_ne!(e6_cache_key(&hevc_60), e6_cache_key(&hevc_120));
        assert_ne!(e6_cache_key(&hevc_120), e6_cache_key(&hevc_fastest));
        assert_ne!(e6_cache_key(&hevc_120), e6_cache_key(&hevc_4k_120));
        assert_ne!(e6_cache_key(&h264_120), e6_cache_key(&hevc_120));
    }

    #[test]
    fn validate_supported_respects_per_codec_resolution_caps() {
        let cfg_h264_5k = EncoderConfig {
            codec: rc_encode::Codec::H264,
            resolution: Resolution::new(5120, 2880),
            fps: 60,
            ..EncoderConfig::default()
        };
        let err =
            validate_supported(&cfg_h264_5k).expect_err("H.264 超过 4096x4096 应在构造前被拒");
        assert!(matches!(err, Error::InvalidConfig(_)));

        let cfg_hevc_5k = EncoderConfig {
            codec: rc_encode::Codec::Hevc,
            resolution: Resolution::new(5120, 2880),
            fps: 60,
            ..EncoderConfig::default()
        };
        assert!(
            validate_supported(&cfg_hevc_5k).is_ok(),
            "HEVC 应放行 5K Studio Display (5120x2880)"
        );

        let cfg_hevc_over_8k = EncoderConfig {
            codec: rc_encode::Codec::Hevc,
            resolution: Resolution::new(8194, 4320),
            fps: 60,
            ..EncoderConfig::default()
        };
        assert!(matches!(
            validate_supported(&cfg_hevc_over_8k),
            Err(Error::InvalidConfig(_))
        ));
    }
}

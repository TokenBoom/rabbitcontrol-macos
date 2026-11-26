//! [`CaptureBackend`] 的 ScreenCaptureKit 实现。
//!
//! 流程（对应 T12 的 M 条目）：
//! 1. 枚举 `SCShareableContent` → 选显示器（坑 5：先枚举才能建 filter）；
//! 2. `SCContentFilter`（整屏，坑 10：每显示器一个 stream）；
//! 3. `SCStreamConfiguration`：`'420v'`（M5）、`showsCursor=false`（M7）、
//!    输出逻辑分辨率（M9：Retina 下采样）；
//! 4. `SCStream` + 独立串行 dispatch queue 上的帧回调（坑 6），
//!    回调只做提取，帧经最新帧优先单槽邮箱交给 [`next_frame`](SckBackend::next_frame)；
//! 5. `stream:didStopWithError:`（M10，权限撤销）→ `AccessLost`，可重建。
//!
//! 关键语义：
//! - **C1（静止）**：SCK 只在内容变化时回调，静止时 `next_frame` 阻塞到超时
//!   返回 `Ok(None)`，绝不编造重复帧；
//! - **C2（时间戳）**：取 CMSampleBuffer 的 PTS（内容变化时刻），以首帧为
//!   原点换算微秒（见 [`pts_to_timestamp`]）；
//! - **C5（rebuild 幂等）**：`rebuild` 丢弃旧流、重建新流（M8：SCStream
//!   停止后不可复用），连续调用无叠加副作用；
//! - **M10**：失效通过 [`InvalidationReason`] 上报而不是 panic。
//!
//! 本文件不含 unsafe —— 所有 ObjC/FFI 交互都在 [`crate::ffi`]。

use std::sync::Arc;
use std::time::Duration;

use rc_capture::{CaptureBackend, CapturedFrame, DisplayInfo, FrameTexture, InvalidationReason};
use rc_types::{CaptureSeq, DisplayId, Error, Rect, Resolution, Result, Timestamp};

use crate::ffi::{self, FrameEvent, FrameMailbox, FramePayload, StreamHandles};

/// SCK 后端配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SckConfig {
    /// 采集的显示器索引：`0` 表示主显示器（`CGMainDisplayID` 匹配，
    /// 找不到时退回列表第一个），`n > 0` 取枚举列表的第 n 个。
    pub display: u32,
    /// 可选精确目标显示器 ID（`CGDirectDisplayID`；设置后优先于 `display` 索引，支持虚拟屏与多屏切换）。
    pub display_id: Option<u32>,
    /// 是否在采集画面中包含系统鼠标光标（默认 `false`，选择「使用被控端鼠标」时为 `true`）。
    pub shows_cursor: bool,
    /// 帧率上限。SCK 只在内容变化时出帧，这是上限不是承诺（C1）。
    pub fps: u32,
    /// 可选的输出分辨率覆写。
    ///
    /// - `None`（默认）：沿用 M9 逻辑分辨率下采样（`backing / scale`，向下取偶）；
    /// - `Some(res)`：按指定分辨率向下取偶作为 SCK 输出尺寸（用于与编码器目标
    ///   分辨率对齐，如 1080p60）。宽高必须均大于 0。
    pub resolution: Option<Resolution>,
    /// 可选的会话时钟原点（微秒，需来自 [`crate::host_time_us`]，与 SCK PTS 同域）。
    ///
    /// - `None`（默认）：以本流首帧 PTS 为原点（独立采集 / 单测语义）；
    /// - `Some(origin)`：所有帧的 `timestamp = pts_us.saturating_sub(origin)`，
    ///   使 `Mark::Captured` 与管线会话时钟严格同轴、零启动偏移。
    pub origin_us: Option<u64>,
}

impl Default for SckConfig {
    fn default() -> Self {
        Self {
            display: 0,
            display_id: None,
            shows_cursor: false,
            fps: 60,
            resolution: None,
            origin_us: None,
        }
    }
}

/// 单帧采集内部时序埋点（仅 `bench-metrics` 启用，生产构建完全屏蔽）。
#[cfg(feature = "bench-metrics")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureFrameTiming {
    /// SCK `CMSampleBuffer` presentation timestamp（微秒，`CMClockGetHostTimeClock` 域）。
    pub pts_us: u64,
    /// `SCStreamOutput` 回调入口采样时刻（微秒，同域）。
    pub callback_us: u64,
    /// `SckBackend::next_frame` 从邮箱取出该帧的时刻（微秒，同域）。
    pub pop_us: u64,
    /// 截至取出本帧时邮箱累计发生的「旧帧被新帧覆写丢弃」次数。
    pub mailbox_overwrites: u64,
}

#[cfg(feature = "bench-metrics")]
impl CaptureFrameTiming {
    /// SCK PTS → 回调到达交付延迟（微秒，饱和防下溢）。
    pub fn pts_to_callback_us(&self) -> u64 {
        self.callback_us.saturating_sub(self.pts_us)
    }

    /// 回调入箱 → `next_frame` 出箱滞留耗时（微秒，饱和防下溢）。
    pub fn mailbox_wait_us(&self) -> u64 {
        self.pop_us.saturating_sub(self.callback_us)
    }
}

/// 刚从 `SessionLocked`（熄屏/锁屏）退出时，WindowServer / SCShareableContent 就绪前的宽限重建重试上限
/// （配合上层 500ms 退避约 4 秒），避免解锁/唤醒瞬态失败误杀整个会话。
const MAX_POST_LOCK_GRACE_RETRIES: u8 = 8;

/// ScreenCaptureKit 采集后端（实现 [`CaptureBackend`]）。
///
/// 用 [`SckBackend::new`] 创建；失败（无显示器、无权限等）返回
/// `Error::Capture`。
pub struct SckBackend {
    config: SckConfig,
    /// 当前流的句柄 + 最新帧优先单槽邮箱。`None` = 已失效待重建。
    live: Option<(StreamHandles, FrameMailbox)>,
    /// 缓存的显示器信息（display_info 用，避免每次枚举）。
    display: DisplaySummary,
    /// 本流首帧的 PTS（微秒），作为时间戳原点（C2）。
    origin_pts_us: Option<u64>,
    /// 采集序号（跨 rebuild 连续，会话内唯一）。
    seq: CaptureSeq,
    /// 最近一帧缓冲的元信息 `(width, height, stride, pixel_format)`，供测试核对。
    /// 真实 `PixelBufferHandle` 的所有权已随 [`CapturedFrame::resource_guard`] 交给
    /// 消费方，消费/丢弃后即刻 `CFRelease` 归还 SCK IOSurface 池，零悬垂、零占池。
    #[cfg_attr(not(test), allow(dead_code))]
    last_buffer_info: Option<(u32, u32, u32, u32)>,
    /// 待上报的失效原因（重建前持续上报，C4/C5）。
    invalidation: Option<InvalidationReason>,
    /// 失效原因的可读描述。
    invalidation_msg: String,
    /// 从 `SessionLocked` 退出后的瞬态宽限重建已用次数（建流成功或再次熄屏/锁屏时清零）。
    post_lock_grace_retries: u8,
    /// 首帧校验是否已完成（格式 / 分辨率核对）。
    first_frame_checked: bool,
    /// 是否在 `SCContentFilter` 中排除当前进程窗口（仅 `bench-metrics` 启用）。
    #[cfg(feature = "bench-metrics")]
    exclude_current_process: bool,
    /// 最近一帧采集时序快照（仅 `bench-metrics` 启用）。
    #[cfg(feature = "bench-metrics")]
    last_frame_timing: Option<CaptureFrameTiming>,
}

/// `display_info` 需要的显示器摘要（plain data，可跨线程）。
struct DisplaySummary {
    id: DisplayId,
    /// 实际请求 SCK 输出的分辨率（配置覆写或逻辑分辨率，M9）。
    resolution: Resolution,
    /// 物理分辨率 / 逻辑分辨率。
    scale: f32,
}

impl SckBackend {
    /// 创建并启动采集。
    ///
    /// 前置条件：屏幕录制权限已授予（用 [`crate::permission::PermissionStatus::check`]
    /// 先检查）；未授予时启动失败并返回可读错误，不会 panic。
    pub fn new(config: SckConfig) -> Result<Self> {
        Self::new_inner(config, false)
    }

    /// 基准测试专用构造：可选排除当前进程自渲染窗口，并启用采集黑盒时序埋点（仅 `bench-metrics`）。
    #[cfg(feature = "bench-metrics")]
    pub fn new_for_bench(config: SckConfig, exclude_current_process: bool) -> Result<Self> {
        Self::new_inner(config, exclude_current_process)
    }

    /// 最近一帧的采集时序埋点快照（仅 `bench-metrics`）。
    #[cfg(feature = "bench-metrics")]
    pub fn last_frame_timing(&self) -> Option<CaptureFrameTiming> {
        self.last_frame_timing
    }

    /// 截至目前邮箱累计发生的「旧帧被新帧覆写丢弃」次数（仅 `bench-metrics`）。
    #[cfg(feature = "bench-metrics")]
    pub fn mailbox_overwrites(&self) -> u64 {
        self.live
            .as_ref()
            .map_or(0, |(_, mailbox)| mailbox.overwrites())
    }

    fn new_inner(config: SckConfig, exclude_current_process: bool) -> Result<Self> {
        validate_config(&config)?;
        #[cfg(not(feature = "bench-metrics"))]
        let _ = exclude_current_process;
        let fallback_display = fallback_display_summary(&config);
        let mut backend = Self {
            config,
            live: None,
            display: fallback_display,
            origin_pts_us: None,
            seq: CaptureSeq(0),
            last_buffer_info: None,
            invalidation: None,
            invalidation_msg: String::new(),
            post_lock_grace_retries: 0,
            first_frame_checked: false,
            #[cfg(feature = "bench-metrics")]
            exclude_current_process,
            #[cfg(feature = "bench-metrics")]
            last_frame_timing: None,
        };
        if let Err(err) = backend.establish() {
            let asleep = ffi::is_display_asleep(backend.config.display_id);
            let locked = ffi::is_screen_locked();
            if asleep || locked {
                tracing::warn!(
                    display_asleep = asleep,
                    screen_locked = locked,
                    error = %err,
                    "SCK 冷启动建流失败且屏幕处于熄屏/锁屏状态，进入挂起待重建模式"
                );
                backend.enter_suspended_state(format!("屏幕熄屏或锁屏，采集挂起待重建: {err}"));
                return Ok(backend);
            }
            return Err(err);
        }
        Ok(backend)
    }

    /// 将后端置入「挂起待重建」态（`live: None`, `invalidation: Some(SessionLocked)`），
    /// 避免冷启动遇熄屏/锁屏时降级至假后端。
    fn enter_suspended_state(&mut self, msg: impl Into<String>) {
        self.live = None;
        self.last_buffer_info = None;
        self.invalidation = Some(InvalidationReason::SessionLocked);
        self.invalidation_msg = msg.into();
        self.post_lock_grace_retries = 0;
    }

    /// 枚举显示器并构建一条全新的 SCStream（`new` 与 `rebuild` 共用，M8）。
    fn establish(&mut self) -> Result<()> {
        let displays = ffi::enumerate_displays().map_err(unknown_capture_error)?;
        let snapshot = pick_display(&displays, self.config.display, self.config.display_id)
            .ok_or_else(|| {
                unknown_capture_error(format!(
                    "找不到可采集的显示器（index={}, id={:?}, 共 {} 个）",
                    self.config.display,
                    self.config.display_id,
                    displays.len()
                ))
            })?;

        // 若配置指定了输出分辨率，取偶后直接请求 SCK 按该尺寸缩放输出；
        // 否则沿用 M9：Retina 下采样到逻辑分辨率（GPU 侧完成，CPU 不过手），
        // 再保证偶数（YUV 4:2:0 / 编码器要求，参考 rc-types 的 to_even）。
        //
        // 注意：这里刻意**不**断言 target == SCDisplay 上报逻辑分辨率取偶
        //——下采样数学的正确性由单测 retina_downsample_computes_correct_target
        // 覆盖，而系统上报数据是否自洽不由我们保证，依赖它的断言放在运行
        // 路径上会在系统数据不一致时把 debug 构建打 panic。真正的防线在
        // 首帧校验：SCK 实际交付的分辨率必须等于我们请求的 target，否则报错。
        let target =
            resolve_target_resolution(self.config.resolution, snapshot.backing, snapshot.scale);

        #[cfg(feature = "bench-metrics")]
        let exclude_self = self.exclude_current_process;
        #[cfg(not(feature = "bench-metrics"))]
        let exclude_self = false;

        let (handles, mailbox) = ffi::build_stream_with_options(
            snapshot,
            target,
            self.config.fps,
            exclude_self,
            self.config.shows_cursor,
        )
        .map_err(unknown_capture_error)?;

        self.display = DisplaySummary {
            id: DisplayId(self.config.display),
            resolution: target,
            scale: snapshot.scale,
        };
        self.live = Some((handles, mailbox));
        self.origin_pts_us = None;
        self.last_buffer_info = None;
        self.post_lock_grace_retries = 0;
        self.first_frame_checked = false;
        Ok(())
    }

    /// 记录失效并拆除当前流（保留原因供 `poll_invalidation` 持续上报）。
    fn invalidate(&mut self, reason: InvalidationReason, msg: impl Into<String>) -> Error {
        self.live = None;
        self.last_buffer_info = None;
        self.invalidation = Some(reason);
        self.invalidation_msg = msg.into();
        reason.to_capture_error(self.invalidation_msg.clone())
    }
}

impl CaptureBackend for SckBackend {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>> {
        std::thread_local! {
            // MSRV 1.75 尚不支持 `const { ... }` 内联常量语法；`Cell::new(false)` 本身已是 const 构造。
            #[allow(clippy::missing_const_for_thread_local)]
            static QOS_PROMOTED: std::cell::Cell<bool> = std::cell::Cell::new(false);
        }
        QOS_PROMOTED.with(|promoted| {
            if !promoted.get() {
                crate::promote_current_thread_qos();
                promoted.set(true);
            }
        });

        // 已失效：持续返回错误，直到上层 rebuild（C4）。
        if let Some(reason) = self.invalidation {
            return Err(reason.to_capture_error(self.invalidation_msg.clone()));
        }
        let Some((_, mailbox)) = &self.live else {
            // 不应出现（无流必有失效记录），防御性兜底。
            return Err(self.invalidation_wrap(InvalidationReason::Unknown, "流不存在且无失效记录"));
        };

        let event = mailbox.recv_timeout(timeout);
        #[cfg(feature = "bench-metrics")]
        let mailbox_overwrites = mailbox.overwrites();
        match event {
            Some(FrameEvent::Frame(payload)) => {
                #[cfg(feature = "bench-metrics")]
                let FramePayload {
                    buffer,
                    pts_us,
                    dirty_ratio,
                    callback_us,
                } = payload;
                #[cfg(not(feature = "bench-metrics"))]
                let FramePayload {
                    buffer,
                    pts_us,
                    dirty_ratio,
                } = payload;

                // C2：优先使用调用方注入的会话时钟原点（与管线同域同零点）；
                // 未注入时以本流首帧 PTS 为原点。SCK 的 PTS 单调递增。
                let origin = match self.config.origin_us {
                    Some(o) => o,
                    None => *self.origin_pts_us.get_or_insert(pts_us),
                };
                let timestamp = pts_to_timestamp(pts_us, origin);

                // 首帧校验：SCK 必须按我们的配置交付 '420v' + 请求的分辨率，
                // 否则零拷贝链路（M5/M9）建立在错误的前提上。
                if !self.first_frame_checked {
                    self.first_frame_checked = true;
                    let (w, h, _stride, format) = buffer.info();
                    if format != ffi::PIXEL_FORMAT_420V {
                        return Err(Error::InvalidFrame(format!(
                            "SCK 交付的像素格式 0x{format:08x} 不是 '420v'（0x{:08x}）",
                            ffi::PIXEL_FORMAT_420V
                        )));
                    }
                    if Resolution::new(w, h) != self.display.resolution {
                        return Err(Error::InvalidFrame(format!(
                            "SCK 交付分辨率 {w}x{h} 与请求的逻辑分辨率 {}x{} 不符",
                            self.display.resolution.width, self.display.resolution.height
                        )));
                    }
                }

                // 将 PixelBufferHandle 装入 CapturedFrame::resource_guard：
                // 帧在跨线程编码期间始终保活，Drop 时立即 CFRelease 归还 SCK 池。
                let next_seq = self.seq.0.checked_add(1).ok_or_else(|| {
                    InvalidationReason::Unknown
                        .to_capture_error("采集序号耗尽 u64，无法继续保证单调".to_string())
                })?;
                self.last_buffer_info = Some(buffer.info());
                let texture = FrameTexture::CVPixelBuffer(buffer.as_opaque_ptr());
                self.seq = CaptureSeq(next_seq);
                #[cfg(feature = "bench-metrics")]
                {
                    let pop_us = ffi::host_time_us();
                    self.last_frame_timing = Some(CaptureFrameTiming {
                        pts_us,
                        callback_us,
                        pop_us,
                        mailbox_overwrites,
                    });
                }
                // 将 SCStreamFrameInfoDirtyRects 面积占比映射为等面积 Rect 供编码器内容自适应码控消费；
                // 无法解析或接近全屏（>= 99.5%）时返回 None（按全帧处理）。SCK 不会重复交同一帧：is_repeat 恒 false。
                let dirty =
                    dirty_ratio.and_then(|r| dirty_ratio_to_rect(r, self.display.resolution));
                Ok(Some(CapturedFrame {
                    texture,
                    timestamp,
                    dirty,
                    seq: self.seq,
                    is_repeat: false,
                    resource_guard: Some(Arc::new(buffer)),
                }))
            }
            Some(FrameEvent::Suspended) => Err(self.invalidation_wrap(
                InvalidationReason::SessionLocked,
                "SCStream 挂起（锁屏 / 会话切换），解锁后请 rebuild",
            )),
            Some(FrameEvent::Stopped { message }) => {
                let reason = classify_stop_reason(
                    ffi::is_display_asleep(self.config.display_id),
                    ffi::is_screen_locked(),
                );
                Err(self.invalidation_wrap(reason, format!("SCStream 已停止: {message}")))
            }
            None => {
                // C1：超时即无变化，返回 None 而不是错误 / 重复帧。
                Ok(None)
            }
        }
    }

    fn poll_invalidation(&mut self) -> Option<InvalidationReason> {
        self.invalidation
    }

    fn rebuild(&mut self) -> Result<()> {
        // C5 幂等 + M8：SCStream 停止后不可复用 —— 直接丢弃旧流
        // （StreamHandles::drop 会显式 stop），然后构建全新实例。
        // 连续调用只是多建几条新流，无叠加副作用。
        // 注意：不在采集线程同步调用 `wake_display_if_asleep`（亮屏唤醒与冷却退避由上层
        // `host_bridge` 统一编排），避免锁屏期间反复唤醒或在停机 `join` 时卡顿。
        let was_session_locked =
            matches!(self.invalidation, Some(InvalidationReason::SessionLocked));
        self.live = None;
        self.invalidation = None;
        self.invalidation_msg.clear();
        self.last_buffer_info = None;

        if let Err(err) = self.establish() {
            let asleep = ffi::is_display_asleep(self.config.display_id);
            let locked = ffi::is_screen_locked();
            let has_access = if was_session_locked && !asleep && !locked {
                ffi::preflight_screen_capture_access()
            } else {
                false
            };
            return Err(self.handle_rebuild_failure(
                err,
                was_session_locked,
                asleep,
                locked,
                has_access,
            ));
        }
        self.post_lock_grace_retries = 0;
        Ok(())
    }

    fn display_info(&self) -> DisplayInfo {
        DisplayInfo {
            id: self.display.id,
            resolution: self.display.resolution,
            scale: self.display.scale,
            // SDR 路径：SCK 的 SCCaptureDynamicRange 默认 SDR。HDR 切换
            // （HdrToggled）在启用 HDR 采集的后续卡里处理。
            hdr: false,
        }
    }
}

impl SckBackend {
    /// [`invalidate`](SckBackend::invalidate) 的内部变体：返回错误而不是只记录。
    fn invalidation_wrap(&mut self, reason: InvalidationReason, msg: impl Into<String>) -> Error {
        self.invalidate(reason, msg)
    }

    /// 处理 `rebuild()` 中的 `establish()` 失败（维持 `live.is_none() => invalidation.is_some()` 不变量）。
    ///
    /// - 仍处于熄屏/锁屏：清零宽限计数并返回可恢复 `SessionLocked`；
    /// - 刚从 `SessionLocked` 恢复且仍有 TCC 屏幕录制权限、宽限未超 `MAX_POST_LOCK_GRACE_RETRIES`：
    ///   递增宽限计数并返回可恢复 `SessionLocked`，等待 WindowServer 完成唤醒/解锁过渡；
    /// - 其余情况（权限撤销、宽限耗尽或非锁屏路径失败）：写入 `InvalidationReason::Unknown` 并返回致命错误。
    fn handle_rebuild_failure(
        &mut self,
        err: Error,
        was_session_locked: bool,
        display_asleep: bool,
        screen_locked: bool,
        has_screen_capture_access: bool,
    ) -> Error {
        if display_asleep || screen_locked {
            self.post_lock_grace_retries = 0;
            let msg = format!("屏幕熄屏或锁屏，重建推迟: {err}");
            return self.invalidate(InvalidationReason::SessionLocked, msg);
        }
        if was_session_locked
            && self.post_lock_grace_retries < MAX_POST_LOCK_GRACE_RETRIES
            && has_screen_capture_access
        {
            self.post_lock_grace_retries = self.post_lock_grace_retries.saturating_add(1);
            let retry_no = self.post_lock_grace_retries;
            tracing::warn!(
                retry = retry_no,
                max_retries = MAX_POST_LOCK_GRACE_RETRIES,
                error = %err,
                "SCK 解锁/唤醒过渡期重建暂未就绪，按可恢复 SessionLocked 宽限重试"
            );
            let msg = format!(
                "屏幕刚唤醒/解锁，SCK 过渡期宽限重试 ({retry_no}/{MAX_POST_LOCK_GRACE_RETRIES}): {err}"
            );
            return self.invalidate(InvalidationReason::SessionLocked, msg);
        }
        self.post_lock_grace_retries = 0;
        self.invalidate(InvalidationReason::Unknown, err.to_string())
    }
}

/// 校验 [`SckConfig`] 参数合法性（在建流之前执行）。
fn validate_config(config: &SckConfig) -> Result<()> {
    if config.fps == 0 {
        return Err(Error::InvalidConfig("fps 必须大于 0".to_string()));
    }
    if let Some(res) = config.resolution {
        if res.width == 0 || res.height == 0 {
            return Err(Error::InvalidConfig(format!(
                "resolution 宽高必须均大于 0，实际 {}x{}",
                res.width, res.height
            )));
        }
    }
    Ok(())
}

/// 将脏区面积占比（`0.0..=1.0`）换算为等面积 `Rect`（裁剪在输出分辨率内）。
///
/// 占比无效或 `>= 0.995` 时返回 `None`（按全帧变化处理）；极小正占比至少返回 `1×1`。
pub(crate) fn dirty_ratio_to_rect(ratio: f32, res: Resolution) -> Option<Rect> {
    if !ratio.is_finite() || ratio <= 0.0 || ratio >= 0.995 || res.width == 0 || res.height == 0 {
        return None;
    }
    let scale = f64::from(ratio.clamp(0.0, 1.0)).sqrt();
    let w = ((f64::from(res.width) * scale).round() as u32).clamp(1, res.width);
    let target_area = f64::from(res.width) * f64::from(res.height) * f64::from(ratio);
    let h = ((target_area / f64::from(w)).round() as u32).clamp(1, res.height);
    Some(Rect::new(0, 0, w, h))
}

/// 决定 SCK 实际请求的输出分辨率：显式配置优先（向下取偶），否则按 M9 逻辑分辨率下采样。
pub(crate) fn resolve_target_resolution(
    configured: Option<Resolution>,
    backing: Resolution,
    scale: f32,
) -> Resolution {
    match configured {
        Some(res) => res.to_even(),
        None => downsample_target(backing, scale),
    }
}

/// 把建流期的未知失败映射为致命采集错误（原因不明时不盲目重试）。
fn unknown_capture_error(msg: impl Into<String>) -> Error {
    InvalidationReason::Unknown.to_capture_error(msg)
}

/// 根据屏幕熄屏/锁屏状态将 `SCStream` 停止回调分类为 `SessionLocked`（可恢复）或 `AccessLost`。
pub(crate) fn classify_stop_reason(
    display_asleep: bool,
    screen_locked: bool,
) -> InvalidationReason {
    if screen_locked || display_asleep {
        InvalidationReason::SessionLocked
    } else {
        InvalidationReason::AccessLost
    }
}

/// 构造冷启动挂起态下的兜底显示器摘要（纯逻辑可单测），保证 `display_info()` 始终返回有效非零偶分辨率。
fn fallback_display_summary_with(
    config: &SckConfig,
    logical_res: Option<Resolution>,
    backing_res: Option<Resolution>,
) -> DisplaySummary {
    let resolution = config
        .resolution
        .map(|r| r.to_even())
        .filter(|r| r.width >= 2 && r.height >= 2)
        .or_else(|| logical_res.filter(|r| r.width >= 2 && r.height >= 2))
        .unwrap_or_else(|| Resolution::new(1920, 1080));
    let scale = backing_res
        .and_then(|backing| {
            if resolution.width > 0 && backing.width >= resolution.width {
                Some(backing.width as f32 / resolution.width as f32)
            } else {
                None
            }
        })
        .filter(|s| s.is_finite() && *s >= 1.0)
        .unwrap_or(1.0);
    DisplaySummary {
        id: DisplayId(config.display),
        resolution,
        scale,
    }
}

/// 结合系统主屏边界查询构造冷启动挂起态兜底显示器摘要。
fn fallback_display_summary(config: &SckConfig) -> DisplaySummary {
    fallback_display_summary_with(
        config,
        ffi::main_display_logical_resolution(),
        ffi::main_display_backing_resolution(),
    )
}

/// 按配置挑选显示器：优先精确匹配 `display_id`，其次 `index == 0` 匹配主显示器，否则取第 `index` 个。
fn pick_display(
    displays: &[ffi::DisplaySnapshot],
    index: u32,
    display_id: Option<u32>,
) -> Option<&ffi::DisplaySnapshot> {
    if displays.is_empty() {
        return None;
    }
    if let Some(target_id) = display_id {
        if let Some(found) = displays.iter().find(|d| d.display_id == target_id) {
            return Some(found);
        }
    }
    if index == 0 {
        let main_id = ffi::main_display_id();
        displays
            .iter()
            .find(|d| d.display_id == main_id)
            .or_else(|| displays.first())
    } else {
        displays.get(index as usize)
    }
}

/// M9：计算 SCK 输出分辨率 —— 物理分辨率按 scale 下采样到逻辑分辨率，
/// 再向下取整到偶数。
///
/// - Retina 2x：`3840x2160 @2.0 → 1920x1080`；
/// - 非 Retina（scale = 1）：原样；
/// - 病态 scale（0 / NaN / Inf）：按 1.0 处理，绝不出 0 尺寸。
pub(crate) fn downsample_target(backing: Resolution, scale: f32) -> Resolution {
    let scale = if scale.is_finite() && scale >= 1.0 {
        scale
    } else {
        1.0
    };
    let width = ((backing.width as f32) / scale).round();
    let height = ((backing.height as f32) / scale).round();
    Resolution::new(width as u32, height as u32).to_even()
}

/// C2：PTS（微秒）→ 相对会话原点的时间戳。
///
/// - 原点取本流首帧的 PTS，因此首帧时间戳为 0；
/// - 超过 `u32::MAX` 微秒（约 71.6 分钟，`Timestamp` 的表示范围）时饱和到
///   最大值而不是回绕 —— 回绕中的 PTS 序列对延迟归因是灾难。
pub(crate) fn pts_to_timestamp(pts_us: u64, origin_us: u64) -> Timestamp {
    Timestamp(u32::try_from(pts_us.saturating_sub(origin_us)).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn retina_downsample_computes_correct_target() {
        // M9：Retina 2x → 逻辑分辨率（不把 2x 喂给编码器）。
        assert_eq!(
            downsample_target(Resolution::new(3840, 2160), 2.0),
            Resolution::new(1920, 1080)
        );
        assert_eq!(
            downsample_target(Resolution::new(3024, 1964), 2.0),
            Resolution::new(1512, 982)
        );
        assert_eq!(
            downsample_target(Resolution::new(3360, 2100), 2.0),
            Resolution::new(1680, 1050)
        );
        assert_eq!(
            downsample_target(Resolution::new(2560, 1600), 2.0),
            Resolution::new(1280, 800)
        );
        // 非 Retina：原样保留。
        assert_eq!(
            downsample_target(Resolution::new(1920, 1080), 1.0),
            Resolution::new(1920, 1080)
        );
        assert_eq!(
            downsample_target(Resolution::new(1440, 900), 1.0),
            Resolution::new(1440, 900)
        );
        // 逻辑尺寸出现奇数时向下取偶（YUV 4:2:0 的前置条件）。
        assert_eq!(
            downsample_target(Resolution::new(3026, 1964), 2.0),
            Resolution::new(1512, 982)
        );
        assert_eq!(
            downsample_target(Resolution::new(1515, 983), 1.0),
            Resolution::new(1514, 982)
        );
        // 病态 scale：按 1.0 处理，绝不出 0 尺寸。
        for bad in [0.0, f32::NAN, f32::INFINITY] {
            assert_eq!(
                downsample_target(Resolution::new(1920, 1080), bad),
                Resolution::new(1920, 1080),
                "scale = {bad} 时应按 1.0 处理"
            );
        }
    }

    #[test]
    fn frame_timestamp_monotonic() {
        // C2：时间戳 = 内容变化时刻（PTS 相对首帧），必须单调递增。
        // 60fps 的典型 PTS 序列（微秒，timescale=600 的等价换算）。
        let pts_series = [0u64, 16_667, 33_334, 50_001, 66_668, 83_335];
        let mut origin: Option<u64> = None;
        let mut stamps = Vec::new();
        for &pts in &pts_series {
            let o = *origin.get_or_insert(pts);
            stamps.push(pts_to_timestamp(pts, o));
        }
        // 首帧即原点：时间戳为 0。
        assert_eq!(stamps[0], Timestamp(0));
        // 严格递增。
        for pair in stamps.windows(2) {
            assert!(
                pair[1] > pair[0],
                "时间戳必须严格递增：{} -> {}",
                pair[0].0,
                pair[1].0
            );
        }
        // 换算正确性：16.667ms 的帧间隔 ≈ 16667us。
        assert_eq!(stamps[1], Timestamp(16_667));
        // 饱和：超过 u32::MAX 微秒（约 71.6 分钟）封顶，绝不回绕。
        // 4e9us（仍在范围内）如实保留；5e9us（超范围）饱和。
        assert_eq!(pts_to_timestamp(u64::MAX, 0), Timestamp(u32::MAX));
        assert_eq!(
            pts_to_timestamp(5_000_000_000, 1_000_000_000),
            Timestamp(4_000_000_000)
        );
        assert_eq!(
            pts_to_timestamp(6_000_000_000, 1_000_000_000),
            Timestamp(u32::MAX)
        );
    }

    #[test]
    fn configured_resolution_validation_and_even_rounding() {
        // 默认配置 resolution 为 None，回退到 M9 逻辑分辨率下采样。
        let default_cfg = SckConfig::default();
        assert_eq!(default_cfg.resolution, None);
        assert!(validate_config(&default_cfg).is_ok());
        assert_eq!(
            resolve_target_resolution(default_cfg.resolution, Resolution::new(3024, 1964), 2.0),
            Resolution::new(1512, 982)
        );

        // 显式指定 resolution：合法配置通过校验，并向下取偶以匹配 YUV 4:2:0。
        let cfg_1080p = SckConfig {
            resolution: Some(Resolution::new(1920, 1080)),
            ..SckConfig::default()
        };
        assert!(validate_config(&cfg_1080p).is_ok());
        assert_eq!(
            resolve_target_resolution(cfg_1080p.resolution, Resolution::new(3024, 1964), 2.0),
            Resolution::new(1920, 1080)
        );

        let cfg_odd = SckConfig {
            resolution: Some(Resolution::new(1921, 1081)),
            ..SckConfig::default()
        };
        assert!(validate_config(&cfg_odd).is_ok());
        assert_eq!(
            resolve_target_resolution(cfg_odd.resolution, Resolution::new(3024, 1964), 2.0),
            Resolution::new(1920, 1080)
        );

        // 宽或高为 0 时 SckBackend::new 必须返回 Error::InvalidConfig（不触发 SCK 建流）。
        for invalid in [
            Resolution::new(0, 1080),
            Resolution::new(1920, 0),
            Resolution::new(0, 0),
        ] {
            let err = SckBackend::new(SckConfig {
                resolution: Some(invalid),
                ..SckConfig::default()
            })
            .err()
            .expect("零宽高分辨率应拒绝");
            assert!(
                matches!(err, Error::InvalidConfig(_)),
                "期望 InvalidConfig，实际: {err:?}"
            );
        }
    }

    // ── 真机测试（需要 macOS + 三项授权，CI / 沙箱跑不了）──

    /// 真机验收（T12）：1080p 采集 ≥58fps。
    ///
    /// 注意：SCK 只在内容变化时出帧 —— 运行本测试时屏幕上必须有动态内容
    /// （播放视频 / 拖动窗口），屏幕静止时拿不到帧不是实现 bug。
    #[test]
    #[ignore = "requires macOS + permissions granted"]
    fn sck_captures_at_60fps() {
        let mut backend = SckBackend::new(SckConfig::default()).expect("构建 SCK 后端");
        let mut count = 0u64;
        let mut last: Option<Timestamp> = None;
        let mut dts_us: Vec<u32> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Some(frame) = backend
                .next_frame(Duration::from_millis(100))
                .expect("采集循环不应出错")
            {
                if let Some(prev) = last {
                    assert!(frame.timestamp > prev, "时间戳必须单调递增（C2）");
                    dts_us.push(frame.timestamp.0 - prev.0);
                }
                last = Some(frame.timestamp);
                count += 1;
            }
        }
        let sample_dts: Vec<u32> = dts_us.iter().copied().take(15).collect();
        let mut sorted = dts_us.clone();
        sorted.sort_unstable();
        let p50 = sorted.get(sorted.len() / 2).copied().unwrap_or(0);
        let min_dt = sorted.first().copied().unwrap_or(0);
        let max_dt = sorted.last().copied().unwrap_or(0);
        eprintln!(
            "sck_captures_at_60fps: count={count}, dt_us(min={min_dt}, p50={p50}, max={max_dt}), first15={sample_dts:?}"
        );
        assert!(
            count >= 116,
            "2 秒内应采集 ≥116 帧（≥58fps），实际 {count} 帧 \
             （若屏幕静止请先制造动态内容再跑）"
        );
    }

    /// M8：SCStream 停止后不可复用 —— rebuild 必须创建全新实例并恢复采集。
    #[test]
    #[ignore = "requires macOS + permissions granted"]
    fn sck_stream_rebuilds_after_stop() {
        let mut backend = SckBackend::new(SckConfig::default()).expect("构建 SCK 后端");
        // 先收 1 秒帧（屏幕静止时收不到帧是正常的，不作为失败）。
        let _ = backend
            .next_frame(Duration::from_secs(1))
            .expect("首帧阶段不应出错");
        let seq_before = backend.seq;

        // rebuild = 丢弃旧流 + 建新流（M8 的可观测行为：成功、无失效残留）。
        backend.rebuild().expect("rebuild 必须成功");
        assert_eq!(backend.poll_invalidation(), None, "重建后失效状态应清零");

        // 重建后必须还能出帧；采集序号跨 rebuild 连续。
        if let Some(frame) = backend
            .next_frame(Duration::from_secs(3))
            .expect("重建后不应出错")
        {
            assert!(
                frame.seq.0 > seq_before.0,
                "重建后的帧序号必须继续前进（会话内唯一）"
            );
            // M9 真机核对：SCK 实际交付的是逻辑分辨率（不是 2x 物理分辨率）。
            let (w, h, _stride, format) = backend
                .last_buffer_info
                .expect("出帧后必须记录最近缓冲信息");
            assert_eq!(
                format,
                ffi::PIXEL_FORMAT_420V,
                "交付格式必须是 '420v'（M5）"
            );
            let info = backend.display_info();
            assert_eq!(
                (w, h),
                (info.resolution.width, info.resolution.height),
                "交付分辨率 {w}x{h} 应等于请求的逻辑分辨率（M9）"
            );
        }
        // 连续 rebuild（C5 幂等）：第二次同样成功。
        backend.rebuild().expect("连续 rebuild 必须成功（C5 幂等）");
    }

    #[cfg(feature = "bench-metrics")]
    #[test]
    fn capture_frame_timing_saturates_underflow_and_computes_deltas() {
        let timing = CaptureFrameTiming {
            pts_us: 10_000,
            callback_us: 12_500,
            pop_us: 12_850,
            mailbox_overwrites: 3,
        };
        assert_eq!(timing.pts_to_callback_us(), 2_500);
        assert_eq!(timing.mailbox_wait_us(), 350);

        let skewed = CaptureFrameTiming {
            pts_us: 20_000,
            callback_us: 19_000,
            pop_us: 18_500,
            mailbox_overwrites: 0,
        };
        assert_eq!(skewed.pts_to_callback_us(), 0);
        assert_eq!(skewed.mailbox_wait_us(), 0);
    }

    #[test]
    fn dirty_ratio_to_rect_preserves_area_and_bounds() {
        let res = Resolution::new(2560, 1662);
        let r = dirty_ratio_to_rect(0.04, res).expect("4% dirty rect");
        let area_ratio = (f64::from(r.width) * f64::from(r.height))
            / (f64::from(res.width) * f64::from(res.height));
        assert!((area_ratio - 0.04).abs() < 0.002, "got {area_ratio}");
        assert!(r.width <= res.width && r.height <= res.height);
        assert_eq!(dirty_ratio_to_rect(0.0, res), None);
        assert_eq!(dirty_ratio_to_rect(1.0, res), None);
        assert_eq!(dirty_ratio_to_rect(f32::NAN, res), None);
    }

    fn make_test_backend(cfg: SckConfig) -> SckBackend {
        let display = fallback_display_summary_with(&cfg, None, None);
        SckBackend {
            config: cfg,
            live: None,
            display,
            origin_pts_us: None,
            seq: CaptureSeq(0),
            last_buffer_info: None,
            invalidation: None,
            invalidation_msg: String::new(),
            post_lock_grace_retries: 0,
            first_frame_checked: false,
            #[cfg(feature = "bench-metrics")]
            exclude_current_process: false,
            #[cfg(feature = "bench-metrics")]
            last_frame_timing: None,
        }
    }

    #[test]
    fn classify_stop_reason_for_sleep_and_lock() {
        // 熄屏或锁屏时停止回调归类为可恢复 SessionLocked；亮屏且解锁时归类为 AccessLost。
        assert_eq!(
            classify_stop_reason(true, false),
            InvalidationReason::SessionLocked
        );
        assert_eq!(
            classify_stop_reason(false, true),
            InvalidationReason::SessionLocked
        );
        assert_eq!(
            classify_stop_reason(true, true),
            InvalidationReason::SessionLocked
        );
        assert_eq!(
            classify_stop_reason(false, false),
            InvalidationReason::AccessLost
        );
    }

    #[test]
    fn rebuild_failure_post_unlock_grace_budget_and_fatal_invariants() {
        // 1. 刚从 SessionLocked 解锁且具备 TCC 权限：允许最多 MAX_POST_LOCK_GRACE_RETRIES 次可恢复重试，
        //    超限后升级为致命 Unknown，且始终满足 live.is_none() => invalidation.is_some()。
        let mut backend = make_test_backend(SckConfig::default());
        backend.enter_suspended_state("locked");

        for expected_retry in 1..=MAX_POST_LOCK_GRACE_RETRIES {
            let was_locked = matches!(
                backend.poll_invalidation(),
                Some(InvalidationReason::SessionLocked)
            );
            assert!(was_locked);
            backend.invalidation = None;
            let err = backend.handle_rebuild_failure(
                unknown_capture_error("WindowServer warming up"),
                was_locked,
                false,
                false,
                true,
            );
            assert!(
                !err.is_fatal(),
                "第 {expected_retry} 次解锁过渡期宽限重试应可恢复"
            );
            assert_eq!(backend.post_lock_grace_retries, expected_retry);
            assert_eq!(
                backend.poll_invalidation(),
                Some(InvalidationReason::SessionLocked)
            );
        }

        // 宽限耗尽后的下一次失败必须转为致命 Unknown，并保留 invalidation == Some(Unknown)。
        let was_locked = matches!(
            backend.poll_invalidation(),
            Some(InvalidationReason::SessionLocked)
        );
        backend.invalidation = None;
        let fatal = backend.handle_rebuild_failure(
            unknown_capture_error("persistent SCK failure"),
            was_locked,
            false,
            false,
            true,
        );
        assert!(fatal.is_fatal(), "宽限重试耗尽后必须升级为致命错误");
        assert_eq!(
            backend.poll_invalidation(),
            Some(InvalidationReason::Unknown),
            "致命失败后必须保留 invalidation == Some(Unknown) 不变量"
        );
        assert_eq!(backend.post_lock_grace_retries, 0);

        // 2. 宽限中途若再次锁屏/熄屏，应清零宽限计数并保持可恢复 SessionLocked。
        backend.enter_suspended_state("locked again");
        let _ = backend.handle_rebuild_failure(
            unknown_capture_error("transient"),
            true,
            false,
            false,
            true,
        );
        assert_eq!(backend.post_lock_grace_retries, 1);
        let asleep_err = backend.handle_rebuild_failure(
            unknown_capture_error("display went back to sleep"),
            true,
            true,
            false,
            true,
        );
        assert!(!asleep_err.is_fatal());
        assert_eq!(backend.post_lock_grace_retries, 0);
        assert_eq!(
            backend.poll_invalidation(),
            Some(InvalidationReason::SessionLocked)
        );

        // 3. TCC 屏幕录制权限丢失时，即使刚从 SessionLocked 恢复也立即致命，不消耗宽限预算。
        let mut no_perm = make_test_backend(SckConfig::default());
        no_perm.enter_suspended_state("locked");
        no_perm.invalidation = None;
        let denied = no_perm.handle_rebuild_failure(
            unknown_capture_error("permission denied"),
            true,
            false,
            false,
            false,
        );
        assert!(denied.is_fatal(), "缺少屏幕录制权限时必须立即返回致命错误");
        assert_eq!(
            no_perm.poll_invalidation(),
            Some(InvalidationReason::Unknown)
        );
        assert_eq!(no_perm.post_lock_grace_retries, 0);

        // 4. 非 SessionLocked 路径（如 AccessLost 或正常建流）下失败：立即致命且写入 Unknown。
        let mut non_locked = make_test_backend(SckConfig::default());
        let err = non_locked.handle_rebuild_failure(
            unknown_capture_error("fatal error"),
            false,
            false,
            false,
            true,
        );
        assert!(err.is_fatal());
        assert_eq!(
            non_locked.poll_invalidation(),
            Some(InvalidationReason::Unknown)
        );
    }

    #[test]
    fn fallback_display_summary_produces_valid_even_resolution() {
        // 1. 显式配置分辨率优先（奇数向下取偶）。
        let cfg = SckConfig {
            display: 2,
            resolution: Some(Resolution::new(1921, 1081)),
            ..SckConfig::default()
        };
        let summary = fallback_display_summary_with(
            &cfg,
            Some(Resolution::new(1512, 982)),
            Some(Resolution::new(3840, 2160)),
        );
        assert_eq!(summary.id, DisplayId(2));
        assert_eq!(summary.resolution, Resolution::new(1920, 1080));
        assert!((summary.scale - 2.0).abs() < 0.01);

        // 2. 未配置分辨率时回退到逻辑分辨率，并结合物理分辨率推算 scale。
        let summary = fallback_display_summary_with(
            &SckConfig::default(),
            Some(Resolution::new(1512, 982)),
            Some(Resolution::new(3024, 1964)),
        );
        assert_eq!(summary.resolution, Resolution::new(1512, 982));
        assert!((summary.scale - 2.0).abs() < 0.01);

        // 3. 退化配置（1x1）与空查询回退到 1920x1080 @ 1.0x，绝不返回 0 尺寸。
        let degenerate = SckConfig {
            resolution: Some(Resolution::new(1, 1)),
            ..SckConfig::default()
        };
        let summary = fallback_display_summary_with(&degenerate, None, None);
        assert_eq!(summary.resolution, Resolution::new(1920, 1080));
        assert!((summary.scale - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn suspended_sck_backend_exposes_valid_display_info_and_recoverable_invalidation() {
        let cfg = SckConfig {
            display: 0,
            resolution: Some(Resolution::new(1600, 900)),
            fps: 60,
            ..SckConfig::default()
        };
        let display = fallback_display_summary_with(&cfg, None, None);
        let mut backend = SckBackend {
            config: cfg,
            live: None,
            display,
            origin_pts_us: None,
            seq: CaptureSeq(0),
            last_buffer_info: None,
            invalidation: None,
            invalidation_msg: String::new(),
            post_lock_grace_retries: 0,
            first_frame_checked: false,
            #[cfg(feature = "bench-metrics")]
            exclude_current_process: false,
            #[cfg(feature = "bench-metrics")]
            last_frame_timing: None,
        };
        backend.enter_suspended_state("display asleep at cold start");

        // display_info() 返回有效非零偶分辨率，供 HostPipeline / VideoToolbox 初始化。
        let info = backend.display_info();
        assert_eq!(info.id, DisplayId(0));
        assert_eq!(info.resolution, Resolution::new(1600, 900));
        assert!(info.scale >= 1.0);

        // poll_invalidation() 持续上报 SessionLocked。
        assert_eq!(
            backend.poll_invalidation(),
            Some(InvalidationReason::SessionLocked)
        );
        assert_eq!(
            backend.poll_invalidation(),
            Some(InvalidationReason::SessionLocked)
        );

        // next_frame() 返回可恢复的 Error::Capture（非致命，避免触发退化或断开）。
        let err = backend
            .next_frame(Duration::from_millis(10))
            .expect_err("挂起态 next_frame 应返回可恢复错误以驱动上层重建");
        assert!(!err.is_fatal());
        assert!(matches!(
            err,
            Error::Capture {
                recoverable: true,
                ..
            }
        ));
    }
}

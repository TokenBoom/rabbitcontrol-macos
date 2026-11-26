//! RabbitControl macOS 平台能力总成。
//!
//! 统一将 macOS 原生硬件后端（ScreenCaptureKit、VideoToolbox、Metal、
//! CGEvent、AudioQueue、Carbon Pasteboard 与显示器控制）注册至
//! [`rabbitcontrol-core`](rc_pipeline) 对应的抽象提供者槽位。

#![deny(missing_docs)]
#![deny(unsafe_code)]

use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

use rc_capture::CaptureBackend;
use rc_client_core::platform::{
    ClientPlatformHooks, DisplayModeSessionGuard, HardwareRendererOutput, PlatformDisplayInfo,
    PlatformDisplayMode, PreallocatedRenderSurface,
};
use rc_decode::Decoder;
use rc_encode::{BitstreamFormat, Codec, Encoder};
use rc_input::InputInjector;
use rc_pipeline::{
    ChromaSubsampling, CodecCapability, LatencyTier, PipelineConfig, PlatformMediaProvider,
    RefinementCapability,
};
use rc_render::Renderer;
use rc_types::{Resolution, Result};

struct MacosMediaProvider;

impl PlatformMediaProvider for MacosMediaProvider {
    fn host_clock_us(&self) -> Option<u64> {
        #[cfg(target_os = "macos")]
        {
            Some(rc_capture_macos::host_time_us())
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    fn promote_current_thread_qos(&self) {
        #[cfg(target_os = "macos")]
        rc_capture_macos::promote_current_thread_qos();
    }

    fn create_capture(
        &self,
        cfg: &PipelineConfig,
    ) -> Result<(Box<dyn CaptureBackend>, &'static str)> {
        #[cfg(target_os = "macos")]
        {
            let sck = rc_capture_macos::new_backend(rc_capture_macos::SckConfig {
                display: 0,
                display_id: cfg.display_id,
                shows_cursor: cfg.shows_cursor,
                fps: cfg.fps,
                resolution: Some(cfg.resolution.to_even()),
                origin_us: cfg.clock_origin_us,
            })?;
            Ok((Box::new(sck), "sck"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cfg;
            Err(rc_types::Error::InvalidConfig(
                "SCK capture is only supported on macOS".into(),
            ))
        }
    }

    fn create_encoder(&self, cfg: &PipelineConfig) -> Result<(Box<dyn Encoder>, &'static str)> {
        #[cfg(target_os = "macos")]
        {
            match cfg.codec {
                Codec::H264 | Codec::Hevc => {
                    let enc =
                        rc_encode_vt::VtEncoder::new(rc_pipeline::default_encoder_config(cfg))?;
                    Ok((Box::new(enc), "videotoolbox"))
                }
                Codec::Av1 => Err(rc_types::Error::InvalidConfig(
                    "macOS VideoToolbox 不支持 AV1 硬件编码（仅支持 H.264 与 HEVC/H.265）".into(),
                )),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cfg;
            Err(rc_types::Error::InvalidConfig(
                "VideoToolbox encoder is only supported on macOS".into(),
            ))
        }
    }

    fn create_decoder(&self, cfg: &PipelineConfig) -> Result<(Box<dyn Decoder>, &'static str)> {
        #[cfg(target_os = "macos")]
        {
            let (bitstream_format, min_max_res) = match cfg.codec {
                Codec::H264 => (BitstreamFormat::AnnexB, Resolution::new(4096, 4096)),
                Codec::Hevc => (BitstreamFormat::AnnexB, Resolution::new(5120, 4096)),
                Codec::Av1 => {
                    if !rc_decode_vt::supports_hardware_av1_decode() {
                        return Err(rc_types::Error::Decoder(
                            "macOS VideoToolbox 当前设备不支持 AV1 硬件解码".into(),
                        ));
                    }
                    (BitstreamFormat::Obu, Resolution::new(5120, 4096))
                }
            };
            let dec = rc_decode_vt::new_decoder(rc_decode_vt::VtDecoderConfig {
                codec: cfg.codec,
                bitstream_format,
                max_resolution: Resolution::new(
                    cfg.resolution.width.max(min_max_res.width),
                    cfg.resolution.height.max(min_max_res.height),
                ),
            })?;
            Ok((Box::new(dec), "videotoolbox"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cfg;
            Err(rc_types::Error::InvalidConfig(
                "VideoToolbox decoder is only supported on macOS".into(),
            ))
        }
    }

    fn supports_hevc(&self) -> bool {
        cfg!(target_os = "macos")
    }

    fn encode_capabilities(&self) -> Vec<CodecCapability> {
        #[cfg(target_os = "macos")]
        {
            // Apple VideoToolbox 仅支持 HEVC 与 H.264 硬件编码，绝不虚报 AV1 硬编。
            vec![
                CodecCapability {
                    codec: Codec::Hevc,
                    hardware: true,
                    max_width: 5120,
                    max_height: 2880,
                    max_pixel_rate: 600_000_000,
                    screen_content_tools: false,
                    chroma: ChromaSubsampling::Yuv420,
                    latency_tier: LatencyTier::UltraLow,
                },
                CodecCapability {
                    codec: Codec::H264,
                    hardware: true,
                    max_width: 4096,
                    max_height: 2304,
                    max_pixel_rate: 280_000_000,
                    screen_content_tools: false,
                    chroma: ChromaSubsampling::Yuv420,
                    latency_tier: LatencyTier::UltraLow,
                },
            ]
        }
        #[cfg(not(target_os = "macos"))]
        {
            Vec::new()
        }
    }

    fn decode_capabilities(&self) -> Vec<CodecCapability> {
        #[cfg(target_os = "macos")]
        {
            let mut caps = Vec::with_capacity(3);
            if rc_decode_vt::supports_hardware_av1_decode() {
                caps.push(CodecCapability {
                    codec: Codec::Av1,
                    hardware: true,
                    max_width: 5120,
                    max_height: 2880,
                    max_pixel_rate: 500_000_000,
                    screen_content_tools: true,
                    chroma: ChromaSubsampling::Yuv420,
                    latency_tier: LatencyTier::Low,
                });
            }
            caps.push(CodecCapability {
                codec: Codec::Hevc,
                hardware: true,
                max_width: 5120,
                max_height: 2880,
                max_pixel_rate: 600_000_000,
                screen_content_tools: false,
                chroma: ChromaSubsampling::Yuv420,
                latency_tier: LatencyTier::UltraLow,
            });
            caps.push(CodecCapability {
                codec: Codec::H264,
                hardware: true,
                max_width: 4096,
                max_height: 2304,
                max_pixel_rate: 280_000_000,
                screen_content_tools: false,
                chroma: ChromaSubsampling::Yuv420,
                latency_tier: LatencyTier::UltraLow,
            });
            caps
        }
        #[cfg(not(target_os = "macos"))]
        {
            Vec::new()
        }
    }

    fn refinement_capability(&self) -> RefinementCapability {
        // 如实上报当前 macOS 侧静帧无损补帧能力状态：
        // 协议层与 QOI 解码格式已对齐，但当前仍维持 SCK(NV12/420v)→VideoToolbox 主视频零拷贝热路径；
        // 待真机按 `BENCHMARK_MACOS.md` 实测 `SCK(BGRA)→VT` 与 Metal 叠加色差达标后再翻开
        // `supported: true`，绝不虚报未就绪能力。
        RefinementCapability {
            supported: false,
            true_rgb_capture: false,
            formats: vec!["qoi".into()],
        }
    }

    fn create_renderer(&self, cfg: &PipelineConfig) -> Result<(Box<dyn Renderer>, &'static str)> {
        #[cfg(target_os = "macos")]
        {
            let mcfg = rc_render_macos::MetalRendererConfig {
                size: (cfg.resolution.width, cfg.resolution.height),
                vsync: false,
                session_start: None,
                cursor_compositing: true,
            };
            let renderer = rc_render_macos::MetalRenderer::new(mcfg)?;
            Ok((Box::new(renderer), "metal"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cfg;
            Err(rc_types::Error::InvalidConfig(
                "Metal renderer is only supported on macOS".into(),
            ))
        }
    }
}

#[cfg(target_os = "macos")]
struct MacosDisplayModeGuardWrapper(rc_capture_macos::DisplayModeGuard);

#[cfg(target_os = "macos")]
impl DisplayModeSessionGuard for MacosDisplayModeGuardWrapper {
    fn original_resolution(&self) -> Resolution {
        self.0.original_resolution()
    }

    fn switch_to(
        &mut self,
        target_w: u32,
        target_h: u32,
    ) -> std::result::Result<Resolution, String> {
        self.0.switch_to(target_w, target_h)
    }

    fn restore(&mut self) {
        self.0.restore();
    }
}

struct MacosClientPlatformHooks;

impl ClientPlatformHooks for MacosClientPlatformHooks {
    fn promote_current_thread_qos(&self) {
        #[cfg(target_os = "macos")]
        rc_capture_macos::promote_current_thread_qos();
    }

    fn begin_latency_critical_activity(
        &self,
        reason: &str,
        interactive: bool,
    ) -> Option<Box<dyn Any + Send>> {
        #[cfg(target_os = "macos")]
        {
            Some(Box::new(rc_capture_macos::LatencyCriticalActivity::begin(
                reason,
                interactive,
            )))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (reason, interactive);
            None
        }
    }

    fn main_display_logical_resolution(&self) -> Option<Resolution> {
        #[cfg(target_os = "macos")]
        {
            rc_capture_macos::main_display_logical_resolution()
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    fn main_display_backing_resolution(&self) -> Option<Resolution> {
        #[cfg(target_os = "macos")]
        {
            rc_capture_macos::main_display_backing_resolution()
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    fn list_main_display_modes(&self) -> Vec<PlatformDisplayMode> {
        #[cfg(target_os = "macos")]
        {
            rc_capture_macos::list_main_display_modes()
                .into_iter()
                .map(|m| PlatformDisplayMode {
                    width: m.width,
                    height: m.height,
                    is_hidpi: m.is_hidpi,
                })
                .collect()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Vec::new()
        }
    }

    fn capture_display_mode_guard(&self) -> Option<Box<dyn DisplayModeSessionGuard>> {
        #[cfg(target_os = "macos")]
        {
            rc_capture_macos::DisplayModeGuard::capture_current().map(|g| {
                Box::new(MacosDisplayModeGuardWrapper(g)) as Box<dyn DisplayModeSessionGuard>
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    fn capture_wallpaper_jpeg(&self, max_width: u32, max_height: u32) -> Option<Vec<u8>> {
        #[cfg(target_os = "macos")]
        {
            rc_capture_macos::main_display_wallpaper_jpeg(max_width, max_height)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (max_width, max_height);
            None
        }
    }

    fn lock_screen_now(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            rc_capture_macos::lock_screen_now()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    fn wake_display_if_asleep(&self) -> std::result::Result<Option<Box<dyn Any + Send>>, String> {
        #[cfg(target_os = "macos")]
        {
            rc_capture_macos::wake_display_if_asleep(
                "RabbitControl 被控会话唤醒显示器",
                std::time::Duration::from_millis(2500),
            )
            .map(|opt| opt.map(|guard| Box::new(guard) as Box<dyn Any + Send>))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(None)
        }
    }

    fn is_display_asleep(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            rc_capture_macos::is_display_asleep(None)
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    fn is_screen_locked(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            rc_capture_macos::is_screen_locked()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    fn set_privacy_curtain(
        &self,
        mode: &str,
        preset: &str,
        custom_image_bytes: Option<&[u8]>,
    ) -> std::result::Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            rc_capture_macos::set_privacy_curtain(mode, preset, custom_image_bytes)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (mode, preset, custom_image_bytes);
            Err("privacy curtain is only supported on macOS".to_string())
        }
    }

    fn clear_privacy_curtain(&self) {
        #[cfg(target_os = "macos")]
        rc_capture_macos::clear_privacy_curtain();
    }

    fn set_host_system_muted(&self, muted: bool) -> std::result::Result<bool, String> {
        #[cfg(target_os = "macos")]
        {
            rc_audio_macos::set_host_system_muted(muted)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = muted;
            Err("host system audio mute is only supported on macOS".to_string())
        }
    }

    fn restore_host_system_audio(&self) {
        #[cfg(target_os = "macos")]
        rc_audio_macos::restore_host_system_audio();
    }

    fn clear_host_audio_snapshot(&self) {
        #[cfg(target_os = "macos")]
        rc_audio_macos::clear_host_audio_snapshot();
    }

    fn list_displays(&self) -> Vec<PlatformDisplayInfo> {
        #[cfg(target_os = "macos")]
        {
            rc_capture_macos::list_online_displays()
                .into_iter()
                .map(|d| PlatformDisplayInfo {
                    id: d.id,
                    name: d.name,
                    width: d.width,
                    height: d.height,
                    origin_x: d.origin_x,
                    origin_y: d.origin_y,
                    is_main: d.is_main,
                    is_virtual: d.is_virtual,
                })
                .collect()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Vec::new()
        }
    }

    fn create_virtual_display(
        &self,
        width: u32,
        height: u32,
        hidpi: bool,
    ) -> std::result::Result<PlatformDisplayInfo, String> {
        #[cfg(target_os = "macos")]
        {
            rc_capture_macos::create_virtual_display(width, height, hidpi).map(|d| {
                PlatformDisplayInfo {
                    id: d.id,
                    name: d.name,
                    width: d.width,
                    height: d.height,
                    origin_x: d.origin_x,
                    origin_y: d.origin_y,
                    is_main: d.is_main,
                    is_virtual: d.is_virtual,
                }
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (width, height, hidpi);
            Err("virtual display is only supported on macOS".to_string())
        }
    }

    fn destroy_virtual_display(&self, display_id: u32) -> bool {
        #[cfg(target_os = "macos")]
        {
            rc_capture_macos::destroy_virtual_display(display_id)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = display_id;
            false
        }
    }

    fn destroy_all_virtual_displays(&self) {
        #[cfg(target_os = "macos")]
        rc_capture_macos::destroy_all_virtual_displays();
    }

    fn create_main_display_injector(&self) -> Option<Box<dyn InputInjector>> {
        #[cfg(target_os = "macos")]
        {
            rc_input_macos::InputBridge::for_main_display()
                .ok()
                .map(|b| Box::new(b) as Box<dyn InputInjector>)
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    fn create_hardware_renderer(
        &self,
        width: u32,
        height: u32,
        session_start: Instant,
        preallocated: Option<&PreallocatedRenderSurface>,
    ) -> std::result::Result<HardwareRendererOutput, String> {
        #[cfg(target_os = "macos")]
        {
            let metal_cfg = rc_render_macos::MetalRendererConfig {
                size: (width, height),
                vsync: false,
                session_start: Some(session_start),
                cursor_compositing: true,
            };
            let metal = match preallocated {
                Some(surface) => {
                    let raw = surface.raw_ptr() as *mut std::ffi::c_void;
                    rc_render_macos::MetalRenderer::from_raw_layer(raw, metal_cfg)
                        .map_err(|e| e.to_string())?
                }
                None => {
                    rc_render_macos::MetalRenderer::new(metal_cfg).map_err(|e| e.to_string())?
                }
            };
            let layer_addr = metal.layer_ptr() as usize;
            Ok(HardwareRendererOutput {
                renderer: Box::new(metal),
                layer_addr,
                label: "metal",
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (width, height, session_start, preallocated);
            Err("Metal renderer is only supported on macOS".to_string())
        }
    }
}

/// 幂等注册全部 macOS 硬件提供者与原生钩子（应在主程序/Agent/CLI 入口最早阶段调用）。
pub fn register_all() {
    let _ = rc_pipeline::register_platform_provider(Arc::new(MacosMediaProvider));
    let _ = rc_audio_macos::register();
    let _ = rc_clipboard_macos::register();
    let _ = rc_client_core::platform::register_platform_hooks(Arc::new(MacosClientPlatformHooks));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asymmetric_capabilities_match_platform_hardware_support() {
        let provider = MacosMediaProvider;
        let enc_caps = provider.encode_capabilities();
        let dec_caps = provider.decode_capabilities();

        // 任何平台均绝不虚报 AV1 硬件编码能力。
        assert!(
            !enc_caps.iter().any(|c| c.codec == Codec::Av1),
            "macOS encode_capabilities 不得包含 AV1，实际 {enc_caps:?}"
        );

        #[cfg(target_os = "macos")]
        {
            let enc_hevc = enc_caps
                .iter()
                .find(|c| c.codec == Codec::Hevc)
                .expect("macOS 编码能力必须包含 HEVC");
            assert!(enc_hevc.hardware);
            assert_eq!(enc_hevc.max_pixel_rate, 600_000_000);
            assert_eq!(enc_hevc.max_width, 5120);
            assert_eq!(enc_hevc.max_height, 2880);

            let enc_h264 = enc_caps
                .iter()
                .find(|c| c.codec == Codec::H264)
                .expect("macOS 编码能力必须包含 H.264");
            assert!(enc_h264.hardware);
            assert_eq!(enc_h264.max_pixel_rate, 280_000_000);

            assert!(
                dec_caps
                    .iter()
                    .any(|c| c.codec == Codec::Hevc && c.hardware),
                "macOS 解码能力必须包含 HEVC 硬解"
            );
            assert!(
                dec_caps
                    .iter()
                    .any(|c| c.codec == Codec::H264 && c.hardware),
                "macOS 解码能力必须包含 H.264 硬解"
            );

            let hw_av1 = rc_decode_vt::supports_hardware_av1_decode();
            let dec_av1 = dec_caps.iter().find(|c| c.codec == Codec::Av1);
            if hw_av1 {
                let cap = dec_av1.expect("本机支持 AV1 硬解时 decode_capabilities 必须上报 AV1");
                assert!(cap.hardware);
                assert!(cap.screen_content_tools);
                assert_eq!(cap.max_pixel_rate, 500_000_000);
            } else {
                assert!(
                    dec_av1.is_none(),
                    "本机不支持 AV1 硬解（如 M1/M2）时不得虚报 AV1 解码能力"
                );
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            assert!(enc_caps.is_empty());
            assert!(dec_caps.is_empty());
        }
    }

    #[test]
    fn encoder_rejects_av1_and_decoder_follows_hardware_av1_probe() {
        let provider = MacosMediaProvider;

        let enc_av1 = provider.create_encoder(&PipelineConfig {
            codec: Codec::Av1,
            ..PipelineConfig::default()
        });
        assert!(
            matches!(enc_av1, Err(rc_types::Error::InvalidConfig(_))),
            "create_encoder(Av1) 必须明确返回 InvalidConfig 错误"
        );

        #[cfg(target_os = "macos")]
        {
            let h264_dec = provider.create_decoder(&PipelineConfig {
                codec: Codec::H264,
                ..PipelineConfig::default()
            });
            assert!(h264_dec.is_ok());

            let hevc_dec = provider.create_decoder(&PipelineConfig {
                codec: Codec::Hevc,
                ..PipelineConfig::default()
            });
            assert!(hevc_dec.is_ok());

            let av1_dec = provider.create_decoder(&PipelineConfig {
                codec: Codec::Av1,
                ..PipelineConfig::default()
            });
            if rc_decode_vt::supports_hardware_av1_decode() {
                assert!(
                    matches!(av1_dec, Ok((_, "videotoolbox"))),
                    "支持 AV1 硬解的设备应成功创建 VideoToolbox AV1 解码器"
                );
            } else {
                assert!(
                    matches!(av1_dec, Err(rc_types::Error::Decoder(_))),
                    "不支持 AV1 硬解的设备应返回 Error::Decoder 供上游熔断降级"
                );
            }
        }
    }

    #[test]
    fn refinement_capability_reports_honest_unready_status() {
        let provider = MacosMediaProvider;
        let cap = provider.refinement_capability();
        assert!(
            !cap.supported,
            "真机实测 SCK(BGRA)→VT 与 Metal 叠加色差达标前绝不得虚报 supported = true"
        );
        assert!(
            !cap.true_rgb_capture,
            "当前仍走 NV12/420v 零拷贝主路径，不得虚报 true_rgb_capture = true"
        );
        assert_eq!(cap.formats, vec!["qoi".to_string()]);
    }
}

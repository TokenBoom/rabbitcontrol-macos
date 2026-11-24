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
    ClientPlatformHooks, DisplayModeSessionGuard, HardwareRendererOutput, PlatformDisplayMode,
    PreallocatedRenderSurface,
};
use rc_decode::Decoder;
use rc_encode::Encoder;
use rc_input::InputInjector;
use rc_pipeline::{PipelineConfig, PlatformMediaProvider};
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
            let enc = rc_encode_vt::VtEncoder::new(rc_pipeline::default_encoder_config(cfg))?;
            Ok((Box::new(enc), "videotoolbox"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cfg;
            Err(rc_types::Error::InvalidConfig(
                "VideoToolbox encoder is only supported on macOS".into(),
            ))
        }
    }

    fn create_decoder(&self, _cfg: &PipelineConfig) -> Result<(Box<dyn Decoder>, &'static str)> {
        #[cfg(target_os = "macos")]
        {
            let dec = rc_decode_vt::new_decoder(rc_decode_vt::VtDecoderConfig::default())?;
            Ok((Box::new(dec), "videotoolbox"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(rc_types::Error::InvalidConfig(
                "VideoToolbox decoder is only supported on macOS".into(),
            ))
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

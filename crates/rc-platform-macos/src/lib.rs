//! RabbitControl macOS 平台能力总成。
//!
//! 统一将 macOS 原生硬件后端（ScreenCaptureKit、VideoToolbox、Metal、
//! CGEvent、AudioQueue、Carbon Pasteboard 与显示器控制）注册至
//! [`rabbitcontrol-core`](rc_pipeline) 对应的抽象提供者槽位。

#![deny(missing_docs)]
#![deny(unsafe_code)]

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use rc_capture::CaptureBackend;
use rc_client_core::platform::{
    ClientPlatformHooks, DisplayModeSessionGuard, HardwareRendererOutput, PlatformDisplayInfo,
    PlatformDisplayMode, PreallocatedRenderSurface,
};
use rc_client_core::proxy::{ProxyConfig, ProxyMode, ProxyProtocol};
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

    fn current_system_username(&self) -> Option<String> {
        #[cfg(target_os = "macos")]
        {
            rc_capture_macos::current_system_username()
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    fn verify_system_password(
        &self,
        username: Option<&str>,
        password: &str,
    ) -> std::result::Result<bool, String> {
        #[cfg(target_os = "macos")]
        {
            rc_capture_macos::verify_system_password(username, password)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (username, password);
            Err("system password verification is only supported on macOS".to_string())
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

    fn resolve_system_proxy(&self) -> Option<ProxyConfig> {
        #[cfg(target_os = "macos")]
        {
            let out = std::process::Command::new("/usr/sbin/scutil")
                .arg("--proxy")
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            parse_scutil_proxy_output(&stdout)
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    fn primary_mac_address(&self) -> Option<String> {
        #[cfg(target_os = "macos")]
        {
            let default_if = std::process::Command::new("/sbin/route")
                .args(["-n", "get", "default"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| parse_default_route_interface(&String::from_utf8_lossy(&o.stdout)));

            if let Some(ref iface) = default_if {
                if let Ok(out) = std::process::Command::new("/sbin/ifconfig")
                    .arg(iface)
                    .output()
                {
                    if out.status.success() {
                        if let Some(mac) = parse_ifconfig_mac(&String::from_utf8_lossy(&out.stdout))
                        {
                            return Some(mac);
                        }
                    }
                }
            }

            let all_out = std::process::Command::new("/sbin/ifconfig")
                .arg("-a")
                .output()
                .ok()?;
            if !all_out.status.success() {
                return None;
            }
            parse_ifconfig_all_primary_mac(
                &String::from_utf8_lossy(&all_out.stdout),
                default_if.as_deref(),
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }
}

/// 解析 macOS `scutil --proxy` 标准输出中的系统代理设置。
///
/// 仅提取顶层字典中已启用的代理项，按 `SOCKS5` → `HTTPS (HTTP CONNECT)` → `HTTP` 优先级返回首个
/// 经 [`ProxyConfig::validate`] 校验合法的代理端点；未启用或解析失败时返回 `None`，交由核心层环境变量兜底。
pub fn parse_scutil_proxy_output(output: &str) -> Option<ProxyConfig> {
    let mut top_level: HashMap<&str, &str> = HashMap::new();
    let mut brace_depth: usize = 0;

    for raw_line in output.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "}" {
            brace_depth = brace_depth.saturating_sub(1);
            continue;
        }
        if line.ends_with('{') {
            brace_depth = brace_depth.saturating_add(1);
            continue;
        }
        if brace_depth == 1 {
            if let Some((k, v)) = line.split_once(':') {
                let key = k.trim();
                let val = v.trim();
                if !key.is_empty() {
                    top_level.insert(key, val);
                }
            }
        }
    }

    let candidates: [(&str, &str, &str, ProxyProtocol); 3] = [
        (
            "SOCKSEnable",
            "SOCKSProxy",
            "SOCKSPort",
            ProxyProtocol::Socks5,
        ),
        (
            "HTTPSEnable",
            "HTTPSProxy",
            "HTTPSPort",
            ProxyProtocol::Http,
        ),
        ("HTTPEnable", "HTTPProxy", "HTTPPort", ProxyProtocol::Http),
    ];

    for (enable_key, host_key, port_key, protocol) in candidates {
        let enabled = top_level
            .get(enable_key)
            .is_some_and(|v| v.trim() == "1" || v.eq_ignore_ascii_case("true"));
        if !enabled {
            continue;
        }
        let Some(raw_host) = top_level.get(host_key).map(|v| v.trim()) else {
            continue;
        };
        let Some(raw_port) = top_level.get(port_key).map(|v| v.trim()) else {
            continue;
        };
        let Ok(port) = raw_port.parse::<u16>() else {
            continue;
        };
        if raw_host.is_empty() || port == 0 {
            continue;
        }
        let cfg = ProxyConfig {
            mode: ProxyMode::Manual,
            protocol: Some(protocol),
            host: raw_host.to_string(),
            port,
            username: None,
            password: None,
        };
        if cfg.validate().is_ok() {
            return Some(cfg);
        }
    }

    None
}

/// 校验 macOS 网络接口名是否安全（防命令行标志注入与非法字符）。
fn is_valid_interface_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// 解析 macOS `route -n get default` 输出中的默认路由网络接口名（如 `"en0"`）。
pub fn parse_default_route_interface(output: &str) -> Option<String> {
    for raw_line in output.lines() {
        let line = raw_line.trim();
        if let Some(rest) = line.strip_prefix("interface:") {
            let iface = rest.trim();
            if is_valid_interface_name(iface) {
                return Some(iface.to_string());
            }
        }
    }
    None
}

/// 解析单个接口 `ifconfig <iface>` 输出中的合法单播 MAC 地址并经核心层归一化为小写 `aa:bb:cc:dd:ee:ff`。
pub fn parse_ifconfig_mac(output: &str) -> Option<String> {
    for raw_line in output.lines() {
        let line = raw_line.trim();
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else {
            continue;
        };
        if key == "ether" || key == "lladdr" {
            if let Some(raw_mac) = parts.next() {
                if let Some(norm) = rc_client_core::wol::normalize_mac_addr(raw_mac) {
                    return Some(norm);
                }
            }
        }
    }
    None
}

/// 解析物理以太网/Wi-Fi 接口名 `en<N>` 的数字序号，用于回退选择主网卡。
fn parse_en_interface_index(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("en")?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse::<u32>().ok()
}

/// 从 `ifconfig -a` 全量输出中提取主网卡合法单播 MAC 地址：
/// 1. 优先采用 `preferred_if`（默认路由接口，如 `en0`）上的合法单播 MAC；
/// 2. 若默认路由为无以太网 MAC 的隧道接口（如 VPN `utun*`），回退至 `status: active` 的最小序号 `en<N>`；
/// 3. 仍无匹配时回退至任意带合法单播 MAC 的最小序号 `en<N>`（排除 `lo*`/`awdl*`/`llw*`/`utun*` 等虚拟接口）。
pub fn parse_ifconfig_all_primary_mac(output: &str, preferred_if: Option<&str>) -> Option<String> {
    struct IfaceEntry {
        name: String,
        mac: String,
        active: bool,
    }

    let mut entries: Vec<IfaceEntry> = Vec::new();
    let mut cur_name: Option<String> = None;
    let mut cur_mac: Option<String> = None;
    let mut cur_active = false;

    let flush = |entries: &mut Vec<IfaceEntry>,
                 name: &mut Option<String>,
                 mac: &mut Option<String>,
                 active: &mut bool| {
        if let (Some(n), Some(m)) = (name.take(), mac.take()) {
            entries.push(IfaceEntry {
                name: n,
                mac: m,
                active: *active,
            });
        } else {
            *name = None;
            *mac = None;
        }
        *active = false;
    };

    for raw_line in output.lines() {
        let is_header = !raw_line.starts_with(char::is_whitespace) && raw_line.contains(':');
        if is_header {
            flush(&mut entries, &mut cur_name, &mut cur_mac, &mut cur_active);
            if let Some((ifname, _)) = raw_line.split_once(':') {
                let trimmed = ifname.trim();
                if is_valid_interface_name(trimmed) {
                    cur_name = Some(trimmed.to_string());
                }
            }
            continue;
        }
        if cur_name.is_none() {
            continue;
        }
        let trimmed = raw_line.trim();
        if cur_mac.is_none() {
            if let Some(mac) = parse_ifconfig_mac(trimmed) {
                cur_mac = Some(mac);
            }
        }
        if let Some(status_val) = trimmed.strip_prefix("status:") {
            if status_val.trim().eq_ignore_ascii_case("active") {
                cur_active = true;
            }
        }
    }
    flush(&mut entries, &mut cur_name, &mut cur_mac, &mut cur_active);

    if let Some(pref) = preferred_if {
        if let Some(hit) = entries.iter().find(|e| e.name == pref) {
            return Some(hit.mac.clone());
        }
    }

    let mut en_candidates: Vec<(u32, bool, &str)> = entries
        .iter()
        .filter_map(|e| {
            let idx = parse_en_interface_index(&e.name)?;
            Some((idx, e.active, e.mac.as_str()))
        })
        .collect();
    en_candidates.sort_by_key(|&(idx, active, _)| (!active, idx));
    en_candidates.first().map(|&(_, _, mac)| mac.to_string())
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

    #[test]
    fn parse_scutil_proxy_output_handles_socks_http_https_and_disabled() {
        let empty = r#"<dictionary> {
  ExceptionsList : <array> {
    0 : *.local
    1 : 169.254/16
  }
  FTPPassive : 1
}"#;
        assert_eq!(parse_scutil_proxy_output(empty), None);

        // SOCKS5 与 HTTP 同时开启时优先 SOCKS5。
        let both = r#"<dictionary> {
  ExceptionsList : <array> {
    0 : *.local
  }
  HTTPEnable : 1
  HTTPPort : 7890
  HTTPProxy : 127.0.0.1
  SOCKSEnable : 1
  SOCKSPort : 7891
  SOCKSProxy : 127.0.0.1
}"#;
        assert_eq!(
            parse_scutil_proxy_output(both),
            Some(ProxyConfig {
                mode: ProxyMode::Manual,
                protocol: Some(ProxyProtocol::Socks5),
                host: "127.0.0.1".to_string(),
                port: 7891,
                username: None,
                password: None,
            })
        );

        // SOCKSEnable = 0 时应忽略 SOCKS 并回退至启用的 HTTPS / HTTP 代理。
        let https_only = r#"<dictionary> {
  SOCKSEnable : 0
  SOCKSPort : 1080
  SOCKSProxy : 127.0.0.1
  HTTPSEnable : 1
  HTTPSPort : 8443
  HTTPSProxy : proxy.corp.internal
}"#;
        assert_eq!(
            parse_scutil_proxy_output(https_only),
            Some(ProxyConfig {
                mode: ProxyMode::Manual,
                protocol: Some(ProxyProtocol::Http),
                host: "proxy.corp.internal".to_string(),
                port: 8443,
                username: None,
                password: None,
            })
        );

        // IPv6 代理主机与非法端口 0 跳过逻辑。
        let ipv6_fallback = r#"<dictionary> {
  SOCKSEnable : 1
  SOCKSPort : 0
  SOCKSProxy : 127.0.0.1
  HTTPEnable : 1
  HTTPPort : 8080
  HTTPProxy : ::1
}"#;
        assert_eq!(
            parse_scutil_proxy_output(ipv6_fallback),
            Some(ProxyConfig {
                mode: ProxyMode::Manual,
                protocol: Some(ProxyProtocol::Http),
                host: "::1".to_string(),
                port: 8080,
                username: None,
                password: None,
            })
        );

        // 非法主机名（含空白或斜杠）应拒绝并返回 None。
        let invalid_host = r#"<dictionary> {
  HTTPEnable : 1
  HTTPPort : 8080
  HTTPProxy : http://bad/path
}"#;
        assert_eq!(parse_scutil_proxy_output(invalid_host), None);

        // 值内含未配对花括号（如 PAC URL 或 ExceptionsList 条目）不应破坏顶层块深度解析。
        let unpaired_brace_val = r#"<dictionary> {
  ExceptionsList : <array> {
    0 : foo{bar
  }
  ProxyAutoConfigURLString : http://pac.local/{unclosed
  HTTPEnable : 1
  HTTPPort : 8888
  HTTPProxy : 10.0.0.2
}"#;
        assert_eq!(
            parse_scutil_proxy_output(unpaired_brace_val),
            Some(ProxyConfig {
                mode: ProxyMode::Manual,
                protocol: Some(ProxyProtocol::Http),
                host: "10.0.0.2".to_string(),
                port: 8888,
                username: None,
                password: None,
            })
        );
    }

    #[test]
    fn parse_default_route_and_ifconfig_mac_normalizes_and_validates() {
        let route_out = r#"   route to: default
destination: default
       mask: default
    gateway: 192.168.50.254
  interface: en0
      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>"#;
        assert_eq!(
            parse_default_route_interface(route_out).as_deref(),
            Some("en0")
        );
        assert_eq!(parse_default_route_interface("interface: -rf\n"), None);
        assert_eq!(parse_default_route_interface("no route"), None);

        let ifcfg_en0 = r#"en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
	ether AA:BB:CC:DD:EE:FF
	inet 192.168.50.23 netmask 0xffffff00 broadcast 192.168.50.255
	status: active"#;
        assert_eq!(
            parse_ifconfig_mac(ifcfg_en0).as_deref(),
            Some("aa:bb:cc:dd:ee:ff")
        );

        // 多播 / 全零 / 全 F 广播 MAC 必须拒绝。
        assert_eq!(parse_ifconfig_mac("\tether 01:00:5e:00:00:01\n"), None);
        assert_eq!(parse_ifconfig_mac("\tether ff:ff:ff:ff:ff:ff\n"), None);
        assert_eq!(parse_ifconfig_mac("\tether 00:00:00:00:00:00\n"), None);

        // 默认路由走 VPN utun3（无 ether）时，应回退至 status: active 的 en* 网卡。
        let ifcfg_all = r#"lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384
	inet 127.0.0.1 netmask 0xff000000
en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
	ether 02:11:22:33:44:55
	status: inactive
en1: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
	ether 02:AA:BB:CC:DD:EE
	status: active
utun3: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1380
	inet 10.8.0.2 --> 10.8.0.1 netmask 0xffffffff
awdl0: flags=8843<UP,BROADCAST,RUNNING,SIMPLEX,MULTICAST> mtu 1500
	ether 02:99:99:99:99:99
	status: active"#;
        assert_eq!(
            parse_ifconfig_all_primary_mac(ifcfg_all, Some("utun3")).as_deref(),
            Some("02:aa:bb:cc:dd:ee")
        );
        // 默认路由指向 en0 时优先返回 en0。
        assert_eq!(
            parse_ifconfig_all_primary_mac(ifcfg_all, Some("en0")).as_deref(),
            Some("02:11:22:33:44:55")
        );
    }
}

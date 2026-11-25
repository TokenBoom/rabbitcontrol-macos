//! 音频、剪贴板、冷启动首帧 (TTFF) 与全功能并发负载基准模块（仅 `bench-metrics` 启用，零 `unsafe`）。
//!
//! 覆盖：
//! - `S14-audio-pipeline`：真实 SCK 音频采集 + IMA-ADPCM 编码 + QUIC Datagram 传输 + `AudioQueue` 静音播放闭环
//! - `S15-clipboard-sync`：真实 `PasteboardSynchronize` 变更嗅探 + QUIC `Reliable` 传输 + UTF-8/128KiB 字节保真度（带 RAII 剪贴板还原）
//! - `S16-mixed-workflow-ttff`：5 轮冷启动首帧 (TTFF) + 真实 QUIC 媒体 Datagram 与键盘/鼠标/滚轮并发高压负载

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rc_audio::{
    create_audio_player, create_system_audio_capture, decode_ima_adpcm, encode_ima_adpcm,
    AudioCapture as _, FakeAudioCapture, PcmFrame, DEFAULT_CHANNELS, DEFAULT_SAMPLE_RATE,
};
use rc_capture::{CaptureBackend, FrameTexture};
use rc_capture_macos::{
    main_display_logical_resolution, read_pixel_buffer_y_region, SckBackend, SckConfig,
};
use rc_clipboard_macos::NspasteboardBackend;
use rc_decode::{DecodeOutput, Decoder};
use rc_decode_vt::{VtDecoder, VtDecoderConfig};
use rc_encode::{Codec, Encoder, EncoderConfig, Preset};
use rc_encode_vt::VtEncoder;
use rc_net::quic::{Identity, QuicEndpoint, QuicTransport};
use rc_net::{NetConfig, NetEvent, Role, Transport};
use rc_render::Renderer;
use rc_render_macos::barcode::{
    decode_barcode_y, BARCODE_HEIGHT, BARCODE_OFFSET_X, BARCODE_OFFSET_Y, BARCODE_WIDTH,
};
use rc_render_macos::{media_time_us, MetalRenderer, PresentTimingSample};
use rc_session::dev_cert::{dev_identity, DEV_CERT_PEM, DEV_SERVER_NAME};
use rc_session::ClipboardBackend;
use rc_types::{Resolution, Timestamp};

use super::keyboard::{scancode_and_vk, KeyReleaseGuard, KeyboardBenchState};
use super::mouse::{map_stage_point, stage_point_from_host, MouseBenchState, QuicInputLoop};
use super::runner::{collect_env_snapshot, BenchSuite};
use super::stats::{DistStats, ScenarioReport, SystemMetrics};

/// 建立一对基于 localhost TLS 1.3 的真实 `QuicTransport` 双向通道。
fn open_quic_pair() -> Result<(QuicEndpoint, QuicEndpoint, QuicTransport, QuicTransport), String> {
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

    let (tx, rx) = std::sync::mpsc::channel::<(QuicEndpoint, Result<QuicTransport, String>)>();
    std::thread::spawn(move || {
        let res = cli_ep
            .connect(peer_addr, &certs, DEV_SERVER_NAME, Duration::from_secs(5))
            .map_err(|e| format!("QUIC connect 失败：{e}"));
        let _ = tx.send((cli_ep, res));
    });

    let server = host_ep
        .accept(Duration::from_secs(5))
        .map_err(|e| format!("QUIC accept 失败：{e}"))?;
    let (cli_ep, client_res) = rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "等待 QUIC connect 超时".to_string())?;
    let client = client_res?;
    Ok((host_ep, cli_ep, server, client))
}

/// RAII 剪贴板还原守卫：进入 S15 前快照原剪贴板内容，退出或报错时无条件恢复，不污染用户粘贴板。
struct ClipboardRestoreGuard {
    backend: NspasteboardBackend,
    original_text: Option<String>,
}

impl Drop for ClipboardRestoreGuard {
    fn drop(&mut self) {
        let restore = self.original_text.as_deref().unwrap_or("");
        let _ = self.backend.write_text(restore);
    }
}

const AUDIO_MAGIC: [u8; 4] = *b"AUD0";
const AUDIO_HDR_LEN: usize = 16;

fn build_audio_bench_packet(seq: u32, send_us: u64, adpcm: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(AUDIO_HDR_LEN + adpcm.len());
    pkt.extend_from_slice(&AUDIO_MAGIC);
    pkt.extend_from_slice(&seq.to_le_bytes());
    pkt.extend_from_slice(&send_us.to_le_bytes());
    pkt.extend_from_slice(adpcm);
    pkt
}

fn parse_audio_bench_packet(bytes: &[u8], expected_seq: u32) -> Option<(u64, &[u8])> {
    if bytes.len() <= AUDIO_HDR_LEN || bytes[..4] != AUDIO_MAGIC {
        return None;
    }
    let mut seq_arr = [0u8; 4];
    seq_arr.copy_from_slice(&bytes[4..8]);
    if u32::from_le_bytes(seq_arr) != expected_seq {
        return None;
    }
    let mut ts_arr = [0u8; 8];
    ts_arr.copy_from_slice(&bytes[8..16]);
    Some((u64::from_le_bytes(ts_arr), &bytes[AUDIO_HDR_LEN..]))
}

/// 执行无须全屏光学窗的独立系统基准：`S14-audio-pipeline` 与 `S15-clipboard-sync`。
pub(super) fn run_standalone_system_scenario(
    suite: BenchSuite,
    duration_override: Option<Duration>,
) -> Result<ScenarioReport, String> {
    rc_platform_macos::register_all();

    match suite {
        BenchSuite::S14AudioPipeline => {
            let dur = duration_override.unwrap_or(Duration::from_secs(4));
            let t0 = Instant::now();
            let (_h_ep, _c_ep, mut host_tx, mut cli_rx) = open_quic_pair()?;
            // 预热 QUIC Datagram 通路并排空首包（哑包无 AUD0 魔数会被直接丢弃）。
            for _ in 0..3 {
                let _ = host_tx.send_datagram(&[0u8; 16]);
                let _ = cli_rx.poll(Duration::from_millis(5));
            }
            while let Ok(evs) = cli_rx.poll(Duration::ZERO) {
                if evs.is_empty() {
                    break;
                }
            }
            let mut cap = create_system_audio_capture()
                .map_err(|e| format!("初始化 SCK 系统音频采集失败：{e}"))?;
            let mut player = create_audio_player(DEFAULT_SAMPLE_RATE, DEFAULT_CHANNELS)
                .map_err(|e| format!("初始化 AudioQueue 播放器失败：{e}"))?;
            // 强制静音：零物理扬声器噪声，但完整经历 AudioQueueAllocateBuffer/Enqueue 路径。
            player.set_muted(true);

            let mut fake_fallback = FakeAudioCapture::new();
            let mut last_cap_ts: Option<u32> = None;
            let mut cap_intervals_us = Vec::new();
            let mut codec_quic_us = Vec::new();
            let mut player_push_us = Vec::new();
            let mut frames_captured = 0u64;
            let mut frames_played = 0u64;
            let mut push_errors = 0u64;
            let mut audio_seq = 0u32;

            let mut next_tick = Instant::now();
            let tick = Duration::from_millis(20);
            let mut live_active = false;

            while t0.elapsed() < dur {
                // 1) 活跃 SCK 音频流期间以 <=1ms 细粒度等待新帧（上限 25ms），并以帧内置 `timestamp_us` 差值
                //    统计采集步进（消除 CoreAudio 1024-sample / 21.33ms 回调与 20ms 睡眠周期产生的 0/40ms 拍频）；
                //    静音零回调时保持 20ms FakeAudioCapture 激励节拍。
                let mut live_frame: Option<PcmFrame> = None;
                let wait_budget = if live_active {
                    Duration::from_millis(25)
                } else {
                    next_tick.saturating_duration_since(Instant::now())
                };
                let wait_deadline = Instant::now() + wait_budget;
                loop {
                    while let Some(f) = cap.try_recv_frame() {
                        frames_captured += 1;
                        if let Some(prev_ts) = last_cap_ts {
                            cap_intervals_us.push(u64::from(f.timestamp_us.wrapping_sub(prev_ts)));
                        }
                        last_cap_ts = Some(f.timestamp_us);
                        live_frame = Some(f);
                        live_active = true;
                    }
                    if live_frame.is_some()
                        || Instant::now() >= wait_deadline
                        || t0.elapsed() >= dur
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                if live_active && live_frame.is_none() {
                    live_active = false;
                    next_tick = Instant::now();
                }

                // 2) 发包前先排空旧 tick 遗留的滞后 Datagram，防止下一次 poll 读到上拍旧包形成 20ms 错位假象。
                while let Ok(evs) = cli_rx.poll(Duration::ZERO) {
                    if evs.is_empty() {
                        break;
                    }
                }

                // 3) 用实时采集帧或等幅 20ms 激励帧驱动真实 IMA-ADPCM + QUIC Datagram + AudioQueue 闭环。
                let pcm = live_frame.unwrap_or_else(|| {
                    fake_fallback.try_recv_frame().unwrap_or_else(|| PcmFrame {
                        sample_rate: DEFAULT_SAMPLE_RATE,
                        channels: DEFAULT_CHANNELS,
                        timestamp_us: 0,
                        samples: vec![0i16; 960 * usize::from(DEFAULT_CHANNELS)],
                    })
                });
                audio_seq = audio_seq.wrapping_add(1);
                let send_us = media_time_us();
                let adpcm = encode_ima_adpcm(&pcm.samples, pcm.channels);
                let pkt = build_audio_bench_packet(audio_seq, send_us, &adpcm);
                if host_tx.send_datagram(&pkt).is_ok() {
                    let rx_deadline = Instant::now() + Duration::from_millis(15);
                    let mut got_pkt = false;
                    while !got_pkt {
                        let Some(rem) = rx_deadline.checked_duration_since(Instant::now()) else {
                            break;
                        };
                        if rem.is_zero() {
                            break;
                        }
                        if let Ok(events) = cli_rx.poll(rem.min(Duration::from_millis(5))) {
                            for ev in events {
                                if let NetEvent::Datagram(bytes) = ev {
                                    let Some((origin_us, payload)) =
                                        parse_audio_bench_packet(&bytes, audio_seq)
                                    else {
                                        continue;
                                    };
                                    if let Ok(samples) = decode_ima_adpcm(payload, pcm.channels) {
                                        let decoded_pcm = PcmFrame {
                                            sample_rate: pcm.sample_rate,
                                            channels: pcm.channels,
                                            timestamp_us: pcm.timestamp_us,
                                            samples,
                                        };
                                        codec_quic_us
                                            .push(media_time_us().saturating_sub(origin_us));
                                        let p_start = Instant::now();
                                        match player.push_frame(&decoded_pcm) {
                                            Ok(()) => frames_played += 1,
                                            Err(_) => push_errors += 1,
                                        }
                                        player_push_us.push(
                                            u64::try_from(p_start.elapsed().as_micros())
                                                .unwrap_or(u64::MAX),
                                        );
                                        got_pkt = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }

                if !live_active {
                    next_tick += tick;
                    let now = Instant::now();
                    if next_tick < now {
                        next_tick = now;
                    }
                }
            }

            cap.stop();
            player.stop();
            let elapsed_s = t0.elapsed().as_secs_f64();
            let mut notes = Vec::new();
            if frames_captured == 0 {
                notes.push(
                    "S14 当前系统无音频输出播放（SCK 静音零回调），已用 20ms PCM 激励验证 ADPCM+QUIC+AudioQueue 闭环"
                        .into(),
                );
            }

            Ok(ScenarioReport {
                id: "S14-audio-pipeline".into(),
                target_fps: 50,
                duration_s: elapsed_s,
                runs_merged: 1,
                encoded_frames: frames_played,
                encoded_fps: frames_played as f64 / elapsed_s.max(0.001),
                system: SystemMetrics {
                    audio_frames_captured: Some(frames_captured),
                    audio_frames_played: Some(frames_played),
                    audio_push_errors: Some(push_errors),
                    audio_capture_interval_ms: (!cap_intervals_us.is_empty())
                        .then(|| DistStats::from_us(&cap_intervals_us)),
                    audio_codec_quic_ms: (!codec_quic_us.is_empty())
                        .then(|| DistStats::from_us(&codec_quic_us)),
                    audio_player_push_ms: (!player_push_us.is_empty())
                        .then(|| DistStats::from_us(&player_push_us)),
                    ..Default::default()
                },
                notes,
                ..Default::default()
            })
        }
        BenchSuite::S15ClipboardSync => {
            let t0 = Instant::now();
            let (_h_ep, _c_ep, mut host_ch, mut cli_ch) = open_quic_pair()?;
            let mut sender_pb =
                NspasteboardBackend::new().map_err(|e| format!("打开发送剪贴板失败：{e}"))?;
            let mut receiver_pb =
                NspasteboardBackend::new().map_err(|e| format!("打开接收剪贴板失败：{e}"))?;
            let mut guard = ClipboardRestoreGuard {
                original_text: sender_pb.read_text().ok().flatten(),
                backend: NspasteboardBackend::new()
                    .map_err(|e| format!("打开剪贴板恢复守卫失败：{e}"))?,
            };
            // 先让 receiver 与当前快照同步。
            let _ = receiver_pb.change_count();

            let mut payloads: Vec<String> = Vec::with_capacity(18);
            for i in 0..10usize {
                payloads.push(format!("RabbitControl-剪贴板测试-{i}-🐰🎉\nLine2"));
            }
            let block_16k = "ABCD1234汉字测试🚀".repeat(900);
            for i in 0..5usize {
                payloads.push(format!("BLK16K-{i}-{block_16k}"));
            }
            let block_128k = "0123456789ABCDEF".repeat(8192);
            for i in 0..3usize {
                payloads.push(format!("BLK128K-{i}-{block_128k}"));
            }

            let mut detect_us = Vec::with_capacity(payloads.len());
            let mut roundtrip_us = Vec::with_capacity(payloads.len());
            let mut exact_ok = 0u64;

            for text in &payloads {
                let prev_cnt = receiver_pb.change_count().unwrap_or(0);
                let start = Instant::now();
                sender_pb
                    .write_text(text)
                    .map_err(|e| format!("写入剪贴板失败：{e}"))?;

                let det_deadline = Instant::now() + Duration::from_millis(500);
                let mut detected = false;
                while Instant::now() < det_deadline {
                    if receiver_pb.change_count().unwrap_or(0) > prev_cnt {
                        detected = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                if !detected {
                    continue;
                }
                let t_det = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
                detect_us.push(t_det);

                let got_src = sender_pb.read_text().ok().flatten().unwrap_or_default();
                let _ = host_ch.send_reliable(got_src.as_bytes());

                let rx_deadline = Instant::now() + Duration::from_secs(2);
                let mut rx_text: Option<String> = None;
                while Instant::now() < rx_deadline {
                    if let Ok(evs) = cli_ch.poll(Duration::from_millis(2)) {
                        for ev in evs {
                            if let NetEvent::Reliable(bytes) = ev {
                                rx_text = String::from_utf8(bytes).ok();
                                break;
                            }
                        }
                    }
                    if rx_text.is_some() {
                        break;
                    }
                }
                if let Some(rx) = rx_text {
                    let rx_prev = receiver_pb.change_count().unwrap_or(0);
                    let _ = guard.backend.write_text(&rx);
                    let rx_settle_deadline = Instant::now() + Duration::from_millis(10);
                    while Instant::now() < rx_settle_deadline {
                        if receiver_pb.change_count().unwrap_or(0) > rx_prev {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    let verify = receiver_pb.read_text().ok().flatten();
                    roundtrip_us
                        .push(u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX));
                    if verify.as_deref() == Some(text.as_str()) {
                        exact_ok += 1;
                    }
                }
            }
            drop(guard);

            let elapsed_s = t0.elapsed().as_secs_f64();
            let samples = payloads.len() as u64;
            let fidelity = (exact_ok as f64 / samples as f64) * 100.0;

            Ok(ScenarioReport {
                id: "S15-clipboard-sync".into(),
                target_fps: 0,
                duration_s: elapsed_s,
                runs_merged: 1,
                encoded_frames: samples,
                system: SystemMetrics {
                    clipboard_samples: Some(samples),
                    clipboard_fidelity_pct: Some(fidelity),
                    clipboard_detect_ms: (!detect_us.is_empty())
                        .then(|| DistStats::from_us(&detect_us)),
                    clipboard_roundtrip_ms: (!roundtrip_us.is_empty())
                        .then(|| DistStats::from_us(&roundtrip_us)),
                    ..Default::default()
                },
                ..Default::default()
            })
        }
        _ => Err(format!("非独立系统场景：{suite:?}")),
    }
}

/// 连续 5 次真实冷启动测量（QUIC 握手 + `SckBackend` + `VtEncoder` + `VtDecoder` → 首帧 `MTLDrawable.presentedTime`）。
pub(super) fn measure_cold_start_ttff(
    res: Resolution,
    target_bin: &std::path::Path,
    renderer: &mut MetalRenderer,
) -> Result<DistStats, String> {
    struct TargetChild(std::process::Child);
    impl Drop for TargetChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let child = std::process::Command::new(target_bin)
        .args(["--mode", "anim", "--fps", "60"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("TTFF 启动 anim 源窗失败：{e}"))?;
    let _target_guard = TargetChild(child);
    std::thread::sleep(Duration::from_millis(250));

    let mut ttff_us = Vec::with_capacity(5);
    for _ in 0..5 {
        let t0 = Instant::now();
        let (_h_ep, _c_ep, mut host_tx, mut cli_rx) = open_quic_pair()?;
        let origin_us = rc_capture_macos::host_time_us();
        let mut capture = SckBackend::new_for_bench(
            SckConfig {
                display: 0,
                fps: 60,
                resolution: Some(res),
                origin_us: Some(origin_us),
                ..SckConfig::default()
            },
            true,
        )
        .map_err(|e| format!("TTFF 初始化 SckBackend 失败：{e}"))?;
        let mut encoder = VtEncoder::new(EncoderConfig {
            codec: Codec::H264,
            resolution: res,
            fps: 60,
            bitrate_bps: 10_000_000,
            b_frames: 0,
            gop_len: 0,
            preset: Preset::Fastest,
            low_latency: true,
        })
        .map_err(|e| format!("TTFF 初始化 VtEncoder 失败：{e}"))?;
        let mut decoder = VtDecoder::new(VtDecoderConfig::default())
            .map_err(|e| format!("TTFF 解码器失败：{e}"))?;

        let presented_us = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let pres_cb = Arc::clone(&presented_us);
        renderer.set_present_timing_sink(Some(Arc::new(move |sample: PresentTimingSample| {
            if sample.presented_us > 0 {
                let _ = pres_cb.compare_exchange(
                    0,
                    sample.presented_us,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
            }
        })));

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut token = 1u64;
        let mut first_frame = true;
        while Instant::now() < deadline && presented_us.load(Ordering::Acquire) == 0 {
            if let Ok(Some(frame)) = capture.next_frame(Duration::from_millis(12)) {
                let is_cv = matches!(frame.texture, FrameTexture::CVPixelBuffer(_));
                let _ = read_pixel_buffer_y_region(
                    &frame,
                    BARCODE_WIDTH + BARCODE_OFFSET_X,
                    BARCODE_HEIGHT + BARCODE_OFFSET_Y,
                )
                .map(|(w, h, y)| {
                    decode_barcode_y(
                        &y[BARCODE_OFFSET_Y * w + BARCODE_OFFSET_X..],
                        w,
                        w - BARCODE_OFFSET_X,
                        h - BARCODE_OFFSET_Y,
                    )
                });
                if is_cv {
                    let deadline_ts = Timestamp(frame.timestamp.0.saturating_add(100_000));
                    if encoder
                        .submit_encode(token, &frame, first_frame, deadline_ts)
                        .is_ok()
                    {
                        token += 1;
                        first_frame = false;
                    }
                }
            }
            if let Ok(Some((_tok, encoded))) = encoder.recv_encoded(Duration::from_millis(2)) {
                // 让真实首片走一遍 QUIC Datagram 通道以对齐网络冷启动语义。
                let slice_len = encoded.data.len().min(1088);
                let _ = host_tx.send_datagram(&encoded.data[..slice_len]);
                let _ = cli_rx.poll(Duration::from_millis(1));
                if let Ok(DecodeOutput::Frame(decoded)) =
                    decoder.decode(&encoded.data, encoded.kind, Timestamp(u32::MAX))
                {
                    renderer.set_next_frame_tag(token);
                    let _ = renderer.present(decoded);
                }
            }
        }
        renderer.set_present_timing_sink(None);
        drop(capture);
        if presented_us.load(Ordering::Acquire) > 0 {
            ttff_us.push(u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX));
        }
    }
    Ok(DistStats::from_us(&ttff_us))
}

/// 驱动 S16 混合真实并发负载（鼠标 `1200 px/s` + 键盘 20 键/秒 + 30Hz 滚轮 + 并发 120fps QUIC 媒体 Datagram 压测）。
pub(super) fn drive_mixed_workflow(
    res: Resolution,
    inj_stop: &AtomicBool,
    drag_points: &std::sync::Mutex<Vec<(u64, f64, f64)>>,
    mouse_state: &MouseBenchState,
    kb_state: &KeyboardBenchState,
) -> Result<(), String> {
    let mut qloop = QuicInputLoop::connect()?;
    let (_h_ep, _c_ep, mut media_host, mut media_cli) = open_quic_pair()?;
    let media_stop = Arc::new(AtomicBool::new(false));
    let media_stop_th = Arc::clone(&media_stop);
    let media_thread = std::thread::Builder::new()
        .name("rc-bench-mixed-media-quic".into())
        .spawn(move || {
            // 并发以 ~120fps × 12 包/帧（≈ 15 Mbps 模拟）灌入 QUIC Datagram + 排空接收端。
            let payload = vec![0x5Au8; 1088];
            let mut tick = Instant::now();
            while !media_stop_th.load(Ordering::Relaxed) {
                for _ in 0..12 {
                    let _ = media_host.send_datagram(&payload);
                }
                let _ = media_cli.poll(Duration::ZERO);
                tick += Duration::from_micros(8_333);
                if let Some(d) = tick.checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
            }
        })
        .map_err(|e| format!("启动并发媒体负载线程失败：{e}"))?;

    let mut guard = KeyReleaseGuard::new();
    let cx = f64::from(res.width) * 0.5;
    let cy = f64::from(res.height) * 0.5;
    let radius = 160.0f64;
    let omega = 1200.0 / radius;

    let keys = [
        "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH",
    ];
    let mut taps = Vec::new();
    let mut burst_sent = Vec::new();
    let start = Instant::now();
    let tick = Duration::from_micros(8_333); // 120Hz 主驱节奏
    let mut step = 0u64;
    let mut key_idx = 0usize;

    while !inj_stop.load(Ordering::Relaxed) {
        let elapsed_s = start.elapsed().as_secs_f64();
        let angle = omega * elapsed_s;
        let (sx, sy) =
            stage_point_from_host(res, cx + radius * angle.cos(), cy + radius * angle.sin());
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

        // 每 6 拍（~50ms ≈ 20 键/秒）交替打字一次；每 4 拍（~33ms ≈ 30Hz）发一条滚轮。
        if step % 6 == 2 {
            if let Some((sc, vk)) = scancode_and_vk(keys[key_idx % keys.len()]) {
                key_idx += 1;
                let down_us = media_time_us();
                guard.press(&mut qloop, sc);
                burst_sent.push((down_us, vk));
                std::thread::sleep(Duration::from_millis(10));
                let up_us = media_time_us();
                guard.release(&mut qloop, sc);
                if taps.len() < 40 {
                    taps.push(super::keyboard::KeyTapEvent {
                        emit_down_us: down_us,
                        emit_up_us: up_us,
                        expected_vk: vk,
                    });
                }
            }
        } else if step % 4 == 0 {
            let dy = if (step / 4) % 2 == 0 { 1 } else { -1 };
            qloop.push_scroll(dy, false);
        }

        step += 1;
        let next = start + tick * (step as u32);
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
    }

    guard.release_all(&mut qloop);
    media_stop.store(true, Ordering::Release);
    let _ = media_thread.join();
    qloop.finish(mouse_state);

    *kb_state.tap_steps.lock().unwrap_or_else(|e| e.into_inner()) = taps;
    *kb_state
        .burst_keys_sent
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = burst_sent.len() as u64;
    *kb_state
        .burst_expected_vks
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = burst_sent;
    Ok(())
}

/// 辅助函数：把环境信息打包给 `run_standalone_system_scenario` 无须依赖 `bench_target` 的独立场景（如需环境快照由上层统一填 `EnvSnapshot`）。
#[allow(dead_code)]
pub(super) fn standalone_env_check(res: Resolution) -> super::stats::EnvSnapshot {
    let _ = main_display_logical_resolution();
    collect_env_snapshot(res, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_packet_seq_filtering_rejects_stale_and_matches_current() {
        let stale = build_audio_bench_packet(4, 1_000_000, &[1, 2, 3]);
        let current = build_audio_bench_packet(5, 1_020_000, &[4, 5, 6, 7]);
        assert!(parse_audio_bench_packet(&stale, 5).is_none());
        assert!(parse_audio_bench_packet(&[0u8; 16], 5).is_none());
        assert!(parse_audio_bench_packet(b"AUD0", 5).is_none());
        let (ts, payload) = parse_audio_bench_packet(&current, 5).expect("matching seq");
        assert_eq!(ts, 1_020_000);
        assert_eq!(payload, &[4, 5, 6, 7]);
    }
}

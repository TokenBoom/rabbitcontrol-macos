#![cfg(target_os = "macos")]

//! macOS 真机系统剪贴板延迟验收测试（`#[ignore]`；会写入真实登录会话剪贴板）。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rc_capture::{CaptureBackend, DisplayInfo, FakeBackend, FakeBackendConfig, FakeContent};
use rc_decode::{FakeDecoder, FakeDecoderConfig};
use rc_encode::{Encoder, EncoderConfig, FakeEncoder};
use rc_input::{InputCapture, InputEvent, InputInjector};
use rc_pipeline::{DiffMode, PipelineConfig};
use rc_render::{FakeRenderer, FakeRendererConfig};
use rc_session::proto::SessionParams;
use rc_session::testkit::{pair, SharedFakeClipboard};
use rc_session::{
    ClientSession, ClientSessionConfig, HostSession, HostSessionConfig, SessionClock,
};
use rc_types::{Resolution, Result};

use rc_clipboard_macos::NspasteboardBackend;

#[derive(Clone, Default)]
struct NoopInjector(Arc<Mutex<Vec<InputEvent>>>);

impl InputInjector for NoopInjector {
    fn inject(&mut self, events: &[InputEvent]) -> Result<()> {
        self.0
            .lock()
            .expect("noop injector")
            .extend_from_slice(events);
        Ok(())
    }

    fn set_screen_size(&mut self, _w: u32, _h: u32) {}
}

#[derive(Clone, Default)]
struct EmptyInputCapture;

impl InputCapture for EmptyInputCapture {
    fn next_events(&mut self, _timeout: Duration) -> Result<Vec<InputEvent>> {
        Ok(Vec::new())
    }
}

fn logical_clock() -> (Arc<Mutex<u64>>, SessionClock) {
    let slot = Arc::new(Mutex::new(0u64));
    let s2 = Arc::clone(&slot);
    let clock: SessionClock =
        Arc::new(move || u32::try_from(*s2.lock().expect("clock lock")).unwrap_or(u32::MAX));
    (slot, clock)
}

fn pcfg() -> PipelineConfig {
    PipelineConfig {
        resolution: Resolution::new(64, 64),
        fps: 30,
        bitrate_bps: 1_000_000,
        prefer_hardware: false,
        target_latency_us: 22_000,
        trace_capacity: 1024,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: None,
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    }
}

fn fake_capture() -> Box<dyn CaptureBackend> {
    Box::new(
        FakeBackend::new(FakeBackendConfig {
            display: DisplayInfo {
                id: rc_types::DisplayId(0),
                resolution: Resolution::new(64, 64),
                scale: 1.0,
                hdr: false,
            },
            fps: 30,
            content: FakeContent::Static,
            dirty_rect: None,
        })
        .expect("fake capture"),
    )
}

fn fake_encoder() -> Box<dyn Encoder> {
    Box::new(
        FakeEncoder::new(EncoderConfig {
            resolution: Resolution::new(64, 64),
            fps: 30,
            bitrate_bps: 1_000_000,
            ..EncoderConfig::default()
        })
        .expect("fake encoder"),
    )
}

#[test]
#[ignore = "会读写当前登录会话的真实系统剪贴板，需手动触发"]
fn real_pasteboard_sync_under_200ms() {
    use rc_session::clipboard::ClipboardBackend;

    let (t_host, t_client) = pair();
    let (clock_slot, clock) = logical_clock();
    let addr = "127.0.0.1:19920".parse().expect("addr");
    let params = SessionParams {
        width: 64,
        height: 64,
        fps: 30,
        bitrate_bps: 1_000_000,
    };

    let mut host_cfg = HostSessionConfig::new(addr, params, Box::new(NoopInjector::default()));
    host_cfg.pipeline = pcfg();
    let mut host = HostSession::with_parts(
        host_cfg,
        Box::new(t_host),
        fake_capture(),
        fake_encoder(),
        Arc::clone(&clock),
    )
    .expect("host with_parts");

    let mut client_cfg =
        ClientSessionConfig::new(addr, params, 20_261_124, Box::new(EmptyInputCapture));
    client_cfg.pipeline = pcfg();
    let mut client = ClientSession::with_parts(
        client_cfg,
        Box::new(t_client),
        Box::new(FakeDecoder::new(FakeDecoderConfig::default()).expect("fake decoder")),
        Box::new(FakeRenderer::new(FakeRendererConfig::default()).expect("fake renderer")),
        Arc::clone(&clock),
    )
    .expect("client with_parts");

    let (mut host, mut client) = std::thread::scope(|s| {
        let h = s.spawn(|| {
            host.accept_and_handshake(Duration::from_secs(2))
                .expect("host handshake");
            host
        });
        let c = s.spawn(|| {
            client.handshake().expect("client handshake");
            client
        });
        (
            h.join().expect("host thread"),
            c.join().expect("client thread"),
        )
    });

    let real_host_cb = NspasteboardBackend::new().expect("macOS pasteboard");
    let client_cb = SharedFakeClipboard::new();
    host.enable_clipboard(Box::new(real_host_cb));
    client.enable_clipboard(Box::new(client_cb.clone()));

    let _ = host.tick(Duration::ZERO).expect("host baseline");
    let _ = client
        .tick(Duration::ZERO, Instant::now())
        .expect("client baseline");

    let payload = format!("rabbitcontrol-t20-real-sync-{}", std::process::id());
    let mut writer = NspasteboardBackend::new().expect("external pasteboard handle");
    let start = Instant::now();
    writer
        .write_text(&payload)
        .expect("write system pasteboard");

    let mut tick_idx = 1u64;
    let deadline = Duration::from_secs(2);
    while start.elapsed() < deadline {
        *clock_slot.lock().expect("clock lock") = tick_idx * 5_000;
        tick_idx += 1;
        let _ = host.tick(Duration::ZERO).expect("host tick");
        let _ = client
            .tick(Duration::ZERO, Instant::now())
            .expect("client tick");
        if client_cb.writes().iter().any(|w| w == &payload) {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    let elapsed = start.elapsed();
    assert!(
        client_cb.writes().iter().any(|w| w == &payload),
        "对端未在 {deadline:?} 内收到真实剪贴板文本"
    );
    assert!(
        elapsed < Duration::from_millis(200),
        "B4 剪贴板同步墙钟耗时 {elapsed:?} 超过 200ms 门槛"
    );
}

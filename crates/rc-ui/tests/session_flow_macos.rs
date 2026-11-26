//! macOS 图形桌面与真实 TCC 权限手动验收测试（`#[ignore]`）。

use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, Instant};

use rc_input::InputEvent;
use rc_ui::session_bridge::{SessionCommand, SessionHandle, SessionUpdate, UiSessionConfig};

fn ui_cfg(addr: SocketAddr, watch_only: bool) -> UiSessionConfig {
    UiSessionConfig {
        connect_addr: addr,
        password: None,
        watch_only,
        width: 64,
        height: 64,
        fps: 60,
        bitrate_bps: 8_000_000,
        prefer_hardware: false,
        clipboard_enabled: false,
        audio_enabled: false,
        force_relay: false,
        relay_grant: None,
        ..UiSessionConfig::default()
    }
}

fn collect_until(
    session: &SessionHandle,
    timeout: Duration,
    mut pred: impl FnMut(&[SessionUpdate]) -> bool,
) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        while let Some(u) = session.try_recv_update() {
            updates.push(u);
        }
        if pred(&updates) {
            return updates;
        }
        thread::sleep(Duration::from_millis(10));
    }
    while let Some(u) = session.try_recv_update() {
        updates.push(u);
    }
    updates
}

/// U4 开工首项 spike：Tauri 窗口 + 离屏 `MetalRenderer` + `attach_layer` 嵌入、resize 跟随、detach 清理。
#[test]
#[ignore = "需要 macOS 图形桌面与 Tauri 主线程事件循环，手动运行验收"]
fn metal_layer_spike_embeds_and_resizes() {}

/// S3「进入全屏幕」/ `F` 键往返验收。
#[test]
#[ignore = "需要 macOS 图形桌面与 Tauri 窗口全屏交互，手动运行验收"]
fn fullscreen_toggle_roundtrip() {}

/// 真机滚轮符号方向钉死：向上滚动一次 → host `RecordingInjector` 收到 `MouseWheel.delta > 0`。
#[test]
#[ignore = "需要 macOS 真机物理滚轮输入（受系统自然滚动设置影响），手动运行验收"]
fn wheel_direction_real_machine() {}

/// 真机全链路验证（ScreenCaptureKit + VideoToolbox + Metal + CGEventTap）。
#[cfg(target_os = "macos")]
#[test]
#[ignore = "需要 macOS 屏幕录制与辅助功能授权，经 ./scripts/macos-dev-run.sh test rc-ui --test session_flow_macos -- --ignored real_hardware_aspect_and_pointer_accuracy --nocapture 运行"]
fn real_hardware_aspect_and_pointer_accuracy() {
    use rc_input::InputCapture as _;
    use rc_ui::host_bridge::{HostHandle, UiHostConfig};
    use rc_ui::input_bridge;

    rc_platform_macos::register_all();

    let display_res = rc_capture_macos::main_display_logical_resolution()
        .expect("macOS 主显示器逻辑分辨率应可读");
    let mut capture =
        rc_input_macos::CgCapture::new().expect("需要辅助功能授权（CGEventTap 见证鼠标注入坐标）");
    let tap_bounds = capture.screen_bounds();

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        password: None,
        approval_timeout_us: 10_000_000,
        prefer_hardware: true,
        require_approval: false,
        auto_accept_with_password: true,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn hardware host");

    let mut cfg = ui_cfg(host.listen_addr(), false);
    cfg.width = 1920;
    cfg.height = 1080;
    cfg.fps = 144;
    cfg.prefer_hardware = true;
    let session = SessionHandle::spawn(cfg);

    let updates = collect_until(&session, Duration::from_secs(10), |list| {
        let got_neg = list
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }));
        let got_disp = list
            .iter()
            .any(|u| matches!(u, SessionUpdate::DisplayConfig { .. }));
        got_neg && got_disp
    });
    let (neg_w, neg_h, neg_fps, backend) = updates
        .iter()
        .find_map(|u| match u {
            SessionUpdate::Negotiated {
                width,
                height,
                fps,
                backend,
                ..
            } => Some((*width, *height, *fps, backend.clone())),
            _ => None,
        })
        .expect("应收到 Negotiated 更新");
    let init_presets = updates
        .iter()
        .find_map(|u| match u {
            SessionUpdate::DisplayConfig {
                max_fps, presets, ..
            } => Some((*max_fps, presets.clone())),
            _ => None,
        })
        .expect("应收到初始 DisplayConfig");
    println!(
        "real_hardware_aspect_and_pointer_accuracy: display={}x{}, negotiated={}x{}@{}fps, max_fps={}, presets={}, backend={backend}, tap_bounds={tap_bounds:?}",
        display_res.width,
        display_res.height,
        neg_w,
        neg_h,
        neg_fps,
        init_presets.0,
        init_presets.1.len()
    );
    assert_eq!(neg_fps, 144);
    assert_eq!(init_presets.0, 144);
    assert!(init_presets.1.len() >= 2);

    let display_aspect = f64::from(display_res.width) / f64::from(display_res.height);
    let neg_aspect = f64::from(neg_w) / f64::from(neg_h);
    assert!((display_aspect - neg_aspect).abs() / display_aspect < 0.002);
    assert!(neg_w <= display_res.width && neg_h <= display_res.height);

    let stage_w = 1280.0;
    let stage_h = 752.0;
    let (ox, oy, fw, fh) = input_bridge::fit_video_rect(stage_w, stage_h, neg_w, neg_h);
    let samples = [
        ("dock_left_chrome", 0.14_f64, 0.94_f64),
        ("center", 0.50_f64, 0.50_f64),
        ("top_right", 0.85_f64, 0.15_f64),
    ];

    let _ = capture.next_events(Duration::from_millis(150));
    for (label, rx, ry) in samples {
        let stage_x = ox + rx * fw;
        let stage_y = oy + ry * fh;
        let (nx, ny) = input_bridge::normalize_pointer_in_video(
            stage_x,
            stage_y,
            stage_w,
            stage_h,
            Some((neg_w, neg_h)),
        );
        let want_pt_x = (f64::from(nx) / 65535.0) * (f64::from(display_res.width) - 1.0);
        let want_pt_y = (f64::from(ny) / 65535.0) * (f64::from(display_res.height) - 1.0);

        session.send_input(InputEvent::MouseMove { x: nx, y: ny });

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut best_err = f64::INFINITY;
        let mut matched = false;
        while Instant::now() < deadline {
            let evs = capture
                .next_events(Duration::from_millis(50))
                .expect("tap next_events");
            for ev in evs {
                if let InputEvent::MouseMove { x: tx, y: ty } = ev {
                    let got_pt_x =
                        tap_bounds.x + (f64::from(tx) / 65535.0) * (tap_bounds.width - 1.0);
                    let got_pt_y =
                        tap_bounds.y + (f64::from(ty) / 65535.0) * (tap_bounds.height - 1.0);
                    let err = (got_pt_x - want_pt_x).hypot(got_pt_y - want_pt_y);
                    if err < best_err {
                        best_err = err;
                    }
                    if err <= 2.0 {
                        println!("  [{label}] err={err:.2}pt OK");
                        matched = true;
                        break;
                    }
                }
            }
            if matched {
                break;
            }
        }
        assert!(matched, "采样点 {label} 最小误差 {best_err:.2}pt");
    }

    let alt = init_presets
        .1
        .iter()
        .find(|p| p.width != neg_w || p.height != neg_h)
        .expect("应存在可切换的非当前分辨率预设");
    session.send(SessionCommand::SetStreamParams {
        width: alt.width,
        height: alt.height,
        fps: 120,
    });
    let switched = collect_until(&session, Duration::from_secs(10), |list| {
        list.iter().any(|u| {
            matches!(
                u,
                SessionUpdate::DisplayConfig {
                    width,
                    height,
                    fps: 120,
                    ..
                } if *width == alt.width && *height == alt.height
            )
        })
    });
    assert!(switched.iter().any(|u| matches!(
        u,
        SessionUpdate::DisplayConfig {
            width,
            height,
            fps: 120,
            ..
        } if *width == alt.width && *height == alt.height
    )));

    session.send(SessionCommand::Disconnect);
    session.join();
    host.stop();

    let restored_res = rc_capture_macos::main_display_logical_resolution()
        .expect("断开后应可读主显示器逻辑分辨率");
    assert_eq!(restored_res, display_res);
}

#[test]
#[ignore = "需要屏幕录制权限且建议前景有 120Hz 动画窗口（通过 ./scripts/macos-dev-run.sh 运行）"]
fn real_hardware_sustained_120fps() {
    use rc_ui::host_bridge::{HostHandle, UiHostConfig};

    rc_platform_macos::register_all();

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        password: None,
        approval_timeout_us: 10_000_000,
        prefer_hardware: true,
        require_approval: false,
        auto_accept_with_password: true,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn hardware host");

    let mut cfg = ui_cfg(host.listen_addr(), false);
    cfg.width = 1512;
    cfg.height = 982;
    cfg.fps = 120;
    cfg.prefer_hardware = true;
    let session = SessionHandle::spawn(cfg);

    let init = collect_until(&session, Duration::from_secs(10), |list| {
        list.iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }))
    });
    let mut hud_samples: Vec<f64> = Vec::new();
    for u in init {
        if let SessionUpdate::Hud(h) = u {
            if h.elapsed != "00:00:00" && h.fps > 0.0 {
                hud_samples.push(h.fps);
            }
        }
    }

    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline {
        while let Some(u) = session.try_recv_update() {
            match u {
                SessionUpdate::Hud(h) if h.elapsed != "00:00:00" => hud_samples.push(h.fps),
                SessionUpdate::Ended { reason, .. } => panic!("稳态压测中途结束：{reason}"),
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(50));
    }

    session.send(SessionCommand::Disconnect);
    session.join();
    host.stop();

    assert!(hud_samples.len() >= 18);
    let tail = &hud_samples[hud_samples.len() - 15..];
    let mut sorted = tail.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median_last_15 = sorted[sorted.len() / 2];
    let min_last_15 = sorted[0];
    assert!(median_last_15 >= 100.0);
    assert!(min_last_15 >= 85.0);
}

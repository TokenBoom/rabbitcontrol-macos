//! 光学视觉条码闭环基准探针（`window-bench` + `bench-metrics`，harness = false，零 `unsafe`）。
//!
//! 铺满主屏绘制受控测试画面与左上角 Y 平面条码（`frame_id` / `event_seq` /
//! `cursor_x` / `cursor_y`），并通过 `MTLDrawable::addPresentedHandler` 捕获
//! 真实硬件扫描上屏时刻 `src_presented_us`，经 UDP 回传给 `rc-cli bench`。

#[cfg(not(all(
    target_os = "macos",
    feature = "window-bench",
    feature = "bench-metrics"
)))]
fn main() {
    eprintln!("bench_target 仅在 macOS + --features \"window-bench bench-metrics\" 下可用");
}

#[cfg(all(
    target_os = "macos",
    feature = "window-bench",
    feature = "bench-metrics"
))]
fn main() {
    use std::collections::VecDeque;
    use std::net::{SocketAddr, UdpSocket};
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use rc_decode::DecodedFrame;
    use rc_render::Renderer;
    use rc_render_macos::barcode::{
        encode_barcode_y, BarcodePayload, BARCODE_HEIGHT, BARCODE_WIDTH,
    };
    use rc_render_macos::{
        media_time_us, window_bench, MetalRenderer, MetalRendererConfig, PresentTimingSample,
    };

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Anim,
        Static,
        Interactive,
        Wakeup,
    }

    #[derive(Clone, Copy)]
    struct FrameMeta {
        frame_id: u16,
        event_seq: u16,
        cursor_x: u16,
        cursor_y: u16,
        event_recv_us: u64,
        click_down_seq: u32,
        click_count: u16,
        is_dragging: bool,
        scroll_seq: u32,
        scroll_accum_tenths: i32,
        key_down_seq: u32,
        key_up_seq: u32,
        last_keycode: u16,
        last_key_mods: u8,
        key_is_down: bool,
        key_recv_us: u64,
    }

    let mut mode = Mode::Anim;
    let mut fps: u32 = 60;
    let mut udp_target: Option<SocketAddr> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--mode" => {
                let val = args.next().unwrap_or_default();
                mode = match val.as_str() {
                    "anim" => Mode::Anim,
                    "static" => Mode::Static,
                    "interactive" => Mode::Interactive,
                    "wakeup" => Mode::Wakeup,
                    other => {
                        eprintln!("bench_target: 未知 --mode {other}");
                        std::process::exit(2);
                    }
                };
            }
            "--fps" => {
                fps = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(60)
                    .clamp(1, 240);
            }
            "--udp-report" => {
                let raw = args.next().unwrap_or_default();
                udp_target = Some(raw.parse::<SocketAddr>().unwrap_or_else(|e| {
                    eprintln!("bench_target: 无效 --udp-report {raw}: {e}");
                    std::process::exit(2);
                }));
            }
            other => {
                eprintln!("bench_target: 未知参数 {other}");
                std::process::exit(2);
            }
        }
    }

    let mtm = objc2::MainThreadMarker::new().expect("bench_target main 必须在主线程");
    let (win, size) =
        window_bench::fullscreen_window_with_metal_layer(mtm).expect("创建全屏探针窗失败");
    win.set_level(25);
    win.set_ignores_mouse_events(false);
    win.set_accepts_mouse_moved(true);

    {
        use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
    win.make_key_and_order_front();

    // 共享输入事件原子状态与唤醒条件变量。
    let packed_input = Arc::new(AtomicU64::new(0));
    let event_recv_us = Arc::new(AtomicU64::new(0));
    let click_down_seq = Arc::new(AtomicU32::new(0));
    let last_click_count = Arc::new(AtomicU32::new(0));
    let is_dragging = Arc::new(AtomicBool::new(false));
    let scroll_seq = Arc::new(AtomicU32::new(0));
    let scroll_accum_tenths = Arc::new(AtomicI32::new(0));
    let key_down_seq = Arc::new(AtomicU32::new(0));
    let key_up_seq = Arc::new(AtomicU32::new(0));
    let last_keycode = Arc::new(AtomicU32::new(0));
    let last_key_mods = Arc::new(AtomicU32::new(0));
    let key_is_down = Arc::new(AtomicBool::new(false));
    let key_event_recv_us = Arc::new(AtomicU64::new(0));
    let wake_pair = Arc::new((Mutex::new(0u64), Condvar::new()));

    let key_udp_sock = udp_target.and_then(|addr| {
        UdpSocket::bind("127.0.0.1:0")
            .ok()
            .map(|s| (Arc::new(s), addr))
    });

    let _key_monitor = if mode == Mode::Interactive {
        let kd_c = Arc::clone(&key_down_seq);
        let ku_c = Arc::clone(&key_up_seq);
        let kc_c = Arc::clone(&last_keycode);
        let km_c = Arc::clone(&last_key_mods);
        let kdown_c = Arc::clone(&key_is_down);
        let krecv_c = Arc::clone(&key_event_recv_us);
        let wake_k = Arc::clone(&wake_pair);
        let udp_k = key_udp_sock.clone();
        window_bench::install_keyboard_monitor(
            mtm,
            Arc::new(move |kev: window_bench::BenchKeyEvent| {
                let now_us = media_time_us();
                kc_c.store(u32::from(kev.keycode), Ordering::Relaxed);
                km_c.store(u32::from(kev.mod_bits), Ordering::Relaxed);
                krecv_c.store(now_us, Ordering::Relaxed);
                let seq = match kev.kind {
                    window_bench::BenchKeyKind::KeyDown => {
                        kdown_c.store(true, Ordering::Relaxed);
                        kd_c.fetch_add(1, Ordering::Relaxed) + 1
                    }
                    window_bench::BenchKeyKind::KeyUp => {
                        kdown_c.store(false, Ordering::Relaxed);
                        ku_c.fetch_add(1, Ordering::Relaxed) + 1
                    }
                    window_bench::BenchKeyKind::FlagsChanged => kd_c.load(Ordering::Relaxed),
                };
                if let Some((ref sock, addr)) = udp_k {
                    let is_down = kev.kind == window_bench::BenchKeyKind::KeyDown;
                    let is_up = kev.kind == window_bench::BenchKeyKind::KeyUp;
                    let line = format!(
                        "{{\"kind\":\"key\",\"seq\":{},\"keycode\":{},\"mods\":{},\"down\":{},\"up\":{},\"recv_us\":{}}}\n",
                        seq, kev.keycode, kev.mod_bits, is_down, is_up, now_us
                    );
                    let _ = sock.send_to(line.as_bytes(), addr);
                }
                let (lock, cvar) = &*wake_k;
                let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
                *g = g.wrapping_add(1);
                cvar.notify_one();
            }),
        )
    } else {
        None
    };

    let _monitor = if matches!(mode, Mode::Interactive | Mode::Wakeup) {
        let packed_clone = Arc::clone(&packed_input);
        let recv_clone = Arc::clone(&event_recv_us);
        let click_seq_c = Arc::clone(&click_down_seq);
        let click_cnt_c = Arc::clone(&last_click_count);
        let dragging_c = Arc::clone(&is_dragging);
        let scroll_seq_c = Arc::clone(&scroll_seq);
        let scroll_accum_c = Arc::clone(&scroll_accum_tenths);
        let wake_clone = Arc::clone(&wake_pair);
        let next_seq = Arc::new(AtomicU64::new(1));
        window_bench::install_local_mouse_monitor(
            mtm,
            f64::from(size.1),
            Arc::new(move |ev: window_bench::BenchMouseEvent| {
                let now_us = media_time_us();
                match ev.kind {
                    window_bench::BenchMouseKind::LeftDown => {
                        click_seq_c.fetch_add(1, Ordering::Relaxed);
                        click_cnt_c.store(ev.click_count.max(0) as u32, Ordering::Relaxed);
                        dragging_c.store(true, Ordering::Relaxed);
                    }
                    window_bench::BenchMouseKind::LeftDragged => {
                        dragging_c.store(true, Ordering::Relaxed);
                    }
                    window_bench::BenchMouseKind::LeftUp => {
                        dragging_c.store(false, Ordering::Relaxed);
                    }
                    window_bench::BenchMouseKind::ScrollWheel { delta_y } => {
                        scroll_seq_c.fetch_add(1, Ordering::Relaxed);
                        let dt = (delta_y * 10.0).round() as i32;
                        scroll_accum_c.fetch_add(dt, Ordering::Relaxed);
                    }
                    window_bench::BenchMouseKind::Moved => {}
                }
                let seq = (next_seq.fetch_add(1, Ordering::Relaxed) & 0xffff) as u16;
                let cx = ev.x.round().clamp(0.0, f64::from(u16::MAX)) as u16;
                let cy = ev.y.round().clamp(0.0, f64::from(u16::MAX)) as u16;
                let packed = (u64::from(seq) << 32) | (u64::from(cx) << 16) | u64::from(cy);
                recv_clone.store(now_us, Ordering::Relaxed);
                packed_clone.store(packed, Ordering::Release);
                let (lock, cvar) = &*wake_clone;
                let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
                *g = g.wrapping_add(1);
                cvar.notify_one();
            }),
        )
    } else {
        None
    };

    let layer_ptr = win.layer_ptr() as usize;
    let render_handle = std::thread::Builder::new()
        .name("rc-bench-target-render".into())
        .spawn(move || {
            let mut renderer = MetalRenderer::from_raw_layer(
                layer_ptr as *mut _,
                MetalRendererConfig {
                    size,
                    ..MetalRendererConfig::default()
                },
            )
            .expect("探针 MetalRenderer 创建失败");

            let meta_ring = Arc::new(Mutex::new(VecDeque::<FrameMeta>::with_capacity(64)));
            if let Some(addr) = udp_target {
                let sock = UdpSocket::bind("127.0.0.1:0").expect("绑定探针 UDP 失败");
                let ring_cb = Arc::clone(&meta_ring);
                renderer.set_present_timing_sink(Some(Arc::new(
                    move |sample: PresentTimingSample| {
                        if sample.presented_us == 0 {
                            return;
                        }
                        let meta = {
                            let mut ring = ring_cb.lock().unwrap_or_else(|e| e.into_inner());
                            ring.iter()
                                .position(|m| u64::from(m.frame_id) == sample.frame_tag)
                                .and_then(|idx| ring.remove(idx))
                        };
                        if let Some(m) = meta {
                            let line = format!(
                                "{{\"frame_id\":{},\"src_presented_us\":{},\"event_seq\":{},\"cursor_x\":{},\"cursor_y\":{},\"event_recv_us\":{},\"click_down_seq\":{},\"click_count\":{},\"is_dragging\":{},\"scroll_seq\":{},\"scroll_accum_tenths\":{},\"key_down_seq\":{},\"key_up_seq\":{},\"last_keycode\":{},\"last_key_mods\":{},\"key_is_down\":{},\"key_recv_us\":{}}}\n",
                                m.frame_id,
                                sample.presented_us,
                                m.event_seq,
                                m.cursor_x,
                                m.cursor_y,
                                m.event_recv_us,
                                m.click_down_seq,
                                m.click_count,
                                m.is_dragging,
                                m.scroll_seq,
                                m.scroll_accum_tenths,
                                m.key_down_seq,
                                m.key_up_seq,
                                m.last_keycode,
                                m.last_key_mods,
                                m.key_is_down,
                                m.key_recv_us
                            );
                            let _ = sock.send_to(line.as_bytes(), addr);
                        }
                    },
                )));
            }

            let make_frame = |src: &window_bench::BenchFrameSource| {
                let (w, h) = src.size();
                DecodedFrame {
                    texture: rc_capture::FrameTexture::CVPixelBuffer(src.ptr()),
                    decode_done_ts: rc_types::Timestamp(0),
                    width: w,
                    height: h,
                }
            };

            let ring: Vec<_> = (0..4)
                .map(|_| {
                    window_bench::BenchFrameSource::nv12(size.0, size.1)
                        .expect("创建探针 NV12 帧源失败")
                })
                .collect();

            if mode == Mode::Static {
                let warmup_code = encode_barcode_y(BarcodePayload::default());
                for src in &ring {
                    src.paint_bench_frame(
                        &warmup_code,
                        BARCODE_WIDTH,
                        BARCODE_HEIGHT,
                        None,
                        None,
                        None,
                    )
                    .expect("静止模式预热绘制失败");
                    renderer.set_next_frame_tag(0);
                    renderer.present(make_frame(src)).expect("present 失败");
                    std::thread::sleep(Duration::from_millis(20));
                }

                let payload = BarcodePayload {
                    frame_id: 1,
                    ..BarcodePayload::default()
                };
                let barcode = encode_barcode_y(payload);
                ring[0]
                    .paint_bench_frame(&barcode, BARCODE_WIDTH, BARCODE_HEIGHT, None, None, None)
                    .expect("绘制静止探针帧失败");
                {
                    let mut q = meta_ring.lock().unwrap_or_else(|e| e.into_inner());
                    q.push_back(FrameMeta {
                        frame_id: 1,
                        event_seq: 0,
                        cursor_x: 0,
                        cursor_y: 0,
                        event_recv_us: 0,
                        click_down_seq: 0,
                        click_count: 0,
                        is_dragging: false,
                        scroll_seq: 0,
                        scroll_accum_tenths: 0,
                        key_down_seq: 0,
                        key_up_seq: 0,
                        last_keycode: 0,
                        last_key_mods: 0,
                        key_is_down: false,
                        key_recv_us: 0,
                    });
                }
                renderer.set_next_frame_tag(1);
                renderer.present(make_frame(&ring[0])).expect("present 失败");
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                }
            }

            // 预热帧，稳定 drawable 池与 Metal 管线。
            let warmup_code = encode_barcode_y(BarcodePayload::default());
            for src in &ring {
                src.paint_bench_frame(&warmup_code, BARCODE_WIDTH, BARCODE_HEIGHT, None, None, None)
                    .expect("预热绘制失败");
                renderer.set_next_frame_tag(0);
                renderer.present(make_frame(src)).expect("预热 present 失败");
            }

            let tick = Duration::from_nanos(1_000_000_000 / u64::from(fps));
            let mut next_tick = Instant::now();
            let mut frame_counter: u64 = 1;
            let mut last_event_gen = 0u64;
            let mut wakeup_until: Option<Instant> = None;

            if mode == Mode::Wakeup {
                // 发一帧初始静态帧让 SCK 完成首帧捕获并随后进入静止 0 编码状态。
                let init_code = encode_barcode_y(BarcodePayload {
                    frame_id: 1,
                    ..BarcodePayload::default()
                });
                ring[0]
                    .paint_bench_frame(
                        &init_code,
                        BARCODE_WIDTH,
                        BARCODE_HEIGHT,
                        None,
                        None,
                        None,
                    )
                    .expect("首帧绘制失败");
                {
                    let mut q = meta_ring.lock().unwrap_or_else(|e| e.into_inner());
                    q.push_back(FrameMeta {
                        frame_id: 1,
                        event_seq: 0,
                        cursor_x: 0,
                        cursor_y: 0,
                        event_recv_us: 0,
                        click_down_seq: 0,
                        click_count: 0,
                        is_dragging: false,
                        scroll_seq: 0,
                        scroll_accum_tenths: 0,
                        key_down_seq: 0,
                        key_up_seq: 0,
                        last_keycode: 0,
                        last_key_mods: 0,
                        key_is_down: false,
                        key_recv_us: 0,
                    });
                }
                renderer.set_next_frame_tag(1);
                renderer.present(make_frame(&ring[0])).expect("present 失败");
                frame_counter = 2;
            }

            loop {
                if mode == Mode::Wakeup {
                    let active = wakeup_until.is_some_and(|dl| Instant::now() < dl);
                    if !active {
                        let (lock, cvar) = &*wake_pair;
                        let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
                        while *g == last_event_gen {
                            g = cvar.wait(g).unwrap_or_else(|e| e.into_inner());
                        }
                        last_event_gen = *g;
                        wakeup_until = Some(Instant::now() + Duration::from_millis(1200));
                    } else {
                        let (lock, cvar) = &*wake_pair;
                        let g = lock.lock().unwrap_or_else(|e| e.into_inner());
                        if *g != last_event_gen {
                            last_event_gen = *g;
                            wakeup_until = Some(Instant::now() + Duration::from_millis(1200));
                        } else if let Some(wait_dur) = next_tick.checked_duration_since(Instant::now())
                        {
                            let (g2, _) = cvar
                                .wait_timeout(g, wait_dur)
                                .unwrap_or_else(|e| e.into_inner());
                            if *g2 != last_event_gen {
                                last_event_gen = *g2;
                                wakeup_until = Some(Instant::now() + Duration::from_millis(1200));
                            }
                        }
                    }
                } else if mode == Mode::Interactive {
                    let (lock, cvar) = &*wake_pair;
                    let g = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if *g != last_event_gen {
                        last_event_gen = *g;
                    } else if let Some(wait_dur) = next_tick.checked_duration_since(Instant::now())
                    {
                        let (g2, _) = cvar
                            .wait_timeout(g, wait_dur)
                            .unwrap_or_else(|e| e.into_inner());
                        if *g2 != last_event_gen {
                            last_event_gen = *g2;
                        }
                    }
                } else {
                    wait_until(next_tick);
                }
                next_tick = Instant::now() + tick;

                let packed = packed_input.load(Ordering::Acquire);
                let recv_us = event_recv_us.load(Ordering::Relaxed);
                let cur_click_seq = click_down_seq.load(Ordering::Relaxed);
                let cur_click_cnt = last_click_count.load(Ordering::Relaxed).min(u32::from(u16::MAX)) as u16;
                let cur_dragging = is_dragging.load(Ordering::Relaxed);
                let cur_scroll_seq = scroll_seq.load(Ordering::Relaxed);
                let cur_scroll_tenths = scroll_accum_tenths.load(Ordering::Relaxed);
                let cur_kd_seq = key_down_seq.load(Ordering::Relaxed);
                let cur_ku_seq = key_up_seq.load(Ordering::Relaxed);
                let cur_kc = last_keycode.load(Ordering::Relaxed).min(u32::from(u16::MAX)) as u16;
                let cur_km = last_key_mods.load(Ordering::Relaxed).min(255) as u8;
                let cur_kdown = key_is_down.load(Ordering::Relaxed);
                let cur_krecv = key_event_recv_us.load(Ordering::Relaxed);
                let event_seq = ((packed >> 32) & 0xffff) as u16;
                let cursor_x = ((packed >> 16) & 0xffff) as u16;
                let cursor_y = (packed & 0xffff) as u16;

                let frame_id = ((frame_counter % 65_534) + 1) as u16;
                let payload = BarcodePayload {
                    frame_id,
                    event_seq,
                    cursor_x,
                    cursor_y,
                };
                let barcode = encode_barcode_y(payload);
                let cursor_opt = if matches!(mode, Mode::Interactive | Mode::Wakeup) {
                    Some((u32::from(cursor_x), u32::from(cursor_y)))
                } else {
                    None
                };
                let anim_opt = if mode == Mode::Anim {
                    Some(frame_counter)
                } else {
                    None
                };
                let band_opt = if mode == Mode::Interactive {
                    let span = (size.1 as i32 - 220).max(120);
                    let band_y = (400i32 + cur_scroll_tenths * 2 + ((cur_kd_seq + cur_ku_seq) as i32) * 19).rem_euclid(span) as u32 + 120;
                    let luma = if cur_dragging {
                        225u8
                    } else if cur_kdown {
                        205u8
                    } else if ((cur_click_seq + cur_kd_seq + cur_ku_seq) & 1) == 1 {
                        185u8
                    } else {
                        120u8
                    };
                    Some((band_y, luma))
                } else {
                    None
                };

                let src = &ring[(frame_counter as usize) % ring.len()];
                src.paint_bench_frame(
                    &barcode,
                    BARCODE_WIDTH,
                    BARCODE_HEIGHT,
                    cursor_opt,
                    anim_opt,
                    band_opt,
                )
                .expect("绘制探针帧失败");

                {
                    let mut q = meta_ring.lock().unwrap_or_else(|e| e.into_inner());
                    if q.len() >= 64 {
                        q.pop_front();
                    }
                    q.push_back(FrameMeta {
                        frame_id,
                        event_seq,
                        cursor_x,
                        cursor_y,
                        event_recv_us: recv_us,
                        click_down_seq: cur_click_seq,
                        click_count: cur_click_cnt,
                        is_dragging: cur_dragging,
                        scroll_seq: cur_scroll_seq,
                        scroll_accum_tenths: cur_scroll_tenths,
                        key_down_seq: cur_kd_seq,
                        key_up_seq: cur_ku_seq,
                        last_keycode: cur_kc,
                        last_key_mods: cur_km,
                        key_is_down: cur_kdown,
                        key_recv_us: cur_krecv,
                    });
                }
                renderer.set_next_frame_tag(u64::from(frame_id));
                renderer.present(make_frame(src)).expect("present 失败");
                frame_counter = frame_counter.wrapping_add(1);
            }
        })
        .expect("创建探针渲染线程失败");

    {
        use objc2_app_kit::NSApplication;
        NSApplication::sharedApplication(mtm).run();
    }
    let _ = render_handle.join();
}

#[cfg(all(
    target_os = "macos",
    feature = "window-bench",
    feature = "bench-metrics"
))]
fn wait_until(target: std::time::Instant) {
    use std::time::{Duration, Instant};
    let now = Instant::now();
    if target <= now {
        return;
    }
    let rem = target - now;
    if rem > Duration::from_millis(2) {
        std::thread::sleep(rem - Duration::from_millis(2));
    }
    while Instant::now() < target {
        std::hint::spin_loop();
    }
}

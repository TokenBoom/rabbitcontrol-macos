//! Disposable loopback acceptance host for the iOS simulator. Never run against a production server.
use core_foundation::runloop::{kCFRunLoopDefaultMode, CFRunLoop};
use rc_client_core::{
    auth_client::{AuthHttpClient, AuthSessionManager},
    cert_manager::{CertManager, RcOwnerTicketVerifier},
    host_bridge::{HostHandle, SessionMtlsBundle, UiHostConfig},
    server_config::ServerConfig,
    signal_client::{SignalClientHandle, SignalClientOptions},
};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

// Even when capture is explicitly enabled, never inject input or lock the machine.
#[derive(Clone)]
struct Checks {
    events: Arc<AtomicU32>,
    capture_display: bool,
}
impl rc_client_core::platform::ClientPlatformHooks for Checks {
    fn main_display_logical_resolution(&self) -> Option<rc_types::Resolution> {
        if self.capture_display {
            rc_capture_macos::main_display_logical_resolution()
        } else {
            Some(rc_types::Resolution::new(1280, 720))
        }
    }
    fn main_display_backing_resolution(&self) -> Option<rc_types::Resolution> {
        if self.capture_display {
            rc_capture_macos::main_display_backing_resolution()
        } else {
            self.main_display_logical_resolution()
        }
    }
    fn create_main_display_injector(&self) -> Option<Box<dyn rc_input::InputInjector>> {
        Some(Box::new(self.clone()))
    }
    fn lock_screen_now(&self) -> bool {
        self.events.fetch_or(32, Ordering::Relaxed);
        true
    }
}
impl rc_input::InputInjector for Checks {
    fn set_screen_size(&mut self, _: u32, _: u32) {}
    fn inject(&mut self, events: &[rc_input::InputEvent]) -> rc_types::Result<()> {
        use rc_input::InputEvent::*;
        for event in events {
            let bit = match event {
                MouseButton {
                    button: rc_input::MouseButton::Right,
                    pressed: true,
                } => 1,
                MouseButton {
                    button: rc_input::MouseButton::Right,
                    pressed: false,
                } => 2,
                MouseWheel {
                    delta: 3,
                    horizontal: false,
                } => 4,
                Key {
                    scancode: 0x1e,
                    pressed: true,
                    ..
                } => 8,
                Key {
                    scancode: 0x1e,
                    pressed: false,
                    ..
                } => 16,
                _ => 0,
            };
            self.events.fetch_or(bit, Ordering::Relaxed);
        }
        Ok(())
    }
}
// The fixture cannot touch the user's general pasteboard. A Mac-side copy is simulated only for the test marker.
#[derive(Default)]
struct Board(u64, String);
impl rc_session::ClipboardBackend for Board {
    fn change_count(&mut self) -> rc_types::Result<u64> {
        Ok(self.0)
    }
    fn read_text(&mut self) -> rc_types::Result<Option<String>> {
        Ok(Some(self.1.clone()))
    }
    fn write_text(&mut self, s: &str) -> rc_types::Result<()> {
        if s == "ios-clip-test" {
            self.0 += 1;
            self.1 = "mac-clip-回传".into();
        }
        Ok(())
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("private fixture directory required")?,
    );
    let read = |name: &str| fs::read_to_string(path.join(name));
    let password = read("code")?;
    assert!(password.len() >= 16);
    let capture_display = match std::env::args_os().nth(2).as_deref() {
        None => false,
        Some(flag) if flag == "--capture-display" => true,
        _ => return Err("expected optional --capture-display".into()),
    };
    let checks = Checks {
        events: Arc::default(),
        capture_display,
    };
    assert!(rc_client_core::platform::register_platform_hooks(Arc::new(
        checks.clone()
    )));
    assert!(rc_session::register_clipboard_provider(Arc::new(|| Ok(
        Box::<Board>::default()
    ))));
    if capture_display {
        rc_platform_macos::register_all();
    }
    let cert = Arc::new(CertManager::load_or_init(path.join("certs"))?);
    let mut cfg = ServerConfig {
        enabled: true,
        server_host: "127.0.0.1".into(),
        use_https: true,
        device_key: cert.device_pubkey_hex(),
        ..Default::default()
    };
    let http = AuthHttpClient::from_server_config(&cfg)?;
    let ca = http.fetch_ca_bundle()?;
    cfg.ca_cert_pem = Some(ca.ca_cert_pem.clone());
    cfg.server_auth_pubkey_hex = Some(ca.auth_pubkey_hex.clone());
    let http = AuthHttpClient::from_server_config(&cfg)?;
    let auth = Arc::new(AuthSessionManager::new(
        cfg.clone(),
        path.join("test-session"),
        cert.clone(),
    ));
    let session = auth.register(
        &read("email")?,
        &read("account-code")?,
        None,
        Some("acceptance-mac"),
        Some("macos"),
    )?;
    let device = http
        .list_my_devices(&session.access_token)?
        .into_iter()
        .find(|d| d.hostname == "acceptance-mac")
        .ok_or("missing Mac")?;
    auth.ensure_device_cert(&device.device_id)?;
    let (cert_pem, meta) = cert.load_device_cert().ok_or("missing cert")?;
    let bundle = SessionMtlsBundle {
        ca_cert_pem: ca.ca_cert_pem,
        cert_pem,
        key_pem: cert.tls_key_pem()?,
        local_user_id: meta.user_id,
        expected_peer_device_id: None,
        expected_peer_user_id: None,
    };
    let verify = RcOwnerTicketVerifier::new(
        &ca.auth_pubkey_hex,
        session.user_id.clone(),
        device.device_id.clone(),
    )?;
    let direct_config = UiHostConfig {
        listen_addr: "127.0.0.1:0".parse()?,
        password: Some(password),
        require_approval: true,
        auto_accept_with_password: true,
        forbid_passwordless_auto_approve: true,
        // Core also gates our recording input and clipboard hooks on this flag.
        // Without --capture-display no SCK provider exists; only test video is possible.
        prefer_hardware: true,
        clipboard_enabled: true,
        audio_enabled: false,
        ..Default::default()
    };
    let mut host = HostHandle::spawn(UiHostConfig {
        mtls_config: Some(bundle),
        accept_mtls_lan: true,
        owner_verifier: Some(Arc::new(verify)),
        ..direct_config.clone()
    })?;
    let mut direct = HostHandle::spawn(direct_config)?;
    let token_auth = auth.clone();
    let signal = SignalClientHandle::spawn_with_options(
        cfg,
        "acceptance-mac".into(),
        vec![],
        SignalClientOptions {
            token_provider: Some(Arc::new(move || {
                token_auth
                    .ensure_valid_access_token()
                    .map_err(|e| e.to_string())
            })),
            device_pubkey: Some(cert.device_pubkey_hex()),
            allow_remote_assist: true,
            dynamic_lan_discovery: false,
        },
        |_| {},
    );
    let http_test = TcpListener::bind("127.0.0.1:0")?;
    http_test.set_nonblocking(true)?;
    fs::write(path.join("endpoint"), direct.listen_addr().to_string())?;
    fs::write(
        path.join("http-port"),
        http_test.local_addr()?.port().to_string(),
    )?;
    fs::write(path.join("device"), device.device_id)?;
    let disconnect = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + Duration::from_secs(900);
    while Instant::now() < deadline && !path.join("stop").exists() {
        while let Some(incoming) = signal.try_recv_incoming() {
            host.offer_relay_incoming(incoming)
        }
        while let Some(update) = host.try_recv_update() {
            if let rc_client_core::host_bridge::HostUpdate::OwnerConnected { .. } = update {
                eprintln!("owner verified");
            }
        }
        if disconnect.swap(false, Ordering::Relaxed) {
            host.disconnect_client();
        }
        if let Ok((mut sock, _)) = http_test.accept() {
            let checks = checks.clone();
            let disconnect = Arc::clone(&disconnect);
            std::thread::spawn(move || -> std::io::Result<()> {
                sock.set_nonblocking(false)?;
                sock.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = sock.read(&mut chunk)?;
                    if n == 0 || request.len() + n > 8192 {
                        return Ok(());
                    }
                    request.extend_from_slice(&chunk[..n]);
                }
                let body = if request.starts_with(b"GET /checks ") {
                    format!("fixture-checks:{}", checks.events.load(Ordering::Relaxed))
                } else if request.starts_with(b"GET /capture-mode ") {
                    if capture_display {
                        "display"
                    } else {
                        "synthetic"
                    }
                    .into()
                } else if request.starts_with(b"POST /disconnect ") {
                    disconnect.store(true, Ordering::Relaxed);
                    "fixture-disconnected".into()
                } else if request.starts_with(b"GET /fixture ") {
                    "rc-loopback".into()
                } else {
                    return Ok(());
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}", body.len(), body);
                sock.write_all(response.as_bytes())?;
                eprintln!("fixture HTTP response delivered: {body}");
                Ok(())
            });
        }
        // ScreenCaptureKit dispatches stream startup work to the main run loop.
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            Duration::from_millis(15),
            false,
        );
    }
    direct.stop();
    host.stop();
    Ok(())
}

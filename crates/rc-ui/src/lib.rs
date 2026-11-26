//! RabbitControl macOS 桌面端 UI 宿主与后台服务控制模块。
//!
//! 跨平台纯业务逻辑已抽离至 [`rc_client_core`] 并在此全量再导出以保持内部代码兼容；
//! 本 crate 专注 macOS Metal 视图层桥接（[`metal_attach`]）、系统 TCC 权限预检（[`permissions`]）、
//! 网络路由切换监测（[`network_monitor`]）、本地守护/被控 IPC（[`service_ipc`]）
//! 及 `LaunchDaemon`/`LaunchAgent` 安装控制（[`service_manager`]）。

#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod metal_attach;
pub mod network_monitor;
pub mod permissions;
pub mod service_ipc;
pub mod service_manager;
pub mod updater_macos;

pub use rc_client_core::{
    app_prefs, auth_client, cert_manager, config_paths, display_ctrl, host_bridge, hud,
    input_bridge, lan_discovery, model, p2p, platform, relay_transport, server_config,
    session_bridge, signal_client, ui_error, updater, upnp,
};

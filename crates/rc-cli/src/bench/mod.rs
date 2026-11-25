//! macOS 量化基准测试套件（仅 `bench-metrics` 启用，零 `unsafe`）。

pub mod diff;
#[cfg(target_os = "macos")]
pub mod keyboard;
#[cfg(target_os = "macos")]
pub mod mouse;
#[cfg(target_os = "macos")]
pub mod runner;
pub mod stats;
#[cfg(target_os = "macos")]
pub mod synth;
#[cfg(target_os = "macos")]
pub mod system;

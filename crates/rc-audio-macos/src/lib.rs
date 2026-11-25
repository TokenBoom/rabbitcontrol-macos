//! RabbitControl macOS 音频硬件适配器。
//!
//! 包装 ScreenCaptureKit 系统音频采集与 `AudioToolbox` `AudioQueue` 播放，
//! 并实现 [`rc_audio::AudioBackendProvider`] 注册到核心层 [`rc_audio`]。

#![deny(missing_docs)]
#![deny(unsafe_code)]

#[cfg(target_os = "macos")]
mod macos_capture;
#[cfg(target_os = "macos")]
mod macos_host_mute;
#[cfg(target_os = "macos")]
mod macos_playback;

use std::sync::Arc;

use rc_audio::{AudioBackendProvider, AudioCapture, AudioPlayer};

struct MacosAudioProvider;

impl AudioBackendProvider for MacosAudioProvider {
    fn create_system_audio_capture(&self) -> Result<Box<dyn AudioCapture>, String> {
        #[cfg(target_os = "macos")]
        {
            let capture = macos_capture::MacOsAudioCapture::new()?;
            Ok(Box::new(capture))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err("macOS audio capture is only supported on macOS".to_string())
        }
    }

    fn create_audio_player(
        &self,
        sample_rate: u32,
        channels: u8,
    ) -> Result<Box<dyn AudioPlayer>, String> {
        #[cfg(target_os = "macos")]
        {
            let player = macos_playback::MacOsAudioPlayer::new(sample_rate, channels)?;
            Ok(Box::new(player))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (sample_rate, channels);
            Err("macOS audio playback is only supported on macOS".to_string())
        }
    }
}

/// 将 macOS 系统音频采集与播放提供者注册到核心层 [`rc_audio`]。
pub fn register() -> bool {
    rc_audio::register_audio_provider(Arc::new(MacosAudioProvider))
}

/// 设置被控端本机扬声器系统静音状态（非 macOS 平台返回错误）。
pub fn set_host_system_muted(muted: bool) -> Result<bool, String> {
    #[cfg(target_os = "macos")]
    {
        macos_host_mute::set_host_system_muted(muted)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = muted;
        Err("host system audio mute is only supported on macOS".to_string())
    }
}

/// 恢复远控前记录的被控端本机扬声器静音状态（幂等）。
pub fn restore_host_system_audio() {
    #[cfg(target_os = "macos")]
    {
        macos_host_mute::restore_host_system_audio();
    }
}

/// 清除已记录的被控端原始声音快照（不改动当前硬件静音状态）。
pub fn clear_host_audio_snapshot() {
    #[cfg(target_os = "macos")]
    {
        macos_host_mute::clear_host_audio_snapshot();
    }
}

//! macOS 被控端本机扬声器系统静音与退出恢复（基于 `CoreAudio.framework` HAL 属性 API）。
//!
//! 仅在收到控制端 `SetHostMute` 时操作默认输出设备的 `kAudioDevicePropertyMute` 属性，
//! 首次修改前缓存设备 ID 与原始静音状态，会话结束调用 [`restore_host_system_audio`] 恢复。

#![allow(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]
#![deny(missing_docs)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

type OSStatus = i32;
type AudioObjectID = u32;
type AudioObjectPropertySelector = u32;
type AudioObjectPropertyScope = u32;
type AudioObjectPropertyElement = u32;

const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectID = 1;
const K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE: AudioObjectPropertySelector =
    u32::from_be_bytes(*b"dOut");
const K_AUDIO_DEVICE_PROPERTY_MUTE: AudioObjectPropertySelector = u32::from_be_bytes(*b"mute");
const K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL: AudioObjectPropertyScope = u32::from_be_bytes(*b"glob");
const K_AUDIO_DEVICE_PROPERTY_SCOPE_OUTPUT: AudioObjectPropertyScope = u32::from_be_bytes(*b"outp");
const K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN: AudioObjectPropertyElement = 0;

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioObjectPropertyAddress {
    m_selector: AudioObjectPropertySelector,
    m_scope: AudioObjectPropertyScope,
    m_element: AudioObjectPropertyElement,
}

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyData(
        in_object_id: AudioObjectID,
        in_address: *const AudioObjectPropertyAddress,
        in_qualifier_data_size: u32,
        in_qualifier_data: *const c_void,
        io_data_size: *mut u32,
        out_data: *mut c_void,
    ) -> OSStatus;

    fn AudioObjectSetPropertyData(
        in_object_id: AudioObjectID,
        in_address: *const AudioObjectPropertyAddress,
        in_qualifier_data_size: u32,
        in_qualifier_data: *const c_void,
        in_data_size: u32,
        in_data: *const c_void,
    ) -> OSStatus;

    fn AudioObjectHasProperty(
        in_object_id: AudioObjectID,
        in_address: *const AudioObjectPropertyAddress,
    ) -> u8;

    fn AudioObjectIsPropertySettable(
        in_object_id: AudioObjectID,
        in_address: *const AudioObjectPropertyAddress,
        out_is_settable: *mut u8,
    ) -> OSStatus;
}

type SavedMuteMap = HashMap<AudioObjectID, Vec<(AudioObjectPropertyElement, u32)>>;

static SAVED_AUDIO_STATE: OnceLock<Mutex<SavedMuteMap>> = OnceLock::new();

fn saved_state_lock() -> &'static Mutex<SavedMuteMap> {
    SAVED_AUDIO_STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn default_output_device() -> Result<AudioObjectID, String> {
    let addr = AudioObjectPropertyAddress {
        m_selector: K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };
    let mut device_id: AudioObjectID = 0;
    let mut size = std::mem::size_of::<AudioObjectID>() as u32;
    // SAFETY: `addr`, `size`, `device_id` 均为栈上有效指针且字节大小匹配 `AudioObjectID`。
    let status = unsafe {
        AudioObjectGetPropertyData(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            (&mut device_id as *mut AudioObjectID).cast::<c_void>(),
        )
    };
    if status != 0 || device_id == 0 {
        return Err(format!("获取默认音频输出设备失败 (OSStatus={status})"));
    }
    Ok(device_id)
}

fn settable_mute_elements(device_id: AudioObjectID) -> Vec<AudioObjectPropertyElement> {
    let mut found = Vec::new();
    for elem in 0u32..=8 {
        let addr = AudioObjectPropertyAddress {
            m_selector: K_AUDIO_DEVICE_PROPERTY_MUTE,
            m_scope: K_AUDIO_DEVICE_PROPERTY_SCOPE_OUTPUT,
            m_element: elem,
        };
        // SAFETY: `addr` 指向有效的 CoreAudio 属性地址。
        let has = unsafe { AudioObjectHasProperty(device_id, &addr) } != 0;
        if !has {
            continue;
        }
        let mut settable: u8 = 0;
        // SAFETY: `addr` 与 `settable` 为有效指针。
        let st = unsafe { AudioObjectIsPropertySettable(device_id, &addr, &mut settable) };
        if st == 0 && settable != 0 {
            found.push(elem);
            if elem == 0 {
                // 主声道支持静音控制时优先只操作主声道。
                break;
            }
        }
    }
    found
}

fn read_mute(device_id: AudioObjectID, element: AudioObjectPropertyElement) -> Option<u32> {
    let addr = AudioObjectPropertyAddress {
        m_selector: K_AUDIO_DEVICE_PROPERTY_MUTE,
        m_scope: K_AUDIO_DEVICE_PROPERTY_SCOPE_OUTPUT,
        m_element: element,
    };
    let mut val: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `addr`, `size`, `val` 均为有效指针且尺寸匹配 u32 (`kAudioDevicePropertyMute`)。
    let st = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            (&mut val as *mut u32).cast::<c_void>(),
        )
    };
    (st == 0).then_some(val)
}

fn write_mute(
    device_id: AudioObjectID,
    element: AudioObjectPropertyElement,
    val: u32,
) -> Result<(), String> {
    let addr = AudioObjectPropertyAddress {
        m_selector: K_AUDIO_DEVICE_PROPERTY_MUTE,
        m_scope: K_AUDIO_DEVICE_PROPERTY_SCOPE_OUTPUT,
        m_element: element,
    };
    // SAFETY: `addr` 与 `val` 均为栈上有效指针，长度精确为 4 字节。
    let st = unsafe {
        AudioObjectSetPropertyData(
            device_id,
            &addr,
            0,
            std::ptr::null(),
            std::mem::size_of::<u32>() as u32,
            (&val as *const u32).cast::<c_void>(),
        )
    };
    if st != 0 {
        return Err(format!(
            "设置系统扬声器静音失败 (elem={element}, OSStatus={st})"
        ));
    }
    Ok(())
}

/// 设置当前默认输出设备的系统静音状态；首次调用前自动保存原始状态供退出恢复。
pub fn set_host_system_muted(muted: bool) -> Result<bool, String> {
    let device_id = default_output_device()?;
    let elements = settable_mute_elements(device_id);
    if elements.is_empty() {
        return Err("当前默认音频输出设备不支持系统静音属性".to_string());
    }

    {
        let mut map = saved_state_lock().lock().unwrap_or_else(|e| e.into_inner());
        map.entry(device_id).or_insert_with(|| {
            let mut snap = Vec::with_capacity(elements.len());
            for &el in &elements {
                if let Some(orig) = read_mute(device_id, el) {
                    snap.push((el, orig));
                }
            }
            snap
        });
    }

    let target: u32 = if muted { 1 } else { 0 };
    for &el in &elements {
        write_mute(device_id, el, target)?;
    }
    Ok(muted)
}

/// 恢复会话前记录的全部输出设备静音状态（幂等；兼容会话中切换默认输出设备）。
pub fn restore_host_system_audio() {
    let saved = {
        let mut map = saved_state_lock().lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *map)
    };
    for (device_id, elements) in saved {
        for (el, orig) in elements {
            let _ = write_mute(device_id, el, orig);
        }
    }
}

/// 清除已保存的原始静音快照而不改动当前硬件静音状态（用于 `restore_on_exit == false` 结束会话，防止污染下次会话）。
pub fn clear_host_audio_snapshot() {
    saved_state_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

//! macOS 原生系统账号密码校验（OpenDirectory）与登录钥匙串凭据存储（Security）。
//!
//! 职责严格收口在本模块：
//! 1. 仅校验被控端 macOS 登录用户名密码（`ODNodeCreateWithNodeType` → `ODNodeCopyRecord` →
//!    `ODRecordVerifyPassword`），**绝不**解锁物理屏幕、不调用 `/usr/bin/dscl` 命令行（防止密码进 argv）。
//! 2. 控制端可选记忆设备系统密码，仅存于本机登录钥匙串（`kSecAttrAccessibleWhenUnlockedThisDeviceOnly`），
//!    绝不写入明文 JSON 或上传服务端。
//! 3. 全部 `unsafe` 块均附 `// SAFETY:` 注释，并由父模块 `rc_capture_macos` 向外暴露纯安全包装。

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::autoreleasepool;
use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFBoolean, CFDictionary,
    CFRetained, CFString,
};

const K_OD_NODE_TYPE_LOCAL_NODES: u32 = 0x2200;
const KEYCHAIN_SERVICE: &str = "com.rabbitcontrol.terminal-unlock";
const MAX_PASSWORD_BYTES: usize = 512;
const MAX_USERNAME_BYTES: usize = 128;
const MAX_ACCOUNT_BYTES: usize = 256;

const ERR_SEC_SUCCESS: i32 = 0;
const ERR_SEC_DUPLICATE_ITEM: i32 = -25299;
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

mod raw {
    use super::c_void;
    use objc2_core_foundation::{CFDictionary, CFString};

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        pub fn CGSessionCopyCurrentDictionary() -> *const CFDictionary;
    }

    #[link(name = "Foundation", kind = "framework")]
    extern "C" {
        pub fn NSUserName() -> *const CFString;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub fn CFRelease(cf: *const c_void);
        pub fn CFGetTypeID(cf: *const c_void) -> usize;
        pub fn CFStringGetTypeID() -> usize;
        pub fn CFDataCreate(
            allocator: *const c_void,
            bytes: *const u8,
            length: isize,
        ) -> *mut c_void;
        pub fn CFDataGetTypeID() -> usize;
        pub fn CFDataGetLength(the_data: *const c_void) -> isize;
        pub fn CFDataGetBytePtr(the_data: *const c_void) -> *const u8;
    }

    #[link(name = "OpenDirectory", kind = "framework")]
    extern "C" {
        pub static kODSessionDefault: *const c_void;
        pub static kODRecordTypeUsers: Option<&'static CFString>;

        pub fn ODNodeCreateWithNodeType(
            allocator: *const c_void,
            session: *const c_void,
            node_type: u32,
            error: *mut *mut c_void,
        ) -> *mut c_void;

        pub fn ODNodeCopyRecord(
            node: *const c_void,
            record_type: *const CFString,
            record_name: *const CFString,
            attributes: *const c_void,
            error: *mut *mut c_void,
        ) -> *mut c_void;

        pub fn ODRecordVerifyPassword(
            record: *const c_void,
            password: *const CFString,
            error: *mut *mut c_void,
        ) -> bool;
    }

    #[link(name = "Security", kind = "framework")]
    extern "C" {
        pub static kSecClass: Option<&'static CFString>;
        pub static kSecClassGenericPassword: Option<&'static CFString>;
        pub static kSecAttrService: Option<&'static CFString>;
        pub static kSecAttrAccount: Option<&'static CFString>;
        pub static kSecAttrAccessible: Option<&'static CFString>;
        pub static kSecAttrAccessibleWhenUnlockedThisDeviceOnly: Option<&'static CFString>;
        pub static kSecValueData: Option<&'static CFString>;
        pub static kSecReturnData: Option<&'static CFString>;
        pub static kSecReturnAttributes: Option<&'static CFString>;
        pub static kSecMatchLimit: Option<&'static CFString>;
        pub static kSecMatchLimitOne: Option<&'static CFString>;

        pub fn SecItemAdd(attributes: *const CFDictionary, result: *mut *mut c_void) -> i32;
        pub fn SecItemCopyMatching(query: *const CFDictionary, result: *mut *mut c_void) -> i32;
        pub fn SecItemUpdate(
            query: *const CFDictionary,
            attributes_to_update: *const CFDictionary,
        ) -> i32;
        pub fn SecItemDelete(query: *const CFDictionary) -> i32;
    }

    extern "C" {
        pub fn geteuid() -> u32;
    }
}

/// +1 所有权 CF 对象 RAII 包装，确保任何路径退出都能配对 `CFRelease`。
struct CfOwned(NonNull<c_void>);

impl CfOwned {
    fn from_raw(ptr: *mut c_void) -> Option<Self> {
        NonNull::new(ptr).map(Self)
    }

    fn as_ptr(&self) -> *mut c_void {
        self.0.as_ptr()
    }
}

impl Drop for CfOwned {
    fn drop(&mut self) {
        // SAFETY: `self.0` 遵循 Create/Copy Rule 拥有 +1 引用，此处配对 `CFRelease`。
        unsafe { raw::CFRelease(self.0.as_ptr().cast_const()) };
    }
}

/// 过滤非法或系统伪账号用户名（如 `root` / `_windowserver` / `loginwindow`）。
pub(crate) fn filter_valid_login_username(raw_name: &str) -> Option<String> {
    let trimmed = raw_name.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_USERNAME_BYTES {
        return None;
    }
    if trimmed
        .chars()
        .any(|c| c == '\0' || c.is_control() || c.is_whitespace())
    {
        return None;
    }
    if matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "root" | "_windowserver" | "loginwindow"
    ) {
        return None;
    }
    Some(trimmed.to_string())
}

/// 从当前 CGSession 会话字典提取已登录桌面用户名。
fn cgsession_username() -> Option<String> {
    // SAFETY: 无参纯查询函数，遵循 Copy Rule（非空返回 +1 引用）。
    let raw_dict = unsafe { raw::CGSessionCopyCurrentDictionary() };
    let dict_ptr = NonNull::new(raw_dict.cast_mut())?;
    // SAFETY: `raw_dict` 为有效 +1 `CFDictionary`，交由 `CFRetained` 自动释放。
    let dict: CFRetained<CFDictionary> = unsafe { CFRetained::from_raw(dict_ptr) };

    let key = CFString::from_str("kCGSSessionUserNameKey");
    let key_ptr: *const c_void = CFRetained::as_ptr(&key).as_ptr().cast();
    // SAFETY: `dict` 与 `key_ptr` 均有效，返回值由 `dict` 持有。
    let raw_val = unsafe { dict.value(key_ptr) };
    let val_ptr = NonNull::new(raw_val.cast_mut())?;
    // SAFETY: 非空字典值是合法 CF 对象且存活期受 `dict` 保护；先校验 `CFStringGetTypeID()`。
    let is_str =
        unsafe { raw::CFGetTypeID(val_ptr.as_ptr().cast_const()) == raw::CFStringGetTypeID() };
    if !is_str {
        return None;
    }
    // SAFETY: 已通过 `CFGetTypeID == CFStringGetTypeID()` 校验，可安全按 `&CFString` 读取。
    let cf_str: &CFString = unsafe { val_ptr.cast::<CFString>().as_ref() };
    filter_valid_login_username(&cf_str.to_string())
}

/// 获取当前 macOS 桌面会话用户名：
/// - 普通 Aqua 会话优先取 `NSUserName()`，回退 `CGSessionCopyCurrentDictionary`；
/// - `root` 进程（如 LoginWindow 阶段 LaunchAgent）仅采信 `CGSessionCopyCurrentDictionary`，
///   未登录用户前返回 `None`。
pub(crate) fn current_system_username() -> Option<String> {
    // SAFETY: POSIX 纯查询当前有效 UID。
    let euid = unsafe { raw::geteuid() };
    let session_user = cgsession_username();
    if euid == 0 {
        return session_user;
    }
    let ns_user = autoreleasepool(|_| -> Option<String> {
        // SAFETY: `NSUserName()` 为 Foundation 标准无参查询，返回 autoreleased `NSString*`（toll-free `CFString*`）。
        let raw_ptr = unsafe { raw::NSUserName() };
        let nn = NonNull::new(raw_ptr.cast_mut())?;
        // SAFETY: 在当前 `autoreleasepool` 作用域内有效，先校验 `CFStringGetTypeID()`。
        let is_str = unsafe {
            raw::CFGetTypeID(nn.as_ptr().cast_const().cast::<c_void>()) == raw::CFStringGetTypeID()
        };
        if !is_str {
            return None;
        }
        // SAFETY: 已校验为有效 `CFString`，借用寿命局限在 `autoreleasepool` 闭包内。
        let cf_str: &CFString = unsafe { nn.cast::<CFString>().as_ref() };
        filter_valid_login_username(&cf_str.to_string())
    });
    ns_user.or(session_user)
}

/// 使用原生 `OpenDirectory` 校验指定（或当前登录）macOS 用户密码，绝不解锁物理屏幕。
pub(crate) fn verify_system_password(
    username: Option<&str>,
    password: &str,
) -> Result<bool, String> {
    if password.is_empty() || password.len() > MAX_PASSWORD_BYTES {
        return Ok(false);
    }

    // SAFETY: POSIX 纯查询当前有效 UID。
    let euid = unsafe { raw::geteuid() };
    let target_user = if euid != 0 {
        let current = current_system_username()
            .ok_or_else(|| "无法读取当前登录的 macOS 用户名".to_string())?;
        if let Some(requested) = username {
            let trimmed = requested.trim();
            if !trimmed.is_empty() && trimmed != current {
                // 非 root 进程仅允许校验当前登录账号本身，防止枚举系统内其他用户。
                return Ok(false);
            }
        }
        current
    } else if let Some(current) = cgsession_username() {
        if let Some(requested) = username {
            let trimmed = requested.trim();
            if !trimmed.is_empty() && trimmed != current {
                return Ok(false);
            }
        }
        current
    } else {
        username
            .and_then(filter_valid_login_username)
            .ok_or_else(|| "当前处于系统登录界面，请输入被控端 macOS 登录用户名".to_string())?
    };

    let mut od_err: *mut c_void = std::ptr::null_mut();
    // SAFETY: `kODSessionDefault` 与 `K_OD_NODE_TYPE_LOCAL_NODES` 均为合法常量；`&mut od_err` 为有效出参。
    let node_ptr = unsafe {
        raw::ODNodeCreateWithNodeType(
            std::ptr::null(),
            raw::kODSessionDefault,
            K_OD_NODE_TYPE_LOCAL_NODES,
            &mut od_err,
        )
    };
    let _node_err_guard = CfOwned::from_raw(od_err);
    let node = CfOwned::from_raw(node_ptr)
        .ok_or_else(|| "无法连接本机 OpenDirectory 本地目录节点".to_string())?;

    // SAFETY: 读取 OpenDirectory 导出的只读 `CFStringRef` 常量。
    let users_record_type = unsafe { raw::kODRecordTypeUsers }
        .ok_or_else(|| "缺少 kODRecordTypeUsers 常量".to_string())?;
    let cf_user = CFString::from_str(&target_user);

    let mut rec_err: *mut c_void = std::ptr::null_mut();
    // SAFETY: `node`、`users_record_type`、`cf_user` 均有效，`&mut rec_err` 为有效出参。
    let rec_ptr = unsafe {
        raw::ODNodeCopyRecord(
            node.as_ptr().cast_const(),
            users_record_type,
            &*cf_user,
            std::ptr::null(),
            &mut rec_err,
        )
    };
    let _rec_err_guard = CfOwned::from_raw(rec_err);
    let Some(record) = CfOwned::from_raw(rec_ptr) else {
        return Ok(false);
    };

    let cf_pass = CFString::from_str(password);
    let mut verify_err: *mut c_void = std::ptr::null_mut();
    // SAFETY: `record` 与 `cf_pass` 均有效；仅做本地口令哈希比对，不解锁屏幕。
    let verified = unsafe {
        raw::ODRecordVerifyPassword(record.as_ptr().cast_const(), &*cf_pass, &mut verify_err)
    };
    let _verify_err_guard = CfOwned::from_raw(verify_err);
    Ok(verified)
}

/// 规范化钥匙串 account 标识（`device_key`），拒绝空串、超长或控制字符。
pub(crate) fn sanitize_keychain_account(device_key: &str) -> Option<String> {
    let trimmed = device_key.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_ACCOUNT_BYTES {
        return None;
    }
    if trimmed.chars().any(|c| c == '\0' || c.is_control()) {
        return None;
    }
    Some(trimmed.to_string())
}

/// 从等长键/值指针切片构造 CoreFoundation 字典。
///
/// # Safety
/// - `keys.len()` 必须等于 `vals.len()`；
/// - `keys` 与 `vals` 中的每个非空指针均须在本调用期间指向有效的 CoreFoundation 对象（由 `CFDictionary::new` 自动 retain）。
unsafe fn build_cf_dictionary(
    keys: &[*const c_void],
    vals: &[*const c_void],
) -> Option<CFRetained<CFDictionary>> {
    debug_assert_eq!(keys.len(), vals.len());
    // SAFETY: 调用方已保证 `keys`、`vals` 等长且全部为有效 CoreFoundation 对象指针。
    unsafe {
        CFDictionary::new(
            None,
            keys.as_ptr() as *mut *const c_void,
            vals.as_ptr() as *mut *const c_void,
            keys.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }
}

/// 将设备对应的被控端系统密码写入（或更新）本机登录钥匙串。
pub(crate) fn keychain_save_terminal_password(
    device_key: &str,
    password: &str,
) -> Result<(), String> {
    let account = sanitize_keychain_account(device_key)
        .ok_or_else(|| "设备标识无效，无法保存至钥匙串".to_string())?;
    if password.is_empty() || password.len() > MAX_PASSWORD_BYTES {
        return Err("密码长度不合法，无法保存至钥匙串".to_string());
    }

    // SAFETY: 读取 Security 框架导出的只读 `CFStringRef` 键常量。
    let (
        k_class,
        k_generic_pw,
        k_service,
        k_account,
        k_accessible,
        k_when_unlocked_this_dev,
        k_value_data,
    ) = unsafe {
        (
            raw::kSecClass.ok_or("缺少 kSecClass")?,
            raw::kSecClassGenericPassword.ok_or("缺少 kSecClassGenericPassword")?,
            raw::kSecAttrService.ok_or("缺少 kSecAttrService")?,
            raw::kSecAttrAccount.ok_or("缺少 kSecAttrAccount")?,
            raw::kSecAttrAccessible.ok_or("缺少 kSecAttrAccessible")?,
            raw::kSecAttrAccessibleWhenUnlockedThisDeviceOnly
                .ok_or("缺少 kSecAttrAccessibleWhenUnlockedThisDeviceOnly")?,
            raw::kSecValueData.ok_or("缺少 kSecValueData")?,
        )
    };

    let cf_service = CFString::from_str(KEYCHAIN_SERVICE);
    let cf_account = CFString::from_str(&account);
    let pw_bytes = password.as_bytes();
    // SAFETY: `pw_bytes` 在函数内有效且长度 ≤ 512，`CFDataCreate` 会内部拷贝字节。
    let cf_data = CfOwned::from_raw(unsafe {
        raw::CFDataCreate(std::ptr::null(), pw_bytes.as_ptr(), pw_bytes.len() as isize)
    })
    .ok_or_else(|| "构造钥匙串 CFData 失败".to_string())?;

    let add_keys = [
        k_class as *const CFString as *const c_void,
        k_service as *const CFString as *const c_void,
        k_account as *const CFString as *const c_void,
        k_accessible as *const CFString as *const c_void,
        k_value_data as *const CFString as *const c_void,
    ];
    let add_vals = [
        k_generic_pw as *const CFString as *const c_void,
        &*cf_service as *const CFString as *const c_void,
        &*cf_account as *const CFString as *const c_void,
        k_when_unlocked_this_dev as *const CFString as *const c_void,
        cf_data.as_ptr().cast_const(),
    ];
    // SAFETY: `add_keys` 与 `add_vals` 等长且均为有效的 `CFString`/`CFData` 引用。
    let add_dict =
        unsafe { build_cf_dictionary(&add_keys, &add_vals) }.ok_or("构造钥匙串属性字典失败")?;

    // SAFETY: `add_dict` 为有效 `CFDictionary`，不需要返回项指针故传 `null_mut()`。
    let status = unsafe { raw::SecItemAdd(&*add_dict, std::ptr::null_mut()) };
    if status == ERR_SEC_SUCCESS {
        return Ok(());
    }
    if status == ERR_SEC_DUPLICATE_ITEM {
        let q_keys = [
            k_class as *const CFString as *const c_void,
            k_service as *const CFString as *const c_void,
            k_account as *const CFString as *const c_void,
        ];
        let q_vals = [
            k_generic_pw as *const CFString as *const c_void,
            &*cf_service as *const CFString as *const c_void,
            &*cf_account as *const CFString as *const c_void,
        ];
        // SAFETY: `q_keys` 与 `q_vals` 等长且均为有效 CoreFoundation 字符串指针。
        let q_dict =
            unsafe { build_cf_dictionary(&q_keys, &q_vals) }.ok_or("构造钥匙串查询字典失败")?;

        let u_keys = [
            k_value_data as *const CFString as *const c_void,
            k_accessible as *const CFString as *const c_void,
        ];
        let u_vals = [
            cf_data.as_ptr().cast_const(),
            k_when_unlocked_this_dev as *const CFString as *const c_void,
        ];
        // SAFETY: `u_keys` 与 `u_vals` 等长且均为有效 `CFString`/`CFData` 指针。
        let u_dict =
            unsafe { build_cf_dictionary(&u_keys, &u_vals) }.ok_or("构造钥匙串更新字典失败")?;

        // SAFETY: `q_dict` 与 `u_dict` 均为有效 `CFDictionary`。
        let upd_status = unsafe { raw::SecItemUpdate(&*q_dict, &*u_dict) };
        if upd_status == ERR_SEC_SUCCESS {
            return Ok(());
        }
        return Err(format!("钥匙串更新失败 (OSStatus {upd_status})"));
    }
    Err(format!("钥匙串写入失败 (OSStatus {status})"))
}

/// 从本机登录钥匙串读取设备对应的被控端系统密码。
pub(crate) fn keychain_load_terminal_password(device_key: &str) -> Result<Option<String>, String> {
    let Some(account) = sanitize_keychain_account(device_key) else {
        return Ok(None);
    };

    // SAFETY: 读取 Security 框架导出的只读 `CFStringRef` 键常量。
    let (k_class, k_generic_pw, k_service, k_account, k_return_data, k_match_limit, k_limit_one) = unsafe {
        (
            raw::kSecClass.ok_or("缺少 kSecClass")?,
            raw::kSecClassGenericPassword.ok_or("缺少 kSecClassGenericPassword")?,
            raw::kSecAttrService.ok_or("缺少 kSecAttrService")?,
            raw::kSecAttrAccount.ok_or("缺少 kSecAttrAccount")?,
            raw::kSecReturnData.ok_or("缺少 kSecReturnData")?,
            raw::kSecMatchLimit.ok_or("缺少 kSecMatchLimit")?,
            raw::kSecMatchLimitOne.ok_or("缺少 kSecMatchLimitOne")?,
        )
    };

    let cf_service = CFString::from_str(KEYCHAIN_SERVICE);
    let cf_account = CFString::from_str(&account);
    let cf_true = CFBoolean::new(true);

    let keys = [
        k_class as *const CFString as *const c_void,
        k_service as *const CFString as *const c_void,
        k_account as *const CFString as *const c_void,
        k_return_data as *const CFString as *const c_void,
        k_match_limit as *const CFString as *const c_void,
    ];
    let vals = [
        k_generic_pw as *const CFString as *const c_void,
        &*cf_service as *const CFString as *const c_void,
        &*cf_account as *const CFString as *const c_void,
        cf_true as *const CFBoolean as *const c_void,
        k_limit_one as *const CFString as *const c_void,
    ];
    // SAFETY: `keys` 与 `vals` 等长且均为有效的 `CFString`/`CFBoolean` 对象指针。
    let query = unsafe { build_cf_dictionary(&keys, &vals) }.ok_or("构造钥匙串查询字典失败")?;

    let mut result: *mut c_void = std::ptr::null_mut();
    // SAFETY: `query` 为有效 `CFDictionary`，`&mut result` 接收 Copy Rule +1 引用（交由 `CfOwned` 释放）。
    let status = unsafe { raw::SecItemCopyMatching(&*query, &mut result) };
    let result_guard = CfOwned::from_raw(result);

    if status == ERR_SEC_ITEM_NOT_FOUND {
        return Ok(None);
    }
    if status != ERR_SEC_SUCCESS {
        return Err(format!("钥匙串读取失败 (OSStatus {status})"));
    }
    let Some(data) = result_guard else {
        return Ok(None);
    };
    // SAFETY: 先校验返回 CF 对象确为 `CFData`。
    let is_data = unsafe { raw::CFGetTypeID(data.as_ptr().cast_const()) == raw::CFDataGetTypeID() };
    if !is_data {
        return Ok(None);
    }
    // SAFETY: `data` 已校验为有效 `CFDataRef`。
    let len = unsafe { raw::CFDataGetLength(data.as_ptr().cast_const()) };
    if len <= 0 || len as usize > MAX_PASSWORD_BYTES {
        return Ok(None);
    }
    // SAFETY: `len > 0`，`CFDataGetBytePtr` 返回至少 `len` 字节可读内存。
    let ptr = unsafe { raw::CFDataGetBytePtr(data.as_ptr().cast_const()) };
    if ptr.is_null() {
        return Ok(None);
    }
    // SAFETY: `ptr` 非空且在 `data` 存活期内有 `len` 字节可读。
    let slice = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    let pw = String::from_utf8(slice.to_vec())
        .map_err(|_| "钥匙串中的密码不是有效 UTF-8".to_string())?;
    if pw.is_empty() {
        return Ok(None);
    }
    Ok(Some(pw))
}

/// 从本机登录钥匙串删除设备对应的被控端系统密码（不存在视为成功）。
pub(crate) fn keychain_delete_terminal_password(device_key: &str) -> Result<(), String> {
    let Some(account) = sanitize_keychain_account(device_key) else {
        return Ok(());
    };

    // SAFETY: 读取 Security 框架导出的只读 `CFStringRef` 键常量。
    let (k_class, k_generic_pw, k_service, k_account) = unsafe {
        (
            raw::kSecClass.ok_or("缺少 kSecClass")?,
            raw::kSecClassGenericPassword.ok_or("缺少 kSecClassGenericPassword")?,
            raw::kSecAttrService.ok_or("缺少 kSecAttrService")?,
            raw::kSecAttrAccount.ok_or("缺少 kSecAttrAccount")?,
        )
    };

    let cf_service = CFString::from_str(KEYCHAIN_SERVICE);
    let cf_account = CFString::from_str(&account);
    let keys = [
        k_class as *const CFString as *const c_void,
        k_service as *const CFString as *const c_void,
        k_account as *const CFString as *const c_void,
    ];
    let vals = [
        k_generic_pw as *const CFString as *const c_void,
        &*cf_service as *const CFString as *const c_void,
        &*cf_account as *const CFString as *const c_void,
    ];
    // SAFETY: `keys` 与 `vals` 等长且均为有效 `CFString` 对象指针。
    let query = unsafe { build_cf_dictionary(&keys, &vals) }.ok_or("构造钥匙串删除字典失败")?;

    // SAFETY: `query` 为有效 `CFDictionary`。
    let status = unsafe { raw::SecItemDelete(&*query) };
    if status == ERR_SEC_SUCCESS || status == ERR_SEC_ITEM_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("钥匙串删除失败 (OSStatus {status})"))
    }
}

/// 检查本机登录钥匙串是否存有该设备的已记忆密码（仅查属性，绝不读取密文或触发授权弹窗）。
pub(crate) fn keychain_has_terminal_password(device_key: &str) -> bool {
    let Some(account) = sanitize_keychain_account(device_key) else {
        return false;
    };

    // SAFETY: 读取 Security 框架导出的只读 `CFStringRef` 键常量。
    let (
        Some(k_class),
        Some(k_generic_pw),
        Some(k_service),
        Some(k_account),
        Some(k_return_attrs),
        Some(k_match_limit),
        Some(k_limit_one),
    ) = (unsafe {
        (
            raw::kSecClass,
            raw::kSecClassGenericPassword,
            raw::kSecAttrService,
            raw::kSecAttrAccount,
            raw::kSecReturnAttributes,
            raw::kSecMatchLimit,
            raw::kSecMatchLimitOne,
        )
    })
    else {
        return false;
    };

    let cf_service = CFString::from_str(KEYCHAIN_SERVICE);
    let cf_account = CFString::from_str(&account);
    let cf_true = CFBoolean::new(true);

    let keys = [
        k_class as *const CFString as *const c_void,
        k_service as *const CFString as *const c_void,
        k_account as *const CFString as *const c_void,
        k_return_attrs as *const CFString as *const c_void,
        k_match_limit as *const CFString as *const c_void,
    ];
    let vals = [
        k_generic_pw as *const CFString as *const c_void,
        &*cf_service as *const CFString as *const c_void,
        &*cf_account as *const CFString as *const c_void,
        cf_true as *const CFBoolean as *const c_void,
        k_limit_one as *const CFString as *const c_void,
    ];
    // SAFETY: `keys` 与 `vals` 等长且均为有效的 `CFString`/`CFBoolean` 对象指针。
    let Some(query) = (unsafe { build_cf_dictionary(&keys, &vals) }) else {
        return false;
    };

    let mut result: *mut c_void = std::ptr::null_mut();
    // SAFETY: 仅请求属性（`kSecReturnAttributes`），不读取 `kSecReturnData`，结果交由 `CfOwned` 释放。
    let status = unsafe { raw::SecItemCopyMatching(&*query, &mut result) };
    let _result_guard = CfOwned::from_raw(result);
    status == ERR_SEC_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_valid_login_username_rejects_pseudo_and_control_names() {
        assert_eq!(filter_valid_login_username("yii").as_deref(), Some("yii"));
        assert_eq!(
            filter_valid_login_username("  alice.dev  ").as_deref(),
            Some("alice.dev")
        );
        assert!(filter_valid_login_username("").is_none());
        assert!(filter_valid_login_username("   ").is_none());
        assert!(filter_valid_login_username("root").is_none());
        assert!(filter_valid_login_username("ROOT").is_none());
        assert!(filter_valid_login_username("_windowserver").is_none());
        assert!(filter_valid_login_username("loginwindow").is_none());
        assert!(filter_valid_login_username("bad\0user").is_none());
        assert!(filter_valid_login_username("bad user").is_none());
        assert!(filter_valid_login_username(&"a".repeat(129)).is_none());
    }

    #[test]
    fn sanitize_keychain_account_validates_bounds_and_control_chars() {
        assert_eq!(
            sanitize_keychain_account("  dev-123456789 ").as_deref(),
            Some("dev-123456789")
        );
        assert!(sanitize_keychain_account("").is_none());
        assert!(sanitize_keychain_account("   ").is_none());
        assert!(sanitize_keychain_account("bad\nkey").is_none());
        assert!(sanitize_keychain_account("bad\0key").is_none());
        assert!(sanitize_keychain_account(&"x".repeat(257)).is_none());
    }

    #[test]
    fn verify_system_password_rejects_empty_and_oversized_without_error() {
        assert_eq!(verify_system_password(None, ""), Ok(false));
        assert_eq!(
            verify_system_password(None, &"p".repeat(MAX_PASSWORD_BYTES + 1)),
            Ok(false)
        );
    }

    #[test]
    fn current_system_username_is_non_pseudo_when_logged_in() {
        if let Some(name) = current_system_username() {
            assert!(!name.is_empty());
            assert_ne!(name.to_ascii_lowercase(), "root");
            assert_ne!(name.to_ascii_lowercase(), "_windowserver");
            assert_ne!(name.to_ascii_lowercase(), "loginwindow");
        }
    }
}

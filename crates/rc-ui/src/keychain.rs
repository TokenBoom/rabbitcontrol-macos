//! W3 远程终端系统密码的登录钥匙串串行读写工作器。
//!
//! - 通过单工作线程串行化 `Save` / `Delete` / `Has` / `Load` 请求，保证同一轮事件中先派发的钥匙串写删
//!   严格早于紧随其后的前端锁屏状态查询执行，消除竞态覆盖与主线程阻塞；
//! - 使用 `#[cfg(target_os = "macos")]` 隔离 `rc_capture_macos::keychain_*` 调用，保持零 `unsafe` 与跨平台可编译性。

#![deny(unsafe_code)]

use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;

use zeroize::Zeroizing;

/// 需要异步持久化到本机登录钥匙串的终端系统密码同步操作。
#[derive(Debug, Clone)]
pub enum KeychainSyncOp {
    /// 写入或更新设备对应的系统密码（包含待记忆明文）。
    Save(String, Zeroizing<String>),
    /// 删除设备对应的已记忆系统密码。
    Delete(String),
}

impl KeychainSyncOp {
    /// 以“同批次最新状态覆盖旧请求”的语义合入新的同步操作。
    pub fn set_latest(slot: &mut Option<Self>, next: Self) {
        *slot = Some(next);
    }
}

trait KeychainOps {
    fn has_password(&mut self, device_key: &str) -> bool;
    fn load_password(&mut self, device_key: &str) -> Result<Option<Zeroizing<String>>, String>;
    fn save_password(&mut self, device_key: &str, password: &str) -> Result<(), String>;
    fn delete_password(&mut self, device_key: &str) -> Result<(), String>;
}

struct OsKeychainOps;

impl KeychainOps for OsKeychainOps {
    fn has_password(&mut self, device_key: &str) -> bool {
        os_keychain_has_terminal_password(device_key)
    }

    fn load_password(&mut self, device_key: &str) -> Result<Option<Zeroizing<String>>, String> {
        os_keychain_load_terminal_password(device_key)
    }

    fn save_password(&mut self, device_key: &str, password: &str) -> Result<(), String> {
        os_keychain_save_terminal_password(device_key, password)
    }

    fn delete_password(&mut self, device_key: &str) -> Result<(), String> {
        os_keychain_delete_terminal_password(device_key)
    }
}

#[cfg(target_os = "macos")]
fn os_keychain_has_terminal_password(device_key: &str) -> bool {
    rc_capture_macos::keychain_has_terminal_password(device_key)
}

#[cfg(not(target_os = "macos"))]
fn os_keychain_has_terminal_password(_device_key: &str) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn os_keychain_load_terminal_password(
    device_key: &str,
) -> Result<Option<Zeroizing<String>>, String> {
    rc_capture_macos::keychain_load_terminal_password(device_key).map(|opt| opt.map(Zeroizing::new))
}

#[cfg(not(target_os = "macos"))]
fn os_keychain_load_terminal_password(
    _device_key: &str,
) -> Result<Option<Zeroizing<String>>, String> {
    Ok(None)
}

#[cfg(target_os = "macos")]
fn os_keychain_save_terminal_password(device_key: &str, password: &str) -> Result<(), String> {
    rc_capture_macos::keychain_save_terminal_password(device_key, password)
}

#[cfg(not(target_os = "macos"))]
fn os_keychain_save_terminal_password(_device_key: &str, _password: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn os_keychain_delete_terminal_password(device_key: &str) -> Result<(), String> {
    rc_capture_macos::keychain_delete_terminal_password(device_key)
}

#[cfg(not(target_os = "macos"))]
fn os_keychain_delete_terminal_password(_device_key: &str) -> Result<(), String> {
    Ok(())
}

enum KeychainReq {
    Sync(KeychainSyncOp),
    Has {
        device_key: String,
        reply: SyncSender<bool>,
    },
    Load {
        device_key: String,
        reply: SyncSender<Result<Option<Zeroizing<String>>, String>>,
    },
    Delete {
        device_key: String,
        reply: SyncSender<Result<(), String>>,
    },
}

struct KeychainWorkerInner {
    tx: Mutex<Option<SyncSender<KeychainReq>>>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Drop for KeychainWorkerInner {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.tx.lock() {
            let _ = guard.take();
        }
        if let Ok(mut guard) = self.thread.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

/// 串行化本机登录钥匙串写/删/查的后台工作器句柄（可克隆，析构自动回收线程）。
#[derive(Clone)]
pub struct KeychainWorker {
    inner: Arc<KeychainWorkerInner>,
}

impl Default for KeychainWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl KeychainWorker {
    /// 启动连接本机钥匙串的单工作线程。
    pub fn new() -> Self {
        Self::with_ops(OsKeychainOps)
    }

    fn with_ops<O: KeychainOps + Send + 'static>(mut ops: O) -> Self {
        let (tx, rx): (SyncSender<KeychainReq>, Receiver<KeychainReq>) = mpsc::sync_channel(64);
        let handle = thread::Builder::new()
            .name("rc-keychain-worker".to_string())
            .spawn(move || {
                while let Ok(req) = rx.recv() {
                    match req {
                        KeychainReq::Sync(KeychainSyncOp::Save(dk, pw)) => {
                            let _ = ops.save_password(&dk, &pw);
                        }
                        KeychainReq::Sync(KeychainSyncOp::Delete(dk)) => {
                            let _ = ops.delete_password(&dk);
                        }
                        KeychainReq::Has { device_key, reply } => {
                            let _ = reply.send(ops.has_password(&device_key));
                        }
                        KeychainReq::Load { device_key, reply } => {
                            let _ = reply.send(ops.load_password(&device_key));
                        }
                        KeychainReq::Delete { device_key, reply } => {
                            let _ = reply.send(ops.delete_password(&device_key));
                        }
                    }
                }
            })
            .ok();
        Self {
            inner: Arc::new(KeychainWorkerInner {
                tx: Mutex::new(Some(tx)),
                thread: Mutex::new(handle),
            }),
        }
    }

    fn sender(&self) -> Option<SyncSender<KeychainReq>> {
        self.inner
            .tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 非阻塞入队一条钥匙串同步写/删请求（严格 FIFO 执行，绝不阻塞轮询线程）。
    pub fn enqueue_sync(&self, op: KeychainSyncOp) {
        if let Some(tx) = self.sender() {
            let _ = tx.try_send(KeychainReq::Sync(op));
        }
    }

    /// 在工作线程上按 FIFO 顺序查询该设备是否已记住系统密码。
    pub fn has_password(&self, device_key: String) -> bool {
        let Some(tx) = self.sender() else {
            return false;
        };
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if tx
            .send(KeychainReq::Has {
                device_key,
                reply: reply_tx,
            })
            .is_err()
        {
            return false;
        }
        reply_rx.recv().unwrap_or(false)
    }

    /// 在工作线程上按 FIFO 顺序读取该设备的已记住系统密码。
    pub fn load_password(&self, device_key: String) -> Result<Option<Zeroizing<String>>, String> {
        let Some(tx) = self.sender() else {
            return Ok(None);
        };
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        tx.send(KeychainReq::Load {
            device_key,
            reply: reply_tx,
        })
        .map_err(|_| "钥匙串工作线程已停止".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "钥匙串工作线程响应中断".to_string())?
    }

    /// 在工作线程上按 FIFO 顺序删除该设备的已记住系统密码。
    pub fn delete_password(&self, device_key: String) -> Result<(), String> {
        let Some(tx) = self.sender() else {
            return Ok(());
        };
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        tx.send(KeychainReq::Delete {
            device_key,
            reply: reply_tx,
        })
        .map_err(|_| "钥匙串工作线程已停止".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "钥匙串工作线程响应中断".to_string())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MemoryKeychain {
        store: HashMap<String, String>,
    }

    impl KeychainOps for MemoryKeychain {
        fn has_password(&mut self, device_key: &str) -> bool {
            self.store.contains_key(device_key)
        }

        fn load_password(&mut self, device_key: &str) -> Result<Option<Zeroizing<String>>, String> {
            Ok(self.store.get(device_key).cloned().map(Zeroizing::new))
        }

        fn save_password(&mut self, device_key: &str, password: &str) -> Result<(), String> {
            self.store
                .insert(device_key.to_string(), password.to_string());
            Ok(())
        }

        fn delete_password(&mut self, device_key: &str) -> Result<(), String> {
            self.store.remove(device_key);
            Ok(())
        }
    }

    #[test]
    fn keychain_sync_op_set_latest_keeps_last_operation_and_worker_is_fifo() {
        let mut slot: Option<KeychainSyncOp> = None;
        KeychainSyncOp::set_latest(&mut slot, KeychainSyncOp::Delete("dev1".to_string()));
        KeychainSyncOp::set_latest(
            &mut slot,
            KeychainSyncOp::Save("dev1".to_string(), Zeroizing::new("pw-final".to_string())),
        );
        assert!(matches!(slot, Some(KeychainSyncOp::Save(ref dk, _)) if dk == "dev1"));

        let worker = KeychainWorker::with_ops(MemoryKeychain {
            store: HashMap::new(),
        });
        // 先删后写同轮入队，随后立即查询必须能读到最新写入结果（严格 FIFO）。
        worker.enqueue_sync(KeychainSyncOp::Delete("dev1".to_string()));
        worker.enqueue_sync(slot.take().unwrap());
        assert!(worker.has_password("dev1".to_string()));
        assert_eq!(
            worker
                .load_password("dev1".to_string())
                .unwrap()
                .as_deref()
                .map(String::as_str),
            Some("pw-final")
        );

        // 先写后删，则最终必须为已删除。
        worker.enqueue_sync(KeychainSyncOp::Save(
            "dev1".to_string(),
            Zeroizing::new("pw-temp".to_string()),
        ));
        worker.enqueue_sync(KeychainSyncOp::Delete("dev1".to_string()));
        assert!(!worker.has_password("dev1".to_string()));
        assert!(worker.load_password("dev1".to_string()).unwrap().is_none());

        worker
            .delete_password("dev1".to_string())
            .expect("delete idempotent");
    }
}

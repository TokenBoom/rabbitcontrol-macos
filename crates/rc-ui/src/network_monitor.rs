//! 本机网络路由与接口切换轻量监测器。
//!
//! 每 `1.5s` 通过零发包 UDP `connect` 路由表查询采集本机可用局域网地址与默认出站本地地址，
//! 排序去重生成 [`NetworkFingerprint`]；当检测到「有网→无网」、「无网→有网」或「IP/网卡/VPN 切换」
//! 时立即通过回调通知调用方触发信令快重连、局域网表刷新与活动会话快检。
//!
//! 全模块 `#![deny(unsafe_code)]` 与 `#![deny(missing_docs)]`。

#![deny(unsafe_code)]
#![deny(missing_docs)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// 默认网络指纹轮询周期（1.5 秒）。
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(1_500);

/// 外网路由探测目标集合（仅 UDP `connect` 查内核路由表，发送 0 个报文）。
const DEFAULT_EXTERNAL_PROBE_TARGETS: [SocketAddr; 3] = [
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 53)),
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 53)),
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(223, 5, 5, 5), 53)),
];

/// 纯 IPv6 网络兜底路由探测目标（仅当全部 IPv4 探测均无结果时使用，避免双栈隐私地址轮换误报）。
const DEFAULT_IPV6_FALLBACK_TARGET: SocketAddr = SocketAddr::V6(SocketAddrV6::new(
    Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111),
    53,
    0,
    0,
));

/// 本机网络指纹（排序去重后的本地出站/局域网地址集合）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkFingerprint {
    /// 排序去重后的地址字符串列表；为空表示当前无有效外网/局域网出站路由。
    pub addrs: Vec<String>,
}

impl NetworkFingerprint {
    /// 由任意地址字符串序列构建规范化指纹（去空白、排序、去重）。
    pub fn from_addrs<I, S>(addrs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut normalized: Vec<String> = addrs
            .into_iter()
            .map(Into::into)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        normalized.sort();
        normalized.dedup();
        Self { addrs: normalized }
    }

    /// 当前指纹是否具备有效网络连通性。
    pub fn has_connectivity(&self) -> bool {
        !self.addrs.is_empty()
    }
}

/// 本机网络路由/接口变更事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkChangeEvent {
    /// 变更前是否具备有效网络连通性。
    pub had_connectivity: bool,
    /// 变更后是否具备有效网络连通性。
    pub has_connectivity: bool,
    /// 变更后的最新本机网络指纹（排序去重后的地址字符串列表）。
    pub fingerprint: Vec<String>,
}

/// 比较前后网络指纹，仅在指纹发生实质变化时产出 [`NetworkChangeEvent`]。
pub fn diff_fingerprint(
    prev: &NetworkFingerprint,
    next: &NetworkFingerprint,
) -> Option<NetworkChangeEvent> {
    if prev == next {
        return None;
    }
    Some(NetworkChangeEvent {
        had_connectivity: prev.has_connectivity(),
        has_connectivity: next.has_connectivity(),
        fingerprint: next.addrs.clone(),
    })
}

fn is_usable_outbound_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => rc_client_core::lan_discovery::is_usable_lan_ipv4(v4),
        IpAddr::V6(v6) => !v6.is_unspecified() && !v6.is_loopback() && !v6.is_multicast(),
    }
}

/// 经 UDP `connect` 查询到达 `target` 的本机默认出站 IP（零发包）。
fn probe_outbound_local_ip(target: SocketAddr) -> Option<IpAddr> {
    let bind_addr: SocketAddr = match target {
        SocketAddr::V4(_) => SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
        SocketAddr::V6(_) => SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0)),
    };
    let sock = UdpSocket::bind(bind_addr).ok()?;
    sock.connect(target).ok()?;
    let ip = sock.local_addr().ok()?.ip().to_canonical();
    is_usable_outbound_ip(ip).then_some(ip)
}

/// 采集当前本机网络指纹（局域网候选 IPv4 + 外网默认出站本地地址，排序去重）。
pub fn capture_network_fingerprint() -> NetworkFingerprint {
    let mut addrs: Vec<String> =
        rc_client_core::lan_discovery::discover_local_lan_ipv4s(&DEFAULT_EXTERNAL_PROBE_TARGETS)
            .into_iter()
            .map(|ip| ip.to_string())
            .collect();

    for target in DEFAULT_EXTERNAL_PROBE_TARGETS {
        if let Some(ip) = probe_outbound_local_ip(target) {
            addrs.push(ip.to_string());
        }
    }

    if addrs.is_empty() {
        if let Some(ip) = probe_outbound_local_ip(DEFAULT_IPV6_FALLBACK_TARGET) {
            addrs.push(ip.to_string());
        }
    }

    NetworkFingerprint::from_addrs(addrs)
}

/// 后台网络切换监测线程句柄。
///
/// 通过 `Condvar` 休眠与唤醒，调用 [`Self::stop`] 或释放（`Drop`）时零延迟干净退出。
pub struct NetworkMonitorHandle {
    stop_signal: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl NetworkMonitorHandle {
    /// 以默认 `1.5s` 轮询周期启动本机网络切换监测线程。
    pub fn spawn<F>(on_change: F) -> Self
    where
        F: FnMut(NetworkChangeEvent) + Send + 'static,
    {
        Self::spawn_with_sampler(
            DEFAULT_POLL_INTERVAL,
            capture_network_fingerprint,
            on_change,
        )
    }

    /// 使用自定义轮询间隔与指纹采集器启动监测线程（便于离线确定性测试）。
    pub fn spawn_with_sampler<S, F>(
        poll_interval: Duration,
        mut sampler: S,
        mut on_change: F,
    ) -> Self
    where
        S: FnMut() -> NetworkFingerprint + Send + 'static,
        F: FnMut(NetworkChangeEvent) + Send + 'static,
    {
        let interval = poll_interval.max(Duration::from_millis(1));
        let stop_signal = Arc::new((Mutex::new(false), Condvar::new()));
        let bg_stop = Arc::clone(&stop_signal);

        let thread = thread::Builder::new()
            .name("rc-ui-net-monitor".to_string())
            .spawn(move || {
                let mut last = sampler();
                loop {
                    let should_stop = {
                        let mut guard = bg_stop.0.lock().unwrap_or_else(|e| e.into_inner());
                        let deadline = Instant::now() + interval;
                        while !*guard {
                            let now = Instant::now();
                            if now >= deadline {
                                break;
                            }
                            let (next_guard, _) = bg_stop
                                .1
                                .wait_timeout(guard, deadline - now)
                                .unwrap_or_else(|e| e.into_inner());
                            guard = next_guard;
                        }
                        *guard
                    };
                    if should_stop {
                        break;
                    }

                    let current = sampler();
                    if let Some(ev) = diff_fingerprint(&last, &current) {
                        last = current;
                        on_change(ev);
                    }
                }
            })
            .ok();

        Self {
            stop_signal,
            thread,
        }
    }

    /// 立即通知后台线程停止并等待回收（幂等，零退出延迟）。
    pub fn stop(&mut self) {
        {
            let mut stopped = self.stop_signal.0.lock().unwrap_or_else(|e| e.into_inner());
            *stopped = true;
        }
        self.stop_signal.1.notify_all();
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for NetworkMonitorHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::mpsc;

    #[test]
    fn fingerprint_normalizes_and_detects_all_transitions() {
        let a = NetworkFingerprint::from_addrs(["192.168.1.10", "10.0.0.2", "192.168.1.10", " "]);
        assert_eq!(a.addrs, vec!["10.0.0.2", "192.168.1.10"]);
        assert!(a.has_connectivity());

        // 相同指纹（输入顺序不同）不触发变更事件。
        let same = NetworkFingerprint::from_addrs(["192.168.1.10", "10.0.0.2"]);
        assert!(diff_fingerprint(&a, &same).is_none());

        // 有网 -> 无网。
        let offline = NetworkFingerprint::default();
        assert!(!offline.has_connectivity());
        let lost = diff_fingerprint(&a, &offline).expect("online -> offline must trigger");
        assert_eq!(
            lost,
            NetworkChangeEvent {
                had_connectivity: true,
                has_connectivity: false,
                fingerprint: Vec::new(),
            }
        );

        // 无网 -> 有网。
        let restored_fp = NetworkFingerprint::from_addrs(["172.20.10.3"]);
        let restored =
            diff_fingerprint(&offline, &restored_fp).expect("offline -> online must trigger");
        assert_eq!(
            restored,
            NetworkChangeEvent {
                had_connectivity: false,
                has_connectivity: true,
                fingerprint: vec!["172.20.10.3".to_string()],
            }
        );

        // IP / 网卡 / VPN 切换。
        let vpn_fp = NetworkFingerprint::from_addrs(["172.20.10.3", "10.8.0.5"]);
        let switched = diff_fingerprint(&restored_fp, &vpn_fp).expect("route switch must trigger");
        assert_eq!(
            switched,
            NetworkChangeEvent {
                had_connectivity: true,
                has_connectivity: true,
                fingerprint: vec!["10.8.0.5".to_string(), "172.20.10.3".to_string()],
            }
        );
    }

    #[test]
    fn monitor_emits_change_events_and_drop_exits_immediately() {
        let samples = Arc::new(Mutex::new(VecDeque::from([
            NetworkFingerprint::from_addrs(["192.168.1.20"]),
            NetworkFingerprint::from_addrs(["192.168.1.20"]),
            NetworkFingerprint::default(),
            NetworkFingerprint::from_addrs(["10.8.0.4", "172.20.10.2"]),
        ])));
        let samples_bg = Arc::clone(&samples);
        let (tx, rx) = mpsc::channel();

        let mut handle = NetworkMonitorHandle::spawn_with_sampler(
            Duration::from_millis(5),
            move || {
                let mut q = samples_bg.lock().unwrap_or_else(|e| e.into_inner());
                if q.len() > 1 {
                    q.pop_front().unwrap_or_default()
                } else {
                    q.front().cloned().unwrap_or_default()
                }
            },
            move |ev| {
                let _ = tx.send(ev);
            },
        );

        let ev1 = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("must receive online->offline event");
        assert!(ev1.had_connectivity);
        assert!(!ev1.has_connectivity);
        assert!(ev1.fingerprint.is_empty());

        let ev2 = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("must receive offline->vpn event");
        assert!(!ev2.had_connectivity);
        assert!(ev2.has_connectivity);
        assert_eq!(
            ev2.fingerprint,
            vec!["10.8.0.4".to_string(), "172.20.10.2".to_string()]
        );

        handle.stop();
        // 幂等二次 stop 不阻塞。
        handle.stop();

        // 长轮询间隔（60s）下 Drop 必须经 Condvar 立即唤醒并在远小于轮询周期内退出。
        let long_monitor = NetworkMonitorHandle::spawn_with_sampler(
            Duration::from_secs(60),
            || NetworkFingerprint::from_addrs(["192.168.1.99"]),
            |_| {},
        );
        let started = Instant::now();
        drop(long_monitor);
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "Drop 必须经 Condvar 零延迟唤醒退出，实际耗时 {:?}",
            started.elapsed()
        );
    }
}

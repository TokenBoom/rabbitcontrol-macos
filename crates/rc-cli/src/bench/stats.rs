//! 量化基准统计引擎与 JSON 报告结构（纯 Rust 跨平台可测，零 `unsafe`）。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 分布统计摘要（单位：毫秒）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DistStats {
    pub count: u64,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
    pub mean: f64,
    pub iqr: f64,
}

impl DistStats {
    /// 由微秒样本集合构建毫秒级分布统计。空切片返回全 0。
    pub fn from_us(samples_us: &[u64]) -> Self {
        if samples_us.is_empty() {
            return Self::default();
        }
        let mut sorted: Vec<f64> = samples_us.iter().map(|&v| v as f64 / 1000.0).collect();
        sorted.sort_by(|a, b| a.total_cmp(b));
        Self::from_sorted_ms(&sorted)
    }

    /// 由毫秒样本集合构建分布统计（过滤非有限数）。
    pub fn from_ms(samples_ms: &[f64]) -> Self {
        let mut sorted: Vec<f64> = samples_ms
            .iter()
            .copied()
            .filter(|v| v.is_finite())
            .collect();
        if sorted.is_empty() {
            return Self::default();
        }
        sorted.sort_by(|a, b| a.total_cmp(b));
        Self::from_sorted_ms(&sorted)
    }

    fn from_sorted_ms(sorted: &[f64]) -> Self {
        let count = sorted.len() as u64;
        let sum: f64 = sorted.iter().sum();
        let q25 = percentile_sorted(sorted, 25.0);
        let q75 = percentile_sorted(sorted, 75.0);
        Self {
            count,
            min: sorted[0],
            p50: percentile_sorted(sorted, 50.0),
            p95: percentile_sorted(sorted, 95.0),
            p99: percentile_sorted(sorted, 99.0),
            max: sorted[sorted.len() - 1],
            mean: sum / sorted.len() as f64,
            iqr: (q75 - q25).max(0.0),
        }
    }
}

/// 由微秒样本构造 1 ms 分桶直方图（键为整数毫秒，≥ 30 ms 归入 30 桶）。
pub fn histogram_1ms_from_us(samples_us: &[u64]) -> BTreeMap<u32, u64> {
    let mut map = BTreeMap::new();
    for &us in samples_us {
        let bucket = u32::try_from(us / 1000).unwrap_or(30).min(30);
        *map.entry(bucket).or_insert(0) += 1;
    }
    map
}

fn percentile_sorted(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = (pct / 100.0) * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    let frac = rank - lo as f64;
    sorted[lo] * (1.0 - frac) + sorted[hi] * frac
}

/// 刷新率与帧节拍质量指标（维度 C，基于真实 `MTLDrawable.presentedTime`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PacingMetrics {
    pub presented_frames: u64,
    pub duration_s: f64,
    pub avg_fps: f64,
    pub low_1pct_fps: f64,
    pub low_0_1pct_fps: f64,
    pub ifi_p50_ms: f64,
    pub ifi_p95_ms: f64,
    pub ifi_p99_ms: f64,
    pub ifi_max_ms: f64,
    pub ifi_jitter_ms: f64,
    pub hitch_ms_per_s: f64,
    pub cadence_lock_pct: f64,
    pub sustained_drop_pct: f64,
}

/// 从真实扫描上屏时间戳序列（微秒）与源帧号序列计算维度 C 指标。
pub fn compute_pacing_metrics(
    presented_us: &[u64],
    source_frame_ids: &[Option<u16>],
    target_fps: u32,
    duration_s: f64,
) -> PacingMetrics {
    let presented_frames = presented_us.len() as u64;
    let safe_dur = if duration_s.is_finite() && duration_s > 0.0 {
        duration_s
    } else if presented_us.len() >= 2 {
        let span = presented_us.last().unwrap().saturating_sub(presented_us[0]);
        (span as f64 / 1_000_000.0).max(0.001)
    } else {
        1.0
    };

    if presented_us.len() < 2 {
        return PacingMetrics {
            presented_frames,
            duration_s: safe_dur,
            avg_fps: presented_frames as f64 / safe_dur,
            ..Default::default()
        };
    }

    let target_period_ms = 1000.0 / f64::from(target_fps.max(1));
    let hitch_threshold_ms = 1.5 * target_period_ms;
    let lock_lo_ms = 0.75 * target_period_ms;
    let lock_hi_ms = 1.25 * target_period_ms;

    let mut deltas_ms: Vec<f64> = Vec::with_capacity(presented_us.len() - 1);
    let mut jitter_diffs_ms: Vec<f64> = Vec::with_capacity(presented_us.len().saturating_sub(2));
    let mut hitch_total_ms = 0.0f64;
    let mut locked_count = 0u64;

    for i in 1..presented_us.len() {
        let dt_ms = presented_us[i].saturating_sub(presented_us[i - 1]) as f64 / 1000.0;
        if i >= 2 {
            let prev_dt = deltas_ms[deltas_ms.len() - 1];
            jitter_diffs_ms.push((dt_ms - prev_dt).abs());
        }
        deltas_ms.push(dt_ms);

        if dt_ms > hitch_threshold_ms {
            hitch_total_ms += dt_ms - hitch_threshold_ms;
        }

        let seq_ok = match (
            source_frame_ids.get(i - 1).copied().flatten(),
            source_frame_ids.get(i).copied().flatten(),
        ) {
            (Some(prev), Some(curr)) => curr == prev.wrapping_add(1),
            _ => true,
        };
        if seq_ok && (lock_lo_ms..=lock_hi_ms).contains(&dt_ms) {
            locked_count += 1;
        }
    }

    let mut sorted_dt = deltas_ms.clone();
    sorted_dt.sort_by(|a, b| a.total_cmp(b));
    let ifi_p50_ms = percentile_sorted(&sorted_dt, 50.0);
    let ifi_p95_ms = percentile_sorted(&sorted_dt, 95.0);
    let ifi_p99_ms = percentile_sorted(&sorted_dt, 99.0);
    let ifi_max_ms = sorted_dt[sorted_dt.len() - 1];

    let mut sorted_jitter = jitter_diffs_ms;
    sorted_jitter.sort_by(|a, b| a.total_cmp(b));
    let ifi_jitter_ms = percentile_sorted(&sorted_jitter, 95.0);

    let low_1pct_fps = if ifi_p99_ms > 0.0 {
        1000.0 / ifi_p99_ms
    } else {
        0.0
    };
    let p999_ms = percentile_sorted(&sorted_dt, 99.9);
    let low_0_1pct_fps = if p999_ms > 0.0 { 1000.0 / p999_ms } else { 0.0 };

    // 稳态衰减：按真实时间前 5s（或前 1/3）对比后段（> 10s 或后 2/3）的 1% Low FPS 降幅。
    let t0 = presented_us[0];
    let total_span_us = presented_us.last().unwrap().saturating_sub(t0);
    let (early_cut_us, late_start_us) = if total_span_us >= 15_000_000 {
        (5_000_000, 10_000_000)
    } else {
        (total_span_us / 3, total_span_us / 3)
    };
    let mut early_dt = Vec::new();
    let mut late_dt = Vec::new();
    for (idx, &dt_ms) in deltas_ms.iter().enumerate() {
        let rel_us = presented_us[idx + 1].saturating_sub(t0);
        if rel_us <= early_cut_us {
            early_dt.push(dt_ms);
        } else if rel_us >= late_start_us {
            late_dt.push(dt_ms);
        }
    }
    let sustained_drop_pct = if early_dt.len() >= 5 && late_dt.len() >= 5 {
        early_dt.sort_by(|a, b| a.total_cmp(b));
        late_dt.sort_by(|a, b| a.total_cmp(b));
        let early_low = 1000.0 / percentile_sorted(&early_dt, 99.0).max(0.001);
        let late_low = 1000.0 / percentile_sorted(&late_dt, 99.0).max(0.001);
        if early_low > 0.0 && late_low < early_low {
            ((early_low - late_low) / early_low) * 100.0
        } else {
            0.0
        }
    } else {
        0.0
    };

    PacingMetrics {
        presented_frames,
        duration_s: safe_dur,
        avg_fps: presented_frames as f64 / safe_dur,
        low_1pct_fps,
        low_0_1pct_fps,
        ifi_p50_ms,
        ifi_p95_ms,
        ifi_p99_ms,
        ifi_max_ms,
        ifi_jitter_ms,
        hitch_ms_per_s: hitch_total_ms / safe_dur,
        cadence_lock_pct: (locked_count as f64 / deltas_ms.len() as f64) * 100.0,
        sustained_drop_pct,
    }
}

/// 采集侧延迟拆解（维度 A）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureSection {
    pub captured_frames: u64,
    #[serde(default)]
    pub barcode_decoded_frames: u64,
    pub skipped_source_frames: u64,
    pub mailbox_overwrites: u64,
    pub stimulus_to_pts_ms: DistStats,
    pub pts_to_callback_ms: DistStats,
    pub mailbox_wait_ms: DistStats,
    pub capture_to_encode_ms: DistStats,
    pub stimulus_to_pts_hist_1ms: BTreeMap<u32, u64>,
}

/// 跟手度指标（维度 B）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SnappinessSection {
    pub step_i2p_ms: Option<DistStats>,
    pub inject_to_app_ms: Option<DistStats>,
    pub app_to_src_present_ms: Option<DistStats>,
    pub e2e_presented_ms: Option<DistStats>,
    pub drag_spatial_lag_px: Option<DistStats>,
    pub drag_equiv_lag_ms: Option<DistStats>,
    pub lag_jitter_ms: Option<f64>,
    pub wakeup_first_frame_ms: Option<DistStats>,
    pub wakeup_ramp_ms: Option<DistStats>,
}

/// 鼠标体验全链路量化指标（维度 D，覆盖 S7–S11）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MouseMetrics {
    pub click_i2p_ms: Option<DistStats>,
    pub double_click_recognized_pct: Option<f64>,
    pub hold_drag_spatial_lag_px: Option<DistStats>,
    pub hold_drag_equiv_lag_ms: Option<DistStats>,
    pub hold_drag_lag_jitter_ms: Option<f64>,
    pub wheel_i2p_ms: Option<DistStats>,
    pub wheel_inject_to_app_ms: Option<DistStats>,
    pub wheel_delivered_ratio_pct: Option<f64>,
    pub quic_input_oneway_ms: Option<DistStats>,
    pub injected_events: Option<u64>,
    pub batches_sent: Option<u64>,
    pub events_after_coalesce: Option<u64>,
    pub coalesce_ratio_pct: Option<f64>,
    pub pointer_accuracy_err_px: Option<DistStats>,
    pub cursor_split_fast_px: Option<DistStats>,
    pub cursor_split_slow_px: Option<DistStats>,
    pub cursor_settle_ms: Option<DistStats>,
}

/// 键盘全链路体验量化指标（维度 E，覆盖 S12–S13 及 S16 键盘并发）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KeyboardMetrics {
    pub key_i2p_ms: Option<DistStats>,
    pub key_up_i2p_ms: Option<DistStats>,
    pub key_inject_to_app_ms: Option<DistStats>,
    pub keys_sent: Option<u64>,
    pub keys_delivered: Option<u64>,
    pub key_drop_rate_pct: Option<f64>,
    pub key_out_of_order_count: Option<u64>,
    pub chord_attempts: Option<u64>,
    pub modifier_sync_accuracy_pct: Option<f64>,
    pub stuck_keys_after_release: Option<u64>,
}

/// 音频、剪贴板、冷启动 TTFF 与混合负载指标（维度 F，覆盖 S14–S16）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemMetrics {
    pub audio_frames_captured: Option<u64>,
    pub audio_frames_played: Option<u64>,
    pub audio_push_errors: Option<u64>,
    pub audio_capture_interval_ms: Option<DistStats>,
    pub audio_codec_quic_ms: Option<DistStats>,
    pub audio_player_push_ms: Option<DistStats>,
    pub clipboard_samples: Option<u64>,
    pub clipboard_fidelity_pct: Option<f64>,
    pub clipboard_detect_ms: Option<DistStats>,
    pub clipboard_roundtrip_ms: Option<DistStats>,
    pub ttff_ms: Option<DistStats>,
}

/// 单个基准场景报告。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScenarioReport {
    pub id: String,
    pub target_fps: u32,
    pub duration_s: f64,
    pub runs_merged: u32,
    pub iqr_warning: bool,
    pub encoded_frames: u64,
    pub encoded_fps: f64,
    pub capture: CaptureSection,
    pub snappiness: SnappinessSection,
    #[serde(default)]
    pub mouse: MouseMetrics,
    #[serde(default)]
    pub keyboard: KeyboardMetrics,
    #[serde(default)]
    pub system: SystemMetrics,
    pub pacing: PacingMetrics,
    pub notes: Vec<String>,
}

/// 运行环境快照。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EnvSnapshot {
    pub chip: String,
    pub os_version: String,
    pub resolution: String,
    pub git_commit: String,
    pub power_source: String,
    pub clock_domain: String,
    #[serde(default)]
    pub clock_delta_us: i64,
    pub runs_per_scenario: u32,
}

/// 完整基准 JSON 报告。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BenchReport {
    pub schema_version: u32,
    pub generated_at_unix_s: u64,
    pub env: EnvSnapshot,
    pub scenarios: Vec<ScenarioReport>,
}

/// 合并同一场景的多轮运行结果（取核心指标中位数，并在跨轮次 P50 极差/IQR > 1.5 ms 时打噪音告警）。
pub fn merge_scenario_runs(runs: &[ScenarioReport]) -> Option<ScenarioReport> {
    if runs.is_empty() {
        return None;
    }
    if runs.len() == 1 {
        let mut single = runs[0].clone();
        single.runs_merged = 1;
        return Some(single);
    }

    let mut p50_spread_values: Vec<f64> = runs
        .iter()
        .filter_map(|r| {
            r.snappiness
                .step_i2p_ms
                .as_ref()
                .map(|d| d.p50)
                .or_else(|| r.mouse.click_i2p_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.mouse.wheel_i2p_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.mouse.cursor_settle_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.mouse.quic_input_oneway_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.keyboard.key_i2p_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.keyboard.key_up_i2p_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.system.ttff_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.system.audio_codec_quic_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.system.clipboard_roundtrip_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.snappiness.e2e_presented_ms.as_ref().map(|d| d.p50))
                .or_else(|| {
                    (r.capture.capture_to_encode_ms.count > 0)
                        .then_some(r.capture.capture_to_encode_ms.p50)
                })
        })
        .collect();
    let iqr_warning = if p50_spread_values.len() >= 2 {
        p50_spread_values.sort_by(|a, b| a.total_cmp(b));
        let spread = p50_spread_values[p50_spread_values.len() - 1] - p50_spread_values[0];
        let stats = DistStats::from_ms(&p50_spread_values);
        stats.iqr > 1.5 || spread > 3.0
    } else {
        false
    };

    // 按核心指标排序选取中位数一轮作为代表快照，确保内部一致性不变式不撕裂。
    let mut ranked: Vec<(usize, f64)> = runs
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let key = r
                .snappiness
                .step_i2p_ms
                .as_ref()
                .map(|d| d.p50)
                .or_else(|| r.mouse.click_i2p_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.mouse.wheel_i2p_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.mouse.cursor_settle_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.mouse.quic_input_oneway_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.keyboard.key_i2p_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.keyboard.key_up_i2p_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.system.ttff_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.system.audio_codec_quic_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.system.clipboard_roundtrip_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.snappiness.drag_equiv_lag_ms.as_ref().map(|d| d.p50))
                .or_else(|| r.snappiness.wakeup_first_frame_ms.as_ref().map(|d| d.p50))
                .unwrap_or(r.pacing.low_1pct_fps);
            (i, key)
        })
        .collect();
    ranked.sort_by(|a, b| a.1.total_cmp(&b.1));
    let mut chosen = runs[ranked[ranked.len() / 2].0].clone();
    chosen.runs_merged = runs.len() as u32;
    chosen.iqr_warning = iqr_warning;
    if iqr_warning {
        chosen.notes.push(
            "多轮复跑核心 P50 波动较大（IQR > 1.5ms 或极差 > 3.0ms），建议排查后台干扰重跑"
                .to_string(),
        );
    }
    Some(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_120hz_pacing_has_zero_jitter_and_full_lock() {
        let step_us = 8_333u64;
        let presented: Vec<u64> = (0..=240).map(|i| 1_000_000 + i * step_us).collect();
        let ids: Vec<Option<u16>> = (1..=241).map(|i| Some(i as u16)).collect();
        let m = compute_pacing_metrics(&presented, &ids, 120, 2.0);
        assert!(m.avg_fps >= 119.0 && m.avg_fps <= 121.0);
        assert!(m.low_1pct_fps >= 119.0);
        assert!(m.ifi_jitter_ms < 0.05);
        assert_eq!(m.hitch_ms_per_s, 0.0);
        assert!((m.cadence_lock_pct - 100.0).abs() < 1e-6);
    }

    #[test]
    fn sawtooth_cadence_triggers_high_jitter_and_low_lock() {
        // 8.33ms / 16.67ms 交替：均值 ~80fps，但相邻帧间隔剧烈抖动 8.33ms。
        let mut presented = vec![1_000_000u64];
        for i in 0..120 {
            let dt = if i % 2 == 0 { 8_333 } else { 16_667 };
            presented.push(presented.last().unwrap() + dt);
        }
        let ids = vec![None; presented.len()];
        let m = compute_pacing_metrics(&presented, &ids, 120, 1.5);
        assert!((m.ifi_jitter_ms - 8.334).abs() < 0.05);
        assert!(m.hitch_ms_per_s > 100.0);
        assert!(m.cadence_lock_pct < 55.0);
    }

    #[test]
    fn single_50ms_hitch_shows_up_in_low_1pct_and_hitch_rate() {
        let mut presented = vec![1_000_000u64];
        for i in 0..120 {
            let dt = if i == 30 || i == 90 { 50_000 } else { 16_667 };
            presented.push(presented.last().unwrap() + dt);
        }
        let ids = vec![None; presented.len()];
        let m = compute_pacing_metrics(&presented, &ids, 60, 2.0);
        assert!(
            m.low_1pct_fps < 30.0,
            "expected low_1pct_fps < 30.0, got {}",
            m.low_1pct_fps
        );
        assert!(m.low_0_1pct_fps < 25.0);
        // 阈值 = 1.5 * 16.667 = 25ms -> 2 次 50ms 产生 50ms hitch / 2s = 25 ms/s
        assert!((m.hitch_ms_per_s - 25.0).abs() < 1.0);
    }

    #[test]
    fn dist_stats_empty_and_nominal() {
        let empty = DistStats::from_us(&[]);
        assert_eq!(empty.count, 0);
        assert_eq!(empty.p50, 0.0);

        let d = DistStats::from_us(&[5_000, 10_000, 15_000, 20_000, 25_000]);
        assert_eq!(d.count, 5);
        assert!((d.min - 5.0).abs() < 1e-6);
        assert!((d.p50 - 15.0).abs() < 1e-6);
        assert!((d.max - 25.0).abs() < 1e-6);
        assert!((d.iqr - 10.0).abs() < 1e-6);
    }

    #[test]
    fn histogram_caps_overflow_bucket_at_30() {
        let h = histogram_1ms_from_us(&[500, 1_200, 1_900, 45_000]);
        assert_eq!(h.get(&0), Some(&1));
        assert_eq!(h.get(&1), Some(&2));
        assert_eq!(h.get(&30), Some(&1));
    }

    #[test]
    fn merge_scenario_runs_picks_median_and_flags_high_iqr() {
        let make = |p50_ms: f64| ScenarioReport {
            id: "S4-step-i2p".into(),
            target_fps: 60,
            snappiness: SnappinessSection {
                step_i2p_ms: Some(DistStats {
                    count: 10,
                    p50: p50_ms,
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let merged = merge_scenario_runs(&[make(14.0), make(22.0), make(16.0)]).unwrap();
        assert_eq!(merged.runs_merged, 3);
        assert!((merged.snappiness.step_i2p_ms.unwrap().p50 - 16.0).abs() < 1e-6);
        assert!(merged.iqr_warning);

        let tight = merge_scenario_runs(&[make(15.0), make(15.4), make(15.2)]).unwrap();
        assert!(!tight.iqr_warning);
    }

    #[test]
    fn mouse_metrics_backward_compatible_and_mergeable() {
        let legacy_json = r#"{
            "id": "S2-motion-60",
            "target_fps": 60,
            "duration_s": 10.0,
            "runs_merged": 1,
            "iqr_warning": false,
            "encoded_frames": 600,
            "encoded_fps": 60.0,
            "capture": {
                "captured_frames": 600,
                "skipped_source_frames": 0,
                "mailbox_overwrites": 0,
                "stimulus_to_pts_ms": {"count":0,"min":0,"p50":0,"p95":0,"p99":0,"max":0,"mean":0,"iqr":0},
                "pts_to_callback_ms": {"count":0,"min":0,"p50":0,"p95":0,"p99":0,"max":0,"mean":0,"iqr":0},
                "mailbox_wait_ms": {"count":0,"min":0,"p50":0,"p95":0,"p99":0,"max":0,"mean":0,"iqr":0},
                "capture_to_encode_ms": {"count":0,"min":0,"p50":0,"p95":0,"p99":0,"max":0,"mean":0,"iqr":0},
                "stimulus_to_pts_hist_1ms": {}
            },
            "snappiness": {},
            "pacing": {"presented_frames":0,"duration_s":10,"avg_fps":0,"low_1pct_fps":0,"low_0_1pct_fps":0,"ifi_p50_ms":0,"ifi_p95_ms":0,"ifi_p99_ms":0,"ifi_max_ms":0,"ifi_jitter_ms":0,"hitch_ms_per_s":0,"cadence_lock_pct":0,"sustained_drop_pct":0},
            "notes": []
        }"#;
        let parsed: ScenarioReport = serde_json::from_str(legacy_json).expect("legacy json");
        assert_eq!(parsed.mouse, MouseMetrics::default());

        let m = |p50: f64| ScenarioReport {
            id: "S7-mouse-click-drag".into(),
            mouse: MouseMetrics {
                click_i2p_ms: Some(DistStats {
                    count: 10,
                    p50,
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let merged = merge_scenario_runs(&[m(26.0), m(22.0), m(24.0)]).unwrap();
        assert!((merged.mouse.click_i2p_ms.unwrap().p50 - 24.0).abs() < 1e-6);
        assert_eq!(parsed.keyboard, KeyboardMetrics::default());
        assert_eq!(parsed.system, SystemMetrics::default());

        let kb = |key_p50: f64| ScenarioReport {
            id: "S12-key-typing-quic".into(),
            keyboard: KeyboardMetrics {
                key_i2p_ms: Some(DistStats {
                    count: 20,
                    p50: key_p50,
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let merged_kb = merge_scenario_runs(&[kb(30.0), kb(24.0), kb(27.0)]).unwrap();
        assert!((merged_kb.keyboard.key_i2p_ms.unwrap().p50 - 27.0).abs() < 1e-6);
    }
}

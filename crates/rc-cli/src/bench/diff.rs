//! 基准 JSON 报告对比与负优化告警（纯 Rust 跨平台可测，零 `unsafe`）。

use std::fmt::Write as _;

use super::stats::{BenchReport, ScenarioReport};

/// 负优化判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffOutcome {
    /// 可读对照表格与结论。
    pub summary_text: String,
    /// 是否触发负优化拦截（如 P50 降低但 1% Low 下降 > 3 fps 或 Hitch 上升 > 5 ms/s）。
    pub has_regression: bool,
}

fn primary_latency_p50(s: &ScenarioReport) -> Option<f64> {
    s.snappiness
        .step_i2p_ms
        .as_ref()
        .map(|d| d.p50)
        .or_else(|| s.mouse.click_i2p_ms.as_ref().map(|d| d.p50))
        .or_else(|| s.mouse.wheel_i2p_ms.as_ref().map(|d| d.p50))
        .or_else(|| s.mouse.hold_drag_equiv_lag_ms.as_ref().map(|d| d.p50))
        .or_else(|| s.mouse.cursor_settle_ms.as_ref().map(|d| d.p50))
        .or_else(|| s.mouse.quic_input_oneway_ms.as_ref().map(|d| d.p50))
        .or_else(|| s.keyboard.key_i2p_ms.as_ref().map(|d| d.p50))
        .or_else(|| s.keyboard.key_up_i2p_ms.as_ref().map(|d| d.p50))
        .or_else(|| s.system.ttff_ms.as_ref().map(|d| d.p50))
        .or_else(|| s.system.clipboard_roundtrip_ms.as_ref().map(|d| d.p50))
        .or_else(|| s.system.audio_codec_quic_ms.as_ref().map(|d| d.p50))
        .or_else(|| s.snappiness.e2e_presented_ms.as_ref().map(|d| d.p50))
        .or_else(|| s.snappiness.drag_equiv_lag_ms.as_ref().map(|d| d.p50))
        .or_else(|| s.snappiness.wakeup_first_frame_ms.as_ref().map(|d| d.p50))
}

/// 对比 `base` 与 `curr` 两份报告，生成对齐表格并检测负优化。
pub fn compare_reports(base: &BenchReport, curr: &BenchReport) -> DiffOutcome {
    let mut out = String::new();
    let mut has_regression = false;

    let _ = writeln!(
        out,
        "Base: commit={} ({}) | Curr: commit={} ({})",
        base.env.git_commit, base.env.resolution, curr.env.git_commit, curr.env.resolution
    );
    let _ = writeln!(
        out,
        "{:<16} | {:>16} | {:>16} | {:>16} | {:>16} | 结论",
        "场景", "主延迟P50(ms)", "Avg FPS", "1% Low FPS", "Hitch(ms/s)"
    );
    let _ = writeln!(out, "{}", "-".repeat(98));

    for c in &curr.scenarios {
        let Some(b) = base.scenarios.iter().find(|x| x.id == c.id) else {
            let _ = writeln!(
                out,
                "{:<16} | {:>16} | {:>16.2} | {:>16.2} | {:>16.2} | 新增场景",
                c.id,
                primary_latency_p50(c).map_or("-".into(), |v| format!("{v:.2}")),
                c.pacing.avg_fps,
                c.pacing.low_1pct_fps,
                c.pacing.hitch_ms_per_s
            );
            continue;
        };

        let lat_str = match (primary_latency_p50(b), primary_latency_p50(c)) {
            (Some(bv), Some(cv)) => format!("{bv:.2}->{cv:.2}({:+.2})", cv - bv),
            _ => "-".to_string(),
        };
        let avg_str = format!(
            "{:.1}->{:.1}({:+.1})",
            b.pacing.avg_fps,
            c.pacing.avg_fps,
            c.pacing.avg_fps - b.pacing.avg_fps
        );
        let low_delta = c.pacing.low_1pct_fps - b.pacing.low_1pct_fps;
        let low_str = format!(
            "{:.1}->{:.1}({:+.1})",
            b.pacing.low_1pct_fps, c.pacing.low_1pct_fps, low_delta
        );
        let hitch_delta = c.pacing.hitch_ms_per_s - b.pacing.hitch_ms_per_s;
        let hitch_str = format!(
            "{:.1}->{:.1}({:+.1})",
            b.pacing.hitch_ms_per_s, c.pacing.hitch_ms_per_s, hitch_delta
        );

        let mut flags = Vec::new();
        if b.pacing.low_1pct_fps > 0.0 && low_delta < -3.0 {
            flags.push("1%Low下降>3fps");
            has_regression = true;
        }
        if hitch_delta > 5.0 {
            flags.push("Hitch上升>5ms/s");
            has_regression = true;
        }
        if let (Some(bv), Some(cv)) = (primary_latency_p50(b), primary_latency_p50(c)) {
            if cv - bv > 2.0 {
                flags.push("主延迟P50劣化>2ms");
                has_regression = true;
            }
        }
        if c.id == "S1-static-idle" && c.encoded_frames > 0 && b.encoded_frames == 0 {
            flags.push("静屏误编码");
            has_regression = true;
        }

        let verdict = if flags.is_empty() {
            "OK".to_string()
        } else {
            format!("REGRESSION: {}", flags.join(", "))
        };
        let _ = writeln!(
            out,
            "{:<16} | {:>16} | {:>16} | {:>16} | {:>16} | {}",
            c.id, lat_str, avg_str, low_str, hitch_str, verdict
        );
    }

    DiffOutcome {
        summary_text: out,
        has_regression,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::stats::{DistStats, PacingMetrics, SnappinessSection};

    fn sample_report(p50_ms: f64, low_1pct: f64, hitch: f64) -> BenchReport {
        BenchReport {
            schema_version: 1,
            scenarios: vec![ScenarioReport {
                id: "S3-motion-120".into(),
                target_fps: 120,
                snappiness: SnappinessSection {
                    e2e_presented_ms: Some(DistStats {
                        count: 100,
                        p50: p50_ms,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                pacing: PacingMetrics {
                    avg_fps: 118.0,
                    low_1pct_fps: low_1pct,
                    hitch_ms_per_s: hitch,
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn diff_catches_pseudo_optimization_with_low_fps_drop_or_hitch_spike() {
        let base = sample_report(17.5, 102.0, 2.0);
        let pseudo_opt = sample_report(15.0, 92.0, 9.5);
        let res = compare_reports(&base, &pseudo_opt);
        assert!(res.has_regression);
        assert!(res.summary_text.contains("REGRESSION"));
        assert!(res.summary_text.contains("1%Low下降>3fps"));
        assert!(res.summary_text.contains("Hitch上升>5ms/s"));
    }

    #[test]
    fn diff_passes_true_improvement() {
        let base = sample_report(17.5, 102.0, 4.0);
        let better = sample_report(14.8, 112.0, 1.2);
        let res = compare_reports(&base, &better);
        assert!(!res.has_regression);
        assert!(res.summary_text.contains("OK"));
    }

    #[test]
    fn diff_detects_mouse_click_regression() {
        let mk = |click_p50: f64| BenchReport {
            schema_version: 1,
            scenarios: vec![ScenarioReport {
                id: "S7-mouse-click-drag".into(),
                target_fps: 120,
                mouse: crate::bench::stats::MouseMetrics {
                    click_i2p_ms: Some(DistStats {
                        count: 20,
                        p50: click_p50,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                pacing: PacingMetrics {
                    avg_fps: 118.0,
                    low_1pct_fps: 95.0,
                    hitch_ms_per_s: 2.0,
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let res = compare_reports(&mk(22.0), &mk(27.5));
        assert!(res.has_regression);
        assert!(res.summary_text.contains("主延迟P50劣化>2ms"));
    }

    #[test]
    fn diff_detects_keyboard_and_ttff_regression() {
        let mk = |key_p50: f64, ttff_p50: f64| BenchReport {
            schema_version: 1,
            scenarios: vec![
                ScenarioReport {
                    id: "S12-key-typing-quic".into(),
                    target_fps: 120,
                    keyboard: crate::bench::stats::KeyboardMetrics {
                        key_i2p_ms: Some(DistStats {
                            count: 20,
                            p50: key_p50,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ScenarioReport {
                    id: "S16-mixed-workflow-ttff".into(),
                    target_fps: 120,
                    system: crate::bench::stats::SystemMetrics {
                        ttff_ms: Some(DistStats {
                            count: 5,
                            p50: ttff_p50,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let res = compare_reports(&mk(25.0, 140.0), &mk(29.0, 180.0));
        assert!(res.has_regression);
        assert!(res.summary_text.contains("主延迟P50劣化>2ms"));
    }
}

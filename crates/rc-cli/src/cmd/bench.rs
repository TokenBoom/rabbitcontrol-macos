//! `rc-cli bench`：macOS 端量化基准运行与报告对比入口（仅 `bench-metrics` 启用）。

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Subcommand};

use super::{parse_duration, CliResult};
use crate::bench::diff::compare_reports;
use crate::bench::stats::BenchReport;

/// `bench` 子命令参数。
#[derive(Args)]
pub struct BenchArgs {
    #[command(subcommand)]
    pub action: BenchAction,
}

/// `bench` 二级操作。
#[derive(Subcommand)]
pub enum BenchAction {
    /// 运行 macOS 端真机基准测试套件（S1–S16）并输出 JSON 报告
    Run(BenchRunArgs),
    /// 对比两份基准 JSON 报告，检测吞吐 / 延迟 / 帧节拍负优化
    Diff(BenchDiffArgs),
}

/// `bench run` 参数。
#[derive(Args)]
pub struct BenchRunArgs {
    /// 场景选择：all | core | mouse | keyboard | system | s1..s16
    #[arg(long, default_value = "all")]
    pub suite: String,
    /// 每个场景复跑次数（≥ 3 时按中位数合并并做 IQR 噪声告警）
    #[arg(long, default_value_t = 1)]
    pub runs: u32,
    /// 覆盖每个场景的运行时长（如 10s / 30s）；省略则走各场景默认时长
    #[arg(long, value_parser = parse_duration)]
    pub duration: Option<Duration>,
    /// 显式指定探针可执行文件路径（`rc-render-macos` `bench_target`）
    #[arg(long)]
    pub bench_target: Option<PathBuf>,
    /// JSON 报告输出路径；省略或 `-` 表示 stdout
    #[arg(long)]
    pub report: Option<PathBuf>,
}

/// `bench diff` 参数。
#[derive(Args)]
pub struct BenchDiffArgs {
    /// 基线 JSON 文件路径
    pub base: PathBuf,
    /// 当前对比 JSON 文件路径
    pub curr: PathBuf,
}

/// 执行 `bench` 子命令。
pub fn run(args: &BenchArgs) -> CliResult<ExitCode> {
    match &args.action {
        BenchAction::Run(run_args) => run_bench(run_args),
        BenchAction::Diff(diff_args) => run_diff(diff_args),
    }
}

#[cfg(target_os = "macos")]
fn run_bench(args: &BenchRunArgs) -> CliResult<ExitCode> {
    use crate::bench::runner::{execute_bench_run, BenchRunConfig, BenchSuite};

    let suite = BenchSuite::parse(&args.suite)?;
    let cfg = BenchRunConfig {
        suite,
        runs: args.runs.max(1),
        duration_override: args.duration,
        bench_target_bin: args.bench_target.clone(),
        report_path: args.report.clone(),
    };
    execute_bench_run(cfg)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(not(target_os = "macos"))]
fn run_bench(_args: &BenchRunArgs) -> CliResult<ExitCode> {
    Err("rc-cli bench run 仅支持在 macOS 上运行".to_string())
}

fn run_diff(args: &BenchDiffArgs) -> CliResult<ExitCode> {
    let base_raw = fs::read_to_string(&args.base)
        .map_err(|e| format!("读取基线报告 {} 失败：{e}", args.base.display()))?;
    let curr_raw = fs::read_to_string(&args.curr)
        .map_err(|e| format!("读取对比报告 {} 失败：{e}", args.curr.display()))?;
    let base: BenchReport = serde_json::from_str(&base_raw)
        .map_err(|e| format!("解析基线 JSON {} 失败：{e}", args.base.display()))?;
    let curr: BenchReport = serde_json::from_str(&curr_raw)
        .map_err(|e| format!("解析对比 JSON {} 失败：{e}", args.curr.display()))?;

    let outcome = compare_reports(&base, &curr);
    print!("{}", outcome.summary_text);
    if outcome.has_regression {
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

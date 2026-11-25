//! `rc-cli bench`：macOS 端量化基准运行与报告对比入口（仅 `bench-metrics` 启用）。

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Subcommand};

use super::{parse_duration, parse_resolution, CliResult};
use crate::bench::diff::compare_reports;
use crate::bench::stats::BenchReport;
use rc_types::Resolution;

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
    /// 无高刷新率物理屏幕限制的高熵合成真机管线吞吐基准（4K@120 / 2K@240 / 1K@380）
    Synth(BenchSynthArgs),
    /// 隔离编解码基准（直测生产 `rc-encode-vt` Sync/Pipelined 与 `rc-decode-vt` 解码性能）
    Codec(BenchCodecArgs),
}

/// 传输路径模式。
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum SynthMode {
    /// HostPipeline → ControllerPipeline 同进程直投（测纯编码/解码/Metal 管线极限）
    Inproc,
    /// 真实本机回环 QUIC + Pacer → ControllerPipeline（含 QUIC 分片、pacing 与重组）
    Quic,
}

/// 编解码器选择。
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum SynthCodec {
    Hevc,
    H264,
}

/// `bench synth` 参数。
#[derive(Args)]
pub struct BenchSynthArgs {
    /// 连续运行全套目标矩阵（3440x1440@120, 4K@120, 2K@240, 1K@380）
    #[arg(long)]
    pub matrix: bool,
    /// 单场景分辨率（非 --matrix 时生效），格式 WxH
    #[arg(long, default_value = "3440x1440", value_parser = parse_resolution)]
    pub resolution: Resolution,
    /// 单场景目标帧率
    #[arg(long, default_value_t = 120)]
    pub fps: u32,
    /// 单场景目标码率（bps），不传时自动按分辨率与帧率计算
    #[arg(long)]
    pub bitrate: Option<u32>,
    /// 视频编码器（默认 hevc）
    #[arg(long, value_enum, default_value_t = SynthCodec::Hevc)]
    pub codec: SynthCodec,
    /// 运行模式（inproc | quic）
    #[arg(long, value_enum, default_value_t = SynthMode::Inproc)]
    pub mode: SynthMode,
    /// 每个场景运行时长
    #[arg(long, default_value = "5s", value_parser = parse_duration)]
    pub duration: Duration,
}

/// `bench codec` 参数。
#[derive(Args)]
pub struct BenchCodecArgs {
    /// 运行全矩阵（1080p60/120、1440p120、3440x1440@120、4K@120 × H.264 & HEVC）
    #[arg(long)]
    pub matrix: bool,
    /// 单场景分辨率（非 --matrix 时生效），格式 WxH
    #[arg(long, default_value = "1920x1080", value_parser = parse_resolution)]
    pub resolution: Resolution,
    /// 单场景目标帧率
    #[arg(long, default_value_t = 120)]
    pub fps: u32,
    /// 单场景目标码率（bps），不传时自动按分辨率与帧率计算
    #[arg(long)]
    pub bitrate: Option<u32>,
    /// 视频编解码器（默认 hevc）
    #[arg(long, value_enum, default_value_t = SynthCodec::Hevc)]
    pub codec: SynthCodec,
    /// 每个场景编码/解码的帧数
    #[arg(long, default_value_t = 150)]
    pub frames: usize,
    /// JSON 报告输出路径
    #[arg(long)]
    pub report: Option<PathBuf>,
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
        BenchAction::Synth(synth_args) => run_synth(synth_args),
        BenchAction::Codec(codec_args) => run_codec(codec_args),
    }
}

#[cfg(target_os = "macos")]
fn run_codec(args: &BenchCodecArgs) -> CliResult<ExitCode> {
    use crate::bench::codec::{execute_codec_bench, CodecBenchCfg};
    let codec = match args.codec {
        SynthCodec::Hevc => rc_encode::Codec::Hevc,
        SynthCodec::H264 => rc_encode::Codec::H264,
    };
    let cfg = CodecBenchCfg {
        matrix: args.matrix,
        resolution: args.resolution,
        fps: args.fps.max(1),
        bitrate_bps: args.bitrate,
        codec,
        frames: args.frames.max(20),
        report_path: args.report.clone(),
    };
    execute_codec_bench(cfg)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(not(target_os = "macos"))]
fn run_codec(_args: &BenchCodecArgs) -> CliResult<ExitCode> {
    Err("rc-cli bench codec 仅支持在 macOS 上运行".to_string())
}

#[cfg(target_os = "macos")]
fn run_synth(args: &BenchSynthArgs) -> CliResult<ExitCode> {
    use crate::bench::synth::{execute_synth_bench, SynthBenchCfg, SynthTransportMode};
    let codec = match args.codec {
        SynthCodec::Hevc => rc_encode::Codec::Hevc,
        SynthCodec::H264 => rc_encode::Codec::H264,
    };
    let mode = match args.mode {
        SynthMode::Inproc => SynthTransportMode::Inproc,
        SynthMode::Quic => SynthTransportMode::Quic,
    };
    let cfg = SynthBenchCfg {
        matrix: args.matrix,
        resolution: args.resolution,
        fps: args.fps.max(1),
        bitrate_bps: args.bitrate,
        codec,
        mode,
        duration: args.duration,
    };
    execute_synth_bench(cfg)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(not(target_os = "macos"))]
fn run_synth(_args: &BenchSynthArgs) -> CliResult<ExitCode> {
    Err("rc-cli bench synth 仅支持在 macOS 上运行".to_string())
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

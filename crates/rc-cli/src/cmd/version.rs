//! `rc-cli version`：版本、git commit 与编译时间。
//!
//! 三个值都由 `build.rs` 在编译期注入（版本来自 `CARGO_PKG_VERSION`，
//! commit 来自 `git rev-parse --short HEAD`，编译时间来自 UTC `date`）；
//! 拿不到的项注入 `"unknown"`，不阻塞构建。

use std::process::ExitCode;

use super::CliResult;

/// 执行 `version` 子命令。
pub fn run() -> CliResult<ExitCode> {
    let core = rc_client_core::version::core_build_info();
    println!(
        "rc-cli {} (commit {}, built {}; core {} commit {} session_proto={} punch={} beacon={} abi={})",
        env!("RC_CLI_VERSION"),
        env!("RC_CLI_GIT_COMMIT"),
        env!("RC_CLI_BUILD_TIME"),
        core.core_version,
        core.git_commit,
        core.session_protocol_version,
        core.punch_version,
        core.beacon_version,
        core.abi_version,
    );
    Ok(ExitCode::SUCCESS)
}

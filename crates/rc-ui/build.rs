fn main() {
    println!("cargo:rerun-if-env-changed=RC_APP_VERSION");
    println!("cargo:rerun-if-env-changed=RC_GIT_SHA");

    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let app_version = std::env::var("RC_APP_VERSION")
        .ok()
        .map(|s| s.trim().trim_start_matches('v').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| pkg_version.trim().trim_start_matches('v').to_string());
    println!("cargo:rustc-env=RC_APP_VERSION={app_version}");

    let git_sha = std::env::var("RC_GIT_SHA")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_default();
    println!("cargo:rustc-env=RC_GIT_SHA={git_sha}");

    tauri_build::build()
}

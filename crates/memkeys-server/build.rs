//! Embed the git sha into `service.version` (ADR-0001 D-2: "build metadata
//! (semver + git sha)"). Falls back to `unknown` outside a git checkout.

use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=MEMKEYS_BUILD_GIT_SHA={sha}");
}

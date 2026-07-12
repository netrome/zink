//! Embeds git build info so a deployed relay can say exactly what it is
//! (`--version`, and the first startup log line). Hand-rolled on purpose:
//! the one `git describe` we need is not worth a build-dependency (vergen
//! et al. — dependency discipline). Builds without a git checkout (e.g.
//! from a tarball) fall back to "unknown".

use std::process::Command;

fn main() {
    // Re-run when the checkout moves — an approximation (HEAD + index
    // cover commits, branch switches, and staged changes), good enough
    // for dev; release builds are fresh anyway.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
    let describe = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=ZINK_BUILD_INFO={describe}");
}

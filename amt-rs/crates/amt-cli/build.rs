use std::process::Command;

fn main() {
    // The version comes from the manifest (Cargo.toml), kept in step with the
    // commit count by scripts/sync-version.sh: what cargo prints is therefore
    // what the program prints. Only the hash is built here, because it changes
    // on every commit and identifies the binary under test.
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=AMT_GIT_HASH={hash}");
    // Only commits and checkouts touch this file.
    println!("cargo:rerun-if-changed=../../../.git/logs/HEAD");
}

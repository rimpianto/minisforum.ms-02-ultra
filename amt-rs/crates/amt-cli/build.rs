use std::process::Command;

fn main() {
    // ver. 0.1.<commit count> — the patch number is the commit count, so the
    // version always moves forward without anyone remembering to bump it.
    let count = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "0".to_string());

    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=AMT_VERSION=0.1.{count}");
    println!("cargo:rustc-env=AMT_GIT_HASH={hash}");
    // .git/logs/HEAD is appended on every commit and checkout, and nothing
    // else. Watching the directory or HEAD itself either misses commits or
    // rebuilds on unrelated activity, and every rerun of this script forces a
    // rebuild of the crate.
    println!("cargo:rerun-if-changed=../../../.git/logs/HEAD");
}

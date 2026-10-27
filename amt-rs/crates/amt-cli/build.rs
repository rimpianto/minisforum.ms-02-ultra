use std::process::Command;

fn main() {
    // La versione viene dal manifest (Cargo.toml), che scripts/sync-version.sh
    // tiene allineato al numero di commit: cosi' quello che stampa cargo e'
    // quello che stampa il programma. Qui resta solo l'hash, che cambia a ogni
    // commit e serve a riconoscere il binario in prova.
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=AMT_GIT_HASH={hash}");
    // Solo i commit e i checkout toccano questo file.
    println!("cargo:rerun-if-changed=../../../.git/logs/HEAD");
}

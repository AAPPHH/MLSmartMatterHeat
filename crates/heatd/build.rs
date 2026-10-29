fn main() {
    let commit = std::env::var("HEATD_COMMIT")
        .ok()
        .filter(|c| !c.is_empty())
        .or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "--short=12", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=HEATD_COMMIT={commit}");
    println!("cargo:rerun-if-env-changed=HEATD_COMMIT");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}

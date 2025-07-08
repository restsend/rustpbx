use std::env;
use std::process::Command;

fn main() {
    println!(
        "cargo:rustc-env=CARGO_PKG_VERSION={}",
        env!("CARGO_PKG_VERSION")
    );

    let build_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let build_time_str = format!("{}", build_time);
    println!("cargo:rustc-env=BUILD_TIME={}", build_time_str);

    let git_commit = get_git_commit_hash();
    println!("cargo:rustc-env=GIT_COMMIT_HASH={}", git_commit);

    let git_branch = get_git_branch();
    println!("cargo:rustc-env=GIT_BRANCH={}", git_branch);

    let git_dirty = get_git_dirty();
    println!("cargo:rustc-env=GIT_DIRTY={}", git_dirty);

    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}

fn get_git_commit_hash() -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

fn get_git_branch() -> String {
    Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

fn get_git_dirty() -> String {
    Command::new("git")
        .args(["diff", "--quiet", "--ignore-submodules"])
        .output()
        .map(|output| {
            if output.status.success() {
                "clean"
            } else {
                "dirty"
            }
        })
        .unwrap_or_else(|_| "unknown")
        .to_string()
}

use chrono::Local;
use std::env;
use std::process::Command;

fn main() {
    println!(
        "cargo:rustc-env=CARGO_PKG_VERSION={}",
        env!("CARGO_PKG_VERSION")
    );

    let git_commit = get_git_commit_hash();
    println!("cargo:rustc-env=GIT_COMMIT_HASH={}", git_commit);

    let git_branch = get_git_branch();
    println!("cargo:rustc-env=GIT_BRANCH={}", git_branch);

    let git_dirty = get_git_dirty();
    println!("cargo:rustc-env=GIT_DIRTY={}", git_dirty);

    let build_time = Local::now();
    println!(
        "cargo:rustc-env=BUILD_TIME_FMT={}",
        build_time.format("%Y-%m-%d %H:%M:%S %Z")
    );
    println!(
        "cargo:rustc-env=BUILD_DATE={}",
        build_time.format("%Y-%m-%d")
    );

    let commercial = if env::var_os("CARGO_FEATURE_COMMERCE").is_some() {
        "commerce"
    } else {
        "community"
    };
    let short_version = if git_dirty == "dirty" {
        format!(
            "{}-{}-dirty-{commercial}",
            env!("CARGO_PKG_VERSION"),
            git_commit
        )
    } else {
        format!("{}-{}-{commercial}", env!("CARGO_PKG_VERSION"), git_commit)
    };
    println!("cargo:rustc-env=SHORT_VERSION={short_version}");

    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}

fn get_git_commit_hash() -> String {
    // First try to get from .git directory
    let commit_from_git = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    // If .git doesn't exist or failed, try environment variable
    if commit_from_git == "unknown" {
        env::var("GIT_COMMIT_HASH").unwrap_or_else(|_| "unknown".to_string())
    } else {
        commit_from_git
    }
}

fn get_git_branch() -> String {
    // First try to get from .git directory
    let branch_from_git = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    // If .git doesn't exist or failed, try environment variable
    if branch_from_git == "unknown" {
        env::var("GIT_BRANCH").unwrap_or_else(|_| "unknown".to_string())
    } else {
        branch_from_git
    }
}

fn get_git_dirty() -> String {
    // First try to get from .git directory
    let dirty_from_git = Command::new("git")
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
        .to_string();

    // If .git doesn't exist or failed, try environment variable
    if dirty_from_git == "unknown" {
        env::var("GIT_DIRTY").unwrap_or_else(|_| "unknown".to_string())
    } else {
        dirty_from_git
    }
}

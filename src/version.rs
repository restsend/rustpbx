use std::sync::OnceLock;

use chrono::{DateTime, Local};

static VERSION_INFO: OnceLock<String> = OnceLock::new();
static SHORT_VERSION: OnceLock<String> = OnceLock::new();

pub fn get_version_info() -> &'static str {
    VERSION_INFO.get_or_init(|| {
        let version = env!("CARGO_PKG_VERSION");
        let build_time = env!("BUILD_TIME");
        let git_commit = env!("GIT_COMMIT_HASH");
        let git_branch = env!("GIT_BRANCH");
        let git_dirty = env!("GIT_DIRTY");

        let build_timestamp: i64 = build_time.parse().unwrap_or(0);
        let build_datetime: DateTime<Local> = DateTime::from_timestamp(build_timestamp, 0)
            .map(|utc| utc.with_timezone(&Local))
            .unwrap_or_else(Local::now);
        let build_time_str = build_datetime.format("%Y-%m-%d %H:%M:%S %Z").to_string();

        format!(
            "rustpbx {} ({})\n\
         Build Time: {}\n\
         Git Commit: {}\n\
         Git Branch: {}\n\
         Git Status: {}",
            version,
            rsipstack::VERSION,
            build_time_str,
            git_commit,
            git_branch,
            git_dirty
        )
    })
}

pub fn get_short_version() -> &'static str {
    SHORT_VERSION.get_or_init(|| {
        let version = env!("CARGO_PKG_VERSION");
        let git_commit = env!("GIT_COMMIT_HASH");
        let git_dirty = env!("GIT_DIRTY");

        if git_dirty == "dirty" {
            format!("{}-{}-dirty", version, git_commit)
        } else {
            format!("{}-{}", version, git_commit)
        }
    })
}

pub fn get_useragent() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let build_time = env!("BUILD_TIME");

    let build_timestamp: i64 = build_time.parse().unwrap_or(0);
    let build_datetime: DateTime<Local> = DateTime::from_timestamp(build_timestamp, 0)
        .map(|utc| utc.with_timezone(&Local))
        .unwrap_or_else(Local::now);
    let build_time_str = build_datetime.format("%Y-%m-%d").to_string();
    format!(
        "rustpbx/{} (built {} {})",
        version,
        build_time_str,
        rsipstack::VERSION
    )
}

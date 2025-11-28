use chrono::{DateTime, Local};

pub fn get_version_info() -> &'static str {
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

    Box::leak(
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
        .into_boxed_str(),
    )
}

pub fn get_short_version() -> &'static str {
    let version = env!("CARGO_PKG_VERSION");
    let git_commit = env!("GIT_COMMIT_HASH");
    let git_dirty = env!("GIT_DIRTY");
    let commercial = if cfg!(feature = "commerce") {
        "commerce"
    } else {
        "community"
    };
    if git_dirty == "dirty" {
        Box::leak(format!("{}-{}-dirty-{commercial}", version, git_commit).into_boxed_str())
    } else {
        Box::leak(format!("{}-{}-{commercial}", version, git_commit).into_boxed_str())
    }
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

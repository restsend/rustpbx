//! Linux `/proc` readers shared by the local stats log and the Prometheus
//! system-metrics sampler.
//!
//! Every reader degrades to `None` on platforms without `/proc` (macOS,
//! Windows) so callers simply skip that sample instead of failing.

/// Clock ticks per second (`sysconf(_SC_CLK_TCK)`); 100 on essentially every
/// Linux userspace configuration.
const CLK_TCK: u64 = 100;

/// Cumulative process CPU time (utime + stime) in whole seconds, read from
/// `/proc/self/stat`. Cumulative since process start — take deltas at the
/// call site.
pub fn process_cpu_seconds() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Field 2 (comm) may contain spaces and parentheses, so parse from after
    // the closing parenthesis. Fields after it start at #3 (state); utime is
    // field 14 (index 11 from here) and stime is field 15 (index 12).
    let after_comm = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) / CLK_TCK)
}

/// Resident set size of this process in bytes (`VmRSS` from
/// `/proc/self/status`).
pub fn resident_memory_bytes() -> Option<u64> {
    read_self_status_kb("VmRSS:").map(|kb| kb * 1024)
}

fn read_self_status_kb(prefix: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix(prefix))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|p| p.parse().ok())
}

/// Number of file descriptors the process currently holds open (entries in
/// `/proc/self/fd`, minus the directory handle itself).
pub fn open_fds() -> Option<usize> {
    let mut count = 0usize;
    for entry in std::fs::read_dir("/proc/self/fd").ok()? {
        if entry.is_ok() {
            count += 1;
        }
    }
    Some(count.saturating_sub(1))
}

/// Established TCP connections (IPv4 + IPv6) visible in `/proc/net/tcp{,6}`
/// (state `01`).
pub fn network_connections() -> Option<usize> {
    let mut count = 0usize;
    let mut any = false;
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        any = true;
        for line in text.lines().skip(1) {
            // sl local_address rem_address st ...
            if line.split_whitespace().nth(3) == Some("01") {
                count += 1;
            }
        }
    }
    if any { Some(count) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On Linux (CI runners) the readers must return values; on other
    /// platforms they must return `None` without panicking.
    #[test]
    fn readers_never_panic() {
        let cpu = process_cpu_seconds();
        let rss = resident_memory_bytes();
        let fds = open_fds();
        let conns = network_connections();
        if std::path::Path::new("/proc/self/stat").exists() {
            assert!(cpu.is_some(), "cpu time should be readable on Linux");
            assert!(rss.is_some(), "rss should be readable on Linux");
            assert!(fds.is_some(), "fd count should be readable on Linux");
            assert!(conns.is_some(), "tcp conns should be readable on Linux");
            assert!(fds.unwrap() > 0);
        } else {
            assert!(cpu.is_none() && rss.is_none() && fds.is_none() && conns.is_none());
        }
    }
}

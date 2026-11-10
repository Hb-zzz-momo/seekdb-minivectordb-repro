//! Resource limits of the cgroup the process runs in.
//!
//! Inside a container with its own cgroup namespace the limits are in
//! `/sys/fs/cgroup` itself. Without one (a host namespace, a limit set on a
//! parent group, a systemd unit) they are in the directory `/proc/self/cgroup`
//! names or in one of its ancestors, so every level is read and the
//! tightest value wins.

use std::fs;
use std::path::{Path, PathBuf};

const ROOT: &str = "/sys/fs/cgroup";

/// The process's own cgroup v2 directory and each ancestor up to the root of
/// the hierarchy. `proc_self_cgroup` is the text of `/proc/self/cgroup`;
/// without a `0::` line (cgroup v1 only) that is the root alone.
fn v2_dirs(proc_self_cgroup: &str) -> Vec<PathBuf> {
    let own = proc_self_cgroup.lines().find_map(|l| l.strip_prefix("0::")).unwrap_or("/");
    let mut dir = PathBuf::from(ROOT);
    let mut dirs = vec![dir.clone()];
    for part in own.trim().split('/').filter(|p| !matches!(*p, "" | "." | "..")) {
        dir.push(part);
        dirs.push(dir.clone());
    }
    dirs
}

/// The value of a limit file. `max`, anything unreadable and values of 2^60
/// or more (cgroup v1 writes "no limit" as a number near 2^63) are no limit.
fn parse_limit(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok().filter(|&v| v < 1 << 60)
}

/// `cpu.max`: `<quota> <period>` in microseconds, or `max <period>`.
fn parse_cpu_max(text: &str) -> Option<f64> {
    let mut fields = text.split_whitespace();
    let quota: f64 = fields.next()?.parse().ok()?;
    let period: f64 = fields.next()?.parse().ok()?;
    (quota > 0.0 && period > 0.0).then(|| quota / period)
}

/// `MemTotal` of `/proc/meminfo`, in bytes.
fn parse_mem_total(meminfo: &str) -> Option<u64> {
    let kb = meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:"))?.split_whitespace().next()?;
    kb.parse::<u64>().ok()?.checked_mul(1024)
}

fn read(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// The tightest memory limit that applies to this process, in bytes:
/// `memory.max` and `memory.high` of its cgroup and of every ancestor, and
/// the cgroup v1 limit.
pub fn memory_limit() -> Option<u64> {
    let own = read("/proc/self/cgroup").unwrap_or_default();
    let v2 = v2_dirs(&own).into_iter().flat_map(|d| [d.join("memory.max"), d.join("memory.high")]);
    let v1 = PathBuf::from(ROOT).join("memory/memory.limit_in_bytes");
    v2.chain([v1]).filter_map(|p| parse_limit(&read(p)?)).min()
}

/// The tightest CPU quota that applies to this process, in CPUs (1.5 is one
/// and a half), or `None` without one.
pub fn cpu_quota() -> Option<f64> {
    let own = read("/proc/self/cgroup").unwrap_or_default();
    let v2 = v2_dirs(&own).into_iter().filter_map(|d| parse_cpu_max(&read(d.join("cpu.max"))?));
    let v1 = ["cpu", "cpu,cpuacct"].into_iter().filter_map(|name| {
        let dir = PathBuf::from(ROOT).join(name);
        // A quota of -1 is no limit; it fails to parse as a positive number.
        parse_cpu_max(&format!("{} {}", read(dir.join("cpu.cfs_quota_us"))?.trim(), read(dir.join("cpu.cfs_period_us"))?.trim()))
    });
    v2.chain(v1).min_by(f64::total_cmp)
}

/// Whether a process under `quota` (from `cpu_quota`) can keep one thread
/// busy without being throttled.
pub fn has_a_whole_cpu(quota: Option<f64>) -> bool {
    quota.is_none_or(|q| q >= 1.0)
}

/// Physical memory of the machine, in bytes.
pub fn mem_total() -> Option<u64> {
    parse_mem_total(&read("/proc/meminfo")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(text: &str) -> Vec<String> {
        v2_dirs(text).iter().map(|d| d.display().to_string()).collect()
    }

    #[test]
    fn cgroup_directories() {
        // A container with its own namespace.
        assert_eq!(dirs("0::/\n"), ["/sys/fs/cgroup"]);
        // A systemd unit, or a container sharing the host's namespace.
        assert_eq!(
            dirs("0::/user.slice/user-1000.slice/run-u1.service\n"),
            [
                "/sys/fs/cgroup",
                "/sys/fs/cgroup/user.slice",
                "/sys/fs/cgroup/user.slice/user-1000.slice",
                "/sys/fs/cgroup/user.slice/user-1000.slice/run-u1.service",
            ]
        );
        // cgroup v1 only, an empty file, a path outside the namespace root.
        assert_eq!(dirs("12:memory:/docker/abc\n11:cpu,cpuacct:/docker/abc\n"), ["/sys/fs/cgroup"]);
        assert_eq!(dirs(""), ["/sys/fs/cgroup"]);
        assert_eq!(dirs("0::/../../a\n"), ["/sys/fs/cgroup", "/sys/fs/cgroup/a"]);
        // A hybrid setup: the v2 line is found among v1 lines.
        assert_eq!(dirs("5:memory:/x\n0::/y\n"), ["/sys/fs/cgroup", "/sys/fs/cgroup/y"]);
    }

    #[test]
    fn limit_values() {
        assert_eq!(parse_limit("67108864\n"), Some(64 << 20));
        assert_eq!(parse_limit("max\n"), None);
        assert_eq!(parse_limit("9223372036854771712\n"), None);
        assert_eq!(parse_limit(""), None);
        assert_eq!(parse_limit("-1"), None);
    }

    #[test]
    fn cpu_quotas() {
        assert_eq!(parse_cpu_max("200000 100000\n"), Some(2.0));
        assert_eq!(parse_cpu_max("50000 100000\n"), Some(0.5));
        assert_eq!(parse_cpu_max("max 100000\n"), None);
        assert_eq!(parse_cpu_max("-1 100000"), None);
        assert_eq!(parse_cpu_max(""), None);
    }

    #[test]
    fn whole_cpus() {
        // The judge's container has 2 CPUs; no quota at all also counts.
        assert!(has_a_whole_cpu(parse_cpu_max("200000 100000")));
        assert!(has_a_whole_cpu(parse_cpu_max("100000 100000")));
        assert!(has_a_whole_cpu(parse_cpu_max("max 100000")));
        assert!(has_a_whole_cpu(None));
        assert!(!has_a_whole_cpu(parse_cpu_max("50000 100000")));
        assert!(!has_a_whole_cpu(Some(0.99)));
    }

    #[test]
    fn meminfo() {
        assert_eq!(parse_mem_total("MemTotal:       16228352 kB\nMemFree: 1 kB\n"), Some(16228352 * 1024));
        assert_eq!(parse_mem_total("MemFree: 1 kB\n"), None);
    }

    /// The readers themselves: whatever this machine says, they must not
    /// fail, and a quota is positive.
    #[test]
    fn readers_run() {
        let _ = memory_limit();
        assert!(cpu_quota().is_none_or(|q| q > 0.0));
        assert!(mem_total().is_some_and(|m| m > 0));
    }
}

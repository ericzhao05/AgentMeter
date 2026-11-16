use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration as StdDuration, Instant, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};

/// How recent the newest log write must be for an agent to count as "active".
pub const ACTIVE_SECS: i64 = 120;

/// Recursively collect files under `root` (bounded depth) matching `filter`,
/// with their modification times. Missing/unreadable dirs yield nothing.
pub fn walk_files(
    root: &Path,
    max_depth: usize,
    filter: &dyn Fn(&Path) -> bool,
) -> Vec<(PathBuf, SystemTime)> {
    let mut out = Vec::new();
    walk_inner(root, max_depth, filter, &mut out);
    out
}

fn walk_inner(
    dir: &Path,
    depth_left: usize,
    filter: &dyn Fn(&Path) -> bool,
    out: &mut Vec<(PathBuf, SystemTime)>,
) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if depth_left > 0 {
                walk_inner(&path, depth_left - 1, filter, out);
            }
        } else if ft.is_file() && filter(&path) {
            if let Ok(m) = entry.metadata().and_then(|m| m.modified()) {
                out.push((path, m));
            }
        }
    }
}

pub fn is_jsonl(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "jsonl")
}

/// Newest modification time among files under `root` (bounded depth).
pub fn newest_mtime(root: &Path, max_depth: usize) -> Option<SystemTime> {
    walk_files(root, max_depth, &|_| true).into_iter().map(|(_, m)| m).max()
}

/// Whether `t` is within [`ACTIVE_SECS`] of `now` (small future skew tolerated).
pub fn is_recent(t: Option<SystemTime>, now: DateTime<Utc>) -> bool {
    match t {
        Some(t) => {
            let t: DateTime<Utc> = t.into();
            let age = (now - t).num_seconds();
            (-30..=ACTIVE_SECS).contains(&age)
        }
        None => false,
    }
}

pub fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// "300" minutes -> "5h", 10080 -> "7d", 90 -> "90m".
pub fn window_label(minutes: i64) -> String {
    if minutes > 0 && minutes % 1440 == 0 {
        format!("{}d", minutes / 1440)
    } else if minutes > 0 && minutes % 60 == 0 {
        format!("{}h", minutes / 60)
    } else {
        format!("{minutes}m")
    }
}

pub fn clamp_pct(v: f64) -> f64 {
    if v.is_nan() {
        0.0
    } else {
        v.clamp(0.0, 100.0)
    }
}

/// Round to one decimal for stable display.
pub fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Run `cmd` and return its stdout if it exits successfully within `timeout`.
/// On timeout the child is killed and reaped, and an error is returned, so a
/// subprocess waiting on user interaction (e.g. a Keychain prompt) cannot hang
/// the caller. stdin/stderr are null; stdout is drained on a helper thread.
pub fn run_with_deadline(cmd: &mut Command, timeout: StdDuration) -> Result<Vec<u8>, String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start subprocess ({})", e.kind()))?;
    let mut stdout = child.stdout.take().ok_or("subprocess has no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() >= deadline => break None,
            Ok(None) => std::thread::sleep(StdDuration::from_millis(20)),
            Err(_) => break None,
        }
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        return Err(format!("subprocess timed out after {}s", timeout.as_secs_f32()));
    };
    let out = reader.join().unwrap_or_default();
    if status.success() {
        Ok(out)
    } else {
        Err(format!("subprocess exited with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn deadline_kills_hung_subprocess() {
        let start = Instant::now();
        let r = run_with_deadline(Command::new("/bin/sleep").arg("10"), StdDuration::from_millis(200));
        assert!(r.unwrap_err().contains("timed out"));
        assert!(start.elapsed() < StdDuration::from_secs(3));
    }

    #[test]
    fn deadline_returns_output_and_failures() {
        let out = run_with_deadline(Command::new("/bin/echo").arg("hi"), StdDuration::from_secs(5)).unwrap();
        assert_eq!(out, b"hi\n");
        assert!(run_with_deadline(&mut Command::new("/usr/bin/false"), StdDuration::from_secs(5)).is_err());
        assert!(run_with_deadline(&mut Command::new("/no/such/bin"), StdDuration::from_secs(1)).is_err());
    }

    #[test]
    fn labels() {
        assert_eq!(window_label(300), "5h");
        assert_eq!(window_label(10080), "7d");
        assert_eq!(window_label(90), "90m");
    }

    #[test]
    fn recency() {
        let now = Utc::now();
        let s = |d: i64| Some(SystemTime::from(now - Duration::seconds(d)));
        assert!(is_recent(s(10), now));
        assert!(!is_recent(s(600), now));
        assert!(!is_recent(None, now));
    }

    #[test]
    fn walk_missing_dir_is_empty() {
        assert!(walk_files(Path::new("/definitely/not/here"), 3, &|_| true).is_empty());
    }

    #[test]
    fn clamps() {
        assert_eq!(clamp_pct(-5.0), 0.0);
        assert_eq!(clamp_pct(150.0), 100.0);
        assert_eq!(clamp_pct(f64::NAN), 0.0);
    }
}

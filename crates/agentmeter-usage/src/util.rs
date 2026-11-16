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
///
/// The child runs in its own process group, and the whole group is killed on
/// timeout. Collecting stdout is also deadline-bound (the reader thread is
/// detached if it doesn't finish), so a descendant that inherited the pipe
/// can't block the caller.
pub fn run_with_deadline(cmd: &mut Command, timeout: StdDuration) -> Result<Vec<u8>, String> {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(cmd, 0);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start subprocess ({})", e.kind()))?;
    let mut stdout = child.stdout.take().ok_or("subprocess has no stdout")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
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
        kill_group(&mut child);
        return Err(format!("subprocess timed out after {}s", timeout.as_secs_f32()));
    };
    // The child has exited; wait for stdout only for the remaining budget (plus
    // a short grace period), then give up and detach the reader.
    let grace = deadline.saturating_duration_since(Instant::now()) + StdDuration::from_millis(100);
    let Ok(out) = rx.recv_timeout(grace) else {
        kill_group(&mut child);
        return Err(format!("subprocess output not closed within {}s", timeout.as_secs_f32()));
    };
    if status.success() {
        Ok(out)
    } else {
        Err(format!("subprocess exited with {status}"))
    }
}

/// Kill the child's whole process group (best effort), then reap the child.
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // Negative pid = process group (the child is its group leader).
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn deadline_holds_when_descendant_keeps_pipe_open() {
        let start = Instant::now();
        let r = run_with_deadline(
            Command::new("/bin/sh").args(["-c", "sleep 30 & sleep 30"]),
            StdDuration::from_millis(300),
        );
        assert!(r.is_err());
        assert!(start.elapsed() < StdDuration::from_secs(3), "took {:?}", start.elapsed());
    }

    #[test]
    fn deadline_holds_when_child_exits_but_descendant_lingers() {
        // Parent exits right away; a background child still holds stdout.
        let start = Instant::now();
        let r = run_with_deadline(
            Command::new("/bin/sh").args(["-c", "sleep 30 & echo hi"]),
            StdDuration::from_millis(300),
        );
        assert!(r.is_err());
        assert!(start.elapsed() < StdDuration::from_secs(3), "took {:?}", start.elapsed());
    }

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

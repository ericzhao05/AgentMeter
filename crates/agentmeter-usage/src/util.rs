use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

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

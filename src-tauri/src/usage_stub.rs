//! TEMPORARY stand-in for the `agentmeter-usage` crate (crates/agentmeter-usage, built in parallel).
//! Same public API as the shared contract. Delete this file and change the one `use ... as usage;`
//! line in lib.rs once the real crate is merged.

use serde::Serialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AgentUsage {
    pub id: String,
    pub name: String,
    pub installed: bool,
    pub percent_left: Option<f64>,
    pub resets_at: Option<String>,
    pub window: Option<String>,
    pub active: bool,
    pub exact: bool,
    pub note: Option<String>,
}

/// The stub has no real detection: it reports demo data.
pub fn poll_all() -> Vec<AgentUsage> {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
    mock_all(t)
}

/// Demo data: levels drain slowly and refill (window reset) every few minutes.
pub fn mock_all(t: f64) -> Vec<AgentUsage> {
    // (id, name, cycle seconds, phase, exact, known)
    let spec: [(&str, &str, f64, f64, bool, bool); 5] = [
        ("claude", "Claude", 240.0, 0.15, true, true),
        ("codex", "Codex", 180.0, 0.55, true, true),
        ("gemini", "Gemini", 300.0, 0.80, false, true),
        ("cursor", "Cursor", 200.0, 0.30, true, false),
        ("copilot", "Copilot", 260.0, 0.65, false, true),
    ];
    spec.iter()
        .enumerate()
        .map(|(i, &(id, name, cycle, phase, exact, known))| {
            let p = ((t / cycle) + phase).fract(); // 0 → 1 over one window
            let percent = (100.0 * (1.0 - p)).clamp(0.0, 100.0);
            let secs_left = ((1.0 - p) * cycle).max(0.0) as u64;
            AgentUsage {
                id: id.into(),
                name: name.into(),
                installed: true,
                percent_left: known.then(|| (percent * 10.0).round() / 10.0),
                resets_at: known.then(|| rfc3339(SystemTime::now() + Duration::from_secs(secs_left))),
                window: known.then(|| "5h".into()),
                active: ((t / 20.0) as usize + i) % 3 != 0,
                exact,
                note: (!known).then(|| "demo: usage not readable".into()),
            }
        })
        .collect()
}

/// Minimal UTC RFC3339 formatter (avoids a chrono dependency in the stub).
fn rfc3339(at: SystemTime) -> String {
    let secs = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

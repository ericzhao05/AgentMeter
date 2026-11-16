//! `cargo run --example poll` — one real poll, printed as JSON (no secrets are ever in the output).
//! `cargo run --example poll -- --offline` — file-based sources only (no Keychain/network).
//! `cargo run --example poll -- --mock 42` — mock data at t=42s.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rows = if args.get(1).map(String::as_str) == Some("--mock") {
        let t = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        agentmeter_usage::mock_all(t)
    } else if args.get(1).map(String::as_str) == Some("--offline") {
        agentmeter_usage::poll_all_at(&dirs::home_dir().expect("home dir"))
    } else {
        agentmeter_usage::poll_all()
    };
    println!("{}", serde_json::to_string_pretty(&rows).unwrap());
}

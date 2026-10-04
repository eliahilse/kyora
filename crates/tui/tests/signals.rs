#![cfg(unix)]

#[test]
fn signals_after_tui_returns() {
    const CHILD: &str = "KYORA_TEST_SIGNAL_PTY_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        // Re-entering the TUI must install working handlers again.
        for session in 1..=2 {
            runtime.block_on(kyora_tui::run(false)).unwrap();
            println!("TUI session {session} returned");
        }
        loop {
            std::thread::park();
        }
    }
    let output = std::process::Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/signals_pty.py"))
        .arg(std::env::current_exe().unwrap())
        .env(CHILD, "1")
        .output()
        .expect("run the signal disposition regression with python3");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

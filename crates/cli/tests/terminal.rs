#[cfg(unix)]
#[test]
fn terminal_restores_after_signals_and_keyboard_quit() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/terminal_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_kyora"))
        .output()
        .expect("run the PTY regression with python3");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

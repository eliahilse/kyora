use std::process::Command;

#[test]
fn version() {
    let output = Command::new(env!("CARGO_BIN_EXE_kyora"))
        .arg("--version")
        .output()
        .expect("run kyora");
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .starts_with("kyora ")
    );
}

#[test]
fn help() {
    let output = Command::new(env!("CARGO_BIN_EXE_kyora"))
        .arg("--help")
        .output()
        .expect("run kyora");
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().contains("run"));
}

#[test]
fn run_is_not_implemented() {
    let output = Command::new(env!("CARGO_BIN_EXE_kyora"))
        .args(["run", "test task"])
        .output()
        .expect("run kyora");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "error: not implemented yet\n"
    );
}

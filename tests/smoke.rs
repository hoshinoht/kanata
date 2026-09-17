mod support;

#[test]
fn binary_reports_package_version() {
    let output = support::run_kanata();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout should be UTF-8"),
        format!("kanata {}\n", env!("CARGO_PKG_VERSION"))
    );
}

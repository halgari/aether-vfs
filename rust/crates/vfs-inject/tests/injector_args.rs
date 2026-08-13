use vfs_inject::parse_injector_args;

#[test]
fn parses_positional_and_double_dash_args() {
    let a: Vec<String> = ["prog", "t.exe", "s.dll", "p.dll", "c.cfg", "r.flag", "--", "x", "y"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let got = parse_injector_args(&a).unwrap();
    let (t, s, p, c, r, args) =
        (got.target, got.shim_dll, got.payload_dll, got.config, got.ready, got.target_args);
    assert_eq!(
        (t.as_str(), s.as_str(), p.as_str(), c.as_str(), r.as_str()),
        ("t.exe", "s.dll", "p.dll", "c.cfg", "r.flag")
    );
    assert_eq!(args, vec!["x".to_string(), "y".to_string()]);
}

#[test]
fn rejects_too_few_args() {
    assert!(parse_injector_args(&["prog".into(), "t".into()]).is_err());
}

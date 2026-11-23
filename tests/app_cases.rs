#[test]
fn cli_app_cases() {
    trycmd::TestCases::new().case("tests/app-cases/*.toml");
}

//! Test-only driver for `maxplayer_trade::cli` (see the `[[bin]]` note in Cargo.toml). Users run
//! `maxplayer trade`; this binary is built only with the `local-test-mints` feature.
fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(maxplayer_trade::cli::main_from(std::env::args_os()))
}

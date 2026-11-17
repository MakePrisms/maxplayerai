//! Test-only driver for `maxplayer_trade::cli` (see the `[[bin]]` note in Cargo.toml). Users run
//! `maxplayer trade`; this binary is built only with the `local-test-mints` feature.
//!
//! With `lab`, two extra entry points drive the money code the CLI no longer exposes (money in
//! and out moved to `maxplayer wallet`) in a real process, so its crash hooks
//! (`TRADE_CRASH_*`) can leave journaled receive/withdrawal records that `recover` must finish:
//! `__lab-receive <home> <mint>` (token on stdin) and `__lab-withdraw <home> <mint> <bolt11>`.
fn main() -> std::process::ExitCode {
    #[cfg(feature = "lab")]
    {
        let args: Vec<String> = std::env::args().collect();
        if let Some(code) = lab_money(&args) {
            return std::process::ExitCode::from(code);
        }
    }
    std::process::ExitCode::from(maxplayer_trade::cli::main_from(std::env::args_os()))
}

#[cfg(feature = "lab")]
fn lab_money(args: &[String]) -> Option<u8> {
    use maxplayer_trade::{journal::Journal, money, receive};
    use std::path::Path;
    let command = args.get(1)?.as_str();
    if command != "__lab-receive" && command != "__lab-withdraw" {
        return None;
    }
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    Some(rt.block_on(async {
        let (home, mint) = (Path::new(&args[2]), args[3].as_str());
        let j = Journal::open(home).await.expect("journal");
        let result = if command == "__lab-receive" {
            let mut token = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut token).expect("stdin");
            receive::receive(home, &j, mint, token.trim())
                .await
                .and_then(|r| receive::verdict(&r))
        } else {
            money::withdraw(home, &j, mint, &args[4]).await.map(|_| ())
        };
        match result {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{error}");
                1
            }
        }
    }))
}

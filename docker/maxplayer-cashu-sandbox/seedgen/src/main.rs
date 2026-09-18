//! test-mint-seed — print one fresh BIP-39 mnemonic on stdout, nothing else.
//!
//! `cdk-mintd` has no seed auto-generation: `lib.rs:1238` bails with "No seed nor remote signatory
//! set", and `--seed-file` is parsed as a BIP-39 mnemonic. The `test-mint` controller redirects this
//! output straight into a 0600 file under a umask of 077 and never echoes it.

fn main() {
    let mnemonic = bip39::Mnemonic::generate(12).expect("generate mnemonic");
    println!("{mnemonic}");
}

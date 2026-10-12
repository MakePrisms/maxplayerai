fn main() {
    println!("cargo:rerun-if-env-changed=MAXPLAYER_MINT_BUILD");
    println!("cargo:rerun-if-changed=../../.git");
    let revision = std::env::var("MAXPLAYER_MINT_BUILD").ok().or_else(|| {
        std::process::Command::new("git").args(["rev-parse", "HEAD"]).output().ok()
            .filter(|o| o.status.success()).and_then(|o| String::from_utf8(o.stdout).ok())
    }).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=MAXPLAYER_MINT_BUILD={}", revision.trim());
}

fn git(args: &[&str]) -> Option<String> {
    std::process::Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
}
fn main() {
    // Cargo also exports rustc-env to test processes; avoid the root CLI config prefix.
    println!("cargo:rerun-if-env-changed=MINT_SIDECAR_BUILD");
    println!("cargo:rerun-if-changed=src");
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&["rev-parse", "--git-path", &branch]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let revision = std::env::var("MINT_SIDECAR_BUILD")
        .ok()
        .or_else(|| {
            git(&["rev-parse", "HEAD"]).map(|mut revision| {
                if git(&["diff", "--quiet", "HEAD", "--", "src"]).is_none() {
                    revision.push_str("-dirty");
                }
                revision
            })
        })
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=MINT_SIDECAR_BUILD={}", revision.trim());
}

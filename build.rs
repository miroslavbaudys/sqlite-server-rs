use std::process::Command;

// Embed the git branch and commit hash so `--version` can report them, mirroring the
// SQLITE_SERVER_GIT_BRANCH / SQLITE_SERVER_GIT_COMMIT_HASH macros in the C++ build.
fn main() {
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=GIT_BRANCH={branch}");
    println!("cargo:rustc-env=GIT_COMMIT_HASH={commit}");
    println!("cargo:rerun-if-changed=.git/HEAD");
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

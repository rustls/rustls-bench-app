use std::process::Command;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // In some cases if new migrations are added without accompanying Rust src changes
    // `cargo build` isn't smart enough to detect the need for recompilation to pick up
    // the new embedded migrations. This build script is the recommended workaround.
    // See <https://docs.rs/sqlx/latest/sqlx/macro.migrate.html#triggering-recompilation-on-migration-changes>
    println!("cargo:rerun-if-changed=migrations");

    // Trigger recompilation if the repository's head changed, to make sure the commit information
    // embedded in the binary is up-to-date
    println!("cargo:rerun-if-changed=.git/HEAD");

    // Expose git head information through environment variables at compile-time, so it can be
    // embedded in the binary through the `env!` macro
    let output = Command::new("git").args(["rev-parse", "HEAD"]).output()?;
    let git_sha = String::from_utf8(output.stdout)?;
    println!("cargo:rustc-env=GIT_HEAD_SHA={}", git_sha);

    let output = Command::new("git")
        .args(["show-branch", "--no-name", "HEAD"])
        .output()?;
    let git_commit_message = String::from_utf8(output.stdout)?;
    println!("cargo:rustc-env=GIT_HEAD_COMMIT_MESSAGE={git_commit_message}");

    Ok(())
}

fn main() {
    // In some cases if new migrations are added without accompanying Rust src changes
    // `cargo build` isn't smart enough to detect the need for recompilation to pick up
    // the new embedded migrations. This build script is the recommended workaround.
    // See <https://docs.rs/sqlx/latest/sqlx/macro.migrate.html#triggering-recompilation-on-migration-changes>
    println!("cargo:rerun-if-changed=migrations");
}

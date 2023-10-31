use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{bail, Context};
use tracing::{debug, error, info};

use crate::RepoAndSha;

pub trait BenchRunner: Send + Sync {
    /// Checkout the specified repository at the given ref and run the benchmarks
    ///
    /// Returns the hash of the benchmarked commit
    fn checkout_and_run_benchmarks(
        &self,
        repo_and_ref: &RepoAndSha,
        local_rustls_dir: &Path,
        job_output_dir: &Path,
        command_logs: &mut Vec<Log>,
    ) -> anyhow::Result<()>;
}

pub struct LocalBenchRunner;

impl BenchRunner for LocalBenchRunner {
    fn checkout_and_run_benchmarks(
        &self,
        repo_and_ref: &RepoAndSha,
        local_rustls_dir: &Path,
        job_output_dir: &Path,
        command_logs: &mut Vec<Log>,
    ) -> anyhow::Result<()> {
        info!(
            "checking out {} at ref {}",
            repo_and_ref.clone_url, repo_and_ref.commit_sha
        );
        debug!("local repo directory: {}", local_rustls_dir.display());

        // Init
        let mut command = Command::new("git");
        command.arg("init").current_dir(local_rustls_dir);

        run_command(command, command_logs)?;

        // Configure remote
        let mut command = Command::new("git");
        command
            .arg("remote")
            .arg("add")
            .arg("origin")
            .arg(&repo_and_ref.clone_url)
            .current_dir(local_rustls_dir);

        run_command(command, command_logs)?;

        // Fetch relevant ref
        let git_ref = &repo_and_ref.commit_sha;
        let mut command = Command::new("git");
        command
            .arg("fetch")
            .arg("origin")
            .arg(git_ref)
            .current_dir(local_rustls_dir);

        run_command(command, command_logs)?;

        // Checkout ref
        let mut command = Command::new("git");
        command
            .arg("checkout")
            .arg(git_ref)
            .current_dir(local_rustls_dir);

        run_command(command, command_logs)?;

        // Build benchmarks
        let bench_path = local_rustls_dir.join("ci-bench");
        info!("building ci benchmarks");
        debug!("ci-bench path: {}", bench_path.display());
        let start = Instant::now();
        let mut command = Command::new("cargo");
        command
            .arg("build")
            .arg("--locked")
            .arg("--release")
            .current_dir(&bench_path);

        run_command(command, command_logs)?;

        debug!(
            "benchmarks built in {:.2} s",
            (Instant::now() - start).as_secs_f64()
        );

        // Run benchmarks
        info!("running ci benchmarks");
        let bench_exe_path = local_rustls_dir.join("target/release/rustls-ci-bench");
        debug!("ci-bench executable path: {}", bench_exe_path.display());

        fs::create_dir_all(job_output_dir).context("Unable to create dir for job output")?;

        let start = Instant::now();
        let mut command = Command::new(&bench_exe_path);
        command
            .arg("run-all")
            .arg("--output-dir")
            .arg(job_output_dir.join("results"))
            .current_dir(&bench_path);

        run_command(command, command_logs)?;

        info!(
            "benchmarks run in {:.2}",
            (Instant::now() - start).as_secs_f64()
        );

        Ok(())
    }
}

pub struct Log {
    pub command: String,
    pub cwd: String,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

fn run_command(mut command: Command, logs: &mut Vec<Log>) -> anyhow::Result<()> {
    let mut command_str = String::new();
    command_str.push_str(&command.get_program().to_string_lossy());
    for arg in command.get_args() {
        command_str.push(' ');
        command_str.push_str(&arg.to_string_lossy());
    }

    let cwd = command
        .get_current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or("/".to_string());

    let output = command.output().context(format!(
        "failed to run command: `{command_str}` at cwd `{cwd}`"
    ))?;

    logs.push(Log {
        command: command_str,
        cwd,
        stdout: output.stdout,
        stderr: output.stderr,
    });

    if !output.status.success() {
        let command_str = &logs.last().unwrap().command;
        error!(
            "`{command_str}` exited with exit status {:?}",
            output.status.code()
        );
        bail!(
            "`{command_str}` exited with exit status {:?}",
            output.status.code()
        );
    }

    Ok(())
}

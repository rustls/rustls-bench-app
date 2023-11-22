use std::fmt::Write;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{bail, Context};
use tracing::trace;

use crate::job::walltimes_path;
use crate::CommitIdentifier;

pub trait BenchRunner: Send + Sync {
    /// Checks out the specified commit and runs the benchmarks
    fn checkout_and_run_benchmarks(
        &self,
        commit: &CommitIdentifier,
        checkout_target_dir: &Path,
        job_output_dir: &Path,
        command_logs: &mut Vec<Log>,
    ) -> anyhow::Result<()>;
}

/// A bench runner that runs benchmarks locally
#[derive(Debug)]
pub struct LocalBenchRunner;

impl BenchRunner for LocalBenchRunner {
    fn checkout_and_run_benchmarks(
        &self,
        commit: &CommitIdentifier,
        checkout_target_dir: &Path,
        job_output_dir: &Path,
        command_logs: &mut Vec<Log>,
    ) -> anyhow::Result<()> {
        trace!(
            "checking out {} at commit {}",
            commit.clone_url,
            commit.commit_sha
        );
        trace!(
            "checkout target directory: {}",
            checkout_target_dir.display()
        );

        // Init
        let mut command = Command::new("git");
        command.arg("init").current_dir(checkout_target_dir);

        run_command(command, command_logs)?;

        // Configure remote
        let mut command = Command::new("git");
        command
            .arg("remote")
            .arg("add")
            .arg("origin")
            .arg(&commit.clone_url)
            .current_dir(checkout_target_dir);

        run_command(command, command_logs)?;

        // Fetch relevant commit
        let git_ref = &commit.commit_sha;
        let mut command = Command::new("git");
        command
            .arg("fetch")
            .arg("origin")
            .arg(git_ref)
            .current_dir(checkout_target_dir);

        run_command(command, command_logs)?;

        // Checkout commit
        let mut command = Command::new("git");
        command
            .arg("checkout")
            .arg(git_ref)
            .current_dir(checkout_target_dir);

        run_command(command, command_logs)?;

        // Build benchmarks
        let bench_path = checkout_target_dir.join("ci-bench");
        trace!("building benchmarks");

        let start = Instant::now();
        let mut command = Command::new("cargo");
        command
            .arg("build")
            .arg("--locked")
            .arg("--release")
            .current_dir(&bench_path);

        run_command(command, command_logs)?;

        trace!(
            "benchmarks built in {:.2} s",
            (Instant::now() - start).as_secs_f64()
        );

        // Run icount benchmarks
        let bench_exe_path = checkout_target_dir.join("target/release/rustls-ci-bench");
        fs::create_dir_all(job_output_dir).context("Unable to create dir for job output")?;

        let start = Instant::now();
        let mut command = Command::new(&bench_exe_path);
        command
            .arg("run-all")
            .arg("--output-dir")
            .arg(job_output_dir.join("results"))
            .current_dir(&bench_path);

        run_command(command, command_logs)?;

        trace!(
            "icount benchmarks run in {:.2} s",
            (Instant::now() - start).as_secs_f64()
        );

        // Run walltime benchmarks (under setarch to disable ASLR, to reduce noise)
        trace!("running walltime benchmarks");
        let start = Instant::now();

        let mut command = Command::new("setarch");
        command
            .arg("-R")
            .arg(bench_exe_path)
            .arg("walltime")
            .arg("--iterations-per-scenario")
            .arg("100")
            .current_dir(&bench_path);

        run_command(command, command_logs)?;

        // The walltimes are printed to stdout and captured in the logs, but we want them in a file
        fs::write(
            walltimes_path(job_output_dir),
            &command_logs.last().unwrap().stdout,
        )
        .context("failed to write walltimes to disk")?;

        trace!(
            "walltime benchmarks run in {:.2} s",
            (Instant::now() - start).as_secs_f64()
        );

        Ok(())
    }
}

/// Runs a command and pushes its logs to the provided buffer
fn run_command(mut command: Command, logs: &mut Vec<Log>) -> anyhow::Result<()> {
    // Get the command string
    let mut command_str = String::new();
    command_str.push_str(&command.get_program().to_string_lossy());
    for arg in command.get_args() {
        command_str.push(' ');
        command_str.push_str(&arg.to_string_lossy());
    }

    // Get the command's CWD
    let cwd = command
        .get_current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or("/".to_string());

    // Run the command
    let output = command.output().context(format!(
        "failed to start command: `{command_str}` at cwd `{cwd}`"
    ))?;

    logs.push(Log {
        command: command_str,
        cwd,
        stdout: output.stdout,
        stderr: output.stderr,
    });

    // Propagate errors
    if !output.status.success() {
        let command_str = &logs.last().unwrap().command;
        bail!(
            "`{command_str}` exited with exit status {:?}",
            output.status.code()
        );
    }

    Ok(())
}

/// Logs for a specific command
#[derive(Debug)]
pub struct Log {
    /// The command in question
    pub command: String,
    /// The current working directory of the command
    pub cwd: String,
    /// The command's stdout output
    pub stdout: Vec<u8>,
    /// The command's stderr output
    pub stderr: Vec<u8>,
}

pub fn write_logs_for_run(s: &mut String, logs: &[Log]) {
    if logs.is_empty() {
        writeln!(s, "_Not available_").ok();
    }

    for log in logs {
        write_log_part(s, "command", &log.command);
        write_log_part(s, "cwd", &log.cwd);
        write_log_part(s, "stdout", &String::from_utf8_lossy(&log.stdout));
        write_log_part(s, "stderr", &String::from_utf8_lossy(&log.stderr));
    }
}

fn write_log_part(s: &mut String, part_name: &str, part: &str) {
    write!(s, "{part_name}:").ok();
    if part.trim().is_empty() {
        writeln!(s, " _empty_.\n").ok();
    } else {
        writeln!(s, "\n```\n{}\n```\n", part.trim_end()).ok();
    }
}

//! Long-lived, headless, single-host Kojora worker.
//! Private Unix socket only. This is not a network service or remote worker.

use kojora_runtime::ProcessLimits;
use kojora_studio::{
    checks::{CheckKind, CheckPolicy},
    default_data_dir,
    scheduler::AdmissionPolicy,
    worker::{serve_with_checks, WorkerClient},
    Studio,
};
use std::io;
use std::path::PathBuf;
use std::str::FromStr;
use std::{
    env,
    ffi::{OsStr, OsString},
};

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Serve,
    Status,
    Stop,
}

const USAGE: &str = "Usage: kojora-worker [OPTIONS]

  --data-dir ABSOLUTE_DIR           Worker state and socket directory
  --address-space-mib N             Per-process virtual memory ceiling
  --cpu-seconds N                   Per-process CPU time ceiling
  --max-parallel N                  Maximum concurrent agent tasks
  --memory-reservation-mib N        Memory reserved per task
  --total-memory-reservation-mib N  Total admission budget
  --enable-checks                   Enable local checks on trusted worktrees
  --post-edit-check PRESET          Queue a preset after a successful edit
  --pre-merge-check PRESET          Allow an explicitly requested pre-merge preset
  --check-timeout-seconds N         Wall-clock limit per check
  --max-pending-checks N            Maximum queued and running checks
  --status                          Show the running Worker status
  --stop                            Stop an idle Worker
  -h, --help                        Show this help

Presets: rustfmt, clippy, test, ruff, biome";

fn parse_number<T: FromStr>(
    args: &mut impl Iterator<Item = OsString>,
    flag: &OsStr,
) -> io::Result<T> {
    args.next()
        .and_then(|value| value.to_str()?.parse().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{flag:?} requires a number"),
            )
        })
}

fn parse(
    mut args: impl Iterator<Item = OsString>,
) -> io::Result<(PathBuf, ProcessLimits, Mode, AdmissionPolicy, CheckPolicy)> {
    let mut data_dir = None;
    let mut limits = ProcessLimits::default();
    let mut admission = AdmissionPolicy::single();
    let mut checks = CheckPolicy::default();
    let mut mode = Mode::Serve;
    while let Some(argument) = args.next() {
        match argument.to_string_lossy().as_ref() {
            "--data-dir" => {
                data_dir = Some(PathBuf::from(args.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--data-dir requires a path")
                })?));
            }
            "--address-space-mib" => {
                limits.address_space_mib = Some(parse_number(&mut args, &argument)?)
            }
            "--cpu-seconds" => limits.cpu_seconds = Some(parse_number(&mut args, &argument)?),
            "--max-parallel" => admission.max_parallel = parse_number(&mut args, &argument)?,
            "--memory-reservation-mib" => {
                admission.memory_mib_per_task = parse_number(&mut args, &argument)?
            }
            "--total-memory-reservation-mib" => {
                admission.total_memory_mib = parse_number(&mut args, &argument)?
            }
            "--enable-checks" => checks.enabled = true,
            "--post-edit-check" | "--pre-merge-check" => {
                let flag = argument.to_string_lossy();
                let value = args.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "check flag requires a preset name",
                    )
                })?;
                let kind = match value.to_string_lossy().as_ref() {
                    "rustfmt" => CheckKind::RustFmt,
                    "clippy" => CheckKind::RustClippy,
                    "test" => CheckKind::RustTest,
                    "ruff" => CheckKind::Ruff,
                    "biome" => CheckKind::Biome,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "check must be rustfmt, clippy, test, ruff or biome",
                        ))
                    }
                };
                if flag == "--post-edit-check" {
                    if !checks.post_edit.contains(&kind) {
                        checks.post_edit.push(kind);
                    }
                } else if !checks.pre_merge.contains(&kind) {
                    checks.pre_merge.push(kind);
                }
            }
            "--check-timeout-seconds" => checks.timeout_secs = parse_number(&mut args, &argument)?,
            "--max-pending-checks" => checks.max_pending = parse_number(&mut args, &argument)?,
            "--status" if mode == Mode::Serve => mode = Mode::Status,
            "--stop" if mode == Mode::Serve => mode = Mode::Stop,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument {:?}; run kojora-worker --help", argument),
                ))
            }
        }
    }
    let data_dir = data_dir.map_or_else(default_data_dir, Ok)?;
    if !data_dir.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "data directory must be absolute",
        ));
    }
    limits.validate()?;
    admission.validate()?;
    if admission.max_parallel > 1 && limits.address_space_mib.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "parallel runs require an explicit per-process address-space ceiling",
        ));
    }
    checks.limits = limits;
    checks.validate()?;
    Ok((data_dir, limits, mode, admission, checks))
}

fn run() -> io::Result<()> {
    let mut args = env::args_os().skip(1).peekable();
    if args
        .peek()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!("{USAGE}");
        return Ok(());
    }
    let (root, limits, mode, admission, checks) = parse(args)?;
    let socket = root.join("worker.sock");
    match mode {
        Mode::Serve => {
            let studio = Studio::open_with_limits_and_policy(root, limits, admission)?;
            serve_with_checks(studio, limits, checks)
        }
        Mode::Status => {
            let client = WorkerClient::connect(socket)?;
            let state = client.status()?;
            println!(
                "Worker pid={} protocol={} active_task={:?} active_count={} max_parallel={} reserved_mib={} checks_pending={} address_space_mib={:?} cpu_seconds={:?}",
                state.pid, state.protocol, state.active_task, state.active_count, state.concurrency,
                state.reserved_memory_mib, state.checks_pending, state.address_space_mib, state.cpu_seconds
            );
            Ok(())
        }
        Mode::Stop => {
            let client = WorkerClient::connect(socket)?;
            client.shutdown()?;
            println!("Worker shutdown requested");
            Ok(())
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("kojora-worker: {error}");
        std::process::exit(1);
    }
}

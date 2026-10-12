use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use grim_reaper::{DEFAULT_STATE_DIR, Status};
use serde::Serialize;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

const INSTALLED_BINARY: &str = "/usr/local/bin/grim-reaper";
#[cfg(target_os = "linux")]
const SERVICE_NAME: &str = "grim-reaper.service";
#[cfg(target_os = "linux")]
const SERVICE_PATH: &str = "/etc/systemd/system/grim-reaper.service";
#[cfg(target_os = "linux")]
const LEGACY_SERVICE: &str = "grim_reaper.service";
const SUDOERS_PATH: &str = "/etc/sudoers.d/grim-reaper";
#[cfg(target_os = "linux")]
const SERVICE_CONTENT: &str = include_str!("../systemd/grim-reaper.service");
const REMOTE_HEARTBEAT: &str = "sudo -n -- /usr/local/bin/grim-reaper heartbeat";
const REMOTE_STATUS: &str = "sudo -n -- /usr/local/bin/grim-reaper status --json";
#[cfg(target_os = "macos")]
const LAUNCHD_LABEL: &str = "system/local.grim-reaper";
#[cfg(target_os = "macos")]
const LAUNCHD_PATH: &str = "/Library/LaunchDaemons/local.grim-reaper.plist";
#[cfg(target_os = "macos")]
const LAUNCHD_CONTENT: &str = include_str!("../launchd/local.grim-reaper.plist");
#[cfg(target_os = "macos")]
const MAC_LOG_PATH: &str = "/var/log/grim-reaper.log";
const HELPER_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HELPER_STREAM_BYTES: usize = 64 * 1024;

#[derive(Parser)]
#[command(
    version,
    about = "Manual fleet heartbeats and a local Linux/macOS dead-man switch"
)]
#[command(
    long_about = "An armed target shuts down when its manual heartbeat expires, after boot grace. Linux attempts abrupt HARD POWEROFF; macOS uses native HALT/POWEROFF attempts, first quick cache-flushing attempts and then hard no-sync fallbacks, never a reboot or panic fallback. Shutdown methods repeat until the machine is dead; graceful shutdown is not guaranteed. No automatic heartbeat sender is provided. The reserved target 'local' uses the installed local root daemon through passwordless sudo; other targets use SSH. Install the local root daemon first, then explicitly save it with 'targets add local'."
)]
struct Cli {
    #[arg(long, global = true, default_value = DEFAULT_STATE_DIR)]
    state_dir: PathBuf,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the Linux/macOS shutdown monitor; dry-run never executes shutdown methods.
    Daemon {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        once: bool,
    },
    /// Record one local heartbeat receipt (normally called through sudo or SSH).
    Heartbeat,
    /// Show the shutdown configuration, phase, and deadline.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Arm abrupt shutdown; this does not send a recurring heartbeat.
    Arm {
        #[arg(long, required = true)]
        confirm_hard_poweroff: bool,
    },
    /// Disarm and clear any persisted shutdown latch.
    Disarm,
    /// Install the user CLI, or the disarmed Linux systemd/macOS launchd root service.
    Install {
        #[arg(long, conflicts_with = "heartbeat_user")]
        local: bool,
        /// Existing non-root account allowed only heartbeat and JSON status via sudo.
        #[arg(long, value_parser = parse_heartbeat_user)]
        heartbeat_user: Option<String>,
        /// Used only when creating a new disarmed configuration.
        #[arg(long, default_value_t = 129600)]
        timeout_secs: u64,
        /// Used only when creating a new disarmed configuration.
        #[arg(long, default_value_t = 1800)]
        boot_grace_secs: u64,
    },
    /// Send one manual heartbeat to named targets, or saved targets; 'local' uses sudo.
    Touch {
        #[arg(value_parser = parse_target)]
        targets: Vec<String>,
    },
    /// Read each target's countdown without sending heartbeats or changing state.
    Clock {
        #[arg(value_parser = parse_target)]
        targets: Vec<String>,
        /// Emit one JSON row per target, including errors and exact second counts.
        #[arg(long)]
        json: bool,
    },
    /// Manage saved targets; add 'local' explicitly after installing its root daemon.
    Targets {
        #[command(subcommand)]
        command: TargetCommands,
    },
}

#[derive(Subcommand)]
enum TargetCommands {
    Add {
        #[arg(value_parser = parse_target)]
        target: String,
    },
    List,
    Remove {
        #[arg(value_parser = parse_target)]
        target: String,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Daemon { dry_run, once } => grim_reaper::daemon(&cli.state_dir, dry_run, once),
        Commands::Heartbeat => {
            let status = grim_reaper::heartbeat(&cli.state_dir)?;
            print_status(&status)?;
            if status.phase == "expired" {
                bail!(
                    "heartbeat did not restore a healthy deadline; the target remains expired (shutdown may be latched)"
                );
            }
            Ok(())
        }
        Commands::Status { json } => {
            let status = grim_reaper::status(&cli.state_dir)?;
            if json {
                let mut out = io::stdout().lock();
                serde_json::to_writer(&mut out, &status)?;
                writeln!(out)?;
                out.flush()?;
                Ok(())
            } else {
                print_status(&status)
            }
        }
        Commands::Arm {
            confirm_hard_poweroff: _,
        } => {
            eprintln!(
                "WARNING: arming abrupt shutdown, repeated until dead. Graceful shutdown is not guaranteed."
            );
            print_status(&grim_reaper::arm(&cli.state_dir)?)
        }
        Commands::Disarm => print_status(&grim_reaper::disarm(&cli.state_dir)?),
        Commands::Install {
            local,
            heartbeat_user,
            timeout_secs,
            boot_grace_secs,
        } => {
            if local {
                install_local()
            } else {
                install_service(
                    &cli.state_dir,
                    heartbeat_user.as_deref(),
                    timeout_secs,
                    boot_grace_secs,
                )
            }
        }
        Commands::Touch { targets } => touch(targets),
        Commands::Clock { targets, json } => clock(targets, json),
        Commands::Targets { command } => manage_targets(command),
    }
}

fn print_status(status: &Status) -> Result<()> {
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "Shutdown dead-man switch: {}",
        if status.armed { "ARMED" } else { "DISARMED" }
    )?;
    writeln!(out, "Phase: {}", status.phase)?;
    writeln!(
        out,
        "Timeout: {}s; boot grace: {}s ({}s remaining)",
        status.config.timeout_secs, status.config.boot_grace_secs, status.boot_grace_remaining_secs
    )?;
    match status.heartbeat_age_secs {
        Some(age) => writeln!(out, "Heartbeat age: {age}s")?,
        None => writeln!(out, "Heartbeat age: unavailable")?,
    }
    match status.expires_in_secs {
        Some(remaining) => writeln!(out, "Heartbeat expires in: {remaining}s")?,
        None => writeln!(out, "Heartbeat deadline: unavailable or inactive")?,
    }
    writeln!(out, "Reason: {}", status.reason)?;
    out.flush()?;
    Ok(())
}

fn validate_target(target: &str) -> Result<()> {
    let syntax = "supported SSH target syntax is host or user@host, using only ASCII letters, digits, '.', '_' and '-'; IPv6 and shell syntax are not supported";
    if target.is_empty()
        || !target
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b'@'))
        || target.bytes().filter(|&c| c == b'@').count() > 1
        || target
            .split('@')
            .any(|part| part.is_empty() || part.starts_with('-'))
    {
        bail!("invalid target {target:?}: {syntax}; no component may start with '-'");
    }
    Ok(())
}

fn parse_target(value: &str) -> std::result::Result<String, String> {
    validate_target(value).map_err(|error| error.to_string())?;
    Ok(value.to_owned())
}

fn validate_heartbeat_user(user: &str) -> Result<()> {
    let mut bytes = user.bytes();
    let first_valid = bytes
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_');
    if !first_valid
        || !bytes.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        || user.eq_ignore_ascii_case("root")
        || user.eq_ignore_ascii_case("ALL")
    {
        bail!(
            "heartbeat user must be a non-root account name starting with an ASCII letter or '_' and containing only ASCII letters, digits, '_' or '-'; root and ALL are reserved"
        );
    }
    Ok(())
}

fn parse_heartbeat_user(value: &str) -> std::result::Result<String, String> {
    validate_heartbeat_user(value).map_err(|error| error.to_string())?;
    Ok(value.to_owned())
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .context("HOME must be set")
}

fn targets_path() -> Result<PathBuf> {
    let config_dir = match env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        Some(value) => {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                bail!("XDG_CONFIG_HOME must be an absolute path");
            }
            path
        }
        None => home_dir()?.join(".config"),
    };
    Ok(config_dir.join("grim-reaper/targets.json"))
}

fn load_targets(path: &Path) -> Result<Vec<String>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let targets: Vec<String> =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    for target in &targets {
        validate_target(target)
            .with_context(|| format!("invalid saved target in {}", path.display()))?;
    }
    Ok(targets)
}

fn save_targets(path: &Path, targets: &[String]) -> Result<()> {
    let dir = path
        .parent()
        .context("targets file has no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let mut bytes = serde_json::to_vec_pretty(targets)?;
    bytes.push(b'\n');
    grim_reaper::atomic_write(path, &bytes)
}

fn manage_targets(command: TargetCommands) -> Result<()> {
    let path = targets_path()?;
    let mut targets = load_targets(&path)?;
    let mut out = io::stdout().lock();
    match command {
        TargetCommands::Add { target } => {
            if targets.contains(&target) {
                writeln!(out, "Target already saved: {target}")?;
            } else {
                targets.push(target);
                save_targets(&path, &targets)?;
                writeln!(
                    out,
                    "Saved target: {}",
                    targets.last().context("target list unexpectedly empty")?
                )?;
            }
        }
        TargetCommands::List => {
            for target in &targets {
                writeln!(out, "{target}")?;
            }
        }
        TargetCommands::Remove { target } => {
            let original_len = targets.len();
            targets.retain(|saved| saved != &target);
            if targets.len() == original_len {
                bail!("target is not saved: {target}");
            }
            save_targets(&path, &targets)?;
            writeln!(out, "Removed target: {target}")?;
        }
    }
    out.flush()?;
    Ok(())
}

fn select_targets(mut targets: Vec<String>) -> Result<Vec<String>> {
    if targets.is_empty() {
        targets = load_targets(&targets_path()?)?;
    }
    if targets.is_empty() {
        bail!(
            "no targets supplied or saved; use 'targets add HOST' (or 'targets add local' after installing its root daemon)"
        );
    }
    // Validate the entire selection before any target can receive a heartbeat.
    for target in &targets {
        validate_target(target)?;
    }
    Ok(targets)
}

#[derive(Clone, Copy)]
enum TargetAction {
    Heartbeat,
    Status,
}

fn target_command(target: &str, action: TargetAction) -> Command {
    if target == "local" {
        let mut command = Command::new("sudo");
        command.args(["-n", "--", INSTALLED_BINARY]);
        match action {
            TargetAction::Heartbeat => command.arg("heartbeat"),
            TargetAction::Status => command.args(["status", "--json"]),
        };
        command
    } else {
        let mut command = Command::new("ssh");
        command
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=2",
                "--",
            ])
            .arg(target)
            .arg(match action {
                TargetAction::Heartbeat => REMOTE_HEARTBEAT,
                TargetAction::Status => REMOTE_STATUS,
            });
        command
    }
}

fn touch(targets: Vec<String>) -> Result<()> {
    let targets = select_targets(targets)?;
    let mut failures = Vec::new();
    for target in targets {
        eprintln!("Sending one manual heartbeat to {target}");
        let result = run_bounded(&mut target_command(&target, TargetAction::Heartbeat));
        match result {
            Ok(output) => {
                // A failed receipt can still contain useful fail-closed status.
                io::stdout().lock().write_all(&output.stdout)?;
                io::stderr().lock().write_all(&output.stderr)?;
                if !output.status.success() {
                    failures.push(format!("{target}: helper exited with {}", output.status));
                }
            }
            Err(error) => failures.push(format!("{target}: {error:#}")),
        }
    }
    io::stdout().lock().flush()?;
    io::stderr().lock().flush()?;
    if !failures.is_empty() {
        bail!(
            "manual heartbeat failed for {} target(s); a failed/timed-out receipt may already have been recorded, and was not retried:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
    Ok(())
}

#[derive(Serialize)]
struct FleetRow {
    target: String,
    status: Option<Status>,
    error: Option<String>,
}

fn clock(targets: Vec<String>, json: bool) -> Result<()> {
    let targets = select_targets(targets)?;
    let mut rows = Vec::with_capacity(targets.len());
    for target in targets {
        let result =
            run_bounded(&mut target_command(&target, TargetAction::Status)).and_then(|output| {
                require_success(&output)?;
                parse_target_status(&output.stdout)
            });
        rows.push(match result {
            Ok(status) => FleetRow {
                target,
                status: Some(status),
                error: None,
            },
            Err(error) => FleetRow {
                target,
                status: None,
                error: Some(format!("{error:#}")),
            },
        });
    }
    write_clock_rows(&rows, json, &mut io::stdout().lock())
}

fn parse_target_status(bytes: &[u8]) -> Result<Status> {
    let status: Status =
        serde_json::from_slice(bytes).context("target returned invalid JSON status")?;
    if !matches!(
        status.phase.as_str(),
        "disarmed" | "boot-grace" | "healthy" | "expired"
    ) {
        bail!(
            "target returned an unknown status phase: {:?}",
            status.phase
        );
    }
    if status.armed != status.config.armed || (status.phase == "disarmed") != !status.armed {
        bail!("target returned inconsistent armed status");
    }
    Ok(status)
}

fn write_clock_rows(rows: &[FleetRow], json: bool, out: &mut impl Write) -> Result<()> {
    if json {
        serde_json::to_writer(&mut *out, rows)?;
        writeln!(out)?;
    } else {
        writeln!(
            out,
            "TARGET\tPHASE\tARMED\tREMAINING\tHEARTBEAT AGE\tBOOT GRACE"
        )?;
        for row in rows {
            if let Some(status) = &row.status {
                writeln!(
                    out,
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    row.target,
                    status.phase,
                    if status.armed { "yes" } else { "no" },
                    DurationText(status.expires_in_secs),
                    DurationText(status.heartbeat_age_secs),
                    DurationText(Some(status.boot_grace_remaining_secs)),
                )?;
            } else {
                writeln!(
                    out,
                    "{}\tERROR\t-\t-\t-\t-\t{}",
                    row.target,
                    row.error
                        .as_deref()
                        .unwrap_or("status unavailable")
                        .replace(char::is_control, " ")
                )?;
            }
        }
    }
    out.flush()?;
    let failures = rows.iter().filter(|row| row.error.is_some()).count();
    if failures != 0 {
        bail!("countdown unavailable for {failures} target(s); no heartbeats were sent");
    }
    Ok(())
}

struct DurationText(Option<u64>);

impl std::fmt::Display for DurationText {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Some(total) = self.0 else {
            return formatter.write_str("-");
        };
        let hours = total / 3600;
        let minutes = total % 3600 / 60;
        let seconds = total % 60;
        if hours != 0 {
            write!(formatter, "{hours}h{minutes:02}m{seconds:02}s")
        } else if minutes != 0 {
            write!(formatter, "{minutes}m{seconds:02}s")
        } else {
            write!(formatter, "{seconds}s")
        }
    }
}

struct HelperOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_bounded(command: &mut Command) -> Result<HelperOutput> {
    run_bounded_for(command, HELPER_TIMEOUT)
}

fn run_bounded_for(command: &mut Command, timeout: Duration) -> Result<HelperOutput> {
    let deadline = Instant::now() + timeout;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("executing {command:?}"))?;
    let result = (|| -> Result<HelperOutput> {
        let mut stdout = child
            .stdout
            .take()
            .context("helper stdout pipe is missing")?;
        let mut stderr = child
            .stderr
            .take()
            .context("helper stderr pipe is missing")?;
        set_nonblocking(&stdout)?;
        set_nonblocking(&stderr)?;
        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();
        let mut stdout_closed = false;
        let mut stderr_closed = false;
        let mut status = None;
        loop {
            if Instant::now() >= deadline {
                bail!("helper timed out after {}ms", timeout.as_millis());
            }
            if !stdout_closed {
                stdout_closed = drain_pipe(&mut stdout, &mut stdout_bytes)?;
            }
            if !stderr_closed {
                stderr_closed = drain_pipe(&mut stderr, &mut stderr_bytes)?;
            }
            if status.is_none() {
                status = child.try_wait().context("checking helper exit")?;
            }
            if stdout_closed && stderr_closed {
                if let Some(status) = status {
                    return Ok(HelperOutput {
                        status,
                        stdout: stdout_bytes,
                        stderr: stderr_bytes,
                    });
                }
            }
            thread::sleep(
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    })();
    if result.is_err() {
        terminate_helper(&mut child);
    }
    result.with_context(|| format!("running {command:?}"))
}

fn set_nonblocking(pipe: &impl AsRawFd) -> Result<()> {
    let fd = pipe.as_raw_fd();
    // Both descriptors belong to this process and remain live for these calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error()).context("setting helper pipe nonblocking");
    }
    Ok(())
}

fn drain_pipe(pipe: &mut impl Read, bytes: &mut Vec<u8>) -> Result<bool> {
    let mut buffer = [0u8; 4096];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                if count > MAX_HELPER_STREAM_BYTES.saturating_sub(bytes.len()) {
                    bail!("helper output exceeds {MAX_HELPER_STREAM_BYTES} bytes on one stream");
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("reading helper output"),
        }
    }
}

fn terminate_helper(child: &mut Child) {
    // Helpers have a private process group, including ordinary descendants.
    // sudo's root child may be outside our permission to signal; never retry
    // an uncertain heartbeat receipt after this best-effort termination.
    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
    let _ = child.kill();
    // Never wait() on a potentially unkillable helper or inherited pipe.
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => return,
        }
    }
}

fn require_success(output: &HelperOutput) -> Result<()> {
    if !output.status.success() {
        bail!(
            "helper exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn install_local() -> Result<()> {
    let bin_dir = home_dir()?.join(".local/bin");
    fs::create_dir_all(&bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;
    let destination = bin_dir.join("grim-reaper");
    let executable = prepare_executable(&destination, false)?;
    executable.persist(&destination)?;
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "Installed manual heartbeat CLI: {}",
        destination.display()
    )?;
    writeln!(
        out,
        "Armed targets can shut down abruptly. No automatic heartbeat sender was installed."
    )?;
    let in_path = env::var_os("PATH")
        .is_some_and(|value| env::split_paths(&value).any(|entry| entry == bin_dir));
    if !in_path {
        writeln!(out, "Add {} to PATH, for example:", bin_dir.display())?;
        writeln!(out, "  export PATH=\"$HOME/.local/bin:$PATH\"")?;
    }
    out.flush()?;
    Ok(())
}

fn require_root_service() -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // geteuid has no preconditions and cannot fail.
        if unsafe { libc::geteuid() } != 0 {
            bail!(
                "daemon installation requires root on Linux/macOS; use 'install --local' for the manual sender"
            );
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    bail!("daemon installation is supported only on Linux/macOS")
}

fn ensure_system_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    for ancestor in path.ancestors() {
        let metadata =
            fs::metadata(ancestor).with_context(|| format!("checking {}", ancestor.display()))?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            bail!(
                "privileged installation directory {} must be root-owned and not group/world-writable",
                ancestor.display()
            );
        }
    }
    Ok(())
}

fn checked(command: &mut Command) -> Result<()> {
    let output = run_bounded(command)?;
    io::stdout().lock().write_all(&output.stdout)?;
    io::stderr().lock().write_all(&output.stderr)?;
    require_success(&output)
}

fn validate_system_account(user: &str) -> Result<()> {
    validate_heartbeat_user(user)?;
    let output = run_bounded(Command::new("/usr/bin/id").args(["-u", "--", user]))
        .context("looking up heartbeat account with id")?;
    if !output.status.success() {
        bail!(
            "heartbeat account {user:?} does not resolve: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let uid: u32 = std::str::from_utf8(&output.stdout)?
        .trim()
        .parse()
        .context("id returned an invalid UID")?;
    if uid == 0 {
        bail!("heartbeat account must not have UID 0");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn legacy_service_exists() -> Result<bool> {
    let output = run_bounded(Command::new("/usr/bin/systemctl").args([
        "show",
        "--property=LoadState",
        "--value",
        LEGACY_SERVICE,
    ]))
    .context("checking legacy systemd service")?;
    let state = std::str::from_utf8(&output.stdout)?.trim();
    if state == "not-found" && (output.status.success() || output.status.code() == Some(4)) {
        return Ok(false);
    }
    if !output.status.success() || state.is_empty() {
        bail!(
            "cannot inspect legacy systemd service ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(true)
}

fn reject_armed_install(state_dir: &Path) -> Result<()> {
    let status = grim_reaper::status(state_dir)?;
    if status.armed {
        bail!(
            "refusing installation while existing configuration is armed or invalid: {}; run explicit 'disarm' first",
            status.reason
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_if_exists(path: &str) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent(Path::new(path)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing retired legacy file {path}")),
    }
}

fn install_service(
    state_dir: &Path,
    heartbeat_user: Option<&str>,
    timeout_secs: u64,
    boot_grace_secs: u64,
) -> Result<()> {
    require_root_service()?;
    if state_dir != Path::new(DEFAULT_STATE_DIR) {
        bail!(
            "daemon installation requires --state-dir {DEFAULT_STATE_DIR}; the global service always uses this fixed directory"
        );
    }
    eprintln!(
        "WARNING: this service can shut down the machine abruptly, repeatedly until dead. Installation requires a DISARMED configuration; arm only after verifying manual heartbeats."
    );
    #[cfg(target_os = "linux")]
    let _ = check_install_state(state_dir, &[SERVICE_PATH, INSTALLED_BINARY])?;
    #[cfg(target_os = "macos")]
    let config_present =
        check_install_state(state_dir, &[LAUNCHD_PATH, MAC_LOG_PATH, INSTALLED_BINARY])?;
    if let Some(user) = heartbeat_user {
        validate_system_account(user)?;
    }
    #[cfg(target_os = "linux")]
    return install_linux_service(state_dir, heartbeat_user, timeout_secs, boot_grace_secs);
    #[cfg(target_os = "macos")]
    return install_macos_service(
        state_dir,
        heartbeat_user,
        timeout_secs,
        boot_grace_secs,
        config_present,
    );
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    bail!("daemon installation is supported only on Linux/macOS")
}

fn check_install_state(state_dir: &Path, installed_records: &[&str]) -> Result<bool> {
    // A present config must be checked before initialization or any cutover changes.
    match fs::symlink_metadata(state_dir.join("config.json")) {
        Ok(_) => {
            reject_armed_install(state_dir)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let existing_paths = installed_records
                .iter()
                .map(|path| PathBuf::from(*path))
                .chain([
                    state_dir.join("heartbeat.json"),
                    state_dir.join("shutdown-latched"),
                ]);
            for existing in existing_paths {
                match fs::symlink_metadata(&existing) {
                    Ok(_) => bail!(
                        "configuration is missing from an existing installation; run explicit 'disarm' before reinstalling"
                    ),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("inspecting {}", existing.display()));
                    }
                }
            }
            Ok(false)
        }
        Err(error) => Err(error).context("inspecting existing configuration before installation"),
    }
}

fn prepare_sudoers(heartbeat_user: Option<&str>) -> Result<Option<PendingFile>> {
    let Some(user) = heartbeat_user else {
        return Ok(None);
    };
    ensure_system_dir(Path::new("/etc/sudoers.d"))?;
    let content = format!(
        "{user} ALL=(root) NOPASSWD: {INSTALLED_BINARY} heartbeat, {INSTALLED_BINARY} status --json\n"
    );
    let pending = prepare_system_file(Path::new(SUDOERS_PATH), content.as_bytes(), 0o440)?;
    checked(
        Command::new("/usr/sbin/visudo")
            .arg("-cf")
            .arg(&pending.path),
    )?;
    Ok(Some(pending))
}

#[cfg(target_os = "linux")]
fn install_linux_service(
    state_dir: &Path,
    heartbeat_user: Option<&str>,
    timeout_secs: u64,
    boot_grace_secs: u64,
) -> Result<()> {
    let legacy_exists = legacy_service_exists()?;
    grim_reaper::initialize(state_dir, timeout_secs, boot_grace_secs)?;
    reject_armed_install(state_dir)?;
    ensure_system_dir(Path::new("/usr/local/bin"))?;
    ensure_system_dir(Path::new("/etc/systemd/system"))?;
    let binary = prepare_executable(Path::new(INSTALLED_BINARY), true)?;
    let service = prepare_system_file(Path::new(SERVICE_PATH), SERVICE_CONTENT.as_bytes(), 0o644)?;
    let sudoers = prepare_sudoers(heartbeat_user)?;
    if legacy_exists {
        checked(Command::new("/usr/bin/systemctl").args(["stop", LEGACY_SERVICE]))?;
        checked(Command::new("/usr/bin/systemctl").args(["disable", LEGACY_SERVICE]))?;
    }
    binary.persist(Path::new(INSTALLED_BINARY))?;
    service.persist(Path::new(SERVICE_PATH))?;
    if let Some(sudoers) = sudoers {
        sudoers.persist(Path::new(SUDOERS_PATH))?;
    }
    remove_if_exists("/usr/local/bin/grim_reaper.py")?;
    remove_if_exists("/etc/systemd/system/grim_reaper.service")?;
    reject_armed_install(state_dir)?;
    checked(Command::new("/usr/bin/systemctl").arg("daemon-reload"))?;
    checked(Command::new("/usr/bin/systemctl").args(["enable", SERVICE_NAME]))?;
    checked(Command::new("/usr/bin/systemctl").args(["restart", SERVICE_NAME]))?;
    checked(Command::new("/usr/bin/systemctl").args(["is-active", "--quiet", SERVICE_NAME]))?;
    print_installed_status(state_dir)
}

#[cfg(target_os = "macos")]
fn install_macos_service(
    state_dir: &Path,
    heartbeat_user: Option<&str>,
    timeout_secs: u64,
    boot_grace_secs: u64,
    config_present: bool,
) -> Result<()> {
    let loaded = launchd_loaded()?;
    if loaded && !config_present {
        bail!(
            "configuration is missing for a loaded launchd service; run explicit 'disarm' before reinstalling"
        );
    }
    grim_reaper::initialize(state_dir, timeout_secs, boot_grace_secs)?;
    reject_armed_install(state_dir)?;
    ensure_system_dir(Path::new("/usr/local/bin"))?;
    ensure_system_dir(Path::new("/Library/LaunchDaemons"))?;
    ensure_system_dir(Path::new("/var/log"))?;
    let binary = prepare_executable(Path::new(INSTALLED_BINARY), true)?;
    let service = prepare_system_file(Path::new(LAUNCHD_PATH), LAUNCHD_CONTENT.as_bytes(), 0o644)?;
    checked(
        Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&service.path),
    )?;
    let sudoers = prepare_sudoers(heartbeat_user)?;
    prepare_mac_log()?;
    // Stop only an actually loaded service; a failed bootout is not ignored.
    if loaded {
        reject_armed_install(state_dir)?;
        checked(Command::new("/bin/launchctl").args(["bootout", LAUNCHD_LABEL]))?;
    }
    binary.persist(Path::new(INSTALLED_BINARY))?;
    service.persist(Path::new(LAUNCHD_PATH))?;
    if let Some(sudoers) = sudoers {
        sudoers.persist(Path::new(SUDOERS_PATH))?;
    }
    reject_armed_install(state_dir)?;
    checked(Command::new("/bin/launchctl").args(["bootstrap", "system", LAUNCHD_PATH]))?;
    verify_launchd_running()?;
    print_installed_status(state_dir)?;
    println!(
        "Root daemon installed. To include it in manual fleet commands, explicitly run 'grim-reaper targets add local' as your normal account."
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn launchd_loaded() -> Result<bool> {
    let output = run_bounded(Command::new("/bin/launchctl").args(["print", LAUNCHD_LABEL]))?;
    if output.status.success() {
        return Ok(true);
    }
    if output.status.code() == Some(113)
        && String::from_utf8_lossy(&output.stderr).contains("Could not find service")
    {
        return Ok(false);
    }
    require_success(&output)?;
    unreachable!("unsuccessful launchctl print must return an error")
}

#[cfg(target_os = "macos")]
fn verify_launchd_running() -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = run_bounded_for(
            Command::new("/bin/launchctl").args(["print", LAUNCHD_LABEL]),
            deadline.saturating_duration_since(Instant::now()),
        )?;
        require_success(&output)?;
        let text = std::str::from_utf8(&output.stdout)
            .context("launchctl returned non-UTF8 service state")?;
        let running = text.lines().any(|line| line.trim() == "state = running");
        let pid = text.lines().find_map(|line| {
            line.trim()
                .strip_prefix("pid = ")
                .and_then(|pid| pid.parse::<u32>().ok())
        });
        if running && pid.is_some_and(|pid| pid != 0) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("launchd service is loaded but not running with a PID: {text}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(target_os = "macos")]
fn prepare_mac_log() -> Result<()> {
    let path = Path::new(MAC_LOG_PATH);
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() || metadata.uid() != 0 => {
            bail!("launchd log must be a root-owned regular file, not a symlink: {MAC_LOG_PATH}");
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspecting launchd log"),
    }
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .context("opening launchd log without following a final symlink")?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != 0 {
        bail!("opened launchd log is not a root-owned regular file");
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    // The live descriptor was checked above; GID 0 is wheel on macOS.
    if unsafe { libc::fchown(file.as_raw_fd(), 0, 0) } != 0 {
        return Err(io::Error::last_os_error())
            .context("setting launchd log ownership to root:wheel");
    }
    file.sync_all().context("syncing launchd log")?;
    sync_parent(path)
}

fn print_installed_status(state_dir: &Path) -> Result<()> {
    let status = grim_reaper::status(state_dir)?;
    if status.armed {
        bail!(
            "service is active, but configuration became armed during installation; inspect status immediately"
        );
    }
    print_status(&status)
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct PendingFile {
    path: PathBuf,
    file: Option<File>,
}

impl PendingFile {
    fn create(destination: &Path, mode: u32) -> Result<Self> {
        check_destination(destination)?;
        let parent = destination
            .parent()
            .context("installation destination has no parent")?;
        let name = destination
            .file_name()
            .context("installation destination has no filename")?;
        for _ in 0..128 {
            let mut temporary_name = OsString::from(".");
            temporary_name.push(name);
            temporary_name.push(format!(
                ".{}.{}.tmp",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let path = parent.join(temporary_name);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            match options.open(&path) {
                Ok(file) => {
                    let pending = Self {
                        path,
                        file: Some(file),
                    };
                    #[cfg(unix)]
                    pending
                        .file()?
                        .set_permissions(fs::Permissions::from_mode(mode))?;
                    #[cfg(not(unix))]
                    let _ = mode;
                    return Ok(pending);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "creating temporary installation file in {}",
                            parent.display()
                        )
                    });
                }
            }
        }
        bail!(
            "could not allocate a unique temporary installation file in {}",
            parent.display()
        )
    }

    fn file(&self) -> Result<&File> {
        self.file
            .as_ref()
            .context("temporary installation file is already closed")
    }

    fn root_owner(&self) -> Result<()> {
        #[cfg(unix)]
        {
            // This descriptor is live; install_service has already required root.
            if unsafe { libc::fchown(self.file()?.as_raw_fd(), 0, 0) } != 0 {
                return Err(io::Error::last_os_error())
                    .context("setting installed file ownership to UID/GID 0");
            }
        }
        Ok(())
    }

    fn persist(mut self, destination: &Path) -> Result<()> {
        let file = self
            .file
            .take()
            .context("temporary installation file is already closed")?;
        file.sync_all()
            .with_context(|| format!("syncing {}", self.path.display()))?;
        drop(file);
        check_destination(destination)?;
        fs::rename(&self.path, destination)
            .with_context(|| format!("atomically installing {}", destination.display()))?;
        sync_parent(destination)
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        drop(self.file.take());
        // Cleanup never changes a command's primary error; successful rename leaves no temp.
        let _ = fs::remove_file(&self.path);
    }
}

fn check_destination(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => bail!(
            "refusing to replace symlink or nonregular installation destination {}",
            path.display()
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("checking installation destination {}", path.display())),
    }
}

fn prepare_executable(destination: &Path, system: bool) -> Result<PendingFile> {
    let source_path = env::current_exe().context("locating current executable")?;
    let mut source = File::open(&source_path)
        .with_context(|| format!("opening current executable {}", source_path.display()))?;
    let pending = PendingFile::create(destination, 0o755)?;
    if system {
        pending.root_owner()?;
    }
    io::copy(&mut source, &mut pending.file()?)
        .context("copying executable into atomic installation file")?;
    Ok(pending)
}

fn prepare_system_file(destination: &Path, bytes: &[u8], mode: u32) -> Result<PendingFile> {
    let pending = PendingFile::create(destination, mode)?;
    pending.root_owner()?;
    pending.file()?.write_all(bytes)?;
    Ok(pending)
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("installed file has no parent directory")?;
    #[cfg(windows)]
    let directory = {
        use std::os::windows::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(0x02000000)
            .open(parent)
    };
    #[cfg(not(windows))]
    let directory = File::open(parent);
    directory
        .with_context(|| format!("opening parent directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("syncing parent directory {}", parent.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_supported_ssh_aliases() {
        for target in [
            "local",
            "server-a.example",
            "server-b.example",
            "operator@server-a.example",
            "my_ssh_alias",
            "a@b",
        ] {
            assert!(validate_target(target).is_ok(), "{target}");
        }
    }

    #[test]
    fn rejects_ssh_options_and_shell_injection() {
        for target in [
            "",
            "-oProxyCommand=sh",
            "-host",
            "user@-host",
            "@host",
            "user@",
            "a@b@c",
            "host;id",
            "$(id)",
            "host\nother",
            "host name",
            "user@host:22",
            "[::1]",
            "é.local",
        ] {
            assert!(validate_target(target).is_err(), "{target:?}");
        }
    }

    #[test]
    fn accepts_safe_heartbeat_accounts() {
        for user in ["operator", "heartbeat_user", "_heartbeat", "heartbeat-user"] {
            assert!(validate_heartbeat_user(user).is_ok(), "{user}");
        }
    }

    #[test]
    fn rejects_sudoers_injection_and_reserved_accounts() {
        for user in [
            "",
            "root",
            "ALL",
            "all",
            "-user",
            "1user",
            "user ALL=(ALL)",
            "user\nALL",
            "user,other",
            "#user",
            "user:other",
            "user@host",
            "é",
        ] {
            assert!(validate_heartbeat_user(user).is_err(), "{user:?}");
        }
    }

    #[test]
    fn invalid_later_target_is_rejected_before_any_fleet_action() {
        for operation in ["touch", "clock"] {
            assert!(Cli::try_parse_from(["grim-reaper", operation, "local", "host;id"]).is_err());
        }
        assert!(select_targets(vec!["local".into(), "-oProxyCommand=sh".into()]).is_err());
    }

    const HEALTHY_STATUS_JSON: &str = r#"{
        "armed":true,"phase":"healthy","reason":"fresh receipt",
        "heartbeat_age_secs":61,"expires_in_secs":129539,
        "boot_grace_remaining_secs":0,
        "config":{"armed":true,"timeout_secs":129600,"boot_grace_secs":1800}
    }"#;

    #[test]
    fn malformed_and_unsafe_status_cannot_be_rendered_as_healthy() {
        let mut value: serde_json::Value = serde_json::from_str(HEALTHY_STATUS_JSON).unwrap();
        for phase in ["healthy\nspoofed", "\u{1b}[32mhealthy", "unexpected"] {
            value["phase"] = phase.into();
            assert!(parse_target_status(&serde_json::to_vec(&value).unwrap()).is_err());
        }
        assert!(parse_target_status(br#"{"phase":"healthy"}"#).is_err());
        let trailing_json = format!("{HEALTHY_STATUS_JSON} {{\"phase\":\"healthy\"}}");
        assert!(parse_target_status(trailing_json.as_bytes()).is_err());
        value["phase"] = "healthy".into();
        value["armed"] = false.into();
        assert!(parse_target_status(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn fleet_partial_failure_keeps_success_countdown_and_exact_json_seconds() {
        let status = parse_target_status(HEALTHY_STATUS_JSON.as_bytes()).unwrap();
        let rows = [
            FleetRow {
                target: "local".into(),
                status: Some(status),
                error: None,
            },
            FleetRow {
                target: "server-a.example".into(),
                status: None,
                error: Some("connection refused\n\u{1b}[31mextra diagnostic".into()),
            },
        ];
        let mut human = Vec::new();
        assert!(write_clock_rows(&rows, false, &mut human).is_err());
        let human = String::from_utf8(human).unwrap();
        assert_eq!(human.lines().count(), 3, "{human}");
        assert!(human.contains("server-a.example\tERROR"));
        assert!(!human.contains('\u{1b}'));
        let mut json = Vec::new();
        assert!(write_clock_rows(&rows, true, &mut json).is_err());
        let json: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 2);
        assert_eq!(json[0]["status"]["expires_in_secs"], 129539);
        assert_eq!(json[0]["status"]["heartbeat_age_secs"], 61);
        assert_eq!(json[1]["target"], "server-a.example");
        assert!(json[1]["status"].is_null());
        assert_eq!(
            json[1]["error"],
            "connection refused\n\u{1b}[31mextra diagnostic"
        );
    }

    #[test]
    fn stalled_helper_returns_a_bounded_consumer_visible_timeout() {
        for script in ["exec /bin/sleep 5", "/bin/sleep 5 & exit 0"] {
            let start = Instant::now();
            let error = run_bounded_for(
                Command::new("/bin/sh").args(["-c", script]),
                Duration::from_millis(100),
            )
            .err()
            .expect("stalled helper or inherited pipe must fail");
            assert!(format!("{error:#}").contains("timed out"), "{error:#}");
            assert!(start.elapsed() < Duration::from_secs(2));
        }
    }

    #[test]
    fn excessive_helper_stdout_and_stderr_fail_without_pipe_deadlock() {
        for script in ["exec /usr/bin/yes noisy", "exec /usr/bin/yes noisy >&2"] {
            let start = Instant::now();
            let error = run_bounded_for(
                Command::new("/bin/sh").args(["-c", script]),
                Duration::from_secs(2),
            )
            .err()
            .expect("unbounded output must fail");
            assert!(format!("{error:#}").contains("output exceeds"), "{error:#}");
            assert!(start.elapsed() < Duration::from_secs(2));
        }
    }
}

//! Fail-closed heartbeat state and abrupt native Linux/macOS shutdown daemons.
//!
//! A heartbeat never arms the daemon. Once shutdown is latched, only explicit
//! disarm resets it. Same-boot age includes sleep; wall time is used across boots.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
#[cfg(target_os = "macos")]
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
pub const DEFAULT_STATE_DIR: &str = "/var/db/grim-reaper";
#[cfg(not(target_os = "macos"))]
pub const DEFAULT_STATE_DIR: &str = "/var/lib/grim-reaper";
const DEFAULT_TIMEOUT_SECS: u64 = 36 * 60 * 60;
const DEFAULT_BOOT_GRACE_SECS: u64 = 30 * 60;
const MAX_TIMEOUT_SECS: u64 = 365 * 24 * 60 * 60;
const MAX_BOOT_GRACE_SECS: u64 = 24 * 60 * 60;
const ARM_FRESHNESS_SECS: u64 = 60;
const MAX_JSON_BYTES: u64 = 16 * 1024;
const CONFIG_FILE: &str = "config.json";
const HEARTBEAT_FILE: &str = "heartbeat.json";
const LATCH_FILE: &str = "shutdown-latched";
const INITIAL_LATCH: &[u8] = b"latched\n";
#[cfg(any(target_os = "linux", test))]
const SYSRQ_SCHEDULED_LATCH: &[u8] = b"reiuso-scheduled\n";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

macro_rules! diagnostic {
    ($($argument:tt)*) => {{
        // Diagnostics are best effort: a broken journal/stderr must never
        // panic the daemon before it can attempt the next shutdown method.
        let _ = writeln!(io::stderr().lock(), $($argument)*);
    }};
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub armed: bool,
    pub timeout_secs: u64,
    pub boot_grace_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            armed: false,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            boot_grace_secs: DEFAULT_BOOT_GRACE_SECS,
        }
    }
}

impl Config {
    fn validate(&self) -> Result<()> {
        if !(1..=MAX_TIMEOUT_SECS).contains(&self.timeout_secs) {
            bail!("timeout_secs must be between 1 and {MAX_TIMEOUT_SECS}");
        }
        if self.boot_grace_secs > MAX_BOOT_GRACE_SECS {
            bail!("boot_grace_secs must be between 0 and {MAX_BOOT_GRACE_SECS}");
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    pub armed: bool,
    pub phase: String,
    pub reason: String,
    pub heartbeat_age_secs: Option<u64>,
    pub expires_in_secs: Option<u64>,
    pub boot_grace_remaining_secs: u64,
    pub config: Config,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Heartbeat {
    wall_secs: u64,
    boot_id: String,
    uptime_secs: u64,
}

#[derive(Debug)]
struct Clock {
    wall_secs: u64,
    boot_id: String,
    uptime_secs: u64,
}

struct LoadedConfig {
    config: Config,
    diagnostic: Option<String>,
}

/// Persist a complete file with mode 0600, then fsync its parent directory.
/// This general-purpose writer does not enforce the state-directory policy.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => {
            bail!("refusing to replace nonregular file {}", path.display());
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    let (temp_path, mut file) = create_temp(parent)?;
    let result = (|| -> Result<()> {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)
            .with_context(|| format!("write {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("fsync {}", temp_path.display()))?;
        fs::rename(&temp_path, path).with_context(|| format!("replace {}", path.display()))?;
        sync_directory(parent)
    })();
    if result.is_err() {
        if let Err(error) = fs::remove_file(&temp_path) {
            if error.kind() != io::ErrorKind::NotFound {
                diagnostic!("grim-reaper: temporary-file cleanup failed: {error}");
            }
        }
    }
    result
}

fn create_temp(parent: &Path) -> Result<(PathBuf, File)> {
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".grim-reaper-{}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("create atomic-write temporary file"),
        }
    }
    bail!("could not allocate a unique atomic-write temporary file")
}

fn sync_directory(path: &Path) -> Result<()> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open directory {} for fsync", path.display()))?;
    directory
        .sync_all()
        .with_context(|| format!("fsync directory {}", path.display()))
}

fn normalized_absolute(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn matches_default_state_dir(path: &Path) -> bool {
    path == Path::new(DEFAULT_STATE_DIR)
        || (cfg!(target_os = "macos") && path == Path::new("/private/var/db/grim-reaper"))
}

fn is_default_state_dir(path: &Path) -> Result<bool> {
    let normalized = normalized_absolute(path)?;
    if matches_default_state_dir(&normalized) {
        return Ok(true);
    }
    // Resolving an existing ancestor also catches aliases when the state dir is absent.
    let mut ancestor = normalized.as_path();
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(ancestor) {
            Ok(mut canonical) => {
                for component in suffix.iter().rev() {
                    canonical.push(component);
                }
                return Ok(matches_default_state_dir(&canonical));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if let Some(name) = ancestor.file_name() {
                    suffix.push(name.to_os_string());
                }
                match ancestor.parent() {
                    Some(parent) => ancestor = parent,
                    None => return Ok(false),
                }
            }
            Err(error) => return Err(error).context("resolve state-directory policy"),
        }
    }
}

fn require_supported_root() -> Result<()> {
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        bail!("live shutdown and the default state directory require Linux or macOS");
    }
    // SAFETY: geteuid has no arguments or memory safety requirements.
    if unsafe { libc::geteuid() } != 0 {
        bail!("live shutdown and the default state directory require root");
    }
    Ok(())
}

fn enforce_state_policy(state_dir: &Path) -> Result<()> {
    if is_default_state_dir(state_dir)? {
        require_supported_root()?;
    }
    Ok(())
}

fn inspect_state_dir(state_dir: &Path, create: bool) -> Result<()> {
    if create {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(state_dir)
            .with_context(|| format!("create state directory {}", state_dir.display()))?;
    }
    let metadata = match fs::symlink_metadata(state_dir) {
        Ok(metadata) => metadata,
        Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspect state directory"),
    };
    if !metadata.file_type().is_dir() {
        bail!("state directory must be a real directory, not a symlink");
    }
    // SAFETY: geteuid has no arguments or memory safety requirements.
    if metadata.uid() != unsafe { libc::geteuid() } {
        bail!("state directory must be owned by the invoking effective user (root in production)");
    }
    if create {
        fs::set_permissions(state_dir, fs::Permissions::from_mode(0o700))?;
    } else if metadata.mode() & 0o777 != 0o700 {
        bail!("state directory must have mode 0700");
    }
    Ok(())
}

struct StateLock {
    file: File,
}

impl StateLock {
    fn acquire(state_dir: &Path) -> Result<Self> {
        Self::acquire_with_timeout(state_dir, Duration::from_secs(3))
    }

    fn acquire_with_timeout(state_dir: &Path, timeout: Duration) -> Result<Self> {
        inspect_state_dir(state_dir, true)?;
        let path = state_dir.join(".state.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&path)
            .context("open state lock")?;
        let metadata = file.metadata()?;
        // SAFETY: geteuid has no arguments or memory safety requirements.
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            bail!("state lock must be an owned regular file");
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        let deadline = Instant::now() + timeout;
        loop {
            // SAFETY: file owns a valid descriptor for the duration of the lock.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self { file });
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted
                && error.kind() != io::ErrorKind::WouldBlock
            {
                return Err(error).context("lock state mutations");
            }
            if Instant::now() >= deadline {
                bail!(
                    "state mutation lock timed out after {}ms",
                    timeout.as_millis()
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        // Closing the descriptor also releases the lock if explicit unlock fails.
        // SAFETY: self.file still owns a valid descriptor.
        if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) } != 0 {
            diagnostic!(
                "grim-reaper: state unlock failed: {}",
                io::Error::last_os_error()
            );
        }
    }
}

fn read_state_file(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no arguments or memory safety requirements.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        bail!("{} must be an owned private regular file", path.display());
    }
    if metadata.len() > MAX_JSON_BYTES {
        bail!("{} exceeds the state size limit", path.display());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_JSON_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        bail!("{} exceeds the state size limit", path.display());
    }
    Ok(Some(bytes))
}

fn decode_config(bytes: Option<&[u8]>) -> LoadedConfig {
    let result = (|| -> Result<Config> {
        let bytes = bytes.context("config.json is missing")?;
        let config: Config = serde_json::from_slice(bytes).context("config.json is invalid")?;
        config
            .validate()
            .context("config.json has invalid timings")?;
        Ok(config)
    })();
    match result {
        Ok(config) => LoadedConfig {
            config,
            diagnostic: None,
        },
        Err(error) => failclosed_config(format!("{error:#}")),
    }
}

fn failclosed_config(diagnostic: String) -> LoadedConfig {
    LoadedConfig {
        config: Config {
            armed: true,
            ..Config::default()
        },
        diagnostic: Some(diagnostic),
    }
}

fn load_config(state_dir: &Path) -> LoadedConfig {
    match read_state_file(&state_dir.join(CONFIG_FILE)) {
        Ok(bytes) => decode_config(bytes.as_deref()),
        Err(error) => failclosed_config(format!("{error:#}")),
    }
}

fn decode_heartbeat(bytes: &[u8]) -> Result<Heartbeat> {
    let heartbeat: Heartbeat =
        serde_json::from_slice(bytes).context("heartbeat.json is invalid")?;
    if heartbeat.wall_secs == 0 || !valid_boot_id(&heartbeat.boot_id) {
        bail!("heartbeat.json has an invalid wall timestamp or boot identifier");
    }
    Ok(heartbeat)
}

fn valid_boot_id(boot_id: &str) -> bool {
    boot_id.len() == 36
        && boot_id.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn load_heartbeat(state_dir: &Path) -> std::result::Result<Option<Heartbeat>, String> {
    match read_state_file(&state_dir.join(HEARTBEAT_FILE)) {
        Ok(Some(bytes)) => decode_heartbeat(&bytes)
            .map(Some)
            .map_err(|error| format!("{error:#}")),
        Ok(None) => Ok(None),
        Err(error) => Err(format!("{error:#}")),
    }
}

fn latch_exists(state_dir: &Path) -> Result<bool> {
    // Any object at this path means latched; malformed or unreadable latches
    // cannot accidentally reopen the heartbeat path.
    match fs::symlink_metadata(state_dir.join(LATCH_FILE)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("inspect shutdown latch"),
    }
}

fn clock_now() -> Result<Clock> {
    let wall_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("server wall clock precedes Unix epoch")?
        .as_secs();
    let (boot_id, uptime_secs) = boot_clock()?;
    Ok(Clock {
        wall_secs,
        boot_id,
        uptime_secs,
    })
}

#[cfg(target_os = "linux")]
fn boot_clock() -> Result<(String, u64)> {
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("read Linux boot identifier")?
        .trim()
        .to_owned();
    if !valid_boot_id(&boot_id) {
        bail!("invalid Linux boot identifier");
    }
    let uptime = fs::read_to_string("/proc/uptime").context("read Linux uptime")?;
    let value = uptime
        .split_whitespace()
        .next()
        .context("missing Linux uptime")?;
    let (seconds, fraction) = value.split_once('.').context("invalid Linux uptime")?;
    if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("invalid Linux uptime fraction");
    }
    let uptime_secs = seconds
        .parse::<u64>()
        .context("invalid Linux uptime seconds")?;
    Ok((boot_id, uptime_secs))
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct MachTimebase {
    numer: u32,
    denom: u32,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mach_continuous_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebase) -> libc::c_int;
    #[link_name = "reboot"]
    fn macos_reboot(howto: libc::c_int) -> libc::c_int;
}

#[cfg(target_os = "macos")]
static MACH_TIMEBASE: LazyLock<std::result::Result<(u32, u32), libc::c_int>> =
    LazyLock::new(|| {
        let mut info = MachTimebase { numer: 0, denom: 0 };
        // SAFETY: info is a writable C-layout mach_timebase_info buffer.
        let result = unsafe { mach_timebase_info(&mut info) };
        if result != 0 {
            Err(result)
        } else {
            Ok((info.numer, info.denom))
        }
    });

#[cfg(any(target_os = "macos", test))]
fn continuous_time_seconds(ticks: u64, numer: u32, denom: u32) -> Result<u64> {
    if numer == 0 || denom == 0 {
        bail!("macOS Mach timebase has a zero conversion factor");
    }
    let nanoseconds = u128::from(ticks)
        .checked_mul(u128::from(numer))
        .context("macOS continuous-clock multiplication overflow")?;
    let divisor = u128::from(denom)
        .checked_mul(1_000_000_000)
        .context("macOS continuous-clock divisor overflow")?;
    let seconds = nanoseconds
        .checked_div(divisor)
        .context("macOS continuous-clock division failed")?;
    u64::try_from(seconds).context("macOS continuous-clock seconds overflow")
}

#[cfg(target_os = "macos")]
fn boot_clock() -> Result<(String, u64)> {
    let (numer, denom) = match &*MACH_TIMEBASE {
        Ok(factors) => *factors,
        Err(code) => bail!("read macOS Mach timebase failed with kernel return {code}"),
    };
    // SAFETY: mach_continuous_time has no arguments and includes system sleep.
    let uptime_secs = continuous_time_seconds(unsafe { mach_continuous_time() }, numer, denom)?;
    // Unlike kern.boottime, the boot-session UUID cannot follow wall-clock jumps.
    let mut boot_id = [0u8; 37];
    let mut size = boot_id.len();
    // SAFETY: the static NUL-terminated name and writable stack buffer are
    // valid; size describes its capacity. No new sysctl value is supplied.
    if unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            boot_id.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error()).context("read macOS boot-session UUID");
    }
    if size != boot_id.len() || boot_id[36] != 0 {
        bail!("invalid macOS boot-session UUID size or terminator");
    }
    let boot_id =
        std::str::from_utf8(&boot_id[..36]).context("non-UTF-8 macOS boot-session UUID")?;
    if !valid_boot_id(boot_id) {
        bail!("invalid macOS boot-session UUID");
    }
    Ok((boot_id.to_owned(), uptime_secs))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn boot_clock() -> Result<(String, u64)> {
    bail!("heartbeat clocks are supported on Linux and macOS only")
}

fn heartbeat_age(heartbeat: Option<&Heartbeat>, clock: &Clock) -> std::result::Result<u64, String> {
    let heartbeat = heartbeat.ok_or_else(|| "heartbeat is missing".to_owned())?;
    if heartbeat.boot_id == clock.boot_id {
        clock
            .uptime_secs
            .checked_sub(heartbeat.uptime_secs)
            .ok_or_else(|| "heartbeat uptime is in the future".to_owned())
    } else {
        clock
            .wall_secs
            .checked_sub(heartbeat.wall_secs)
            .ok_or_else(|| "heartbeat wall timestamp is in the future across boots".to_owned())
    }
}

fn evaluate(
    loaded: LoadedConfig,
    heartbeat: std::result::Result<Option<Heartbeat>, String>,
    clock: std::result::Result<Clock, String>,
    latch: std::result::Result<bool, String>,
    in_memory_latch: bool,
) -> Status {
    let LoadedConfig { config, diagnostic } = loaded;
    let mut status = Status {
        armed: config.armed,
        phase: "disarmed".to_owned(),
        reason: "explicitly disarmed".to_owned(),
        heartbeat_age_secs: None,
        expires_in_secs: None,
        boot_grace_remaining_secs: 0,
        config,
    };
    let age = match (&heartbeat, &clock) {
        (Ok(heartbeat), Ok(clock)) => heartbeat_age(heartbeat.as_ref(), clock),
        (Err(error), _) => Err(error.clone()),
        (_, Err(error)) => Err(format!("clock unavailable: {error}")),
    };
    status.heartbeat_age_secs = age.as_ref().ok().copied();
    if status.armed {
        match clock {
            Err(error) => {
                status.phase = "expired".to_owned();
                status.reason =
                    format!("cannot establish safe heartbeat age or boot grace: {error}");
            }
            Ok(clock) => {
                status.boot_grace_remaining_secs = status
                    .config
                    .boot_grace_secs
                    .saturating_sub(clock.uptime_secs);
                let latched = in_memory_latch || latch.as_ref().copied().unwrap_or(true);
                if latched {
                    status.reason = match latch {
                        Err(error) => {
                            format!("shutdown latch unavailable; treating as latched: {error}")
                        }
                        Ok(_) => "shutdown is latched; explicit disarm is required".to_owned(),
                    };
                } else {
                    status.reason = match &age {
                        Ok(age) if *age < status.config.timeout_secs => {
                            "heartbeat is current".to_owned()
                        }
                        Ok(_) => "heartbeat timeout reached".to_owned(),
                        Err(error) => error.clone(),
                    };
                }
                if status.boot_grace_remaining_secs > 0 {
                    status.phase = "boot-grace".to_owned();
                    status.reason = format!("boot grace active; {}", status.reason);
                } else if latched
                    || age
                        .as_ref()
                        .map_or(true, |age| *age >= status.config.timeout_secs)
                {
                    status.phase = "expired".to_owned();
                } else {
                    status.phase = "healthy".to_owned();
                }
                if !latched {
                    status.expires_in_secs = Some(match age {
                        Ok(age) => status
                            .config
                            .timeout_secs
                            .saturating_sub(age)
                            .max(status.boot_grace_remaining_secs),
                        Err(_) => status.boot_grace_remaining_secs,
                    });
                } else {
                    status.expires_in_secs = Some(status.boot_grace_remaining_secs);
                }
            }
        }
    }
    if let Some(diagnostic) = diagnostic {
        status.reason = format!("fail-closed config ({diagnostic}); {}", status.reason);
    }
    status
}

fn read_status(state_dir: &Path, in_memory_latch: bool) -> Result<Status> {
    inspect_state_dir(state_dir, false)?;
    Ok(evaluate(
        load_config(state_dir),
        load_heartbeat(state_dir),
        clock_now().map_err(|error| format!("{error:#}")),
        latch_exists(state_dir).map_err(|error| format!("{error:#}")),
        in_memory_latch,
    ))
}

pub fn status(state_dir: &Path) -> Result<Status> {
    enforce_state_policy(state_dir)?;
    read_status(state_dir, false)
}

pub fn heartbeat(state_dir: &Path) -> Result<Status> {
    enforce_state_policy(state_dir)?;
    let _lock = StateLock::acquire(state_dir)?;
    if latch_exists(state_dir)? {
        bail!("shutdown is latched; disarm explicitly before sending another heartbeat");
    }
    let clock = clock_now()?;
    let heartbeat = Heartbeat {
        wall_secs: clock.wall_secs,
        boot_id: clock.boot_id,
        uptime_secs: clock.uptime_secs,
    };
    atomic_write(
        &state_dir.join(HEARTBEAT_FILE),
        &serde_json::to_vec(&heartbeat)?,
    )?;
    read_status(state_dir, false)
}

fn require_fresh_heartbeat(
    config: &Config,
    heartbeat: Option<&Heartbeat>,
    clock: &Clock,
    latched: bool,
) -> Result<()> {
    if latched {
        bail!("shutdown is latched; explicit disarm is required before arming");
    }
    let age = heartbeat_age(heartbeat, clock).map_err(anyhow::Error::msg)?;
    if age > ARM_FRESHNESS_SECS || age >= config.timeout_secs {
        bail!(
            "arming requires an unexpired heartbeat no more than {ARM_FRESHNESS_SECS} seconds old (age {age}s)"
        );
    }
    Ok(())
}

pub fn arm(state_dir: &Path) -> Result<Status> {
    enforce_state_policy(state_dir)?;
    let _lock = StateLock::acquire(state_dir)?;
    let mut config = load_config(state_dir).config;
    let clock = clock_now()?;
    let heartbeat = load_heartbeat(state_dir).map_err(anyhow::Error::msg)?;
    require_fresh_heartbeat(
        &config,
        heartbeat.as_ref(),
        &clock,
        latch_exists(state_dir)?,
    )?;
    config.armed = true;
    atomic_write(&state_dir.join(CONFIG_FILE), &serde_json::to_vec(&config)?)?;
    read_status(state_dir, false)
}

pub fn disarm(state_dir: &Path) -> Result<Status> {
    enforce_state_policy(state_dir)?;
    let _lock = StateLock::acquire(state_dir)?;
    let mut config = load_config(state_dir).config;
    config.armed = false;
    // Persist disarmed first: it takes precedence even if clearing the latch fails.
    atomic_write(&state_dir.join(CONFIG_FILE), &serde_json::to_vec(&config)?)?;
    match fs::remove_file(state_dir.join(LATCH_FILE)) {
        Ok(()) => sync_directory(state_dir)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).context("clear shutdown latch (config is already disarmed)");
        }
    }
    read_status(state_dir, false)
}

pub fn initialize(state_dir: &Path, timeout_secs: u64, boot_grace_secs: u64) -> Result<()> {
    enforce_state_policy(state_dir)?;
    let config = Config {
        armed: false,
        timeout_secs,
        boot_grace_secs,
    };
    config.validate()?;
    let _lock = StateLock::acquire(state_dir)?;
    match fs::symlink_metadata(state_dir.join(CONFIG_FILE)) {
        Ok(_) => {
            let existing = load_config(state_dir);
            if let Some(diagnostic) = existing.diagnostic {
                bail!(
                    "existing config was preserved but is unusable: {diagnostic}; disarm explicitly to recover"
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            atomic_write(&state_dir.join(CONFIG_FILE), &serde_json::to_vec(&config)?)
        }
        Err(error) => Err(error).context("inspect existing config before initialization"),
    }
}

fn runtime_failure(error: anyhow::Error) -> Status {
    Status {
        armed: true,
        phase: "expired".to_owned(),
        reason: format!("cannot establish safe state; fail-closed: {error:#}"),
        heartbeat_age_secs: None,
        expires_in_secs: Some(0),
        boot_grace_remaining_secs: 0,
        config: Config {
            armed: true,
            ..Config::default()
        },
    }
}

fn prepare_shutdown(state_dir: &Path, in_memory_latch: bool) -> Result<Status> {
    let _lock = StateLock::acquire_with_timeout(state_dir, Duration::from_millis(250))?;
    // Recheck under the writers' lock so a concurrent heartbeat/disarm that
    // completed before latching is not mistaken for a failed heartbeat.
    let status = read_status(state_dir, in_memory_latch)?;
    if status.phase == "expired" {
        // Persistence is attempted before any shutdown method. The daemon
        // continues shutting down on an error and retries persistence next pass.
        if !latch_exists(state_dir)? {
            atomic_write(&state_dir.join(LATCH_FILE), INITIAL_LATCH)?;
        }
    }
    Ok(status)
}

/// Poll once per second. Live expiry never returns voluntarily: every available
/// shutdown method is attempted repeatedly, even when state persistence fails.
pub fn daemon(state_dir: &Path, dry_run: bool, once: bool) -> Result<()> {
    if once && !dry_run {
        bail!("--once is only permitted with --dry-run");
    }
    if !dry_run {
        require_supported_root()?;
    } else {
        enforce_state_policy(state_dir)?;
    }
    let mut in_memory_latch = false;
    let mut previous_transition: Option<(String, String)> = None;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let mut shutdown = ShutdownEngine::new();
    loop {
        let mut current = read_status(state_dir, in_memory_latch).unwrap_or_else(runtime_failure);
        if current.phase == "expired" && !dry_run {
            match prepare_shutdown(state_dir, in_memory_latch) {
                Ok(rechecked) => current = rechecked,
                Err(error) => {
                    diagnostic!(
                        "grim-reaper: shutdown latch persistence/recheck failed; continuing shutdown: {error:#}"
                    );
                    current = runtime_failure(error);
                }
            }
        }
        if current.phase == "disarmed" {
            in_memory_latch = false;
        } else if current.phase == "expired" && !dry_run {
            in_memory_latch = true;
        }
        let transition = (current.phase.clone(), current.reason.clone());
        if previous_transition.as_ref() != Some(&transition) {
            diagnostic!(
                "grim-reaper: armed={} phase={} reason={}",
                current.armed,
                current.phase,
                current.reason
            );
            previous_transition = Some(transition);
        }
        if once {
            println!("{}", serde_json::to_string(&current)?);
            return Ok(());
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if !dry_run {
            shutdown.cycle(current.phase == "expired", state_dir);
        }
        thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum ShutdownMethod {
    SysRq,
    PowerOff,
    Systemctl,
    PoweroffHelper,
    Halt,
}

#[cfg(target_os = "linux")]
impl ShutdownMethod {
    const ALL: [Self; 5] = [
        Self::SysRq,
        Self::PowerOff,
        Self::Systemctl,
        Self::PoweroffHelper,
        Self::Halt,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::SysRq => "SysRq REIUSO (_reiuso in one write)",
            Self::PowerOff => "reboot(POWER_OFF)",
            Self::Systemctl => "systemctl poweroff --force --force",
            Self::PoweroffHelper => "/sbin/poweroff -f",
            Self::Halt => "reboot(HALT)",
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
enum ShutdownMethod {
    QuickHalt,
    QuickHaltHelper,
    NoSyncHalt,
    NoSyncHaltHelper,
    ShutdownHelper,
}

#[cfg(target_os = "macos")]
impl ShutdownMethod {
    const ALL: [Self; 5] = [
        Self::QuickHalt,
        Self::QuickHaltHelper,
        Self::NoSyncHalt,
        Self::NoSyncHaltHelper,
        Self::ShutdownHelper,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::QuickHalt => "reboot(RB_HALT|RB_QUICK)",
            Self::QuickHaltHelper => "/sbin/halt -q",
            Self::NoSyncHalt => "reboot(RB_HALT|RB_NOSYNC|RB_QUICK)",
            Self::NoSyncHaltHelper => "/sbin/halt -q -n",
            Self::ShutdownHelper => "/sbin/shutdown -h now",
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn should_schedule_sysrq(latch: Option<&[u8]>) -> Result<bool> {
    match latch {
        Some(bytes) if bytes == INITIAL_LATCH => Ok(true),
        Some(bytes) if bytes == SYSRQ_SCHEDULED_LATCH => Ok(false),
        Some(_) => bail!("shutdown latch has unknown cascade progress"),
        None => bail!("shutdown latch is missing; cannot durably schedule REIUSO"),
    }
}

#[cfg(any(target_os = "linux", test))]
fn schedule_sysrq(state_dir: &Path) -> Result<bool> {
    let _lock = StateLock::acquire_with_timeout(state_dir, Duration::from_millis(250))?;
    if !load_config(state_dir).config.armed {
        return Ok(false);
    }
    let latch = read_state_file(&state_dir.join(LATCH_FILE))?;
    if !should_schedule_sysrq(latch.as_deref())? {
        return Ok(false);
    }
    // REIUSO kills this daemon and its worker at i. Persist the next step first:
    // PID 1 can restart us after a surviving sequence, but must not loop back
    // into killing us before direct poweroff. Explicit disarm clears this file.
    atomic_write(&state_dir.join(LATCH_FILE), SYSRQ_SCHEDULED_LATCH)?;
    Ok(true)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct ShutdownEngine {
    workers: [Option<ShutdownWorker>; 5],
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl ShutdownEngine {
    fn new() -> Self {
        Self {
            workers: std::array::from_fn(|_| None),
        }
    }

    fn cycle(&mut self, mut active: bool, state_dir: &Path) {
        for (slot, method) in self.workers.iter_mut().zip(ShutdownMethod::ALL) {
            // Keep the cascade ordered: preferred poweroff/cache-flushing
            // methods get their deadline before harder or last alternatives.
            if active && !load_config(state_dir).config.armed {
                active = false;
            }
            if let Some(worker) = slot {
                if worker.poll(method.name()) {
                    *slot = None;
                } else if worker.killed || !active {
                    if !worker.killed {
                        diagnostic!("grim-reaper: {} cancelled by disarm", method.name());
                        worker.killed = true;
                    }
                    worker.kill(method.name());
                    // An unreaped D-state task keeps its slot, but cannot hold
                    // up any other method or accumulate another copy.
                    continue;
                }
            }
            if !active {
                continue;
            }
            if slot.is_none() {
                #[cfg(target_os = "linux")]
                if matches!(method, ShutdownMethod::SysRq) {
                    match schedule_sysrq(state_dir) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(error) => {
                            diagnostic!(
                                "grim-reaper: cannot durably schedule REIUSO; continuing with direct poweroff: {error:#}"
                            );
                            continue;
                        }
                    }
                }
                // Scheduling may have waited for a writer; honor any explicit
                // disarm before starting this method as well as between methods.
                if !load_config(state_dir).config.armed {
                    active = false;
                    continue;
                }
                diagnostic!("grim-reaper: attempting {}", method.name());
                match ShutdownWorker::spawn(method) {
                    Ok(worker) => *slot = Some(worker),
                    Err(error) => {
                        diagnostic!(
                            "grim-reaper: {} worker start failed: {error:#}",
                            method.name()
                        );
                        continue;
                    }
                }
            }
            let mut last_disarm_check = Instant::now();
            while let Some(worker) = slot {
                if worker.poll(method.name()) {
                    *slot = None;
                    break;
                }
                if last_disarm_check.elapsed() >= Duration::from_secs(1) {
                    last_disarm_check = Instant::now();
                    if !load_config(state_dir).config.armed {
                        active = false;
                    }
                }
                if !active || worker.started.elapsed() >= Duration::from_secs(3) {
                    diagnostic!(
                        "grim-reaper: {} {}; killing worker without blocking reap",
                        method.name(),
                        if active {
                            "exceeded 3-second deadline"
                        } else {
                            "cancelled by disarm"
                        },
                    );
                    worker.killed = true;
                    worker.kill(method.name());
                    break;
                }
                // A returned/failed method advances immediately; a stuck
                // method advances after its bounded deadline, never blocking
                // on waitpid or preventing the remaining cascade.
                thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct ShutdownWorker {
    pid: libc::pid_t,
    started: Instant,
    killed: bool,
    report: Option<File>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl ShutdownWorker {
    fn spawn(method: ShutdownMethod) -> Result<Self> {
        use std::os::fd::FromRawFd;

        let pipe = match worker_error_pipe() {
            Ok(pipe) => pipe,
            Err(error) => {
                // Lack of report descriptors must not prevent enforcement.
                diagnostic!("grim-reaper: worker error-report pipe failed: {error}");
                [-1; 2]
            }
        };
        // SAFETY: the child immediately uses only raw libc/Mach operations and
        // _exit; no Rust locks/destructors, Objective-C, Foundation, or GUI code.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // SAFETY: this descriptor belongs to this child's copy of the pipe.
            if pipe[0] >= 0 {
                unsafe {
                    libc::close(pipe[0]);
                }
            }
            run_shutdown_worker(method, pipe[1]);
        }
        let fork_error = if pid < 0 {
            Some(io::Error::last_os_error())
        } else {
            None
        };
        // SAFETY: the parent does not need the pipe's write end.
        if pipe[1] >= 0 {
            unsafe {
                libc::close(pipe[1]);
            }
        }
        let report = if pipe[0] >= 0 {
            // SAFETY: the parent takes unique ownership of its read descriptor.
            Some(unsafe { File::from_raw_fd(pipe[0]) })
        } else {
            None
        };
        if let Some(error) = fork_error {
            return Err(error).context("fork shutdown worker");
        }
        // The child also sets its group. Doing it in both processes closes the
        // race where a timeout/disarm reaches the parent before the child runs.
        // SAFETY: pid identifies the newly forked child.
        if unsafe { libc::setpgid(pid, pid) } != 0 {
            let error = io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(libc::EACCES | libc::ESRCH)) {
                diagnostic!("grim-reaper: worker process-group setup failed: {error}");
            }
        }
        Ok(Self {
            pid,
            started: Instant::now(),
            killed: false,
            report,
        })
    }

    fn poll(&mut self, name: &str) -> bool {
        let mut status = 0;
        // SAFETY: pid is this tracked child and status is writable. WNOHANG is
        // essential: even SIGKILL cannot reap an uninterruptible kernel task.
        let result = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
        if result == 0 {
            return false;
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                diagnostic!("grim-reaper: {name} worker poll failed: {error}");
            }
            return error.raw_os_error() == Some(libc::ECHILD);
        }
        if libc::WIFSIGNALED(status) {
            diagnostic!(
                "grim-reaper: {name} worker terminated by signal {}",
                libc::WTERMSIG(status)
            );
        } else if libc::WIFEXITED(status) {
            let code = libc::WEXITSTATUS(status);
            if code == 0 {
                diagnostic!("grim-reaper: {name} returned; machine still running, retrying");
            } else {
                self.log_failure(name, code);
            }
        } else {
            diagnostic!("grim-reaper: {name} worker returned unexpected wait status {status}");
        }
        true
    }

    fn log_failure(&mut self, name: &str, code: i32) {
        if let Some(report) = &mut self.report {
            let mut bytes = [0; 8];
            match report.read(&mut bytes) {
                Ok(8) => {
                    let stage =
                        u32::from_ne_bytes(bytes[..4].try_into().expect("fixed stage bytes"));
                    let errno =
                        i32::from_ne_bytes(bytes[4..].try_into().expect("fixed errno bytes"));
                    let stage = match stage {
                        1 => "open SysRq trigger",
                        2 => "write SysRq trigger",
                        3 => "shutdown syscall",
                        4 => "exec helper",
                        5 => "set process group",
                        6 => "redirect helper stdin",
                        7 => "reset worker signal state",
                        _ => "unknown worker stage",
                    };
                    diagnostic!(
                        "grim-reaper: {name} failed at {stage}: {}",
                        io::Error::from_raw_os_error(errno)
                    );
                    return;
                }
                Ok(0) => {}
                Ok(count) => {
                    diagnostic!("grim-reaper: {name} error report truncated ({count} bytes)")
                }
                Err(error) => diagnostic!("grim-reaper: {name} error-report read failed: {error}"),
            }
        }
        diagnostic!("grim-reaper: {name} worker/helper exited with status {code}");
    }

    fn kill(&self, name: &str) {
        // SAFETY: an unreaped child retains its PID; its own process group also
        // contains helper descendants. Neither kill is followed by blocking wait.
        for target in [-self.pid, self.pid] {
            if unsafe { libc::kill(target, libc::SIGKILL) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    diagnostic!("grim-reaper: {name} worker kill failed: {error}");
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn worker_error_pipe() -> io::Result<[libc::c_int; 2]> {
    let mut pipe = [-1; 2];
    // SAFETY: pipe points to two writable descriptor slots.
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pipe)
}

#[cfg(target_os = "macos")]
fn worker_error_pipe() -> io::Result<[libc::c_int; 2]> {
    let mut pipe = [-1; 2];
    // macOS has no pipe2. Set both flags in the parent before forking; the
    // daemon is single-threaded and does not use Objective-C/Foundation.
    // SAFETY: all operations use the two descriptors allocated by pipe.
    unsafe {
        if libc::pipe(pipe.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        for fd in pipe {
            let descriptor_flags = libc::fcntl(fd, libc::F_GETFD);
            let status_flags = libc::fcntl(fd, libc::F_GETFL);
            if descriptor_flags < 0
                || status_flags < 0
                || libc::fcntl(fd, libc::F_SETFD, descriptor_flags | libc::FD_CLOEXEC) < 0
                || libc::fcntl(fd, libc::F_SETFL, status_flags | libc::O_NONBLOCK) < 0
            {
                let error = io::Error::last_os_error();
                libc::close(pipe[0]);
                libc::close(pipe[1]);
                return Err(error);
            }
        }
    }
    Ok(pipe)
}

#[cfg(target_os = "linux")]
fn worker_errno() -> libc::c_int {
    // SAFETY: libc returns this thread's live errno slot.
    unsafe { *libc::__errno_location() }
}

#[cfg(target_os = "macos")]
fn worker_errno() -> libc::c_int {
    // SAFETY: Darwin libc returns this thread's live errno slot.
    unsafe { *libc::__error() }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn reset_worker_signals(report_fd: libc::c_int) {
    // Helpers must not inherit ignored or blocked termination signals. Default
    // termination uses no Rust handler, unwinding, allocator, or exit hooks.
    // SAFETY: sigaction is a C POD struct; its mask is initialized explicitly.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        if libc::sigemptyset(&mut action.sa_mask) != 0 {
            worker_failure(report_fd, 7, worker_errno());
        }
        for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM, libc::SIGPIPE] {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                worker_failure(report_fd, 7, worker_errno());
            }
        }
        if libc::sigprocmask(libc::SIG_SETMASK, &action.sa_mask, std::ptr::null_mut()) != 0 {
            worker_failure(report_fd, 7, worker_errno());
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn redirect_worker_input(report_fd: libc::c_int) {
    // SAFETY: raw descriptors and static C strings only, including after fork.
    unsafe {
        let input = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        if input < 0 {
            worker_failure(report_fd, 6, worker_errno());
        }
        if libc::dup2(input, libc::STDIN_FILENO) < 0 {
            worker_failure(report_fd, 6, worker_errno());
        }
        if input != libc::STDIN_FILENO {
            libc::close(input);
        }
    }
}

// Values verified against the macOS SDK's sys/reboot.h. Darwin libc does not
// export these private Apple constants; never substitute reboot/panic flags.
#[cfg(target_os = "macos")]
const MAC_RB_NOSYNC: libc::c_int = 0x04;
#[cfg(target_os = "macos")]
const MAC_RB_HALT: libc::c_int = 0x08;
#[cfg(target_os = "macos")]
const MAC_RB_QUICK: libc::c_int = 0x400;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_shutdown_worker(method: ShutdownMethod, report_fd: libc::c_int) -> ! {
    // Everything in the child is allocation-free and avoids inherited Rust
    // locks, Objective-C, and Foundation. Static exec argv has no shell or
    // untrusted interpolation. All exits are signal termination or _exit.
    reset_worker_signals(report_fd);
    // SAFETY: these raw operations act only on this worker or static C strings.
    unsafe {
        if libc::setpgid(0, 0) != 0 {
            worker_failure(report_fd, 5, worker_errno());
        }
        #[cfg(target_os = "linux")]
        match method {
            ShutdownMethod::SysRq => {
                let fd = libc::open(
                    c"/proc/sysrq-trigger".as_ptr(),
                    libc::O_WRONLY | libc::O_CLOEXEC,
                );
                if fd < 0 {
                    worker_failure(report_fd, 1, worker_errno());
                }
                // Linux >= 6.12 processes every character after '_' in one
                // kernel write. Separate writes cannot survive i to reach o.
                // Preserve the requested r/e/i/u/s/o order, with no newline.
                let result = libc::write(fd, b"_reiuso".as_ptr().cast(), 7);
                let errno = if result < 0 {
                    worker_errno()
                } else {
                    libc::EIO
                };
                libc::close(fd);
                if result != 7 {
                    worker_failure(report_fd, 2, errno);
                }
            }
            ShutdownMethod::PowerOff | ShutdownMethod::Halt => {
                let command = if matches!(method, ShutdownMethod::PowerOff) {
                    libc::LINUX_REBOOT_CMD_POWER_OFF
                } else {
                    libc::LINUX_REBOOT_CMD_HALT
                };
                if libc::reboot(command) != 0 {
                    worker_failure(report_fd, 3, worker_errno());
                }
            }
            ShutdownMethod::Systemctl | ShutdownMethod::PoweroffHelper => {
                redirect_worker_input(report_fd);
                if matches!(method, ShutdownMethod::Systemctl) {
                    let argv = [
                        c"/usr/bin/systemctl".as_ptr(),
                        c"poweroff".as_ptr(),
                        c"--force".as_ptr(),
                        c"--force".as_ptr(),
                        std::ptr::null(),
                    ];
                    libc::execv(argv[0], argv.as_ptr());
                } else {
                    let argv = [c"/sbin/poweroff".as_ptr(), c"-f".as_ptr(), std::ptr::null()];
                    libc::execv(argv[0], argv.as_ptr());
                }
                worker_failure(report_fd, 4, worker_errno());
            }
        }
        #[cfg(target_os = "macos")]
        match method {
            ShutdownMethod::QuickHalt | ShutdownMethod::NoSyncHalt => {
                let mut flags = MAC_RB_HALT | MAC_RB_QUICK;
                if matches!(method, ShutdownMethod::NoSyncHalt) {
                    flags |= MAC_RB_NOSYNC;
                }
                if macos_reboot(flags) != 0 {
                    worker_failure(report_fd, 3, worker_errno());
                }
            }
            ShutdownMethod::QuickHaltHelper
            | ShutdownMethod::NoSyncHaltHelper
            | ShutdownMethod::ShutdownHelper => {
                redirect_worker_input(report_fd);
                match method {
                    ShutdownMethod::QuickHaltHelper => {
                        let argv = [c"/sbin/halt".as_ptr(), c"-q".as_ptr(), std::ptr::null()];
                        libc::execv(argv[0], argv.as_ptr());
                    }
                    ShutdownMethod::NoSyncHaltHelper => {
                        let argv = [
                            c"/sbin/halt".as_ptr(),
                            c"-q".as_ptr(),
                            c"-n".as_ptr(),
                            std::ptr::null(),
                        ];
                        libc::execv(argv[0], argv.as_ptr());
                    }
                    _ => {
                        let argv = [
                            c"/sbin/shutdown".as_ptr(),
                            c"-h".as_ptr(),
                            c"now".as_ptr(),
                            std::ptr::null(),
                        ];
                        libc::execv(argv[0], argv.as_ptr());
                    }
                }
                worker_failure(report_fd, 4, worker_errno());
            }
        }
        libc::_exit(0);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn worker_failure(report_fd: libc::c_int, stage: u32, errno: i32) -> ! {
    let mut bytes = [0; 8];
    bytes[..4].copy_from_slice(&stage.to_ne_bytes());
    bytes[4..].copy_from_slice(&errno.to_ne_bytes());
    // SAFETY: the child owns the report descriptor, and bytes is a live 8-byte
    // buffer. Nonblocking reporting cannot hold up failure exit.
    unsafe {
        if report_fd >= 0 {
            libc::write(report_fd, bytes.as_ptr().cast(), bytes.len());
        }
        libc::_exit(255);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(armed: bool) -> Config {
        Config {
            armed,
            timeout_secs: 120,
            boot_grace_secs: 30,
        }
    }

    fn clock(wall_secs: u64, uptime_secs: u64) -> Clock {
        Clock {
            wall_secs,
            boot_id: "11111111-1111-1111-1111-111111111111".to_owned(),
            uptime_secs,
        }
    }

    fn heartbeat_at(wall_secs: u64, uptime_secs: u64, same_boot: bool) -> Heartbeat {
        Heartbeat {
            wall_secs,
            uptime_secs,
            boot_id: if same_boot {
                "11111111-1111-1111-1111-111111111111"
            } else {
                "22222222-2222-2222-2222-222222222222"
            }
            .to_owned(),
        }
    }

    fn state(armed: bool, heartbeat: Option<Heartbeat>, now: Clock, latched: bool) -> Status {
        evaluate(
            LoadedConfig {
                config: config(armed),
                diagnostic: None,
            },
            Ok(heartbeat),
            Ok(now),
            Ok(latched),
            false,
        )
    }

    #[test]
    fn defaults_are_disarmed_with_required_timings() {
        let defaults = Config::default();
        assert!(!defaults.armed);
        assert_eq!(defaults.timeout_secs, 129_600);
        assert_eq!(defaults.boot_grace_secs, 1_800);
    }

    #[test]
    fn missing_heartbeat_gets_only_boot_grace() {
        let grace = state(true, None, clock(1000, 29), false);
        assert_eq!(grace.phase, "boot-grace");
        assert_eq!(grace.boot_grace_remaining_secs, 1);
        assert_eq!(grace.expires_in_secs, Some(1));
        assert_eq!(state(true, None, clock(1000, 30), false).phase, "expired");
    }

    #[test]
    fn same_boot_age_resists_both_wall_clock_directions() {
        for wall in [1, 10_000_000] {
            let status = state(
                true,
                Some(heartbeat_at(1000, 100, true)),
                clock(wall, 130),
                false,
            );
            assert_eq!(status.phase, "healthy");
            assert_eq!(status.heartbeat_age_secs, Some(30));
            assert_eq!(status.expires_in_secs, Some(90));
        }
    }

    #[test]
    fn sleep_inclusive_age_expires_at_boundary_despite_wall_clock_rollback() {
        // Simulate Mach continuous ticks with a nontrivial timebase, including
        // a sleep gap. The same-boot wall clock goes backwards independently.
        let before_sleep = continuous_time_seconds(2_400_000_000, 125, 3).unwrap();
        let heartbeat = heartbeat_at(1000, before_sleep, true);
        let before_boundary = continuous_time_seconds(5_256_000_000, 125, 3).unwrap();
        let healthy = state(true, Some(heartbeat), clock(1, before_boundary), false);
        assert_eq!(healthy.heartbeat_age_secs, Some(119));
        assert_eq!(healthy.expires_in_secs, Some(1));
        assert_eq!(healthy.phase, "healthy");

        let at_boundary = continuous_time_seconds(5_280_000_000, 125, 3).unwrap();
        let expired = state(
            true,
            Some(heartbeat_at(1000, before_sleep, true)),
            clock(1, at_boundary),
            false,
        );
        assert_eq!(expired.heartbeat_age_secs, Some(120));
        assert_eq!(expired.expires_in_secs, Some(0));
        assert_eq!(expired.phase, "expired");
    }

    #[test]
    fn continuous_clock_conversion_is_checked_and_rounds_down() {
        assert_eq!(continuous_time_seconds(23_999_999, 125, 3).unwrap(), 0);
        assert_eq!(continuous_time_seconds(24_000_000, 125, 3).unwrap(), 1);
        assert!(continuous_time_seconds(1, 0, 1).is_err());
        assert!(continuous_time_seconds(1, 1, 0).is_err());
        assert!(continuous_time_seconds(u64::MAX, u32::MAX, 1).is_err());
    }

    #[test]
    fn persisted_cascade_progress_advances_on_restart_and_disarm_clears_it() {
        let state_dir = std::env::temp_dir().join(format!(
            "grim-reaper-cascade-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        initialize(&state_dir, 120, 0).unwrap();
        heartbeat(&state_dir).unwrap();
        arm(&state_dir).unwrap();
        atomic_write(&state_dir.join(LATCH_FILE), INITIAL_LATCH).unwrap();
        assert!(schedule_sysrq(&state_dir).unwrap());
        assert_eq!(
            read_state_file(&state_dir.join(LATCH_FILE))
                .unwrap()
                .as_deref(),
            Some(SYSRQ_SCHEDULED_LATCH)
        );
        // No in-memory state is involved: a fresh daemon reads this same
        // durable record and must start at direct poweroff rather than REIUSO.
        assert!(!schedule_sysrq(&state_dir).unwrap());
        assert_eq!(status(&state_dir).unwrap().phase, "expired");
        assert!(heartbeat(&state_dir).is_err());
        assert!(arm(&state_dir).is_err());
        assert_eq!(disarm(&state_dir).unwrap().phase, "disarmed");
        assert!(
            read_state_file(&state_dir.join(LATCH_FILE))
                .unwrap()
                .is_none()
        );
        assert!(!schedule_sysrq(&state_dir).unwrap());

        heartbeat(&state_dir).unwrap();
        arm(&state_dir).unwrap();
        atomic_write(&state_dir.join(LATCH_FILE), INITIAL_LATCH).unwrap();
        assert!(schedule_sysrq(&state_dir).unwrap());
        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn missing_or_unknown_cascade_progress_cannot_schedule_reiuso() {
        assert!(should_schedule_sysrq(None).is_err());
        assert!(should_schedule_sysrq(Some(b"")).is_err());
        assert!(should_schedule_sysrq(Some(b"unknown\n")).is_err());
        assert!(should_schedule_sysrq(Some(INITIAL_LATCH)).unwrap());
        assert!(!should_schedule_sysrq(Some(SYSRQ_SCHEDULED_LATCH)).unwrap());
    }

    #[test]
    fn timeout_expires_at_exact_monotonic_boundary() {
        assert_eq!(
            state(
                true,
                Some(heartbeat_at(1000, 100, true)),
                clock(1001, 219),
                false
            )
            .phase,
            "healthy"
        );
        let expired = state(
            true,
            Some(heartbeat_at(1000, 100, true)),
            clock(1001, 220),
            false,
        );
        assert_eq!(expired.phase, "expired");
        assert_eq!(expired.expires_in_secs, Some(0));
    }

    #[test]
    fn prior_boot_uses_wall_age_and_still_observes_boot_grace() {
        let healthy = state(
            true,
            Some(heartbeat_at(950, 50_000, false)),
            clock(1000, 31),
            false,
        );
        assert_eq!(healthy.heartbeat_age_secs, Some(50));
        assert_eq!(healthy.phase, "healthy");
        assert_eq!(
            state(
                true,
                Some(heartbeat_at(800, 50_000, false)),
                clock(1000, 29),
                false
            )
            .phase,
            "boot-grace"
        );
        assert_eq!(
            state(
                true,
                Some(heartbeat_at(800, 50_000, false)),
                clock(1000, 30),
                false
            )
            .phase,
            "expired"
        );
    }

    #[test]
    fn future_heartbeat_cannot_extend_life() {
        let uptime_future = state(
            true,
            Some(heartbeat_at(999, 41, true)),
            clock(1000, 40),
            false,
        );
        assert_eq!(uptime_future.phase, "expired");
        assert_eq!(uptime_future.heartbeat_age_secs, None);
        let wall_future = state(
            true,
            Some(heartbeat_at(1001, 1, false)),
            clock(1000, 40),
            false,
        );
        assert_eq!(wall_future.phase, "expired");
        assert_eq!(wall_future.heartbeat_age_secs, None);
    }

    #[test]
    fn corrupt_heartbeat_expires_after_grace() {
        for bytes in [
            b"not json".as_slice(),
            br#"{"wall_secs":1000,"uptime_secs":2,"boot_id":""}"#.as_slice(),
            br#"{"wall_secs":1000,"uptime_secs":2,"boot_id":"corrupt"}"#.as_slice(),
        ] {
            let invalid = decode_heartbeat(bytes).unwrap_err().to_string();
            let status = evaluate(
                LoadedConfig {
                    config: config(true),
                    diagnostic: None,
                },
                Err(invalid),
                Ok(clock(1000, 40)),
                Ok(false),
                false,
            );
            assert_eq!(status.phase, "expired");
            assert_eq!(status.heartbeat_age_secs, None);
        }
    }

    #[test]
    fn missing_corrupt_and_invalid_config_are_failclosed() {
        for bytes in [
            None,
            Some(b"invalid".as_slice()),
            Some(br#"{"armed":false,"timeout_secs":0,"boot_grace_secs":30}"#.as_slice()),
        ] {
            let loaded = decode_config(bytes);
            assert!(loaded.config.armed);
            assert!(loaded.diagnostic.is_some());
            assert_eq!(loaded.config.timeout_secs, DEFAULT_TIMEOUT_SECS);
            let status = evaluate(loaded, Ok(None), Ok(clock(1000, 2000)), Ok(false), false);
            assert_eq!(status.phase, "expired");
            assert!(status.reason.contains("fail-closed config"));
        }
    }

    #[test]
    fn latch_overrides_heartbeat_but_not_disarm_or_boot_grace() {
        assert_eq!(
            state(
                true,
                Some(heartbeat_at(1000, 40, true)),
                clock(1000, 40),
                true
            )
            .phase,
            "expired"
        );
        assert_eq!(state(true, None, clock(1000, 20), true).phase, "boot-grace");
        assert_eq!(state(false, None, clock(1000, 40), true).phase, "disarmed");
        let memory_latched = evaluate(
            LoadedConfig {
                config: config(true),
                diagnostic: None,
            },
            Ok(Some(heartbeat_at(1000, 40, true))),
            Ok(clock(1000, 40)),
            Ok(false),
            true,
        );
        assert_eq!(memory_latched.phase, "expired");
    }

    #[test]
    fn clock_or_latch_failure_cannot_make_armed_state_healthy() {
        let clock_failure = evaluate(
            LoadedConfig {
                config: config(true),
                diagnostic: None,
            },
            Ok(None),
            Err("uptime unavailable".to_owned()),
            Ok(false),
            false,
        );
        assert_eq!(clock_failure.phase, "expired");
        let latch_failure = evaluate(
            LoadedConfig {
                config: config(true),
                diagnostic: None,
            },
            Ok(Some(heartbeat_at(1000, 40, true))),
            Ok(clock(1000, 40)),
            Err("permission denied".to_owned()),
            false,
        );
        assert_eq!(latch_failure.phase, "expired");
        let disarmed = evaluate(
            LoadedConfig {
                config: config(false),
                diagnostic: None,
            },
            Err("corrupt".to_owned()),
            Err("clock unavailable".to_owned()),
            Err("unreadable".to_owned()),
            true,
        );
        assert_eq!(disarmed.phase, "disarmed");
    }

    #[test]
    fn arm_requires_fresh_real_unlatched_heartbeat() {
        let config = config(false);
        let now = clock(1000, 200);
        assert!(require_fresh_heartbeat(&config, None, &now, false).is_err());
        assert!(
            require_fresh_heartbeat(&config, Some(&heartbeat_at(940, 140, true)), &now, false)
                .is_ok()
        );
        assert!(
            require_fresh_heartbeat(&config, Some(&heartbeat_at(939, 139, true)), &now, false)
                .is_err()
        );
        assert!(
            require_fresh_heartbeat(&config, Some(&heartbeat_at(1000, 200, true)), &now, true)
                .is_err()
        );
        assert!(
            require_fresh_heartbeat(&config, Some(&heartbeat_at(1000, 201, true)), &now, false)
                .is_err()
        );
        let short = Config {
            timeout_secs: 1,
            ..config
        };
        assert!(
            require_fresh_heartbeat(&short, Some(&heartbeat_at(999, 199, true)), &now, false)
                .is_err()
        );
    }

    #[test]
    fn config_bounds_allow_zero_grace_but_not_unbounded_timings() {
        assert!(
            Config {
                armed: false,
                timeout_secs: 1,
                boot_grace_secs: 0
            }
            .validate()
            .is_ok()
        );
        assert!(
            Config {
                armed: false,
                timeout_secs: 0,
                boot_grace_secs: 0
            }
            .validate()
            .is_err()
        );
        assert!(
            Config {
                armed: true,
                timeout_secs: MAX_TIMEOUT_SECS + 1,
                boot_grace_secs: 0
            }
            .validate()
            .is_err()
        );
        assert!(
            Config {
                armed: true,
                timeout_secs: 1,
                boot_grace_secs: MAX_BOOT_GRACE_SECS + 1
            }
            .validate()
            .is_err()
        );
    }
}

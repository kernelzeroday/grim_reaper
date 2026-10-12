# CLI reference

This reference describes version 0.2.0. The [README](readme.md) covers initial setup and everyday operation; the [security model](security-model.md) defines authorization and enforcement limits.

## Invocation and privileges

```text
grim-reaper [--state-dir PATH] COMMAND
```

`--help`, command-specific `--help`, and `--version` are available. `--state-dir` is global and may appear after a command. Its default is `/var/lib/grim-reaper` on Linux and `/var/db/grim-reaper` on macOS. Production state and live daemon execution require root. An isolated private directory is supported for non-destructive inspection and state-machine exercises; it is not an alternate fleet target configuration.

Sender commands (`touch`, `clock`, `targets`, and `install --local`) run as the normal user. Fleet commands invoke the installed root receipt/status command through the exact configured sudo rule. `--state-dir` does not override the fixed state directory used by fleet helpers or installed services. Root service installation rejects a non-default state directory.

## Command reference

| Command | Behavior | Privilege / important conditions |
|---|---|---|
| `touch [TARGET...]` | Submit one manual receipt to each selected target | Normal user; local or remote receipt authorization required |
| `clock [TARGET...] [--json]` | Read each selected countdown; never sends receipts | Normal user; local or remote JSON-status authorization required |
| `targets add TARGET` | Save a target; adding an existing target is harmless | Normal user; does not contact the target |
| `targets list` | List saved targets in stored order | Normal user |
| `targets remove TARGET` | Remove a saved target; error if not present | Normal user; does not disarm the removed machine |
| `heartbeat` | Record one receipt using this machine's clock | Root for production state; refuses any existing/unreadable latch |
| `status [--json]` | Read this machine's state, policy, and remaining time | Root for production state; does not write state |
| `arm --confirm-hard-poweroff` | Explicitly enable shutdown enforcement | Root; unexpired receipt at most 60 seconds old; no latch |
| `disarm` | Persist disabled enforcement, then clear the latch/progress | Root; write failure is an error, not successful recovery |
| `daemon [--dry-run] [--once]` | Monitor local state, normally under the service manager | Live execution requires root; `--once` requires `--dry-run` |
| `install --local` | Atomically install the user CLI into `~/.local/bin` | Normal user; does not install/arm a daemon |
| `install [OPTIONS]` | Install/update root service and optionally receipt delegation | Root on Linux/macOS; existing installation must be disarmed |

`--confirm-hard-poweroff` is mandatory even on macOS, where the platform actions are halt/poweroff calls rather than Linux SysRq. Arming does not schedule heartbeats and does not reset the deadline.

### Targets and saved configuration

No positional targets means the saved target array. Explicit targets replace that selection for this invocation; they do not update the saved array. An empty selection is an error. The entire selection is validated before any receipt/query is attempted.

`local` is reserved for the installed local root CLI through sudo, not an SSH hostname. Other targets support `host` or `user@host`, with ASCII letters, digits, `.`, `_`, and `-`; components cannot start with `-`. Shell syntax, inline ports, and literal IPv6 addresses are rejected. Use an SSH configuration alias for a nonstandard port or IPv6 host.

Saved configuration is a JSON string array at `$XDG_CONFIG_HOME/grim-reaper/targets.json` or `~/.config/grim-reaper/targets.json`. A supplied `XDG_CONFIG_HOME` must be absolute. Saving/removing targets does not authorize access or alter the remote switch. **Removing a target from the sender does not stop its countdown.**

### Fleet delivery and failure handling

Remote commands use SSH with `BatchMode=yes`, a ten-second connection timeout, and keepalive settings. Host-key verification is not disabled. SSH uses the operator's existing configuration and keys. Local commands use noninteractive sudo. Neither path prompts for a remote account/sudo password; configure access before relying on it.

Each helper is bounded to 30 seconds, with stdout and stderr separately limited to 64 KiB. Every selected target is attempted despite earlier target failures. Any failure causes an unsuccessful aggregate exit. A timed-out/failed heartbeat may already have reached its target: the CLI does not automatically retry or claim that the receipt was not recorded.

`clock` prints a table with target, phase, armed state, remaining time, heartbeat age, and remaining boot grace. Durations are compact hours/minutes/seconds. Error rows preserve successful hosts' results. Reads are sequential snapshots, not a simultaneous fleet measurement or remote attestation.

### JSON status contract

`status --json` returns one status object. `clock --json` returns an array of rows, each with `target`, `status`, and `error`. Successful rows have a status object and `error: null`; unsuccessful rows have `status: null` and an error string. Even when some rows fail, the complete result array is printed to stdout before the CLI exits unsuccessfully. Diagnostics go to stderr.

| Status field | Meaning |
|---|---|
| `armed` | Effective enforcement setting, including fail-closed defaults |
| `phase` | `disarmed`, `boot-grace`, `healthy`, or `expired` |
| `reason` | Diagnostic explanation; not a stable machine-readable error code |
| `heartbeat_age_secs` | Valid receipt age, or null if it cannot be established |
| `expires_in_secs` | Effective remaining lifetime accounting for grace/latch, or null when inactive/unavailable |
| `boot_grace_remaining_secs` | Remaining seconds before boot grace ends |
| `config` | Effective `armed`, `timeout_secs`, and `boot_grace_secs` policy |

Exact seconds are preserved in JSON. Zero remaining time is not a shutdown receipt; an unreachable target is not proof of poweroff. JSON parsing rejects malformed/trailing data and unknown/inconsistent phases. Human error rows strip terminal control characters.

### Installation options

```text
install [--heartbeat-user USER] [--timeout-secs SECONDS] [--boot-grace-secs SECONDS]
install --local
```

- `--heartbeat-user`: existing non-root account allowed exact passwordless receipt and JSON status. Validated account/sudoers syntax; `root`, `ALL`, and UID 0 accounts are rejected. Omitting it does not create or alter an existing sudoers rule.
- `--timeout-secs`: new-configuration lifetime; default 129600; accepted range 1 through 31536000.
- `--boot-grace-secs`: new-configuration grace; default 1800; accepted range 0 through 86400.
- `--local` conflicts with `--heartbeat-user`. Timing options do not change the sender installation.

Existing policy timings are preserved, not overwritten by installation arguments. For intentional policy changes, disarm first, update the root-owned configuration with valid timings, verify status, deliver a fresh receipt, and explicitly rearm. Do not edit armed configuration or delete state files as a maintenance shortcut.

## Convenience script

Repository script: `heartbeat.sh`. Installed convenience command: `~/bin/grim-touch`.

```sh
grim-touch                       # All saved targets
grim-touch local                 # This Mac only
grim-touch server-a.example server-b.example
```

The script requires a readable SSH private key and its `.pub` file. Select the key with `GRIM_SSH_KEY`; the default is `~/.ssh/id_ed25519`. It finds the CLI in `~/.local/bin` first, otherwise through PATH. It checks the SSH agent, tests whether this specific key is usable, and calls `ssh-add` at most once to unlock it. It then forwards the selected targets to `touch` and preserves failure status. A custom key path belongs in private shell configuration, not a deployment-specific repository edit.

An inherited agent/key remains available according to that agent's lifetime and constraints. With no usable agent, the script starts one for the invocation and cleans it up through an EXIT trap. SIGKILL or a process/system crash can bypass cleanup; an agent is not a secure automatic lock mechanism. No passphrase is stored. Existing SSH settings still determine which identity/account each host accepts.

## Exit status and safe inspection

Successful operations exit 0. Runtime/authorization/partial-fleet errors exit 1. Clap argument errors normally exit 2. The shell convenience script propagates failures from key handling or the CLI. Use exit status and per-target results together; stdout may contain useful successful rows even on failure.

```sh
grim-reaper --state-dir /tmp/grim-inspect daemon --dry-run --once
```

Dry-run never persists a shutdown latch/progress or invokes shutdown workers. Missing configuration is still evaluated fail-closed rather than disguised as disarmed. Live `--once` is forbidden. Do not run a live daemon to experiment with expiry.

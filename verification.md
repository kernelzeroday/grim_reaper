# Verification guide

This document describes repeatable checks and their limits. It intentionally contains no machine inventory, deployment dates, account names, network addresses, installed service records, or authentication history. Keep deployment-specific evidence in private storage outside this repository.

## Safe development checks

```sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets -- -D warnings
cargo build --release --locked
bash -n heartbeat.sh
```

Build natively on Linux and macOS to cover conditional platform code. On macOS, also run `plutil -lint launchd/local.grim-reaper.plist`. Kernel shutdown interfaces cannot be proven by compiling a cross-platform sender.

The regression suite covers state boundaries and transitions, malformed/missing/future receipts, same-boot wall-clock changes, boot grace, latch precedence, fresh arming, durable Linux sequence progress across restart, sleep-inclusive clock conversion, target/account injection, partial fleet errors, invalid status data, subprocess deadlines, inherited pipes, and excessive helper output.

Tests do not invoke real shutdown workers. They are not evidence of firmware poweroff or a hardware-watchdog guarantee.

## CLI inspection

Use help/version commands to check the documented interface without writing state:

```sh
grim-reaper --version
grim-reaper --help
grim-reaper clock --help
grim-reaper install --help
```

Use a private isolated directory for non-destructive daemon decisions:

```sh
grim-reaper --state-dir /tmp/grim-inspect daemon --dry-run --once
```

Dry-run does not persist a shutdown latch/progress or fork destructive workers. Missing configuration intentionally evaluates fail-closed. **Never run a live root daemon against uninitialized state as a test.**

A useful isolated CLI smoke exercises disarm, rejected arming without a fresh receipt, receipt, explicit confirmed arm, valid status, future/corrupt receipt, dry-run expiry, latch refusal, and explicit disarm recovery. Fixture files must have the required private ownership/modes. Run production-state commands only as intentional operations on an installed target.

## Fleet and wrapper checks

For a safe development smoke, replace user-level SSH/sudo helpers only within the smoke environment and use real isolated-state readers. Verify:

- `clock` returns all selected successful rows and preserves exact JSON seconds.
- Querying leaves configuration/receipt/latch bytes unchanged.
- Failed/unreachable/malformed targets produce error rows and nonzero aggregate exit while preserving other targets.
- Invalid target syntax is rejected before any delivery.
- Human error output cannot inject terminal control sequences or additional rows.
- Oversized output and hung helpers terminate within the bounded failure path.
- A locked selected key is unlocked at most once; an already usable key is not unlocked again.
- Failed key unlock prevents delivery; sender failure propagates; a temporary owned agent is cleaned up on normal exit.
- `GRIM_SSH_KEY` selects a custom key without publishing a private pathname or editing repository defaults.

Inspect a real installed fleet with `clock` only after configuring authorization. `grim-touch`/`touch` are explicit receipt/reset operations, not read-only checks. A helper timeout does not prove that no receipt reached the target, and an unreachable target does not prove that it powered off.

## Shutdown-supervisor smoke boundary

Real worker control flow can be exercised without poweroff using temporary platform interposition guards and isolated state, while running as an **actually unprivileged user**. Such guards must block every shutdown syscall and helper execution; a Linux SysRq write must be redirected to an inert destination or denied. If the root gate is simulated, do not grant actual root privileges.

A guarded smoke can check ordered attempts, exact `_reiuso` bytes, durable first-sequence progress, restart continuation, continued fallback after a deliberately hung first worker, tracked worker bounds, and disarming between attempts. On macOS it can check native halt-only method scheduling without invoking the platform helpers.

Temporary guards belong outside the repository's production configuration and installed service environment. Do not install them, claim their output proves real poweroff, or run an unguarded live daemon to reproduce a timeout. Isolated guards and smoke scaffolding should be removed after verification.

## Installation verification

Fresh installation must initialize disarmed state before starting a supervised process. Check service syntax, root ownership/private state, exact sudoers parsing, installed binary version, and active/running service-manager state. A loaded service without a running PID is not sufficient. Existing armed or unsafe installations must refuse silent replacement/reset.

Check passwordless receipt/status authorization without relying on a cached administrative sudo timestamp:

```sh
sudo -n -k -- /usr/local/bin/grim-reaper status --json
```

This checks only exact JSON-status authorization. It is not evidence of permission to arm, disarm, install, or invoke arbitrary privileged commands.

For activation, authenticate administration before recording the fresh receipt; otherwise password-entry delay can consume the 60-second arming window. See the README for the explicit sequence. No automatic receipt scheduler is installed or used as a safety-test shortcut.

## Explicitly disruptive verification

Actual poweroff, reboot/startup behavior, real suspend/resume expiry, storage crash consistency under power loss, and firmware/hardware-watchdog behavior require separately planned tests with backups and independent observation. They cannot be inferred from unit tests, dry-run, guarded syscalls, a running PID, or a missing SSH response.

The project does not claim remote attestation, secure erasure, containment of root/kernel attackers, or guaranteed enforcement while the OS cannot execute. Record results and machine-specific limitations privately; publish only generalized methodology or anonymized findings with deliberate review.

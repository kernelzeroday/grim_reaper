# Grim Reaper

A **manual SSH/local heartbeat dead-man switch** for Linux and macOS. One Rust executable provides a sender, fleet countdowns, and a supervised root daemon. An armed target deliberately shuts down when its heartbeat expires. **Unsaved data and buffered writes can be lost.**

Read the [security model](security-model.md) and [threat model](threat-model.md) before relying on enforcement.

Public documentation: [CLI reference](cli.md), [architecture](architecture.md), and [verification guide](verification.md). Deployment inventories, logs, account names, and authorization records belong outside the repository.

## Everyday use

```sh
~/bin/grim-touch                  # Manually refresh all saved local/remote targets
~/bin/grim-touch server-a.example # Refresh only the named target
grim-reaper clock                # Read every saved host's countdown; does NOT refresh anything
grim-reaper clock --json         # Machine-readable per-host status/error rows
```

**You must run the heartbeat command manually before each host's 36-hour deadline.** There is no recurring heartbeat sender, cron job, or heartbeat launch agent. The daemon's automatic monitoring does not refresh the timer. A successful receipt resets only that target's countdown; partial fleet failures return a nonzero exit code and do not rescue failed targets.

`~/bin/grim-touch` is installed from `heartbeat.sh`. It reuses `ssh-agent` and checks the key selected by `GRIM_SSH_KEY` (default `~/.ssh/id_ed25519`). If locked, it asks for that key's passphrase **once for the entire invocation**, not once per host. If already unlocked, no passphrase is needed. With no usable agent, it creates a temporary one and removes it on normal exit; otherwise it leaves your existing agent intact. Heartbeat/status sudo rules are passwordless; administrative operations remain privileged. Existing SSH settings select remote users/identities. Unlocking a key does not change server authorization or require direct root SSH access. For a custom key, export `GRIM_SSH_KEY` in your private shell configuration; never store keys or passphrases in this repository.

## Policy and shutdown

- Default lifetime: **36 hours**. Default boot grace: **30 minutes**.
- Receipts use each target's clock. Same-boot expiry uses sleep-inclusive uptime; across boots it uses the saved wall timestamp.
- An armed missing, invalid, unreadable, or expired receipt fails closed after applicable boot grace. Missing/unusable configuration evaluates as armed with default timings. Runtime failures preventing a safe decision also fail closed.
- Expiry persists a latch. A late heartbeat is refused. Disarm **before** expiry; an already-started shutdown cannot be cancelled reliably.
- Linux first submits your exact **REIUSO** sequence as one `_reiuso` write to the kernel SysRq trigger. It persists sequence progress first, so if `i` kills the daemon and the machine survives, a restarted daemon continues to direct kernel poweroff, forced systemd poweroff, forced `poweroff`, then kernel halt. Separate per-letter userspace writes are not used.
- macOS uses quick kernel halt/poweroff and native halt helpers, then no-sync hard fallbacks and the native shutdown helper. macOS does not support Linux SysRq.
- Shutdown workers have three-second deadlines and bounded tracked slots. Remaining methods repeat while execution continues. REIUSO is an initial attempt rather than an endlessly repeated self-killing prelude.
- No real shutdown occurs in dry-run. A status snapshot is not proof that firmware will power off successfully.
- Sleep counts toward expiry, but the daemon cannot execute while the Mac is asleep. It enforces expiry on wake. A wedged kernel/hardware or a privileged attacker can defeat userspace enforcement; use a hardware watchdog for that boundary.

## Build and install

Rust 1.85+ and a native linker are required on both platforms.

```sh
cargo test --locked
cargo build --release --locked
./target/release/grim-reaper install --local
# Add ~/.local/bin and ~/bin to PATH if needed.
mkdir -p ~/bin
install -m 755 heartbeat.sh ~/bin/grim-touch
```

Install the root daemon on each Linux/macOS shutdown target:

```sh
sudo ./target/release/grim-reaper install --heartbeat-user "$(id -un)"
```

Fresh installation initializes **disarmed** state before starting the service. The installer validates a restricted sudoers rule allowing the named non-root account only exact receipt and JSON-status commands. It does not delegate arming, disarming, arbitrary state directories, or installation. Existing installations must be explicitly disarmed before upgrading; missing/corrupt state must not be silently reset by reinstalling.

| Platform | Root state | Supervisor | Installed root CLI |
|---|---|---|---|
| Linux | `/var/lib/grim-reaper` | `grim-reaper.service` | `/usr/local/bin/grim-reaper` |
| macOS | `/var/db/grim-reaper` | `system/local.grim-reaper` | `/usr/local/bin/grim-reaper` |

State directories are root-owned mode 0700 and files are private root-owned regular files. `/root/.death_touch` is not used. The old Python systemd service is retired during Linux cutover.

Custom timings apply only to new configuration:

```sh
sudo ./target/release/grim-reaper install --heartbeat-user "$(id -un)" \
  --timeout-secs 129600 --boot-grace-secs 1800
```

## Choose targets and activate

For a local daemon and two example remote targets, save the targets explicitly:

```sh
grim-reaper targets add local
grim-reaper targets add server-a.example
grim-reaper targets add server-b.example
grim-reaper targets list
~/bin/grim-touch
```

`local` is reserved for the installed local root CLI through sudo; it is not SSH to localhost. Other targets use SSH host aliases or `user@host`, existing keys, and normal host-key verification. Saved targets are in `$XDG_CONFIG_HOME/grim-reaper/targets.json`, or `~/.config/grim-reaper/targets.json`.

Authenticate before recording the activation receipt, so password-entry delays cannot consume its 60-second freshness window. Explicitly arm each machine:

```sh
sudo -v && sudo -n /usr/local/bin/grim-reaper heartbeat && sudo -n /usr/local/bin/grim-reaper arm --confirm-hard-poweroff
ssh -t server-a.example 'sudo -v && sudo -n /usr/local/bin/grim-reaper heartbeat && sudo -n /usr/local/bin/grim-reaper arm --confirm-hard-poweroff'
ssh -t server-b.example 'sudo -v && sudo -n /usr/local/bin/grim-reaper heartbeat && sudo -n /usr/local/bin/grim-reaper arm --confirm-hard-poweroff'
grim-reaper clock
```

Arming requires a receipt no more than 60 seconds old. Countdown is from the last receipt, not from arming. Countdown queries never arm or send receipts. Unreachable hosts are reported as errors, not assumed powered off.

## Maintenance and recovery

Disarm before changing installation or storage:

```sh
sudo /usr/local/bin/grim-reaper disarm
ssh -t server-a.example 'sudo /usr/local/bin/grim-reaper disarm'
```

Deleting a receipt/configuration is not a way to disable the switch. If an expired machine survives, disarm explicitly to clear its latch, deliver a new receipt, and arm only when ready.

Linux service/log inspection:

```sh
ssh -t server-a.example 'sudo systemctl status grim-reaper.service'
ssh -t server-a.example 'sudo journalctl -u grim-reaper.service -n 50 --no-pager'
```

Mac service/log inspection:

```sh
sudo launchctl print system/local.grim-reaper
sudo cat /var/log/grim-reaper.log
```

## Uninstall

First disarm. Linux:

```sh
sudo systemctl disable --now grim-reaper.service
sudo rm /etc/systemd/system/grim-reaper.service
sudo systemctl daemon-reload
sudo rm -r /var/lib/grim-reaper
```

macOS:

```sh
sudo launchctl bootout system/local.grim-reaper
sudo rm /Library/LaunchDaemons/local.grim-reaper.plist
sudo rm -r /var/db/grim-reaper
sudo rm /var/log/grim-reaper.log
```

On either target, remove `/usr/local/bin/grim-reaper` and `/etc/sudoers.d/grim-reaper` (omit sudoers removal if installed without `--heartbeat-user`). Remove sender binaries/scripts with `rm ~/.local/bin/grim-reaper ~/bin/grim-touch`; remove saved entries with `grim-reaper targets remove HOST` before removing the CLI if retaining other targets.

## Non-destructive inspection

```sh
grim-reaper --state-dir /tmp/grim-inspect daemon --dry-run --once
```

An absent config deliberately reports armed/fail-closed, even in dry-run. **Never run a live daemon against uninitialized state.** Live `--once` is forbidden. Installed services never use dry-run. State tests and blocked-syscall smoke checks verify control flow without actually killing a machine; real poweroff testing is a separate disruptive operation.

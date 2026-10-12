# Security model

## Purpose and enforcement boundary

Grim Reaper is a **manual-presence dead-man switch**, not an intrusion detector, remote attestation service, or cryptographic proof that its owner is alive. An authorized operator periodically sends a heartbeat to each armed machine. The default deadline is 36 hours, with 30 minutes of grace after boot. No recurring sender is installed.

Once an armed deadline expires, the daemon persists a shutdown latch and attempts abrupt shutdown. Late heartbeats are refused. A successful explicit administrative disarm is the only supported way to reset a latch. Disarm before expiry: an already-started shutdown operation cannot be undone.

Poweroff does not securely erase storage. Disk encryption, key handling, backups, secure boot, and physical security remain separate responsibilities. Sleep is not poweroff.

## Principals and trust boundaries

| Principal/component | Authority | Trust assumption |
|---|---|---|
| Operator's user account | Invoke manual sender, manage saved target list, request authorized receipts/status | Anyone controlling this account can prolong deadlines |
| SSH client, SSH agent, private keys | Authenticate remote connections; agent signs without exposing private key material to hosts | Client machine and agent socket are trusted while key is unlocked |
| SSH server and host keys | Authenticate the target and transport the command | Host-key verification is enabled; trusted SSH configuration resolves the intended host |
| Named heartbeat account | Exact passwordless sudo commands for receipt and JSON status | Authorized to keep this machine alive, but not to administer the switch without separate privilege |
| Root daemon and installed binary | Read/write state, invoke kernel shutdown, run fixed shutdown helpers | Root, kernel, binary, service manager, and installation chain are trusted |
| Administrator | Install, arm, disarm, change policy, stop service | Can defeat enforcement by design; not an adversary this software can contain |

The reserved `local` target invokes the installed root CLI through exact local sudo commands. Remote targets use SSH and the same receipt/status commands. There is no inbound network listener in the daemon.

## Authorization

Installation grants the selected non-root user only these passwordless commands:

```text
/usr/local/bin/grim-reaper heartbeat
/usr/local/bin/grim-reaper status --json
```

There are no argument wildcards. This does not authorize `arm`, `disarm`, `daemon`, `install`, arbitrary `--state-dir`, shell commands, or changes to configuration. Existing system administrator/sudo permissions are independent and may grant broader access after authentication.

The root CLI is `/usr/local/bin/grim-reaper`. State is `/var/lib/grim-reaper` on Linux and `/var/db/grim-reaper` on macOS. The state directory is root-owned mode 0700; state files are private root-owned regular files. Final-component symlinks and unsafe ownership/permissions are rejected. Privileged install directories are checked for root ownership and absence of group/world write permission. This is not a defense against root manipulating parent directories or mounts.

## Heartbeat integrity and deadlines

Receipts contain the **target's** wall timestamp, boot identity, and sleep-inclusive uptime. Client timestamps are not accepted. On the same boot, expiry uses uptime rather than wall time. After reboot, expiry uses persisted wall time because monotonic time cannot span boots. A future monotonic timestamp, malformed receipt, unreadable receipt, or missing receipt does not extend an armed deadline.

Boot grace is not a fresh heartbeat and does not reset the 36-hour lifetime. A valid persisted receipt may remain current across reboot. Missing/unusable configuration is treated as armed with default timings, never silently disarmed. Runtime failures that prevent a safe state decision trigger fail-closed enforcement.

State mutations are serialized with a bounded file lock. Files are replaced through exclusive same-directory temporary files, file fsync, atomic rename, and parent-directory fsync. The expiry transition rechecks state under the mutation lock so a receipt/disarm completed before latching is not ignored. Persistence failure does not stop enforcement.

`clock` reads status only. It does not create a receipt, arm, disarm, or reset expiry. An unreachable host is reported as an error, not as healthy or proven powered off.

## Shutdown policy

Linux's first attempt submits the exact requested sequence as one write of `_reiuso` to `/proc/sysrq-trigger`: keyboard reset, terminate userspace, kill userspace, read-only remount, sync, poweroff. The underscore enables multi-command processing. This is **not** a userspace loop of separate writes: `i` could kill that loop before `o`.

The daemon persists sequence progress in the shutdown latch before invoking REIUSO, because the sequence can kill the daemon itself. If the machine survives and systemd restarts it, it continues with direct kernel poweroff and forced userspace fallbacks rather than indefinitely repeating only the self-killing first step. If progress cannot safely be persisted/read, enforcement uses the remaining poweroff methods rather than relying on a sequence that could prevent fallback.

Sequence ordering does not prove that asynchronous filesystem sync/remount completed before poweroff. Buffered writes may still be lost. Do not mistake REIUSO for graceful application shutdown.

macOS has no SysRq equivalent. It uses quick kernel halt/poweroff and native halt helpers, followed by no-sync hard fallbacks and the platform shutdown helper. No panic or reboot fallback is used. Platform service managers supervise the daemon: systemd on Linux and a root launchd LaunchDaemon on macOS.

Shutdown methods use isolated workers with bounded waits. A hung worker cannot block later methods; unreaped workers remain tracked so the daemon does not create unlimited copies of a stuck method. Remaining methods repeat while the machine survives. Kernel halt may stop execution without removing electrical power on some hardware.

## Key handling and manual workflow

`~/bin/grim-touch` checks whether the key selected by `GRIM_SSH_KEY` (default `~/.ssh/id_ed25519`) is already usable through the current SSH agent. If necessary, it asks once to unlock that key before invoking the manual sender. An inherited agent is reused; if no agent is available, the script owns a temporary agent for that invocation and cleans it up on normal exit. No passphrase is saved in repository files, environment variables, or command arguments.

Unlocking a key does not grant a server authorization it does not already have. The sender uses each configured SSH user and its restricted sudo rules; direct root SSH is not required. An administrative/root-capable key is more powerful than a dedicated heartbeat-only identity. Prefer a dedicated restricted sender identity if its compromise must not grant other administrative access.

## Explicit limits

- No userspace program can guarantee poweroff with a wedged kernel, failed firmware/hardware, or a privileged attacker controlling the service.
- No shutdown work executes while a Mac is asleep. Sleep counts toward expiry; enforcement resumes on wake. An awake trusted sender is required to heartbeat sleeping/offline remote targets successfully.
- After-reboot age depends on trustworthy wall time. NTP errors and privileged clock changes can affect that comparison.
- Unavailable networking, locked keys, or operator forgetfulness can cause intentional shutdown and data loss. A partial fleet delivery does not extend failed hosts' timers.
- Durable state depends on filesystem and device behavior. Fsync improves crash consistency but is not a guarantee against failing hardware or storage that lies about persistence.
- Hardware watchdogs and independently controlled power systems are needed for enforcement beyond the running OS. They are not installed or configured by this project.

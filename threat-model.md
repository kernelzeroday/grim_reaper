# Threat model

## Assets and desired properties

1. An armed machine must stop executing after its last valid heartbeat expires, subject to boot grace and the running-kernel boundary.
2. An unprivileged unauthorized account must not forge receipts, alter policy, clear a latch, or replace privileged code.
3. A sender must not treat an unreachable host or failed remote command as a successful receipt.
4. Status/clock inspection must not extend life.
5. Once expiry is latched, a delayed receipt must not reopen the deadline.
6. Installation and upgrades must not accidentally arm, silently disarm an existing installation, or start an uninitialized live daemon.

Availability of unsaved data is intentionally subordinate to enforcing shutdown. The machine is expected to become unavailable when the operator stops sending manual heartbeats.

## Adversaries

- Unauthenticated network attackers capable of scanning, interfering with traffic, or pretending to be a target.
- Local unprivileged accounts without receipt authorization.
- An attacker controlling an authorized heartbeat account, its SSH credential, the sender machine, or an unlocked agent socket.
- A privileged/root attacker or attacker controlling the kernel/firmware.
- Non-adversarial failures: clock jumps, reboot, suspend, disk corruption/full disk, hung helpers, stopped/crashed processes, network outages, and operator mistakes.

## Threats, controls, and residual risk

| Threat | Implemented control | Residual risk / required response |
|---|---|---|
| Network impersonation or interception | SSH authentication, normal host-key verification, no daemon network listener | Compromised host keys, trusted SSH configuration, or the SSH endpoint defeat transport trust |
| Remote command/option injection | Validated host/account syntax; fixed remote command; argv execution rather than shell interpolation of user data | SSH configuration and SSH implementation are trusted; privileged administrators can change them |
| Unprivileged local state modification | Root-private state, ownership/permission checks, final symlink rejection, root-owned installed binary | Root and insecure parent mounts/directories remain outside containment |
| Broad passwordless privilege escalation | Exact sudoers commands for heartbeat and JSON status, validated with visudo | Other existing sudo rules are independent; the authorized user may already be an administrator |
| Forged client time or delayed receipt | Target records receipt time itself; same-boot expiry uses uptime | Any authorized sender can submit a new genuine receipt without proof of human presence |
| Clock rollback/forward or suspend extending same-boot life | Sleep-inclusive monotonic uptime; immutable boot identity; reject future uptime | No execution during suspend; enforcement on wake. Across boots, wall time remains necessary |
| Malformed/missing receipt disables switch | Armed evaluation expires after allowed grace rather than silently stopping monitoring | Corruption can deliberately cause denial of service and data loss |
| Missing/corrupt config silently disarms | Armed default policy and diagnostic; unsafe runtime state fails closed | Default timings replace unavailable custom policy; filesystem failure may prevent administrative recovery |
| Daemon exits after shutdown error | Repeated isolated workers; bounded deadlines; service-manager restart | Uninterruptible kernel tasks and kernel-wide failure cannot be fixed by userspace |
| REIUSO kills daemon before fallback | One kernel multi-command write plus durable progress before sequence; restarted daemon continues other methods | Kernel support and implementation are trusted; sequence sync/remount is not a graceful-shutdown guarantee |
| Deadlock or hung privileged helper | Bounded lock acquisition and worker waits; track unreaped method slots | Filesystem/kernel operations and scheduler can still fail globally |
| Late heartbeat races expiry | Recheck while holding mutation lock; durable/in-memory latch; latched receipt refused | A request merely sent before expiry is not necessarily accepted before expiry |
| Malicious/huge status output or stalled SSH command | Bounded command runtime/output; error rows and unsuccessful aggregate exit | Caller cannot prove physical shutdown from network failure; fleet reads are snapshots, not simultaneous attestation |
| Fleet partial failure hidden from operator | Attempt all selected hosts, display results, return nonzero for any failure | Operator must read errors and recover access before remaining deadlines expire |
| Authorized account/key compromised | Least-privilege receipt/status delegation limits switch administration | **Receipt authority is enough to keep the machine alive indefinitely.** This is not prevented by the dead-man switch |
| Unlocked SSH-agent socket stolen | No passphrase storage; temporary agent cleanup when script creates one | An attacker with access to an inherited unlocked agent can request signatures; agent forwarding increases exposure |
| Root/kernel/firmware attacker disables enforcement | No claimed software containment | Requires separate hardware/power-controller trust; root can stop the service, disarm, change state, or alter clocks |
| Operator forgets heartbeat or selects wrong hosts | Saved explicit targets, readable timers, no fake success | Manual operation is intentional; no automatic sender rescues a missed deadline |
| Abrupt shutdown corrupts data | Quick/cache-flushing attempts where supported, then hard fallbacks | Data loss remains accepted policy; keep backups and disarm before maintenance |

## Abuse scenarios

### Compromised sender keeps all machines alive

A compromised sender can invoke the legitimate heartbeat command repeatedly. SSH and sudo authorization work exactly as intended, but operator-presence assumptions are false. A root-capable key makes the compromise broader still. Mitigation is separating heartbeat credentials from administrative credentials and securing the sender; this project does not implement an independent human-presence attestation mechanism.

### Network outage falsely appears to be shutdown

The operator cannot reach a target. It may be shut down, asleep, partitioned, or still running with a broken daemon. `clock` reports an error and must not claim shutdown. Observe physical power or independent trusted telemetry if that distinction matters. A failed `grim-touch` does not reset that target's deadline.

### Invalid state produces a surprise poweroff

An armed receipt is removed, damaged, or made unreadable. Fail-closed policy intentionally starts shutdown after applicable grace. Deleting files is not a maintenance procedure. Disarm through the root CLI before changing installation, storage, or policy.

### Kernel task cannot be killed

A shutdown worker is stuck in uninterruptible sleep. Its deadline expires, the supervisor sends SIGKILL without blocking on reap, retains the worker's slot, and attempts the remaining methods. If the kernel or firmware is globally stuck, all userspace control is ineffective. A hardware watchdog is needed for that threat.

## Verification boundary

Unit/state tests and non-destructive smoke checks can verify decision logic, read-only countdown behavior, partial-failure reporting, latch transitions, bounded fallback scheduling, and installation/service state. Interposed/blocked destructive calls verify supervisor behavior, not actual firmware poweroff. A full real-poweroff test requires deliberately interrupting the target; it is not silently performed during development or installation.

## Operational decisions

- Heartbeats remain manual; no cron, launchd heartbeat agent, or systemd heartbeat timer is installed.
- Default 36-hour expiry and 30-minute boot grace remain in force on all three targets.
- Local macOS is an explicit saved `local` target, not an implicit heartbeat side effect of status commands.
- Service updates require explicit disarming, disarmed verification, fresh receipts, and explicit rearming.
- REIUSO is the requested first Linux attempt, not a claim of secure erasure or graceful shutdown.
- Treat every authorized receipt sender as trusted to prolong life. Treat every administrator as trusted to disable the switch.

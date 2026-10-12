#!/usr/bin/env bash
# Unlock the sender's key once, then deliver one heartbeat to each selected host.
set -euo pipefail

key="${GRIM_SSH_KEY:-$HOME/.ssh/id_ed25519}"
cli="$HOME/.local/bin/grim-reaper"
if [[ ! -x "$cli" ]]; then
    cli=$(command -v grim-reaper) || {
        printf 'error: install grim-reaper with install --local first\n' >&2
        exit 1
    }
fi
if [[ ! -r "$key" || ! -r "$key.pub" ]]; then
    printf 'error: expected readable SSH key and public key at %s and %s.pub\n' "$key" "$key" >&2
    exit 1
fi

agent_status=0
ssh-add -l >/dev/null 2>&1 || agent_status=$?
if [[ "$agent_status" -eq 2 ]]; then
    # Without an inherited agent, own a temporary one for this run only.
    agent_environment=$(ssh-agent -s)
    eval "$agent_environment" >/dev/null
    trap 'ssh-agent -k >/dev/null 2>&1 || true' EXIT
elif [[ "$agent_status" -ne 0 && "$agent_status" -ne 1 ]]; then
    printf 'error: cannot inspect SSH agent (ssh-add exited %s)\n' "$agent_status" >&2
    exit 1
fi

# -T verifies that this specific key is unlocked, not merely some other identity.
if ! ssh-add -T "$key.pub" >/dev/null 2>&1; then
    ssh-add "$key"
fi

# Uses saved targets when no hosts are supplied. Preserve aggregate failure status.
"$cli" touch "$@"

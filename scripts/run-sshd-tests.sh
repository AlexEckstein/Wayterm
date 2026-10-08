#!/usr/bin/env bash
#
# Run tests/transport_sshd.rs against a real, throwaway sshd on this machine.
#
# The integration suite skips itself unless SSH_TEST_* is set, which means it
# normally only ever exercises the skip path locally. This script provisions the
# same environment CI provisions in .github/workflows/ci.yml ("Provision test
# sshd") — but on a dedicated port with a dedicated sshd instance, so the system
# sshd (port 22) and /etc/ssh/sshd_config are never modified or restarted.
#
# Usage:
#   scripts/run-sshd-tests.sh              # run the whole suite
#   scripts/run-sshd-tests.sh key_auth_exec_echo   # filter by substring
#
# Requires: sudo (passwordless on CI-like dev machines), openssh-server,
#           openssl, useradd/chpasswd (passwd package), cargo.
#
# Secrets: the test password exists only in this script's process environment.
# It is never written to disk, never echoed, never logged.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG_TEMPLATE="$REPO_ROOT/scripts/sshd-test-config.inc"
SSHD_BIN="${SSHD_BIN:-/usr/sbin/sshd}"
SSHD_USER="wayterm-test"
SSHD_PORT="${WAYTERM_SSHD_PORT:-2422}"
TEST_FILTER="${1:-}"

RUN_DIR=""
SSHD_PID=""

log()  { printf '[sshd-tests] %s\n' "$*"; }
fail() { printf '[sshd-tests] ERROR: %s\n' "$*" >&2; exit 1; }

cleanup() {
    local status=$?
    if [ -n "$SSHD_PID" ] && kill -0 "$SSHD_PID" 2>/dev/null; then
        log "stopping test sshd (pid $SSHD_PID, port $SSHD_PORT)"
        kill "$SSHD_PID" 2>/dev/null || true
        wait "$SSHD_PID" 2>/dev/null || true
    fi
    if [ -n "$RUN_DIR" ] && [ -d "$RUN_DIR" ]; then
        # The rendered sshd_config is root-owned by design (sshd refuses to run
        # with a non-root-owned config), so teardown needs sudo.
        log "removing temp dir $RUN_DIR (private key material lives here)"
        sudo rm -rf "$RUN_DIR" 2>/dev/null || rm -rf "$RUN_DIR" 2>/dev/null || \
            log "WARNING: could not remove $RUN_DIR — delete it manually"
    fi
    return $status
}
trap cleanup EXIT

# ── Preconditions ───────────────────────────────────────────────────────────
[ -x "$SSHD_BIN" ] || fail "sshd not found at $SSHD_BIN (install openssh-server)"
[ -f "$CONFIG_TEMPLATE" ] || fail "missing config template $CONFIG_TEMPLATE"
[ -f "$REPO_ROOT/tests/transport_sshd.rs" ] || fail "tests/transport_sshd.rs not found"
command -v openssl >/dev/null || fail "openssl not found"
command -v ssh-keygen >/dev/null || fail "ssh-keygen not found"
command -v cargo >/dev/null || fail "cargo not found"
sudo -n true 2>/dev/null || fail "passwordless sudo required (test sshd binds a port and creates a user)"

# ── Host keys ───────────────────────────────────────────────────────────────
# The test instance reuses the system host keys read-only; never regenerates or
# overwrites them (that would break the user's own sshd trust).
if [ ! -f /etc/ssh/ssh_host_ed25519_key ]; then
    log "generating missing system host keys"
    sudo "$SSHD_BIN" -A 2>/dev/null || sudo ssh-keygen -A
fi

# ── Test account ────────────────────────────────────────────────────────────
if id "$SSHD_USER" >/dev/null 2>&1; then
    log "reusing existing user $SSHD_USER"
else
    log "creating throwaway user $SSHD_USER (no sudo, random password)"
    sudo useradd -m -s /bin/bash "$SSHD_USER"
fi

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/wayterm-sshd-test.XXXXXX")"
chmod 700 "$RUN_DIR"
log "temp dir: $RUN_DIR"

# Password lives only in this shell's environment and in the child's env below.
TEST_PASSWORD="$(openssl rand -hex 16)"
printf '%s:%s\n' "$SSHD_USER" "$TEST_PASSWORD" | sudo chpasswd
log "set random password for $SSHD_USER"

# ── Key material ────────────────────────────────────────────────────────────
KEY_PATH="$RUN_DIR/wayterm_test_ed25519"
ssh-keygen -t ed25519 -N "" -C "wayterm-local-sshd-tests" -f "$KEY_PATH" >/dev/null
chmod 600 "$KEY_PATH"

TEST_HOME="$(getent passwd "$SSHD_USER" | cut -d: -f6)"
sudo install -d -m 700 -o "$SSHD_USER" -g "$SSHD_USER" "$TEST_HOME/.ssh"
sudo install -m 600 -o "$SSHD_USER" -g "$SSHD_USER" "$KEY_PATH.pub" "$TEST_HOME/.ssh/authorized_keys"
log "installed test public key into $TEST_HOME/.ssh/authorized_keys"

# ── Dedicated sshd instance ─────────────────────────────────────────────────
SSHD_RUN="$RUN_DIR/sshd"
sudo install -d -m 755 -o root -g root "$SSHD_RUN"
SSHD_CONFIG="$SSHD_RUN/sshd_config"

# The template is rendered by root into a root-owned, non-world-writable
# directory: sshd refuses to start if its config or host keys are writable by
# anyone but root.
sudo env \
    WAYTERM_SSHD_PORT="$SSHD_PORT" \
    WAYTERM_SSHD_RUN="$SSHD_RUN" \
    WAYTERM_SSHD_USER="$SSHD_USER" \
    /bin/sh -c 'umask 077; sed -e "s|\$WAYTERM_SSHD_PORT|$WAYTERM_SSHD_PORT|g" \
        -e "s|\$WAYTERM_SSHD_RUN|$WAYTERM_SSHD_RUN|g" \
        -e "s|\$WAYTERM_SSHD_USER|$WAYTERM_SSHD_USER|g" "$0" > "$1"' \
    "$CONFIG_TEMPLATE" "$SSHD_CONFIG"
sudo chmod 600 "$SSHD_CONFIG"

SSHD_LOG="$RUN_DIR/sshd.log"
log "validating test sshd config"
sudo "$SSHD_BIN" -t -f "$SSHD_CONFIG" || fail "generated sshd_config is invalid"

log "starting test sshd on 127.0.0.1:$SSHD_PORT (system sshd on 22 untouched)"
sudo "$SSHD_BIN" -D -f "$SSHD_CONFIG" -E "$SSHD_LOG" &
SSHD_PID=$!

# Wait for the listener rather than sleeping blindly. A TCP probe is the
# authority; `ss` is only a nicety and may be absent.
listening() {
    if command -v ss >/dev/null 2>&1 && ss -ltn 2>/dev/null | grep -q ":$SSHD_PORT[[:space:]]"; then
        return 0
    fi
    (exec 3<>"/dev/tcp/127.0.0.1/$SSHD_PORT") 2>/dev/null
}
for _ in $(seq 1 60); do
    if listening; then
        break
    fi
    if ! kill -0 "$SSHD_PID" 2>/dev/null; then
        log "--- test sshd log ---"
        sudo cat "$SSHD_LOG" 2>/dev/null || true
        fail "test sshd exited during startup"
    fi
    sleep 0.2
done
listening || fail "test sshd never accepted a connection on port $SSHD_PORT"
log "test sshd is listening on 127.0.0.1:$SSHD_PORT"

# ── Run the suite ───────────────────────────────────────────────────────────
export SSH_TEST_HOST=127.0.0.1
export SSH_TEST_PORT="$SSHD_PORT"
export SSH_TEST_USER="$SSHD_USER"
export SSH_TEST_PASSWORD="$TEST_PASSWORD"
export SSH_TEST_KEY_PATH="$KEY_PATH"
# The agent is deliberately NOT offered: the suite's auth-failure test asserts
# on the untouched-methods list, and a stray agent would change that.
unset SSH_AUTH_SOCK

TEST_CMD=(cargo test --test transport_sshd --test tmux_persistence --test connect_flow -- --test-threads=1 --nocapture)
if [ -n "$TEST_FILTER" ]; then
    TEST_CMD+=("$TEST_FILTER")
fi

log "running: ${TEST_CMD[*]}"
CARGO_LOG="$RUN_DIR/cargo-test.log"
set +e
(cd "$REPO_ROOT" && "${TEST_CMD[@]}" 2>&1 | tee "$CARGO_LOG")
CARGO_STATUS="${PIPESTATUS[0]}"
set -e

unset SSH_TEST_PASSWORD TEST_PASSWORD

# ── Per-test summary ────────────────────────────────────────────────────────
# Parsed from the harness lines ("test <name> ... ok|FAILED") so the summary
# reflects what actually ran, not what we expect.
echo
echo "──────────────────── per-test summary ────────────────────"
while read -r _ name _ result; do
    case "$result" in
        ok)      echo "PASS  $name" ;;
        FAILED)  echo "FAIL  $name" ;;
        ignored) echo "SKIP  $name" ;;
        *)       echo "----  $name  ($result)" ;;
    esac
done < <(grep -E '^test [a-z_]+ \.\.\. (ok|FAILED|ignored)$' "$CARGO_LOG" || true)

if ! grep -qE '^test [a-z_]+ \.\.\. ok$' "$CARGO_LOG"; then
    echo "WARNING: no passing test result lines captured — the suite may not have run"
fi
if grep -q 'Skipping sshd integration test' "$CARGO_LOG"; then
    echo "WARNING: SSH_TEST_* reached the harness but a test still took the skip path"
fi
echo "cargo exit status: $CARGO_STATUS"
echo "──────────────────────────────────────────────────────────"

if [ "$CARGO_STATUS" -ne 0 ]; then
    log "suite FAILED; last 40 lines of test sshd log:"
    sudo tail -n 40 "$SSHD_LOG" 2>/dev/null || true
fi

log "left behind: user $SSHD_USER with home $TEST_HOME and a public key in .ssh/authorized_keys."
log "no secrets survive the run — the password is unset here and the private key lived only in $RUN_DIR (removed)."
log "to remove the account entirely: sudo userdel -r $SSHD_USER"

exit "$CARGO_STATUS"
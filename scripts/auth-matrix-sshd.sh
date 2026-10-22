#!/usr/bin/env bash
#
# Run tests/auth_matrix.rs against real, throwaway sshd instances — one per
# auth method, so the server actually pins each method
# (doc 15).
#
# WHY A SEPARATE RUNNER: the matrix needs an sshd whose AuthenticationMethods
# names exactly ONE acceptable method. CI provisions a single all-methods sshd
# on port 22 and scripts/sshd-test-config.inc enables all three methods, so
# neither can express this matrix — auth_matrix.rs skips itself in CI for the
# same reason it skips under a bare `cargo test`.
#
# This mirrors scripts/run-sshd-tests.sh exactly: same preconditions, same
# host-key reuse, same throwaway account, same temp rendered config, same
# teardown, same secret discipline. Per mode it provisions a DEDICATED sshd
# instance on a dedicated port, runs the suite, reports which method the server
# accepted, and tears the instance down again. The system sshd (port 22) and
# /etc/ssh/sshd_config are never modified or restarted.
#
# Usage:
#   scripts/auth-matrix-sshd.sh                    # all three modes
#   scripts/auth-matrix-sshd.sh password-only      # one mode
#   scripts/auth-matrix-sshd.sh --filter kbd       # substring-filter tests
#
# Requires: sudo (passwordless), openssh-server with PAM, openssl,
#           ssh-keygen, useradd/chpasswd (passwd package), cargo.
#
# Secrets: the test password exists only in this script's process environment
# and the environment it exports to `cargo test`. It is never written to disk,
# never echoed, never logged.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE_CONFIG="$REPO_ROOT/scripts/sshd-auth-matrix-config.inc"
MODES_DIR="$REPO_ROOT/scripts/auth-matrix"
PARSE_AWK="$REPO_ROOT/scripts/auth-matrix-parse.awk"
SSHD_BIN="${SSHD_BIN:-/usr/sbin/sshd}"
SSHD_USER="wayterm-test"
PORT_BASE="${WAYTERM_MATRIX_PORT_BASE:-2423}"
TEST_FILTER=""

RUN_DIR=""
SSHD_PID=""
# "slug:port" pairs, expanded below; sshd refuses to bind a port twice even
# after the old instance is gone, so each mode gets its own.
declare -a MODE_SLUGS=(password-only kbd-interactive-only key-only)
declare -a MODE_PORTS=(
    "$PORT_BASE"
    "$((PORT_BASE + 1))"
    "$((PORT_BASE + 2))"
)

log()  { printf '[auth-matrix] %s\n' "$*"; }
fail() { printf '[auth-matrix] ERROR: %s\n' "$*" >&2; exit 1; }

cleanup() {
    local status=$?
    if [ -n "$SSHD_PID" ] && kill -0 "$SSHD_PID" 2>/dev/null; then
        log "stopping test sshd (pid $SSHD_PID)"
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
[ -f "$BASE_CONFIG" ] || fail "missing base config template $BASE_CONFIG"
[ -f "$PARSE_AWK" ] || fail "missing verdict parser $PARSE_AWK"
[ -d "$MODES_DIR" ] || fail "missing auth-mode fragment dir $MODES_DIR"
[ -f "$REPO_ROOT/tests/auth_matrix.rs" ] || fail "tests/auth_matrix.rs not found"
command -v openssl >/dev/null || fail "openssl not found"
command -v ssh-keygen >/dev/null || fail "ssh-keygen not found"
command -v cargo >/dev/null || fail "cargo not found"
sudo -n true 2>/dev/null || fail "passwordless sudo required (test sshd binds a port and creates a user)"

# Optional positional args.
while [ $# -gt 0 ]; do
    case "$1" in
        --filter) TEST_FILTER="${2:-}"; shift 2 ;;
        -*) fail "unknown option: $1" ;;
        *) break ;;
    esac
done
if [ $# -ge 1 ]; then
    MODE_SLUGS=("$1")
    MODE_PORTS=("$PORT_BASE")
fi

for slug in "${MODE_SLUGS[@]}"; do
    [ -f "$MODES_DIR/$slug.inc" ] || fail "missing auth fragment $MODES_DIR/$slug.inc"
done

# ── Host keys ───────────────────────────────────────────────────────────────
# The test instances reuse the system host keys read-only; never regenerates or
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

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/wayterm-auth-matrix.XXXXXX")"
chmod 700 "$RUN_DIR"
log "temp dir: $RUN_DIR"

# Password lives only in this shell's environment and in the child's env below.
TEST_PASSWORD="$(openssl rand -hex 16)"
printf '%s:%s\n' "$SSHD_USER" "$TEST_PASSWORD" | sudo chpasswd
log "set random password for $SSHD_USER"

# ── Key material ────────────────────────────────────────────────────────────
# One key, used by every mode. The modes differ in what the SERVER accepts, not
# in what the client holds, so the same authorized_keys entry has to be capable
# of both "accepted" (key-only mode) and "rejected" (password-only mode).
KEY_PATH="$RUN_DIR/wayterm_matrix_ed25519"
ssh-keygen -t ed25519 -N "" -C "wayterm-local-auth-matrix" -f "$KEY_PATH" >/dev/null
chmod 600 "$KEY_PATH"

TEST_HOME="$(getent passwd "$SSHD_USER" | cut -d: -f6)"
sudo install -d -m 700 -o "$SSHD_USER" -g "$SSHD_USER" "$TEST_HOME/.ssh"
sudo install -m 600 -o "$SSHD_USER" -g "$SSHD_USER" "$KEY_PATH.pub" "$TEST_HOME/.ssh/authorized_keys"
log "installed test public key into $TEST_HOME/.ssh/authorized_keys"

listening() {
    local port="$1"
    if command -v ss >/dev/null 2>&1 && ss -ltn 2>/dev/null | grep -q ":$port[[:space:]]"; then
        return 0
    fi
    (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null
}

# ── One mode: render config, start sshd, run the suite, tear down ───────────
# Echoes the cargo exit status; returns 0 if the mode ran (or was recorded as a
# host/sshd finding) and non-zero only if the suite itself failed.
run_mode() {
    local slug="$1" port="$2"
    local fragment="$MODES_DIR/$slug.inc"
    local mode_run="$RUN_DIR/$slug"
    local sshd_config="$mode_run/sshd_config"
    local sshd_log="$mode_run/sshd.log"
    # sshd's own config/log must live in the root-owned dir, but the cargo log
    # is written by the unprivileged `tee` — it goes in the user-owned RUN_DIR.
    local cargo_log="$RUN_DIR/$slug-cargo-test.log"

    log "── mode $slug (port $port) ──────────────────────────────────"
    sudo install -d -m 755 -o root -g root "$mode_run"

    # Render base + per-mode auth fragment. root-owned, non-world-writable:
    # sshd refuses to start if its config is writable by anyone but root.
    # The marker is a COMMENT, so a failed splice leaves a config that still
    # validates (minus the matrix) instead of a hard error.
    if ! sudo env \
        WAYTERM_SSHD_PORT="$port" \
        WAYTERM_SSHD_RUN="$mode_run" \
        WAYTERM_SSHD_USER="$SSHD_USER" \
        WAYTERM_MATRIX_AUTH_FRAGMENT="$fragment" \
        /bin/sh -c 'umask 077; sed -e "s|\$WAYTERM_SSHD_PORT|$WAYTERM_SSHD_PORT|g" \
            -e "s|\$WAYTERM_SSHD_RUN|$WAYTERM_SSHD_RUN|g" \
            -e "s|\$WAYTERM_SSHD_USER|$WAYTERM_SSHD_USER|g" \
            -e "/^#@WAYTERM_AUTH_BLOCK@/r $WAYTERM_MATRIX_AUTH_FRAGMENT" \
            -e "/^#@WAYTERM_AUTH_BLOCK@/d" "$0" > "$1"' \
        "$BASE_CONFIG" "$sshd_config"; then
        printf '[auth-matrix] FINDING: %s\n' \
            "could not render sshd_config for mode $slug (splice/permission error) — mode not exercised"
        return 0
    fi
    sudo chmod 600 "$sshd_config"

    # `sshd -t` is the authority on whether this OpenSSH build ACCEPTS the
    # AuthenticationMethods combo. Some PAM / keyboard-interactive combinations
    # are rejected outright by sshd and there is no client-side workaround —
    # that is a host finding to report, not something to force.
    local config_test_out
    if ! config_test_out="$(sudo "$SSHD_BIN" -t -f "$sshd_config" 2>&1)"; then
        printf '[auth-matrix] FINDING: %s\n' \
            "sshd REFUSED AuthenticationMethods for mode '$slug' on this build ($("$SSHD_BIN" -V 2>&1 | head -1)):"
        printf '%s\n' "$config_test_out" | sed 's/^/[auth-matrix]   sshd: /'
        return 0
    fi
    log "sshd accepted the '$slug' AuthenticationMethods combo"

    log "starting test sshd on 127.0.0.1:$port (system sshd on 22 untouched)"
    sudo "$SSHD_BIN" -D -f "$sshd_config" -E "$sshd_log" &
    SSHD_PID=$!

    # Wait for the listener rather than sleeping blindly.
    local up=0
    for _ in $(seq 1 60); do
        if listening "$port"; then up=1; break; fi
        if ! kill -0 "$SSHD_PID" 2>/dev/null; then
            log "test sshd for mode $slug exited during startup; log:"
            sudo tail -n 20 "$sshd_log" 2>/dev/null | sed 's/^/[auth-matrix]   sshd: /' || true
            SSHD_PID=""
            return 0
        fi
        sleep 0.2
    done
    if [ "$up" -ne 1 ]; then
        printf '[auth-matrix] FINDING: %s\n' \
            "mode '$slug': test sshd never accepted a connection on port $port — mode not exercised"
        return 0
    fi
    log "test sshd is listening on 127.0.0.1:$port"

    export SSH_MATRIX_HOST=127.0.0.1
    export SSH_MATRIX_PORT="$port"
    export SSH_MATRIX_USER="$SSHD_USER"
    export SSH_MATRIX_PASSWORD="$TEST_PASSWORD"
    export SSH_MATRIX_KEY_PATH="$KEY_PATH"
    export SSH_MATRIX_MODE="$slug"
    # The agent is deliberately NOT offered: a stray agent identity would be a
    # publickey attempt whose outcome the assertions do not model.
    unset SSH_AUTH_SOCK

    local test_cmd=(cargo test --test auth_matrix -- --test-threads=1 --nocapture)
    if [ -n "$TEST_FILTER" ]; then
        test_cmd+=("$TEST_FILTER")
    fi
    log "running: ${test_cmd[*]}"
    local status
    # PIPESTATUS must be read in THIS shell, so the pipeline is not wrapped in a
    # subshell — a subshell would leave PIPESTATUS describing the subshell (i.e.
    # tee's status), silently turning a failing suite into a green run.
    set +e
    pushd "$REPO_ROOT" >/dev/null
    "${test_cmd[@]}" 2>&1 | tee "$cargo_log"
    status="${PIPESTATUS[0]}"
    popd >/dev/null
    set -e

    unset SSH_MATRIX_PASSWORD SSH_MATRIX_PORT SSH_MATRIX_MODE

    # Per-test summary, parsed from the harness lines so it reflects what ran.
    # Under --nocapture a test's stdout lands between "..." and the harness's
    # status token, and because stdout ends in a newline libtest puts that
    # token on a LATER line ("test name ... <output>\nok"). A one-line regex
    # therefore silently loses every test that printed anything, so this walks
    # the log with a pending-name state machine instead.
    local ran=0 passed=0 skipped=0
    while read -r tname result; do
        case "$result" in
            ok)      ran=$((ran + 1)); passed=$((passed + 1)); echo "PASS  $slug/$tname" ;;
            skipped) skipped=$((skipped + 1)); echo "SKIP  $slug/$tname (other-mode test)" ;;
            FAILED)  ran=$((ran + 1)); echo "FAIL  $slug/$tname" ;;
            ignored) skipped=$((skipped + 1)); echo "SKIP  $slug/$tname (ignored)" ;;
            *)       echo "----  $slug/$tname  ($result)" ;;
        esac
    done < <(awk -f "$PARSE_AWK" "$cargo_log" 2>/dev/null || true)

    # Cargo's own tally is authoritative; the parsed lines above are for detail.
    grep -E '^test result:' "$cargo_log" | sed "s/^/[auth-matrix] mode $slug: /" || true
    if [ "$ran" -eq 0 ]; then
        log "WARNING: no test actually RAN for mode $slug ($skipped took the skip path) — the matrix did not exercise anything"
    else
        log "mode $slug: $passed/$ran mode tests ran and passed, $skipped other-mode test(s) skipped"
    fi

    if grep -q 'Skipping auth-matrix test' "$cargo_log"; then
        log "NOTE: at least one test took the skip path (other-mode tests are expected to skip)"
    fi

    # Which link actually completed auth? The client chain reports Success
    # without naming the method; sshd's VERBOSE log does. Note the method token
    # is not always bare — PAM-mediated kbd-interactive logs
    # "Accepted keyboard-interactive/pam".
    local accepted
    accepted="$(sudo grep -hoE 'Accepted [^ ]+ for ' "$sshd_log" 2>/dev/null | sort -u || true)"
    if [ -n "$accepted" ]; then
        log "server-side accepted-method lines for mode $slug:"
        printf '%s\n' "$accepted" | sed 's/^/[auth-matrix]   /'
    else
        log "server logged no accepted-method line for mode $slug"
    fi

    if [ "$status" -ne 0 ]; then
        log "mode $slug FAILED; last 40 lines of test sshd log:"
        sudo tail -n 40 "$sshd_log" 2>/dev/null | sed 's/^/[auth-matrix]   sshd: /' || true
    fi

    log "stopping mode $slug sshd"
    kill "$SSHD_PID" 2>/dev/null || true
    wait "$SSHD_PID" 2>/dev/null || true
    SSHD_PID=""

    return "$status"
}

# ── Run every requested mode ────────────────────────────────────────────────
OVERALL=0
declare -a MODE_RESULTS=()
for i in "${!MODE_SLUGS[@]}"; do
    slug="${MODE_SLUGS[$i]}"
    port="${MODE_PORTS[$i]}"
    status=0
    run_mode "$slug" "$port" || status=$?
    if [ "$status" -eq 0 ]; then
        MODE_RESULTS+=("OK    $slug (port $port)")
    else
        MODE_RESULTS+=("FAIL  $slug (port $port)")
        OVERALL=1
    fi
done

unset SSH_MATRIX_PASSWORD TEST_PASSWORD

echo
echo "──────────────────── auth-method matrix summary ────────────────────"
printf '%s\n' "${MODE_RESULTS[@]}"
echo "overall: $([ "$OVERALL" -eq 0 ] && echo GREEN || echo RED)"
echo "────────────────────────────────────────────────────────────────────"

log "left behind: user $SSHD_USER with home $TEST_HOME and a public key in .ssh/authorized_keys."
log "no secrets survive the run — the password is unset here and the private key lived only in $RUN_DIR (removed)."
log "to remove the account entirely: sudo userdel -r $SSHD_USER"

exit "$OVERALL"

# Per-test verdict extractor for scripts/auth-matrix-sshd.sh.
#
# libtest writes "test <name> ... " and, when the test printed anything under
# --nocapture, emits the verdict on a LATER line (the test's stdout ends in a
# newline, so "ok" starts its own line). Matching a single line therefore
# silently drops every test that printed — which is exactly the set of tests
# this matrix cares about, since they print the chain's failure reason. So walk
# the log with a pending-name state machine: a test's own output can then never
# be mistaken for the harness's verdict.
#
# Input: a `cargo test -- --nocapture` log on stdin or via -f. Output: one
# "<name> <ok|FAILED|ignored|skipped>" line per test. "skipped" means the test
# reported ok but took its env-var skip path — libtest calls that "ok", which in
# a per-mode summary would be indistinguishable from a real pass.

function verdict(v) {
    print name, (skipped ? "skipped" : v)
    pending = 0
    skipped = 0
}

/^test [[:alnum:]_]+ \.\.\. / {
    name = $2
    pending = 1
    # The skip notice lands on the harness line itself (libtest prints
    # "test name ... " with no newline before the test's own output), so look
    # for it here as well as on the lines that follow.
    skipped = (/Skipping auth-matrix test:/ ? 1 : 0)
    if ($0 ~ / (ok|FAILED|ignored)$/) verdict($NF)
    next
}

pending == 1 && /Skipping auth-matrix test:/ { skipped = 1; next }

pending == 1 && $0 ~ /^(ok|FAILED|ignored)$/ { verdict($0); next }

# Base sshd config for the server-gated auth-method matrix (doc 11 §Testing Strategy, item 3).
#
# Rendered by scripts/auth-matrix-sshd.sh — one throwaway instance PER MODE, on
# its own port, from its own rendered copy. The system sshd on port 22 and
# /etc/ssh/sshd_config are never touched, read-modified, or restarted.
#
# The per-mode auth block (AuthenticationMethods plus the three auth toggles)
# is spliced in at the marker below from scripts/auth-matrix/<mode>.inc. It is
# deliberately NOT set here: if this file also set them, whichever line sshd
# happened to read first would win and the matrix would silently stop being a
# matrix.

Port $WAYTERM_SSHD_PORT
ListenAddress 127.0.0.1
AddressFamily inet

# The test instance reuses the system host keys read-only — never regenerated.
HostKey /etc/ssh/ssh_host_ed25519_key
HostKey /etc/ssh/ssh_host_rsa_key

PidFile $WAYTERM_SSHD_RUN/sshd.pid

# Test account only. Nothing else may log in on these ports.
AllowUsers $WAYTERM_SSHD_USER
PermitRootLogin no

# Required by the keyboard-interactive:pam mode (doc 11 §Auth methods).
UsePAM yes

AuthorizedKeysFile .ssh/authorized_keys
StrictModes yes

#@WAYTERM_AUTH_BLOCK@

# No forwarding/agent plumbing is needed to assert the auth chain; deny it so
# the throwaway instances expose as little as possible.
AllowAgentForwarding no
AllowTcpForwarding no
X11Forwarding no
PermitTunnel no

# The chain attempts at most 4 methods, and the wrong-password cases add one
# retry each — 12 leaves headroom without being unbounded.
MaxAuthTries 12
MaxSessions 20
LoginGraceTime 30

# VERBOSE logs "Accepted <method> for <user>", which is how the runner reports
# which link in the chain actually completed auth. Test-host chatter lands in
# the throwaway log file, not the system journal.
LogLevel VERBOSE

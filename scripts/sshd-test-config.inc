# Minimal sshd config fragment for the local Wayterm SSH integration tests.
#
# Rendered by scripts/run-sshd-tests.sh (path substituted for $WAYTERM_SSHD_PORT).
# It is fed to a SEPARATE `sshd -f` instance; the system sshd on port 22 and
# /etc/ssh/sshd_config are never touched or modified.

Port $WAYTERM_SSHD_PORT
ListenAddress 127.0.0.1
AddressFamily inet

HostKey /etc/ssh/ssh_host_ed25519_key
HostKey /etc/ssh/ssh_host_rsa_key

PidFile $WAYTERM_SSHD_RUN/sshd.pid

# Test account only. Nothing else may log in on this port.
AllowUsers $WAYTERM_SSHD_USER
PermitRootLogin no
UsePAM yes

# All three auth methods the integration suite exercises (doc 15).
PubkeyAuthentication yes
PasswordAuthentication yes
KbdInteractiveAuthentication yes

AuthorizedKeysFile .ssh/authorized_keys
StrictModes yes

# No agent/X11/tunnel plumbing is needed by the transport tests; deny it so
# the throwaway instance exposes as little as possible. TCP forwarding IS
# allowed: the Phase 11 suites (forward_sshd, jump_sshd) prove direct-tcpip
# channels against this instance. It listens on 127.0.0.1 for one
# throwaway user only, matching the CI sshd default.
AllowAgentForwarding no
AllowTcpForwarding yes
X11Forwarding no
PermitTunnel no

MaxAuthTries 12
MaxSessions 20
LoginGraceTime 30

# The dedicated config does not inherit the distro's default sshd_config, so
# the sftp subsystem must be declared explicitly (doc 07 SFTP integration).
Subsystem sftp internal-sftp

# Test-host chatter lands in the throwaway log file, not the system journal.
LogLevel VERBOSE
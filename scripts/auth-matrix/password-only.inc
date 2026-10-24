# Matrix mode (a): PASSWORD ONLY.
#
# Pubkey is left ENABLED on purpose. With `AuthenticationMethods password`,
# sshd accepts only a password, but a client may still offer its key — and the
# server rejects it. That rejection is the point: it is what forces the auth
# chain to fall through from publickey to password (doc 04 §Authentication).
#
# Turning PubkeyAuthentication off instead would make the same test pass for
# the wrong reason — a client that never offered its key at all.

AuthenticationMethods password
PasswordAuthentication yes
KbdInteractiveAuthentication no
PubkeyAuthentication yes

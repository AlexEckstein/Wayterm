# Matrix mode (c): KEY ONLY.
#
# The two trailing links are switched off, so password and keyboard-interactive
# are offered, rejected, and the key is the only thing that can complete auth.
# Also the mode that pins the "wrong key path reports Failed cleanly" case: with
# no other usable method there is nothing for the chain to fall through to, and
# the outcome must still be a clean Failed rather than a panic or a transport
# error.

AuthenticationMethods publickey
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no

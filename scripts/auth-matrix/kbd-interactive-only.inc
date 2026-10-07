# Matrix mode (b): KEYBOARD-INTERACTIVE ONLY, mediated by PAM.
#
# `keyboard-interactive:pam` requires UsePAM yes and KbdInteractiveAuthentication
# yes (both set in scripts/sshd-auth-matrix-config.inc). Password and pubkey are
# switched off, so neither can complete auth — only the LAST link in the chain
# (doc 11 §Flow step 4) can, which is what makes this mode the proof that the
# challenge-response path engages and is not dead code.
#
# Depends on the host having /etc/pam.d/sshd and pam_unix.so. On a host without
# PAM the mode is reported as an sshd-side finding rather than forced.

AuthenticationMethods keyboard-interactive:pam
KbdInteractiveAuthentication yes
PasswordAuthentication no
PubkeyAuthentication no

#!/usr/bin/env bash
#
# Run tests/serial_pty.rs against real serial hardware. Optional and local
# only: CI never runs this (it has no serial ports; the PTY tests cover CI).
#
# Two-port loopback (preferred): two USB-serial adapters joined by a
# null-modem cable — TX↔RX crossed, GND↔GND (and RTS↔CTS crossed if you test
# hardware flow control). The transport opens the first port; the test
# opens the second as the far end of the line.
#
#   scripts/serial-loopback.sh /dev/ttyUSB0 /dev/ttyUSB1 [baud]
#
# One-port loopback: a single adapter with a TX→RX jumper; the test asserts
# that the transport's own writes echo back as output.
#
#   scripts/serial-loopback.sh /dev/ttyUSB0 [baud]
#
# The user needs read/write access to both devices — usually membership of
# the `dialout` group (log out and back in after `usermod -aG dialout`).
# Nothing else may hold the ports open (ModemManager, screen, minicom).

set -euo pipefail

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

[[ $# -ge 1 && $# -le 3 ]] || usage

device=$1
peer=
baud=115200
if [[ $# -ge 2 ]]; then
    if [[ $2 =~ ^[0-9]+$ ]]; then
        baud=$2
    else
        peer=$2
        baud=${3:-$baud}
    fi
fi

for port in "$device" ${peer:+"$peer"}; do
    if [[ ! -r $port || ! -w $port ]]; then
        echo "error: no read/write access to $port (missing, or not in the dialout group?)" >&2
        exit 1
    fi
done

export SERIAL_TEST_DEVICE=$device
export SERIAL_TEST_BAUD=$baud
if [[ -n $peer ]]; then
    export SERIAL_TEST_PEER=$peer
else
    unset SERIAL_TEST_PEER
fi

cd "$(dirname "$0")/.."
exec cargo test --test serial_pty -- --nocapture

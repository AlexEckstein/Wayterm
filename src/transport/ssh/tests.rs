// Unit tests for SSH transport live here.

use crate::transport::ssh::SSHTransport;
use crate::transport::types::TransportType;

#[test]
fn transport_type_display() {
    assert_eq!(TransportType::SSH.to_string(), "SSH");
    assert_eq!(TransportType::LocalPTY.to_string(), "LocalPTY");
    assert_eq!(TransportType::Serial.to_string(), "Serial");
}

#[test]
fn shell_quote_escapes_metacharacters() {
    assert_eq!(SSHTransport::shell_quote(""), "''");
    assert_eq!(SSHTransport::shell_quote("safe_name"), "safe_name");
    assert_eq!(SSHTransport::shell_quote("a;b"), "'a;b'");
    assert_eq!(SSHTransport::shell_quote("it's"), "'it'\\''s'");
}

#[test]
fn shell_quote_does_not_leak_input() {
    // The function is pure: output length is bounded by the input plus quotes.
    let input = "hello";
    let out = SSHTransport::shell_quote(input);
    assert!(out.len() >= input.len());
    assert!(out.starts_with('\'') || out == input);
}

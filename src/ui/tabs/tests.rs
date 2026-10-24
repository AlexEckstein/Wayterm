use super::*;

#[test]
fn every_session_type_has_its_tab_type() {
    assert_eq!(TabType::of(&SessionType::SSH), TabType::Ssh);
    assert_eq!(TabType::of(&SessionType::LocalShell), TabType::Local);
    assert_eq!(TabType::of(&SessionType::Serial), TabType::Local);
    assert_eq!(TabType::of(&SessionType::VNC), TabType::Vnc);
    assert_eq!(TabType::of(&SessionType::RDP), TabType::Rdp);
}

//! Endpoint OS identity. This describes the sender, never a peer-selected injection backend.
use crate::pb::OperatingSystem;

pub fn local() -> OperatingSystem {
    match std::env::consts::OS {
        "windows" => OperatingSystem::Windows,
        "macos" => OperatingSystem::Macos,
        "linux" => OperatingSystem::Linux,
        "android" => OperatingSystem::Android,
        "ios" => OperatingSystem::Ios,
        _ => OperatingSystem::Other,
    }
}

pub fn from_wire(value: i32) -> Result<OperatingSystem, &'static str> {
    match OperatingSystem::try_from(value) {
        Ok(OperatingSystem::Unspecified) | Err(_) => Err("系统信息缺失或无效，请同步升级双方程序"),
        Ok(system) => Ok(system),
    }
}

pub fn name(system: OperatingSystem) -> &'static str {
    match system {
        OperatingSystem::Windows => "windows",
        OperatingSystem::Macos => "macos",
        OperatingSystem::Linux => "linux",
        OperatingSystem::Android => "android",
        OperatingSystem::Ios => "ios",
        OperatingSystem::Other => "other",
        OperatingSystem::Unspecified => "unspecified",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    #[test]
    fn both_handshake_directions_preserve_every_system() {
        for system in 1..=6 {
            let hello = crate::pb::Hello {
                system,
                ..Default::default()
            };
            let welcome = crate::pb::Welcome {
                system,
                ..Default::default()
            };
            assert_eq!(
                crate::pb::Hello::decode(hello.encode_to_vec().as_slice())
                    .unwrap()
                    .system,
                system
            );
            assert_eq!(
                crate::pb::Welcome::decode(welcome.encode_to_vec().as_slice())
                    .unwrap()
                    .system,
                system
            );
            assert!(!name(from_wire(system).unwrap()).is_empty());
        }
        assert!(from_wire(local() as i32).is_ok());
        for value in [0, -1, 7, i32::MAX] {
            assert!(from_wire(value).is_err());
        }
        assert_eq!(crate::PROTO_VER, 1);
    }
}

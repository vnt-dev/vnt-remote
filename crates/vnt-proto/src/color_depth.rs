//! RGB precision of a session, independent of video codec component bit depth.
pub const DEFAULT: u32 = 24;

pub fn valid(depth: u32) -> bool {
    matches!(depth, 15 | 16 | 24 | 32)
}

pub fn from_wire(depth: u32) -> Option<u32> {
    valid(depth).then_some(depth)
}

pub fn confirmed(requested: u32, applied: u32) -> bool {
    valid(requested) && requested == applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    #[test]
    fn explicit_depth_and_confirmation() {
        assert_eq!(from_wire(0), None);
        for depth in [15, 16, 24, 32] {
            assert_eq!(from_wire(depth), Some(depth));
            assert!(confirmed(depth, depth));
            assert!(!confirmed(depth, 0));
            let hello = crate::pb::Hello {
                desktop_effects: Some(Default::default()),
                color_depth: depth,
                ..Default::default()
            };
            assert_eq!(
                crate::pb::Hello::decode(hello.encode_to_vec().as_slice())
                    .unwrap()
                    .color_depth,
                depth
            );
            let welcome = crate::pb::Welcome {
                color_depth: depth,
                ..Default::default()
            };
            assert_eq!(
                crate::pb::Welcome::decode(welcome.encode_to_vec().as_slice())
                    .unwrap()
                    .color_depth,
                depth
            );
        }
        for depth in [1, 8, 17, 30, u32::MAX] {
            assert_eq!(from_wire(depth), None);
            assert!(!confirmed(depth, depth));
        }
        assert!(!confirmed(15, 16));
    }
}

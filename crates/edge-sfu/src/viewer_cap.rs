//! Viewer cap enforcement (R8.8).
//! Cap = 30 viewer per session, source of truth = local SFU state.

use crate::VIEWER_CAP_PER_SESSION;

pub fn can_accept_viewer(current_count: u32) -> bool {
    current_count < VIEWER_CAP_PER_SESSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn never_exceeds_cap(count in 0u32..1000) {
            // R8.8 invariant: when count >= cap, reject
            if count >= VIEWER_CAP_PER_SESSION {
                prop_assert!(!can_accept_viewer(count));
            } else {
                prop_assert!(can_accept_viewer(count));
            }
        }
    }
}

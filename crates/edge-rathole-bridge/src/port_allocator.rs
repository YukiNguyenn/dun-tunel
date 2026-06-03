//! Local upstream port allocation for tunnel forwarding.
//! Range: 11000-11999 (separate from container API ports 10200-10299).

use edge_shared::errors::{EdgeError, EdgeResult};
use std::collections::BTreeSet;
use tokio::sync::Mutex;

const PORT_MIN: u16 = 11_000;
const PORT_MAX: u16 = 11_999;

pub struct PortAllocator {
    free: Mutex<BTreeSet<u16>>,
}

impl PortAllocator {
    pub fn new() -> Self {
        let free: BTreeSet<u16> = (PORT_MIN..=PORT_MAX).collect();
        Self {
            free: Mutex::new(free),
        }
    }

    pub async fn allocate(&self) -> EdgeResult<u16> {
        let mut guard = self.free.lock().await;
        let port = guard
            .iter()
            .next()
            .copied()
            .ok_or(EdgeError::PortPoolExhausted)?;
        guard.remove(&port);
        Ok(port)
    }

    pub async fn release(&self, port: u16) {
        if (PORT_MIN..=PORT_MAX).contains(&port) {
            self.free.lock().await.insert(port);
        }
    }
}

impl Default for PortAllocator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn allocated_port_in_range(seed in 0u32..1000) {
            tokio::runtime::Runtime::new().unwrap().block_on(async move {
                let alloc = PortAllocator::new();
                for _ in 0..(seed % 50) {
                    let port = alloc.allocate().await.unwrap();
                    prop_assert!((PORT_MIN..=PORT_MAX).contains(&port));
                }
                Ok::<_, proptest::test_runner::TestCaseError>(())
            }).unwrap();
        }
    }
}

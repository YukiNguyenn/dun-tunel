//! edge-rathole-bridge — manage rathole config + spawn process.
//!
//! Tham chiếu R23.2.

pub mod config_writer;
pub mod port_allocator;
pub mod service_registry;

pub use port_allocator::PortAllocator;
pub use service_registry::ServiceRegistry;

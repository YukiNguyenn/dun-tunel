//! Auth — Phase 1-3 dùng API key header `X-Edge-Api-Key`,
//! Phase 4 chuyển sang mTLS với client cert pinning (xem mtls.rs).

pub mod api_key;
pub mod mtls;

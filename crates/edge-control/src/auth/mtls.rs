//! mTLS client cert pinning for Phase 4 hardening.
//! TODO: extract client cert from TLS connection, verify SHA-256 fingerprint
//! against allowed list (from EdgeConfig.allowed_client_fingerprints).

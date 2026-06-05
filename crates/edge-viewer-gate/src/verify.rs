//! Cookie token verifier — pure logic, no I/O.
//!
//! Caddy passes us the original `Cookie` and `X-Forwarded-Host`
//! headers. We extract the `dun_share_session` cookie, run JWT
//! verify (EdDSA + aud + exp), then check the `host` claim matches
//! the request host (D11.5 cross-session leak protection) and the
//! `jti` is not on the revocation list.

use axum::http::HeaderMap;
use jsonwebtoken::{decode, decode_header, Algorithm, Validation};
use serde::Deserialize;

use crate::jwks::JwksCache;
use crate::revocation::RevocationList;

const COOKIE_NAME: &str = "dun_share_session";
const COOKIE_AUDIENCE: &str = "viewer-cookie";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing or empty Cookie header")]
    MissingCookie,
    #[error("missing X-Forwarded-Host header")]
    MissingHost,
    #[error("cookie not present")]
    CookieAbsent,
    #[error("malformed JWT header")]
    BadHeader,
    #[error("unsupported alg: {0:?}")]
    UnsupportedAlg(Algorithm),
    #[error("missing kid header")]
    MissingKid,
    #[error("unknown kid: {0}")]
    UnknownKid(String),
    #[error("decode failed: {0}")]
    DecodeFailed(String),
    #[error("audience mismatch")]
    AudienceMismatch,
    #[error("host claim mismatch (got {got:?}, expected {expected:?})")]
    HostMismatch { got: String, expected: String },
    #[error("token revoked: {0}")]
    Revoked(String),
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // claims read into struct for diagnostics; not all fields used in code path
pub struct CookieClaims {
    pub sub: String,
    pub aud: String,
    pub exp: i64,
    pub iat: i64,
    pub jti: String,
    pub host: String,
}

pub async fn authorize(
    headers: &HeaderMap,
    jwks: &JwksCache,
    revocation: &RevocationList,
) -> Result<CookieClaims, AuthError> {
    let cookie_raw = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .ok_or(AuthError::MissingCookie)?;
    let token = extract_cookie(cookie_raw, COOKIE_NAME).ok_or(AuthError::CookieAbsent)?;

    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(':').next().unwrap_or(s).to_lowercase())
        .ok_or(AuthError::MissingHost)?;

    let header = decode_header(token).map_err(|_| AuthError::BadHeader)?;
    if header.alg != Algorithm::EdDSA {
        return Err(AuthError::UnsupportedAlg(header.alg));
    }
    let kid = header.kid.ok_or(AuthError::MissingKid)?;
    let key = jwks.get(&kid).await.ok_or_else(|| AuthError::UnknownKid(kid.clone()))?;

    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_audience(&[COOKIE_AUDIENCE]);
    validation.validate_exp = true;
    let data = decode::<CookieClaims>(token, &key, &validation)
        .map_err(|e| AuthError::DecodeFailed(e.to_string()))?;
    let claims = data.claims;

    if claims.aud != COOKIE_AUDIENCE {
        return Err(AuthError::AudienceMismatch);
    }
    if claims.host.to_lowercase() != host {
        return Err(AuthError::HostMismatch {
            got: claims.host.clone(),
            expected: host,
        });
    }
    if revocation.contains(&claims.jti).await {
        return Err(AuthError::Revoked(claims.jti.clone()));
    }
    Ok(claims)
}

/// Extract a single cookie value from a raw `Cookie` header. Cookies
/// are `name=value; name2=value2` separated. We do NOT call out to a
/// full-blown cookie crate because we only ever read one well-known
/// name.
fn extract_cookie<'a>(raw: &'a str, name: &str) -> Option<&'a str> {
    for kv in raw.split(';') {
        let kv = kv.trim();
        if let Some((k, v)) = kv.split_once('=') {
            if k.trim() == name {
                return Some(v.trim());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_cookie_finds_the_named_one() {
        assert_eq!(
            extract_cookie("a=1; dun_share_session=eyJxxx; b=2", "dun_share_session"),
            Some("eyJxxx"),
        );
        assert_eq!(
            extract_cookie("dun_share_session=alone", "dun_share_session"),
            Some("alone"),
        );
        assert_eq!(extract_cookie("a=1; b=2", "dun_share_session"), None);
        assert_eq!(extract_cookie("", "dun_share_session"), None);
    }
}

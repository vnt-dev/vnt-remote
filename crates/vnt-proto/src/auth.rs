//! Password challenge-response shared by TCP hosts and local session agents.
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const NONCE_LEN: usize = 32;
pub const PROOF_LEN: usize = 32;

pub fn new_nonce() -> Result<[u8; NONCE_LEN], getrandom::Error> {
    let mut nonce = [0; NONCE_LEN];
    getrandom::fill(&mut nonce)?;
    Ok(nonce)
}

fn mac(password: &str, nonce: &[u8; NONCE_LEN]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(password.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(b"vnt-remote-auth-v1\0");
    mac.update(nonce);
    mac
}

pub fn response(password: &str, nonce: &[u8; NONCE_LEN]) -> Vec<u8> {
    mac(password, nonce).finalize().into_bytes().to_vec()
}

/// HMAC verification uses a constant-time comparison and rejects empty passwords.
pub fn verify(password: &str, nonce: &[u8; NONCE_LEN], proof: &[u8]) -> bool {
    !password.trim().is_empty() && mac(password, nonce).verify_slice(proof).is_ok()
}

/// Remote access follows the host's configured password policy. A challenge still
/// precedes every session; empty configuration explicitly permits passwordless use.
pub fn verify_remote(password: &str, nonce: &[u8; NONCE_LEN], proof: &[u8]) -> bool {
    if password.trim().is_empty() {
        proof.len() == PROOF_LEN
    } else {
        verify(password, nonce, proof)
    }
}

/// Session recovery is domain separated from password authentication.
pub fn session_response(secret: &[u8], session: u32, attempt: &[u8], nonce: &[u8]) -> Vec<u8> {
    session_mac(secret, session, attempt, nonce)
        .finalize()
        .into_bytes()
        .to_vec()
}
fn session_mac(secret: &[u8], session: u32, attempt: &[u8], nonce: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC key");
    mac.update(b"vnt-control-resume-v1\0");
    mac.update(&session.to_be_bytes());
    mac.update(attempt);
    mac.update(nonce);
    mac
}
pub fn verify_session(
    secret: &[u8],
    session: u32,
    attempt: &[u8],
    nonce: &[u8],
    proof: &[u8],
) -> bool {
    secret.len() == 32
        && attempt.len() == 16
        && nonce.len() == NONCE_LEN
        && session_mac(secret, session, attempt, nonce)
            .verify_slice(proof)
            .is_ok()
}
/// Compare binding credentials without a data-dependent early exit.
pub fn equal_token(expected: &[u8], actual: &[u8]) -> bool {
    if expected.len() != 32 || actual.len() != 32 {
        return false;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(expected).expect("HMAC key");
    mac.update(b"vnt-binding-v1");
    let mut other = Hmac::<Sha256>::new_from_slice(actual).expect("HMAC key");
    other.update(b"vnt-binding-v1");
    mac.verify_slice(&other.finalize().into_bytes()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn passwordless_remote_access_does_not_weaken_agent_authentication() {
        let nonce = [7; NONCE_LEN];
        let proof = response("", &nonce);
        assert!(verify_remote("", &nonce, &proof));
        assert!(verify_remote("", &nonce, &response("unused", &nonce)));
        assert!(!verify("", &nonce, &proof));
        assert!(!verify_remote("configured", &nonce, &proof));
        assert!(!verify_remote("", &nonce, &[0; 3]));
        assert!(verify_remote(
            "configured",
            &nonce,
            &response("configured", &nonce)
        ));
    }

    #[test]
    fn recovery_proofs_bind_secret_session_attempt_nonce_and_purpose() {
        let secret = [7; 32];
        let attempt = [8; 16];
        let nonce = [9; 32];
        let proof = session_response(&secret, 123, &attempt, &nonce);
        assert!(verify_session(&secret, 123, &attempt, &nonce, &proof));
        assert!(!verify_session(&[6; 32], 123, &attempt, &nonce, &proof));
        assert!(!verify_session(&secret, 124, &attempt, &nonce, &proof));
        assert!(!verify_session(&secret, 123, &[0; 16], &nonce, &proof));
        assert!(!verify_session(&secret, 123, &attempt, &[0; 32], &proof));
        assert!(!verify_session(
            &secret,
            123,
            &attempt,
            &nonce,
            &proof[..31]
        ));
        assert!(!verify_session(
            &secret[..31],
            123,
            &attempt,
            &nonce,
            &proof
        ));
        assert!(!verify_session(
            &secret,
            123,
            &attempt[..15],
            &nonce,
            &proof
        ));
        assert!(!verify("password", &nonce, &proof));
        assert!(equal_token(&secret, &secret));
        assert!(!equal_token(&secret, &[6; 32]));
        assert!(!equal_token(&secret, &secret[..31]));
    }

    #[test]
    fn proof_is_bound_to_password_and_connection() {
        let nonce = new_nonce().unwrap();
        let other = new_nonce().unwrap();
        assert_ne!(nonce, other);
        let proof = response("pw-测试", &nonce);
        assert!(verify("pw-测试", &nonce, &proof));
        assert!(!verify("wrong", &nonce, &proof));
        assert!(!verify("pw-测试", &other, &proof));
        assert!(!verify("", &nonce, &response("", &nonce)));
        assert!(!verify("pw-测试", &nonce, &proof[..31]));
        let mut tampered = proof;
        tampered[0] ^= 1;
        assert!(!verify("pw-测试", &nonce, &tampered));
    }
}

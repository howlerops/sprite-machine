//! Web Push crypto and request building (pure Rust, no I/O, builds for wasm32).
//!
//! - RFC 8291 message encryption, `Content-Encoding: aes128gcm` (RFC 8188): ECDH on P-256 between
//!   an ephemeral application-server key and the subscription's `p256dh`, HKDF-SHA256 keyed with
//!   the subscription's `auth` secret, one AES-128-GCM record. [`encrypt_with`] takes the
//!   ephemeral key and the salt so the RFC 8291 Appendix A vector can be checked byte for byte.
//! - RFC 8292 VAPID: an ES256 JWT (`aud`, `exp`, `sub`) signed with the hub's VAPID key and sent as
//!   `Authorization: vapid t=<jwt>, k=<public key>`. The private key comes from the Worker secret
//!   `VAPID_PRIVATE_KEY` (base64url of the 32-byte P-256 scalar); [`Vapid`] never prints it.

use std::fmt;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hkdf::Hkdf;
use p256::ecdh::diffie_hellman;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use sha2::Sha256;

/// The record size written in the aes128gcm header. One record holds the whole message.
const RECORD_SIZE: u32 = 4096;
/// Size of an uncompressed P-256 point.
const POINT_LEN: usize = 65;
/// Largest plaintext that fits one record: record size minus the GCM tag and the delimiter byte.
pub const MAX_PLAINTEXT: usize = RECORD_SIZE as usize - 16 - 1;
/// JWT lifetime (RFC 8292 §2: at most 24 hours).
const VAPID_TTL_SECS: u64 = 12 * 60 * 60;

/// Why a message could not be encrypted or a VAPID header could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebPushError {
    /// The subscription's `p256dh` is not an uncompressed P-256 point on the curve.
    BadPublicKey,
    /// The subscription's `auth` secret is not 16 bytes.
    BadAuthSecret,
    /// The plaintext does not fit one record.
    TooLarge,
    /// The VAPID private key is not a base64url P-256 scalar.
    BadVapidKey,
    /// The endpoint has no usable `scheme://host` origin.
    BadEndpoint,
    /// The system random number generator failed.
    Random,
}

impl fmt::Display for WebPushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WebPushError::BadPublicKey => "p256dh is not a P-256 public key",
            WebPushError::BadAuthSecret => "auth is not a 16-byte secret",
            WebPushError::TooLarge => "payload is too large for one record",
            WebPushError::BadVapidKey => "VAPID private key is not a base64url P-256 scalar",
            WebPushError::BadEndpoint => "endpoint is not an absolute URL",
            WebPushError::Random => "random number generator failed",
        })
    }
}

impl std::error::Error for WebPushError {}

/// Decodes base64url, with or without `=` padding (browsers send it without).
pub fn b64_decode(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(text.trim().trim_end_matches('='))
        .ok()
}

/// base64url without padding.
pub fn b64_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Checks the key material of a subscription: `p256dh` is a P-256 point, `auth` is 16 bytes.
pub fn validate_keys(p256dh: &str, auth: &str) -> Result<(), WebPushError> {
    let key = b64_decode(p256dh).ok_or(WebPushError::BadPublicKey)?;
    parse_public(&key)?;
    match b64_decode(auth) {
        Some(secret) if secret.len() == 16 => Ok(()),
        _ => Err(WebPushError::BadAuthSecret),
    }
}

fn parse_public(bytes: &[u8]) -> Result<PublicKey, WebPushError> {
    if bytes.len() != POINT_LEN || bytes[0] != 4 {
        return Err(WebPushError::BadPublicKey);
    }
    PublicKey::from_sec1_bytes(bytes).map_err(|_| WebPushError::BadPublicKey)
}

fn uncompressed(key: &PublicKey) -> Vec<u8> {
    key.to_encoded_point(false).as_bytes().to_vec()
}

/// RFC 8291 §3: encrypts `plaintext` for a subscription with a fresh ephemeral key and salt.
/// Returns the request body (aes128gcm header followed by the record).
pub fn encrypt(
    plaintext: &[u8],
    ua_public: &[u8],
    auth_secret: &[u8],
) -> Result<Vec<u8>, WebPushError> {
    let mut salt = [0_u8; 16];
    getrandom::getrandom(&mut salt).map_err(|_| WebPushError::Random)?;
    // A random 32-byte string is a valid P-256 scalar except with negligible probability; retry
    // a few times rather than depend on a `rand_core` generator.
    for _ in 0..8 {
        let mut seed = [0_u8; 32];
        getrandom::getrandom(&mut seed).map_err(|_| WebPushError::Random)?;
        if let Ok(secret) = SecretKey::from_slice(&seed) {
            return encrypt_with(plaintext, ua_public, auth_secret, &secret, &salt);
        }
    }
    Err(WebPushError::Random)
}

/// [`encrypt`] with the application-server key and the salt given (for the RFC test vector).
pub fn encrypt_with(
    plaintext: &[u8],
    ua_public: &[u8],
    auth_secret: &[u8],
    as_secret: &SecretKey,
    salt: &[u8; 16],
) -> Result<Vec<u8>, WebPushError> {
    if auth_secret.len() != 16 {
        return Err(WebPushError::BadAuthSecret);
    }
    if plaintext.len() > MAX_PLAINTEXT {
        return Err(WebPushError::TooLarge);
    }
    let ua = parse_public(ua_public)?;
    let as_public = uncompressed(&as_secret.public_key());

    let shared = diffie_hellman(as_secret.to_nonzero_scalar(), ua.as_affine());
    // PRK_key = HKDF-Extract(salt = auth_secret, IKM = ecdh_secret)
    let prk_key = Hkdf::<Sha256>::new(Some(auth_secret), shared.raw_secret_bytes());
    // IKM = HKDF-Expand(PRK_key, "WebPush: info" || 0x00 || ua_public || as_public, 32)
    let mut key_info = Vec::with_capacity(14 + 2 * POINT_LEN);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(&as_public);
    let mut ikm = [0_u8; 32];
    prk_key
        .expand(&key_info, &mut ikm)
        .map_err(|_| WebPushError::Random)?;

    // RFC 8188: PRK = HKDF-Extract(salt, IKM); CEK and NONCE are expanded from it.
    let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0_u8; 16];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|_| WebPushError::Random)?;
    let mut nonce = [0_u8; 12];
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|_| WebPushError::Random)?;

    // One record: data, then the 0x02 "last record" delimiter, no further padding.
    let mut record = Vec::with_capacity(plaintext.len() + 1);
    record.extend_from_slice(plaintext);
    record.push(2);
    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|_| WebPushError::Random)?;
    let sealed = cipher
        .encrypt(Nonce::from_slice(&nonce), record.as_slice())
        .map_err(|_| WebPushError::Random)?;

    // Header: salt(16) || record size(4, BE) || key id length(1) || key id (the AS public key).
    let mut body = Vec::with_capacity(16 + 4 + 1 + POINT_LEN + sealed.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(POINT_LEN as u8);
    body.extend_from_slice(&as_public);
    body.extend_from_slice(&sealed);
    Ok(body)
}

/// The user agent's side of [`encrypt`]: opens an aes128gcm body with the subscription's private
/// key and `auth` secret (RFC 8291 §3.4). The hub never needs it; the tests use it to read what a
/// fake push service received, with the key the test generated.
pub fn decrypt(
    body: &[u8],
    ua_secret: &SecretKey,
    auth_secret: &[u8],
) -> Result<Vec<u8>, WebPushError> {
    // salt(16) || record size(4) || key id length(1) || key id || record
    if body.len() < 21 + POINT_LEN + 16 || usize::from(body[20]) != POINT_LEN {
        return Err(WebPushError::BadPublicKey);
    }
    let salt = &body[..16];
    let as_public = &body[21..21 + POINT_LEN];
    let sealed = &body[21 + POINT_LEN..];
    let ua_public = uncompressed(&ua_secret.public_key());
    let shared = diffie_hellman(
        ua_secret.to_nonzero_scalar(),
        parse_public(as_public)?.as_affine(),
    );

    let prk_key = Hkdf::<Sha256>::new(Some(auth_secret), shared.raw_secret_bytes());
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(&ua_public);
    key_info.extend_from_slice(as_public);
    let mut ikm = [0_u8; 32];
    prk_key
        .expand(&key_info, &mut ikm)
        .map_err(|_| WebPushError::Random)?;
    let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0_u8; 16];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|_| WebPushError::Random)?;
    let mut nonce = [0_u8; 12];
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|_| WebPushError::Random)?;
    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|_| WebPushError::Random)?;
    let mut record = cipher
        .decrypt(Nonce::from_slice(&nonce), sealed)
        .map_err(|_| WebPushError::BadAuthSecret)?;
    // Strip the padding: zeros, then the 0x02 delimiter of the last record.
    while record.last() == Some(&0) {
        record.pop();
    }
    if record.pop() != Some(2) {
        return Err(WebPushError::TooLarge);
    }
    Ok(record)
}

/// The hub's VAPID identity (RFC 8292).
pub struct Vapid {
    key: SigningKey,
    public: String,
    subject: String,
}

impl fmt::Debug for Vapid {
    // The private key never appears in a log line or a panic message.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vapid")
            .field("public", &self.public)
            .field("subject", &self.subject)
            .finish_non_exhaustive()
    }
}

impl Vapid {
    /// From the base64url 32-byte scalar (the `VAPID_PRIVATE_KEY` secret) and the `sub` claim
    /// (`mailto:` or `https:` contact).
    pub fn from_private(private_b64: &str, subject: &str) -> Result<Self, WebPushError> {
        let bytes = b64_decode(private_b64).ok_or(WebPushError::BadVapidKey)?;
        Self::from_scalar(&bytes, subject)
    }

    /// From the raw 32-byte scalar.
    pub fn from_scalar(scalar: &[u8], subject: &str) -> Result<Self, WebPushError> {
        let key = SigningKey::from_slice(scalar).map_err(|_| WebPushError::BadVapidKey)?;
        let public = b64_encode(&uncompressed(&PublicKey::from(key.verifying_key())));
        Ok(Self {
            key,
            public,
            subject: subject.to_owned(),
        })
    }

    /// The public key, base64url of the uncompressed point: the `k=` parameter and the
    /// `applicationServerKey` the phone subscribes with.
    pub fn public_key(&self) -> &str {
        &self.public
    }

    /// The `Authorization` header value for a push to `endpoint` at `now_secs`.
    pub fn authorization(&self, endpoint: &str, now_secs: u64) -> Result<String, WebPushError> {
        let aud = endpoint_origin(endpoint).ok_or(WebPushError::BadEndpoint)?;
        let header = b64_encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::json!({
            "aud": aud,
            "exp": now_secs + VAPID_TTL_SECS,
            "sub": self.subject,
        });
        let claims = b64_encode(claims.to_string().as_bytes());
        let signing_input = format!("{header}.{claims}");
        let signature: Signature = self.key.sign(signing_input.as_bytes());
        Ok(format!(
            "vapid t={signing_input}.{}, k={}",
            b64_encode(&signature.to_bytes()),
            self.public
        ))
    }
}

/// `scheme://host[:port]` of an absolute URL (the VAPID `aud`), or `None`.
pub fn endpoint_origin(endpoint: &str) -> Option<String> {
    let (scheme, rest) = endpoint.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if scheme.is_empty() || authority.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier;

    // RFC 8291 Appendix A.
    const PLAINTEXT: &str = "When I grow up, I want to be a watermelon";
    const AS_PRIVATE: &str = "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw";
    const UA_PUBLIC: &str =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const AUTH_SECRET: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    const SALT: &str = "DGv6ra1nlYgDCS1FRnbzlw";
    const ENCRYPTED: &str = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN";

    #[test]
    fn rfc8291_appendix_a_vector_matches_byte_for_byte() {
        let as_secret = SecretKey::from_slice(&b64_decode(AS_PRIVATE).unwrap()).unwrap();
        let salt: [u8; 16] = b64_decode(SALT).unwrap().try_into().unwrap();
        let body = encrypt_with(
            PLAINTEXT.as_bytes(),
            &b64_decode(UA_PUBLIC).unwrap(),
            &b64_decode(AUTH_SECRET).unwrap(),
            &as_secret,
            &salt,
        )
        .expect("encrypt");
        assert_eq!(b64_encode(&body), ENCRYPTED);
    }

    #[test]
    fn rfc8291_appendix_a_message_decrypts_with_the_user_agent_key() {
        let ua_secret = SecretKey::from_slice(
            &b64_decode("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94").unwrap(),
        )
        .unwrap();
        let plain = decrypt(
            &b64_decode(ENCRYPTED).unwrap(),
            &ua_secret,
            &b64_decode(AUTH_SECRET).unwrap(),
        )
        .expect("decrypt");
        assert_eq!(plain, PLAINTEXT.as_bytes());
        // A wrong auth secret does not open it.
        assert!(decrypt(&b64_decode(ENCRYPTED).unwrap(), &ua_secret, &[0; 16]).is_err());
    }

    #[test]
    fn fresh_encryption_differs_per_call_and_has_the_aes128gcm_header() {
        let ua = b64_decode(UA_PUBLIC).unwrap();
        let auth = b64_decode(AUTH_SECRET).unwrap();
        let a = encrypt(b"hello", &ua, &auth).expect("a");
        let b = encrypt(b"hello", &ua, &auth).expect("b");
        assert_ne!(a, b, "salt and ephemeral key are random");
        assert_eq!(&a[16..20], &RECORD_SIZE.to_be_bytes());
        assert_eq!(a[20], 65);
        assert_eq!(a[21], 4, "uncompressed point");
    }

    #[test]
    fn bad_keys_and_oversized_payloads_are_refused() {
        let ua = b64_decode(UA_PUBLIC).unwrap();
        let auth = b64_decode(AUTH_SECRET).unwrap();
        assert_eq!(
            encrypt(b"x", &ua[..64], &auth),
            Err(WebPushError::BadPublicKey)
        );
        assert_eq!(
            encrypt(b"x", &ua, &auth[..15]),
            Err(WebPushError::BadAuthSecret)
        );
        assert_eq!(
            encrypt(&vec![0; MAX_PLAINTEXT + 1], &ua, &auth),
            Err(WebPushError::TooLarge)
        );
        assert!(encrypt(&vec![0; MAX_PLAINTEXT], &ua, &auth).is_ok());
        assert!(validate_keys(UA_PUBLIC, AUTH_SECRET).is_ok());
        assert!(validate_keys("AAAA", AUTH_SECRET).is_err());
        assert!(validate_keys(UA_PUBLIC, "AAAA").is_err());
    }

    #[test]
    fn vapid_header_is_a_valid_es256_jwt_for_the_endpoint_origin() {
        let vapid = Vapid::from_private(AS_PRIVATE, "mailto:ops@example.com").expect("vapid");
        let header = vapid
            .authorization("https://push.example.net/wpush/v2/abc?x=1", 1_700_000_000)
            .expect("header");
        let rest = header.strip_prefix("vapid t=").expect("vapid scheme");
        let (jwt, k) = rest.split_once(", k=").expect("k parameter");
        assert_eq!(k, vapid.public_key());

        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let head: serde_json::Value =
            serde_json::from_slice(&b64_decode(parts[0]).unwrap()).unwrap();
        assert_eq!(head, serde_json::json!({"typ": "JWT", "alg": "ES256"}));
        let claims: serde_json::Value =
            serde_json::from_slice(&b64_decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://push.example.net");
        assert_eq!(claims["sub"], "mailto:ops@example.com");
        assert_eq!(claims["exp"], 1_700_000_000_u64 + 12 * 3600);

        let public = b64_decode(k).unwrap();
        let verifying = VerifyingKey::from_sec1_bytes(&public).expect("public key");
        let signature = Signature::from_slice(&b64_decode(parts[2]).unwrap()).expect("raw r||s");
        verifying
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("signature verifies");
    }

    #[test]
    fn vapid_debug_output_does_not_contain_the_private_key() {
        let vapid = Vapid::from_private(AS_PRIVATE, "mailto:ops@example.com").expect("vapid");
        assert!(!format!("{vapid:?}").contains(AS_PRIVATE));
        assert_eq!(
            Vapid::from_private("not a key", "mailto:a@b.c").unwrap_err(),
            WebPushError::BadVapidKey
        );
    }

    #[test]
    fn endpoint_origin_keeps_scheme_host_and_port() {
        assert_eq!(
            endpoint_origin("https://fcm.googleapis.com/fcm/send/abc").as_deref(),
            Some("https://fcm.googleapis.com")
        );
        assert_eq!(
            endpoint_origin("http://127.0.0.1:8080/push").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(endpoint_origin("no-scheme"), None);
    }
}

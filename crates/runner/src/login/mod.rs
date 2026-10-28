//! `sm login` (M2B-005): pair this machine with a hub by device code (RFC 8628) and keep the
//! machine credential safe.
//!
//! - [`flow`]: request a code, show the user code, the verification URL and a terminal QR, poll
//!   with the server's interval (honouring `slow_down`), receive the machine id and the `smm_`
//!   credential once;
//! - [`store`]: the credential store: the OS keychain (`keyring`, platform-native backends only)
//!   with a `0600` file fallback, used when the keychain is unavailable and forced by
//!   `SM_CREDENTIAL_FILE` (CI, containers);
//! - [`credentials`]: [`KeychainCredentials`], the tunnel's `CredentialSource` over that store.
//!
//! The credential goes into the tunnel's `Authorization` header and nowhere else: never a URL,
//! never a log line, never the terminal.

pub mod credentials;
pub mod flow;
pub mod store;

pub use self::credentials::KeychainCredentials;
pub use self::flow::{LoginError, Sleeper, login, real_sleeper};
pub use self::store::{Backend, CredentialStore, Keychain, Login};

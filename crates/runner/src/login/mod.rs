//! `sm login` (M2B-005): pair this machine with a hub by device code (RFC 8628) and keep the
//! machine credential safe.
//!
//! - [`flow`]: request a code, show the user code, the verification URL and a terminal QR, poll
//!   with the server's interval (honouring `slow_down`), receive the machine id and the `smm_`
//!   credential once, then show the approving org and user and ask "Join org X approved by Y?"
//!   (`--yes` skips the question); on "no" the credential is dropped and `DELETE
//!   /v1/devices/self` revokes the machine;
//! - [`store`]: the credential store: a `0600` file (the persistence path on Linux), with the
//!   macOS Keychain (`keyring`, platform-native backend) in front of it; `SM_CREDENTIAL_FILE`
//!   forces the file (CI, containers);
//! - [`account`]: `sm logout` (wipe the credential) and `sm status` (hub, machine, presence);
//! - [`credentials`]: [`KeychainCredentials`], the tunnel's `CredentialSource` over that store.
//!
//! The credential goes into the tunnel's `Authorization` header and nowhere else: never a URL,
//! never a log line, never the terminal.

pub mod account;
pub mod credentials;
pub mod flow;
pub mod store;

pub use self::account::{logout, status};
pub use self::credentials::KeychainCredentials;
pub use self::flow::{
    Approval, Confirm, LoginError, Prompter, Sleeper, login, real_sleeper, terminal_prompter,
};
pub use self::store::{Backend, CredentialStore, Keychain, Login};

//! Worker configuration of the notifier (wasm32 only).

use worker::Env;

use super::webpush::Vapid;
use crate::threads::auth::dev_fixtures_enabled;

/// The `sub` claim when `VAPID_SUBJECT` is not set. Push services only need a contact.
const DEFAULT_SUBJECT: &str = "mailto:push@sprite-machine.invalid";

fn var(env: &Env, name: &str) -> Option<String> {
    env.var(name)
        .ok()
        .map(|value| value.to_string())
        .filter(|value| !value.trim().is_empty())
}

pub fn dev_fixtures(env: &Env) -> bool {
    dev_fixtures_enabled(var(env, "DEV_FIXTURES").as_deref())
}

/// The hub's VAPID identity, or `None` when no `VAPID_PRIVATE_KEY` secret is set (then no pushes
/// are sent).
///
/// The private key is the Worker secret `VAPID_PRIVATE_KEY` (base64url, 32 bytes) and is never
/// logged. There is no fallback: `scripts/hub-test.sh` generates a P-256 key per run and passes it
/// with `--var`, and production sets the secret with `wrangler secret put`.
pub fn load_vapid(env: &Env) -> Option<Vapid> {
    let subject = var(env, "VAPID_SUBJECT").unwrap_or_else(|| DEFAULT_SUBJECT.to_owned());
    let private = env
        .secret("VAPID_PRIVATE_KEY")
        .ok()
        .map(|secret| secret.to_string())
        .filter(|secret| !secret.trim().is_empty())?;
    Vapid::from_private(&private, &subject).ok()
}

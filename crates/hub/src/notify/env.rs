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

/// The hub's VAPID identity, or `None` when none is configured (then pushes are not sent).
///
/// The private key is the Worker secret `VAPID_PRIVATE_KEY` (base64url, 32 bytes) and is never
/// logged. Only with `DEV_FIXTURES=1` and no such secret, a throwaway key is derived from
/// `ATTACHMENT_SIGNING_KEY`, which `scripts/hub-test.sh` sets to a random per-run value.
pub fn load_vapid(env: &Env) -> Option<Vapid> {
    let subject = var(env, "VAPID_SUBJECT").unwrap_or_else(|| DEFAULT_SUBJECT.to_owned());
    if let Some(private) = env
        .secret("VAPID_PRIVATE_KEY")
        .ok()
        .map(|secret| secret.to_string())
        .filter(|secret| !secret.trim().is_empty())
    {
        return Vapid::from_private(&private, &subject).ok();
    }
    if dev_fixtures(env) {
        let seed = env
            .secret("ATTACHMENT_SIGNING_KEY")
            .ok()
            .map(|secret| secret.to_string())
            .filter(|secret| !secret.is_empty())?;
        return Vapid::dev_derived(&seed, &subject).ok();
    }
    None
}

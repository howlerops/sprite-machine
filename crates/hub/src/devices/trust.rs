//! Trusted devices (plan §4.2 "Auth", §4.10): how `orgs.require_trusted_devices` applies.
//!
//! Model: every machine has an `origin`. `device` = it was created by an approved pairing (a
//! member vouched for it: a trusted device). `admin` = created by `POST /v1/machines` (an admin
//! issued the credential directly, so an admin vouched for it too, and it stays allowed when the
//! setting is on). With the setting on, a machine that is neither is refused; today no such
//! machine can exist (both creation paths vouch), so the rule is the gate any future creation
//! path (self-registration) must pass. A revoked machine is refused either way.

use crate::db::{Db, DbError, Param, int, text};

/// How a machine came to exist (`machines.origin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Admin,
    Device,
}

impl Origin {
    pub fn parse(stored: &str) -> Option<Self> {
        match stored {
            "admin" => Some(Self::Admin),
            "device" => Some(Self::Device),
            _ => None,
        }
    }
}

/// Whether a machine may open a tunnel. `origin` is `None` for a stored value this code does
/// not know, which is never trusted.
pub fn tunnel_allowed(require_trusted: bool, origin: Option<Origin>, revoked: bool) -> bool {
    if revoked {
        return false;
    }
    !require_trusted || origin.is_some()
}

/// [`tunnel_allowed`] for a stored machine; `false` when there is no such machine.
pub async fn machine_may_connect(db: &impl Db, machine_id: &str) -> Result<bool, DbError> {
    let rows = db
        .query(
            "SELECT m.origin AS origin, m.revoked_at IS NOT NULL AS revoked, \
             o.require_trusted_devices AS require_trusted \
             FROM machines m JOIN orgs o ON o.id = m.org_id WHERE m.id = ?",
            &[Param::Text(machine_id.to_owned())],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(false);
    };
    Ok(tunnel_allowed(
        int(row, "require_trusted")? == 1,
        Origin::parse(&text(row, "origin")?),
        int(row, "revoked")? == 1,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule() {
        assert!(tunnel_allowed(true, Some(Origin::Device), false));
        assert!(tunnel_allowed(true, Some(Origin::Admin), false));
        assert!(tunnel_allowed(false, None, false));
        assert!(!tunnel_allowed(true, None, false));
        assert!(!tunnel_allowed(false, Some(Origin::Device), true));
    }
}

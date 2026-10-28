//! `sm logout` and `sm status`: what the stored login says, never the credential itself.

use std::io::Write;

use anyhow::Result;

use super::store::CredentialStore;

/// `sm logout`: forgets the stored credential in every store (keychain and file). The hub keeps
/// the machine registered; an admin of the org revokes it there.
pub fn logout(store: &CredentialStore, out: &mut impl Write) -> Result<()> {
    let previous = store.load().unwrap_or(None);
    store.wipe()?;
    match previous {
        Some(login) => writeln!(
            out,
            "logged out: removed the credential for machine {} ({})",
            login.machine_id, login.hub
        )?,
        None => writeln!(out, "not logged in: nothing to remove")?,
    }
    Ok(())
}

/// `sm status`: the hub, the machine id, and whether a credential is stored. The credential is
/// never printed.
pub fn status(store: &CredentialStore, out: &mut impl Write) -> Result<()> {
    match store.load()? {
        Some(login) => {
            writeln!(out, "hub:        {}", login.hub)?;
            writeln!(out, "machine:    {}", login.machine_id)?;
            writeln!(out, "credential: present")?;
        }
        None => writeln!(out, "credential: none (run `sm login --hub <url>`)")?,
    }
    Ok(())
}

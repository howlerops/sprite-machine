//! Which role each thread route needs and which action it audits (plan §4.2 "Auth").
//!
//! Reads (`GET`, subscribe) need `viewer`. Sending a prompt, resolving an approval and cancelling
//! need `member`, so a viewer gets `403 forbidden`. Removing a queued task (`DELETE .../pending/
//! {task_id}`, client-api §5.3) needs `member`, like cancel. Every mutation names an audit action;
//! reads name none.

use crate::db::Role;

/// What a route requires and records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// The thread the route names; empty for `POST /v1/threads`, whose id is derived later.
    pub thread_id: String,
    pub role: Role,
    /// The audit action for a mutation; `None` for reads.
    pub action: Option<&'static str>,
    /// Extra identifier worth recording (`interrupt_id`, `task_id`) as `(key, value)`.
    pub extra: Option<(&'static str, String)>,
}

/// The policy of a thread route, or `None` if the method and path are not a known thread route.
pub fn thread_policy(method: &str, path: &str) -> Option<Policy> {
    if method == "POST" && path.trim_end_matches('/') == "/v1/threads" {
        return Some(Policy {
            thread_id: String::new(),
            role: Role::Member,
            action: Some("thread.create"),
            extra: None,
        });
    }
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let (dev, rest) = match segments.as_slice() {
        ["v1", "threads", rest @ ..] => (false, rest),
        ["v1", "dev", "threads", rest @ ..] => (true, rest),
        _ => return None,
    };
    let (thread_id, tail) = rest.split_first()?;
    let thread_id = (*thread_id).to_owned();
    let make = |role, action, extra| {
        Some(Policy {
            thread_id: thread_id.clone(),
            role,
            action,
            extra,
        })
    };
    match (dev, method, tail) {
        (false, "GET", []) | (false, "GET", ["subscribe"]) | (false, "GET", ["pending"]) => {
            make(Role::Viewer, None, None)
        }
        // Minting a ticket is a read-class action (it only opens a subscribe); the ticket is
        // never recorded, so there is no audit action.
        (false, "POST", ["subscribe-ticket"]) => make(Role::Viewer, None, None),
        (false, "PATCH", []) => make(Role::Member, Some("thread.update"), None),
        (false, "DELETE", []) => make(Role::Admin, Some("thread.delete"), None),
        (false, "POST", ["prompts"]) => make(Role::Member, Some("thread.prompt"), None),
        (false, "POST", ["cancel"]) => make(Role::Member, Some("thread.cancel"), None),
        (false, "POST", ["session"]) => make(Role::Member, Some("thread.session"), None),
        (false, "POST", ["interrupts", interrupt_id]) => make(
            Role::Member,
            Some("thread.approval.resolve"),
            Some(("interrupt_id", (*interrupt_id).to_owned())),
        ),
        (false, "DELETE", ["pending", task_id]) => make(
            Role::Member,
            Some("thread.pending.delete"),
            Some(("task_id", (*task_id).to_owned())),
        ),
        (true, "POST", ["events"]) => make(Role::Member, Some("thread.dev_event.append"), None),
        _ => None,
    }
}

/// What an org-level members route requires and records (M2-003b). Every route is scoped to the
/// caller's own org (the org of the session), so a non-member has no session and gets `401`, and
/// a member of another org cannot name this org's rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgPolicy {
    pub role: Role,
    /// The audit action for a mutation; `None` for reads.
    pub action: Option<&'static str>,
    /// The `{user_id}` or `{invite_id}` the path names.
    pub target: Option<String>,
}

/// The policy of a members or invites route, or `None` if it is not one (fail closed: `404`).
pub fn member_policy(method: &str, path: &str) -> Option<OrgPolicy> {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let make = |role, action, target: Option<&str>| {
        Some(OrgPolicy {
            role,
            action,
            target: target.map(str::to_owned),
        })
    };
    match (method, segments.as_slice()) {
        ("GET", ["v1", "members"]) => make(Role::Viewer, None, None),
        ("PATCH", ["v1", "members", user_id]) => {
            make(Role::Admin, Some("member.role"), Some(user_id))
        }
        ("DELETE", ["v1", "members", user_id]) => {
            make(Role::Admin, Some("member.remove"), Some(user_id))
        }
        ("GET", ["v1", "invites"]) => make(Role::Admin, None, None),
        ("POST", ["v1", "invites"]) => make(Role::Admin, Some("member.invite"), None),
        ("DELETE", ["v1", "invites", invite_id]) => {
            make(Role::Admin, Some("member.invite.revoke"), Some(invite_id))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_routes_need_their_roles_and_audit_their_mutations() {
        let p = member_policy("GET", "/v1/members").expect("policy");
        assert_eq!((p.role, p.action), (Role::Viewer, None));
        let p = member_policy("POST", "/v1/invites").expect("policy");
        assert_eq!((p.role, p.action), (Role::Admin, Some("member.invite")));
        let p = member_policy("DELETE", "/v1/invites/inv_1").expect("policy");
        assert_eq!(p.action, Some("member.invite.revoke"));
        assert_eq!(p.target.as_deref(), Some("inv_1"));
        let p = member_policy("PATCH", "/v1/members/usr_1").expect("policy");
        assert_eq!((p.role, p.action), (Role::Admin, Some("member.role")));
        let p = member_policy("DELETE", "/v1/members/usr_1").expect("policy");
        assert_eq!(p.action, Some("member.remove"));
        assert!(member_policy("GET", "/v1/invites").expect("policy").role == Role::Admin);
        assert!(member_policy("PUT", "/v1/members").is_none());
        assert!(member_policy("POST", "/v1/members").is_none());
        assert!(member_policy("GET", "/v1/members/usr_1/x").is_none());
    }

    #[test]
    fn routes_map_to_roles_and_actions() {
        let p = thread_policy("POST", "/v1/threads/t1/prompts").expect("policy");
        assert_eq!((p.role, p.action), (Role::Member, Some("thread.prompt")));
        let p = thread_policy("GET", "/v1/threads/t1/subscribe").expect("policy");
        assert_eq!((p.role, p.action), (Role::Viewer, None));
        let p = thread_policy("POST", "/v1/threads/t1/subscribe-ticket").expect("policy");
        assert_eq!((p.role, p.action), (Role::Viewer, None));
        let p = thread_policy("DELETE", "/v1/threads/t1/pending/k1").expect("policy");
        assert_eq!(
            (p.role, p.action),
            (Role::Member, Some("thread.pending.delete"))
        );
        assert_eq!(p.extra, Some(("task_id", "k1".to_owned())));
        let p = thread_policy("POST", "/v1/threads/t1/interrupts/i1").expect("policy");
        assert_eq!(p.action, Some("thread.approval.resolve"));
        let p = thread_policy("POST", "/v1/dev/threads/t1/events").expect("policy");
        assert_eq!(p.thread_id, "t1");
        let p = thread_policy("POST", "/v1/threads").expect("policy");
        assert_eq!((p.role, p.action), (Role::Member, Some("thread.create")));
        assert!(p.thread_id.is_empty());
    }

    #[test]
    fn rename_is_member_and_delete_is_admin_and_both_are_audited() {
        let p = thread_policy("PATCH", "/v1/threads/t1").expect("policy");
        assert_eq!((p.role, p.action), (Role::Member, Some("thread.update")));
        assert_eq!(p.thread_id, "t1");
        let p = thread_policy("DELETE", "/v1/threads/t1").expect("policy");
        assert_eq!((p.role, p.action), (Role::Admin, Some("thread.delete")));
        // A viewer may read the thread but not rename or delete it.
        assert_eq!(
            thread_policy("GET", "/v1/threads/t1").expect("policy").role,
            Role::Viewer
        );
    }

    #[test]
    fn unknown_routes_have_no_policy() {
        assert!(thread_policy("PUT", "/v1/threads/t1/prompts").is_none());
        assert!(thread_policy("GET", "/v1/dev/threads/t1/events").is_none());
        assert!(thread_policy("GET", "/v1/other").is_none());
        assert!(thread_policy("GET", "/v1/threads").is_none());
        assert!(thread_policy("DELETE", "/v1/threads").is_none());
    }
}

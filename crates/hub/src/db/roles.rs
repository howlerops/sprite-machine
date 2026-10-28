//! Org roles (plan §4.2 "Auth"): owner, admin, member, viewer. The `members.role` CHECK in
//! migration 0002 uses the same four strings.

use std::fmt;
use std::str::FromStr;

/// A member's role in one org. Ordered from most to least privileged by [`Role::rank`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    Viewer,
    Member,
    Admin,
    Owner,
}

/// A role string that is not one of the four known roles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownRole(pub String);

impl fmt::Display for UnknownRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown role {:?}", self.0)
    }
}

impl std::error::Error for UnknownRole {}

impl Role {
    /// The stored string (matches the `members.role` CHECK).
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Member => "member",
            Role::Viewer => "viewer",
        }
    }

    /// Higher is more privileged: owner 3, admin 2, member 1, viewer 0.
    pub fn rank(self) -> u8 {
        match self {
            Role::Owner => 3,
            Role::Admin => 2,
            Role::Member => 1,
            Role::Viewer => 0,
        }
    }

    /// Whether this role may do something that needs `required` (at least that privileged).
    pub fn allows(self, required: Role) -> bool {
        self.rank() >= required.rank()
    }
}

impl FromStr for Role {
    type Err = UnknownRole;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "owner" => Ok(Role::Owner),
            "admin" => Ok(Role::Admin),
            "member" => Ok(Role::Member),
            "viewer" => Ok(Role::Viewer),
            other => Err(UnknownRole(other.to_owned())),
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_round_trip_through_their_stored_strings() {
        for role in [Role::Owner, Role::Admin, Role::Member, Role::Viewer] {
            assert_eq!(role.as_str().parse::<Role>(), Ok(role));
        }
        assert_eq!(
            "superuser".parse::<Role>(),
            Err(UnknownRole("superuser".into()))
        );
    }

    #[test]
    fn allows_is_a_privilege_order() {
        assert!(Role::Owner.allows(Role::Viewer));
        assert!(Role::Admin.allows(Role::Admin));
        assert!(!Role::Member.allows(Role::Admin));
        assert!(!Role::Viewer.allows(Role::Member));
    }
}

//! The permission model: every route declares one [`Permission`], evaluated against
//! the principal's effective roles before the handler runs.

use openporch_store::{AppRole, OrgRole, Roles};

/// Organization-level permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrgPerm {
    /// List members and environment types: any member.
    ReadMembers,
    /// Read everything, including the audit log: administrators and org viewers.
    ReadAll,
    /// Invite, create and remove non-administrators: administrators and managers.
    ManageMembers,
    /// Create applications: administrators and managers.
    CreateApp,
    /// Manage resource types, classes, definitions, drivers, cloud accounts, secret
    /// stores, environment types and their roles: administrators only.
    Administer,
}

/// Application-level permissions, checked against the `{app}` path parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppPerm {
    /// Read the application: viewers and above, and org viewers.
    Read,
    /// Edit sets, deltas, values and environments: developers and owners.
    Configure,
    /// Manage application users and delete the application: owners.
    Manage,
    /// Deploy to, create or delete the `{env}` environment: developers and owners
    /// who are also `deployer` on its environment type.
    Deploy,
}

/// The permission a route requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    /// No credential needed: health, version and login.
    Public,
    /// Any authenticated member; the handler applies object-level checks.
    Authenticated,
    /// An organization permission.
    Org(OrgPerm),
    /// An application permission.
    App(AppPerm),
}

/// The outcome of a permission check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Allowed.
    Allow,
    /// `403` with the reason.
    Deny(String),
    /// `404`: the principal may not see the object at all.
    Hidden,
}

/// The scope a permission is evaluated in.
#[derive(Debug, Clone, Default)]
pub struct Scope<'a> {
    /// The `{app}` path parameter.
    pub app: Option<&'a str>,
    /// The environment type of the `{env}` path parameter.
    pub env_type: Option<&'a str>,
}

fn is_admin(roles: &Roles) -> bool {
    roles.org == Some(OrgRole::Administrator)
}

/// Whether the principal may see application `app` at all.
pub fn can_see_app(roles: &Roles, app: &str) -> bool {
    is_admin(roles) || roles.org == Some(OrgRole::OrgViewer) || roles.apps.contains_key(app)
}

/// Evaluates `perm` for a principal with `roles`.
pub fn evaluate(perm: Permission, roles: &Roles, scope: &Scope<'_>) -> Decision {
    let Some(org_role) = roles.org else {
        return Decision::Deny("not a member of the organization".into());
    };
    if org_role == OrgRole::Administrator {
        return Decision::Allow;
    }
    let deny = |what: &str| Decision::Deny(format!("requires {what}"));
    match perm {
        Permission::Public | Permission::Authenticated => Decision::Allow,
        Permission::Org(p) => match (p, org_role) {
            (OrgPerm::ReadMembers, _) => Decision::Allow,
            (OrgPerm::ReadAll, OrgRole::OrgViewer) => Decision::Allow,
            (OrgPerm::ReadAll, _) => deny("the `administrator` or `orgViewer` role"),
            (OrgPerm::ManageMembers | OrgPerm::CreateApp, OrgRole::Manager) => Decision::Allow,
            (OrgPerm::ManageMembers | OrgPerm::CreateApp, _) => {
                deny("the `administrator` or `manager` role")
            }
            (OrgPerm::Administer, _) => deny("the `administrator` role"),
        },
        Permission::App(p) => {
            let Some(app) = scope.app else {
                return Decision::Deny("no application in scope".into());
            };
            if !can_see_app(roles, app) {
                return Decision::Hidden;
            }
            let role = roles.apps.get(app).copied();
            match p {
                AppPerm::Read => Decision::Allow,
                AppPerm::Configure if role >= Some(AppRole::Developer) => Decision::Allow,
                AppPerm::Configure => deny(&format!("`developer` or `owner` on `{app}`")),
                AppPerm::Manage if role == Some(AppRole::Owner) => Decision::Allow,
                AppPerm::Manage => deny(&format!("`owner` on `{app}`")),
                AppPerm::Deploy => {
                    if role < Some(AppRole::Developer) {
                        return deny(&format!("`developer` or `owner` on `{app}`"));
                    }
                    match scope.env_type {
                        Some(t) if roles.deployer.contains(t) => Decision::Allow,
                        Some(t) => Decision::Deny(format!(
                            "requires the `deployer` role on environment type `{t}`"
                        )),
                        None => Decision::Deny("no environment in scope".into()),
                    }
                }
            }
        }
    }
}

pub use openporch_store::covers;

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(org: OrgRole, apps: &[(&str, AppRole)], deployer: &[&str]) -> Roles {
        Roles {
            org: Some(org),
            apps: apps.iter().map(|(a, r)| ((*a).to_owned(), *r)).collect(),
            deployer: deployer.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn app(app: &str) -> Scope<'_> {
        Scope {
            app: Some(app),
            env_type: None,
        }
    }

    use AppPerm::*;
    use Permission::App;

    #[test]
    fn union_of_roles() {
        // bob: member, viewer on shop, developer on billing.
        let bob = roles(
            OrgRole::Member,
            &[("shop", AppRole::Viewer), ("billing", AppRole::Developer)],
            &[],
        );
        assert_eq!(evaluate(App(Read), &bob, &app("shop")), Decision::Allow);
        assert!(matches!(
            evaluate(App(Configure), &bob, &app("shop")),
            Decision::Deny(_)
        ));
        assert_eq!(
            evaluate(App(Configure), &bob, &app("billing")),
            Decision::Allow
        );
        assert_eq!(evaluate(App(Read), &bob, &app("other")), Decision::Hidden);
    }

    #[test]
    fn organization_permissions() {
        let admin = roles(OrgRole::Administrator, &[], &[]);
        let manager = roles(OrgRole::Manager, &[], &[]);
        let viewer = roles(OrgRole::OrgViewer, &[], &[]);
        let member = roles(OrgRole::Member, &[], &[]);
        let s = Scope::default();
        for p in [
            OrgPerm::ReadMembers,
            OrgPerm::ReadAll,
            OrgPerm::ManageMembers,
            OrgPerm::CreateApp,
            OrgPerm::Administer,
        ] {
            assert_eq!(evaluate(Permission::Org(p), &admin, &s), Decision::Allow);
        }
        // Managers cannot create definitions.
        assert!(matches!(
            evaluate(Permission::Org(OrgPerm::Administer), &manager, &s),
            Decision::Deny(_)
        ));
        assert_eq!(
            evaluate(Permission::Org(OrgPerm::CreateApp), &manager, &s),
            Decision::Allow
        );
        assert_eq!(
            evaluate(Permission::Org(OrgPerm::ReadAll), &viewer, &s),
            Decision::Allow
        );
        assert_eq!(evaluate(App(Read), &viewer, &app("any")), Decision::Allow);
        assert!(matches!(
            evaluate(App(Configure), &viewer, &app("any")),
            Decision::Deny(_)
        ));
        assert_eq!(evaluate(App(Read), &member, &app("any")), Decision::Hidden);
        assert!(matches!(
            evaluate(Permission::Org(OrgPerm::ReadAll), &member, &s),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn application_permissions() {
        let viewer = roles(
            OrgRole::Member,
            &[("shop", AppRole::Viewer)],
            &["development"],
        );
        let developer = roles(OrgRole::Member, &[("shop", AppRole::Developer)], &[]);
        // A developer without an administrator role cannot manage definitions.
        assert!(matches!(
            evaluate(
                Permission::Org(OrgPerm::Administer),
                &developer,
                &Scope::default()
            ),
            Decision::Deny(_)
        ));
        let dev_env = Scope {
            app: Some("shop"),
            env_type: Some("development"),
        };
        // A viewer cannot deploy, even with deployer on the type.
        assert!(matches!(
            evaluate(App(Deploy), &viewer, &dev_env),
            Decision::Deny(_)
        ));
        // A viewer cannot delete the application.
        assert!(matches!(
            evaluate(App(Manage), &viewer, &app("shop")),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn deployer_gate_names_the_missing_role() {
        let developer = roles(
            OrgRole::Member,
            &[("shop", AppRole::Developer)],
            &["development"],
        );
        let prod = Scope {
            app: Some("shop"),
            env_type: Some("production"),
        };
        assert_eq!(
            evaluate(App(Deploy), &developer, &prod),
            Decision::Deny("requires the `deployer` role on environment type `production`".into())
        );
        let dev = Scope {
            app: Some("shop"),
            env_type: Some("development"),
        };
        assert_eq!(evaluate(App(Deploy), &developer, &dev), Decision::Allow);
    }

    #[test]
    fn delegation_never_increases_authority() {
        let manager = roles(
            OrgRole::Manager,
            &[("shop", AppRole::Developer)],
            &["development"],
        );
        let ci_dev = roles(
            OrgRole::Member,
            &[("shop", AppRole::Developer)],
            &["development"],
        );
        let ci_prod = roles(
            OrgRole::Member,
            &[("shop", AppRole::Developer)],
            &["production"],
        );
        let admin = roles(OrgRole::Administrator, &[], &[]);
        assert!(covers(&manager, &ci_dev));
        assert!(!covers(&manager, &ci_prod));
        assert!(!covers(&manager, &admin));
        assert!(covers(&admin, &ci_prod));
        let owner_target = roles(OrgRole::Member, &[("shop", AppRole::Owner)], &[]);
        assert!(!covers(&manager, &owner_target));
        let viewer = roles(OrgRole::OrgViewer, &[], &[]);
        assert!(!covers(&manager, &viewer));
    }
}

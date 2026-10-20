//! First-release acceptance suites that walk the whole API rather than one
//! feature.

use super::*;
use crate::rbac::OrgPerm;

/// A probing principal: its organization role and its role on `shop`.
struct Prober {
    id: &'static str,
    org: &'static str,
    shop: Option<&'static str>,
}

const PROBERS: [Prober; 5] = [
    Prober {
        id: "outsider",
        org: "member",
        shop: None,
    },
    Prober {
        id: "shop-viewer",
        org: "member",
        shop: Some("viewer"),
    },
    Prober {
        id: "shop-developer",
        org: "member",
        shop: Some("developer"),
    },
    Prober {
        id: "manager",
        org: "manager",
        shop: None,
    },
    Prober {
        id: "org-viewer",
        org: "orgViewer",
        shop: None,
    },
];

/// The status `p` must get on a route requiring `perm`, written out from the
/// `rbac` spec rather than derived from the code under test, or `None` when
/// the route allows it. No prober is `deployer` anywhere.
fn expected(p: &Prober, perm: Permission, has_app: bool) -> Option<StatusCode> {
    let forbidden = Some(StatusCode::FORBIDDEN);
    match perm {
        Permission::Public | Permission::Authenticated => None,
        Permission::Org(OrgPerm::ReadMembers) => None,
        Permission::Org(OrgPerm::ReadAll) => {
            (p.org != "orgViewer").then_some(StatusCode::FORBIDDEN)
        }
        Permission::Org(OrgPerm::ManageMembers | OrgPerm::CreateApp) => {
            (p.org != "manager").then_some(StatusCode::FORBIDDEN)
        }
        Permission::Org(OrgPerm::Administer) => forbidden,
        Permission::App(_) if !has_app => forbidden,
        Permission::App(perm) => {
            if p.shop.is_none() && p.org != "orgViewer" {
                // Applications a member may not see do not exist for them.
                return Some(StatusCode::NOT_FOUND);
            }
            match (perm, p.shop) {
                (AppPerm::Read, _) => None,
                (AppPerm::Configure, Some("developer")) => None,
                _ => forbidden,
            }
        }
    }
}

/// Every route that needs a permission, called by every member who lacks it,
/// is refused with `403`, or `404` for an application they may not see, and
/// changes nothing: the only audit entries the probers leave are denials.
#[tokio::test]
async fn every_route_refuses_members_without_its_permission() {
    let s = server().await;
    s.app("shop").await;
    let (status, b) = s
        .post(
            "/orgs/acme/apps/shop/envs",
            &s.admin,
            json!({"id": "dev", "type": "development"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    let mut tokens = Vec::new();
    for p in &PROBERS {
        let token = s.member(p.id, p.org).await;
        if let Some(role) = p.shop {
            s.grant_app("shop", p.id, role).await;
        }
        tokens.push(token);
    }
    let (_, before) = s.get("/orgs/acme/apps/shop", &s.admin).await;

    let mut probed = 0;
    for spec in &s.specs {
        let has_app = spec.path.contains("{app}");
        let path = spec
            .path
            .split('/')
            .map(|seg| match seg {
                "{org}" => "acme",
                "{app}" => "shop",
                "{env}" => "dev",
                s if s.starts_with('{') => "x",
                s => s,
            })
            .collect::<Vec<_>>()
            .join("/");
        let body = (spec.method != Method::GET && spec.method != Method::DELETE).then(|| json!({}));
        for (p, token) in PROBERS.iter().zip(&tokens) {
            let Some(want) = expected(p, spec.permission, has_app) else {
                continue;
            };
            let (status, b) = s
                .call(spec.method.clone(), &path, Some(token), body.clone())
                .await;
            let code = if want == StatusCode::NOT_FOUND {
                "API-001"
            } else {
                "AUTH-002"
            };
            assert_eq!(
                (status, b["error"].as_str()),
                (want, Some(code)),
                "{} {} as {}: {b}",
                spec.method,
                spec.path,
                p.id
            );
            probed += 1;
        }
    }
    // The walk covers the administrator-only surface for every prober.
    assert!(probed > 5 * 20, "only {probed} refusals were probed");

    // Nothing changed.
    assert_eq!(s.get("/orgs/acme/apps/shop", &s.admin).await.1, before);
    for p in &PROBERS {
        let (status, page) = s
            .get(
                &format!("/orgs/acme/audit-logs?user_id={}&limit=1000", p.id),
                &s.admin,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let entries = page["entries"].as_array().unwrap();
        assert!(!entries.is_empty(), "{} left no denials", p.id);
        for e in entries {
            assert_eq!(e["outcome"], "denied", "{}: {e}", p.id);
        }
    }
}

/// `docs/acceptance.md` lists every scenario of every spec with a known status,
/// and every test it names exists.
#[test]
fn the_acceptance_matrix_lists_every_scenario() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let matrix = std::fs::read_to_string(root.join("docs/acceptance.md")).unwrap();
    let specs = root.join("openspec/changes/initial-platform-orchestrator-spec/specs");
    let mut missing = Vec::new();
    let mut names: Vec<_> = std::fs::read_dir(&specs)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    for name in names {
        let spec = std::fs::read_to_string(specs.join(&name).join("spec.md")).unwrap();
        let heading = format!("\n## {name}\n");
        let Some(start) = matrix.find(&heading) else {
            missing.push(format!("section {name}"));
            continue;
        };
        let section = &matrix[start + heading.len()..];
        let section = &section[..section.find("\n## ").unwrap_or(section.len())];
        for scenario in spec
            .lines()
            .filter_map(|l| l.strip_prefix("#### Scenario: "))
        {
            let listed = ["covered", "manual", "deferred", "gap"]
                .iter()
                .any(|status| section.contains(&format!("| {} | {status}", scenario.trim())));
            if !listed {
                missing.push(format!("{name}: {scenario}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "not in docs/acceptance.md: {missing:#?}"
    );

    let mut stale = Vec::new();
    for cell in matrix.split('`') {
        let Some((path, test)) = cell.split_once(".rs::") else {
            continue;
        };
        if !path.starts_with("crates/")
            || !test.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            continue;
        }
        let file = std::fs::read_to_string(root.join(format!("{path}.rs"))).unwrap_or_default();
        if !file.contains(&format!("fn {test}(")) && !file.contains(&format!("mod {test}")) {
            stale.push(cell.to_owned());
        }
    }
    assert!(
        stale.is_empty(),
        "docs/acceptance.md names missing tests: {stale:#?}"
    );
}

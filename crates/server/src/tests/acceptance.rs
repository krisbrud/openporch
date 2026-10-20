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

    // Every test the matrix names, in Rust, Go or OpenTofu, exists, and every
    // file it names is there.
    let go_tests: String = files_under(&root.join("provider"), "_test.go")
        .iter()
        .map(|f| std::fs::read_to_string(f).unwrap())
        .collect();
    let mut stale = Vec::new();
    for cell in matrix.split('`').skip(1).step_by(2) {
        let exists = if let Some((path, test)) = cell.split_once("::")
            && [".rs", ".go", ".hcl"].iter().any(|ext| path.ends_with(ext))
        {
            let file = std::fs::read_to_string(root.join(path)).unwrap_or_default();
            let declared = match path.rsplit('.').next() {
                Some("rs") => vec![format!("fn {test}("), format!("mod {test} ")],
                Some("go") => vec![format!("func {test}(")],
                _ => vec![format!("run \"{test}\"")],
            };
            declared.iter().any(|d| file.contains(d.as_str()))
        } else if cell.starts_with("::") {
            false
        } else if ["crates/", "provider/", "examples/", "scripts/"]
            .iter()
            .any(|dir| cell.starts_with(dir))
            && !cell.contains('*')
            && !cell.contains(' ')
        {
            root.join(cell).exists()
        } else if cell.starts_with("Test")
            && cell.len() > 4
            && cell.chars().all(|c| c.is_ascii_alphanumeric())
        {
            go_tests.contains(&format!("func {cell}("))
        } else {
            true
        };
        if !exists {
            stale.push(cell.to_owned());
        }
    }
    assert!(
        stale.is_empty(),
        "docs/acceptance.md names missing tests or files: {stale:#?}"
    );
}

/// The permission `docs/api.md` documents for a route, as the permissions a
/// route documented that way may declare. The reference is written for
/// readers, so a route's declared permission is checked against it rather than
/// against itself: a route weakened in code no longer matches its documentation.
fn documented(phrase: &str) -> Option<&'static [Permission]> {
    use Permission::*;
    let phrase = phrase.trim();
    Some(match phrase {
        "public" => &[Public],
        "authenticated" => &[Authenticated],
        "org read" => &[Org(OrgPerm::ReadMembers), Authenticated],
        "org read all" => &[Org(OrgPerm::ReadAll)],
        "manage members" => &[Org(OrgPerm::ManageMembers)],
        "create apps" => &[Org(OrgPerm::CreateApp)],
        "administer" | "administrator" | "org administer" => &[Org(OrgPerm::Administer)],
        "app read" => &[App(AppPerm::Read)],
        "app manage" => &[App(AppPerm::Manage)],
        "app deploy" | "deploy to the environment" => &[App(AppPerm::Deploy)],
        p if p.starts_with("app deploy;") => &[App(AppPerm::Deploy)],
        // Configuring and also needing `deployer` is checked by the handler.
        p if p.starts_with("app configure") => &[App(AppPerm::Configure)],
        _ => return None,
    })
}

/// Every route's declared permission is the one `docs/api.md` documents, and
/// every route is documented.
#[tokio::test]
async fn every_route_declares_the_documented_permission() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let api = std::fs::read_to_string(root.join("docs/api.md")).unwrap();
    let mut docs = std::collections::BTreeMap::new();
    for line in api.lines() {
        let cells: Vec<_> = line.split('|').map(str::trim).collect();
        let [_, method, path, phrase, _] = cells[..] else {
            continue;
        };
        let Ok(method) = method.parse::<Method>() else {
            continue;
        };
        let Some(paths) = path.strip_prefix('`').and_then(|p| p.strip_suffix('`')) else {
            continue;
        };
        // One row can document several paths.
        for path in paths.split("`, `") {
            docs.insert((method.to_string(), path.to_owned()), phrase.to_owned());
        }
    }
    let specs = server().await.specs;
    let mut wrong = Vec::new();
    for spec in &specs {
        let key = (spec.method.to_string(), spec.path.to_owned());
        let Some(phrase) = docs.get(&key) else {
            wrong.push(format!("{} {} is not in docs/api.md", key.0, key.1));
            continue;
        };
        // `as POST` documents a route like the POST of the same path's parent.
        let phrase = if phrase == "as POST" {
            let parent = &spec.path[..spec.path.rfind('/').unwrap()];
            docs.get(&("POST".to_owned(), parent.to_owned()))
                .cloned()
                .unwrap_or_default()
        } else {
            phrase.clone()
        };
        match documented(&phrase) {
            Some(allowed) if allowed.contains(&spec.permission) => {}
            Some(_) => wrong.push(format!(
                "{} {} declares {:?}, documented as `{phrase}`",
                key.0, key.1, spec.permission
            )),
            None => wrong.push(format!(
                "{} {}: unknown permission `{phrase}`",
                key.0, key.1
            )),
        }
    }
    for (method, path) in docs.keys() {
        if !specs
            .iter()
            .any(|s| s.method.as_str() == method && s.path == path)
        {
            wrong.push(format!("{method} {path} is documented but not served"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// The files under `dir` whose names end with `suffix`.
fn files_under(dir: &std::path::Path, suffix: &str) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files_under(&path, suffix));
        } else if path.to_string_lossy().ends_with(suffix) {
            found.push(path);
        }
    }
    found
}

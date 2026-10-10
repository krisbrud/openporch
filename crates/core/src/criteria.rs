//! Matching resource definitions to a resource context.
//!
//! Each criterion matches when every field it sets equals the context. Its weight
//! is the sum of `env_type` 1, `app_id` 2, `env_id` 4, `res_id` 8 and `class` 16.
//! A criterion without `class` matches only the `default` class. The definition
//! with the heaviest matching criterion wins, and a tie between definitions is an error.

use crate::ids::DefinitionId;
use crate::model::{Criterion, ResourceContext, ResourceDefinition};

/// Why no single definition could be selected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MatchError {
    /// No definition of the type matched the context.
    #[error(
        "no resource definition matches type `{}`, class `{}`, resource ID `{}` \
         in app `{}`, env `{}` (env type `{}`)",
        .0.res.ty, .0.res.class, .0.res.id, .0.app, .0.env, .0.env_type
    )]
    NoMatch(Box<ResourceContext>),
    /// Several definitions matched with the same weight.
    #[error(
        "ambiguous match for `{descriptor}`: definitions {} match with equal weight {weight}",
        .definitions.iter().map(|d| format!("`{d}`")).collect::<Vec<_>>().join(" and ")
    )]
    Ambiguous {
        /// The resource being matched.
        descriptor: String,
        /// The tied definitions, sorted.
        definitions: Vec<DefinitionId>,
        /// Their weight.
        weight: u32,
    },
}

/// Returns the weight of `criterion` if it matches `ctx`.
pub fn criterion_weight(criterion: &Criterion, ctx: &ResourceContext) -> Option<u32> {
    let mut weight = 0;
    if let Some(v) = &criterion.env_type {
        if *v != ctx.env_type {
            return None;
        }
        weight += 1;
    }
    if let Some(v) = &criterion.app_id {
        if *v != ctx.app {
            return None;
        }
        weight += 2;
    }
    if let Some(v) = &criterion.env_id {
        if *v != ctx.env {
            return None;
        }
        weight += 4;
    }
    if let Some(v) = &criterion.res_id {
        if *v != ctx.res.id {
            return None;
        }
        weight += 8;
    }
    match &criterion.class {
        Some(v) if *v == ctx.res.class => weight += 16,
        Some(_) => return None,
        None if ctx.res.class.is_default() => {}
        None => return None,
    }
    Some(weight)
}

/// Returns the weight of the best matching criterion of `def` for `ctx`, if any.
pub fn definition_weight(def: &ResourceDefinition, ctx: &ResourceContext) -> Option<u32> {
    if def.ty != ctx.res.ty {
        return None;
    }
    def.criteria
        .iter()
        .filter_map(|c| criterion_weight(c, ctx))
        .max()
}

/// Selects the definition for `ctx` among `defs`.
pub fn select_definition<'a>(
    defs: impl IntoIterator<Item = &'a ResourceDefinition>,
    ctx: &ResourceContext,
) -> Result<&'a ResourceDefinition, MatchError> {
    let mut best: Vec<&ResourceDefinition> = Vec::new();
    let mut best_weight = 0;
    for def in defs {
        let Some(w) = definition_weight(def, ctx) else {
            continue;
        };
        if best.is_empty() || w > best_weight {
            best = vec![def];
            best_weight = w;
        } else if w == best_weight {
            best.push(def);
        }
    }
    match best.len() {
        0 => Err(MatchError::NoMatch(Box::new(ctx.clone()))),
        1 => Ok(best[0]),
        _ => {
            let mut definitions: Vec<DefinitionId> = best.iter().map(|d| d.id.clone()).collect();
            definitions.sort();
            Err(MatchError::Ambiguous {
                descriptor: ctx.res.to_string(),
                definitions,
                weight: best_weight,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Descriptor;

    fn ctx(app: &str, env: &str, env_type: &str, res: &str) -> ResourceContext {
        ResourceContext {
            org: "acme".parse().unwrap(),
            app: app.parse().unwrap(),
            env: env.parse().unwrap(),
            env_type: env_type.parse().unwrap(),
            res: Descriptor::parse(res).unwrap(),
        }
    }

    fn def(id: &str, ty: &str, criteria: serde_json::Value) -> ResourceDefinition {
        serde_json::from_value(serde_json::json!({
            "id": id, "type": ty, "driver_type": "openporch/echo", "criteria": criteria
        }))
        .unwrap()
    }

    #[test]
    fn more_specific_criterion_wins() {
        let defs = [
            def(
                "random-dns",
                "dns",
                serde_json::json!([{"env_type": "development"}]),
            ),
            def(
                "static-dev-dns",
                "dns",
                serde_json::json!([{"app_id": "shop", "env_id": "dev"}]),
            ),
        ];
        let c = ctx("shop", "dev", "development", "dns.default#shared.api-dns");
        assert_eq!(
            select_definition(&defs, &c).unwrap().id.as_str(),
            "static-dev-dns"
        );
        // Another app only matches the generic definition.
        let c = ctx("blog", "dev", "development", "dns.default#shared.api-dns");
        assert_eq!(
            select_definition(&defs, &c).unwrap().id.as_str(),
            "random-dns"
        );
    }

    #[test]
    fn no_match_names_type_class_id_and_context() {
        let defs = [def(
            "dev-pg",
            "postgres",
            serde_json::json!([{"env_type": "development"}]),
        )];
        let c = ctx(
            "shop",
            "prod",
            "production",
            "postgres.default#modules.web.externals.db",
        );
        let err = select_definition(&defs, &c).unwrap_err().to_string();
        for part in [
            "postgres",
            "default",
            "modules.web.externals.db",
            "shop",
            "prod",
            "production",
        ] {
            assert!(err.contains(part), "{err} lacks {part}");
        }
    }

    #[test]
    fn definition_without_criteria_never_matches() {
        let defs = [def("none", "s3", serde_json::json!([]))];
        let c = ctx("shop", "dev", "development", "s3.default#shared.b");
        assert!(matches!(
            select_definition(&defs, &c),
            Err(MatchError::NoMatch(_))
        ));
    }

    #[test]
    fn ties_are_ambiguous_and_name_both() {
        let defs = [
            def(
                "s3-b",
                "s3",
                serde_json::json!([{"env_type": "development"}]),
            ),
            def(
                "s3-a",
                "s3",
                serde_json::json!([{"app_id": "shop"}, {"env_type": "development"}]),
            ),
            def("s3-c", "s3", serde_json::json!([{"app_id": "other"}])),
        ];
        // s3-a's best criterion (app_id, 2) beats s3-b (env_type, 1).
        let c = ctx("shop", "dev", "development", "s3.default#shared.b");
        assert_eq!(select_definition(&defs, &c).unwrap().id.as_str(), "s3-a");

        let defs = [
            def(
                "s3-b",
                "s3",
                serde_json::json!([{"env_type": "development"}]),
            ),
            def(
                "s3-a",
                "s3",
                serde_json::json!([{"env_type": "development"}]),
            ),
        ];
        let err = select_definition(&defs, &c).unwrap_err();
        assert!(matches!(&err, MatchError::Ambiguous { weight: 1, .. }));
        let msg = err.to_string();
        assert!(msg.contains("`s3-a` and `s3-b`"), "{msg}");
    }

    #[test]
    fn weights() {
        let c = ctx("shop", "dev", "development", "s3.large#shared.b");
        let all: Criterion = serde_json::from_value(serde_json::json!({
            "env_type": "development", "app_id": "shop", "env_id": "dev",
            "res_id": "shared.b", "class": "large"
        }))
        .unwrap();
        assert_eq!(criterion_weight(&all, &c), Some(31));
    }

    #[test]
    fn class_must_match_and_absent_class_means_default() {
        let large = ctx("shop", "dev", "development", "s3.large#shared.b");
        let default = ctx("shop", "dev", "development", "s3.default#shared.b");
        let no_class = Criterion::default();
        let with_large = Criterion {
            class: Some("large".parse().unwrap()),
            ..Default::default()
        };
        assert_eq!(criterion_weight(&no_class, &default), Some(0));
        assert_eq!(criterion_weight(&no_class, &large), None);
        assert_eq!(criterion_weight(&with_large, &large), Some(16));
        assert_eq!(criterion_weight(&with_large, &default), None);
    }

    #[test]
    fn other_types_are_ignored() {
        let defs = [def("pg", "postgres", serde_json::json!([{}]))];
        let c = ctx("shop", "dev", "development", "s3.default#shared.b");
        assert!(select_definition(&defs, &c).is_err());
    }
}

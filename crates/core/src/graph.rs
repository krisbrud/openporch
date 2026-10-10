//! Resource graph construction, co-provisioning, cycle detection and ordering.
//!
//! Nodes come from workloads, their dependencies, shared resources, the implicit
//! `base-env`, resource references in matched definitions, and co-provisioned
//! resources. An edge `a -> b` means `a` depends on `b` and is provisioned after it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;
use serde_json::Value;

use crate::catalog::{ClassRegistry, ResourceType, ResourceUse};
use crate::criteria::{MatchError, select_definition};
use crate::ids::{
    AppId, ClassId, DefinitionId, Descriptor, EnvId, EnvTypeId, OrgId, ResId, ResourceTypeId,
};
use crate::model::{ResourceContext, ResourceDefinition};
use crate::placeholder::{Dialect, Expr, PlaceholderError, ResourceRef, collect_placeholders};
use crate::sets::DeploymentSet;

/// Everything graph construction needs.
#[derive(Debug, Clone, Copy)]
pub struct GraphInput<'a> {
    /// Organization.
    pub org: &'a OrgId,
    /// Application.
    pub app: &'a AppId,
    /// Environment.
    pub env: &'a EnvId,
    /// Environment type.
    pub env_type: &'a EnvTypeId,
    /// The set being deployed.
    pub set: &'a DeploymentSet,
    /// All resource definitions.
    pub definitions: &'a [ResourceDefinition],
    /// All resource types, built-in and custom.
    pub types: &'a [ResourceType],
    /// Defined resource classes.
    pub classes: &'a ClassRegistry,
}

/// Why a graph could not be built.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum GraphError {
    /// A request names a type that does not exist.
    #[error("{path}: unknown resource type `{ty}`")]
    UnknownType {
        /// Where the type was requested.
        path: String,
        /// The type.
        ty: ResourceTypeId,
    },
    /// A request produces an invalid resource ID.
    #[error("{path}: {source}")]
    InvalidId {
        /// Where the resource was requested.
        path: String,
        /// The ID error.
        source: crate::ids::IdError,
    },
    /// A request names a class that is not defined for its type.
    #[error("{path}: class `{class}` is not defined for resource type `{ty}`")]
    MissingClass {
        /// Where the class was requested.
        path: String,
        /// The type.
        ty: ResourceTypeId,
        /// The missing class.
        class: ClassId,
    },
    /// No single definition matches a node.
    #[error("{path}: {source}")]
    Match {
        /// Where the resource was requested.
        path: String,
        /// The matching error.
        source: MatchError,
    },
    /// A placeholder in a definition is invalid.
    #[error("definition `{definition}`: {source}")]
    Placeholder {
        /// The definition.
        definition: DefinitionId,
        /// The placeholder error.
        source: PlaceholderError,
    },
    /// Two requests for one descriptor disagree on its inputs.
    #[error(
        "conflicting requests for `{descriptor}`: `{first}` and `{second}` ask for different inputs"
    )]
    Conflict {
        /// The resource.
        descriptor: Descriptor,
        /// One request path.
        first: String,
        /// The other request path.
        second: String,
    },
    /// The graph has a cycle.
    #[error("dependency cycle: {}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join(" -> "))]
    Cycle(Vec<Descriptor>),
}

/// Why a node is in the graph.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Request {
    /// Where the request comes from, such as `modules.web.externals.db`.
    pub path: String,
    /// The requested inputs. References carry none and never conflict.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<String>,
}

/// A node of the resource graph.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Node {
    /// The resource.
    pub descriptor: Descriptor,
    /// The matched definition.
    pub definition: DefinitionId,
    /// The definition's driver.
    pub driver_type: String,
    /// The resource's inputs (`params`), when a request supplied them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// The requests that put the node in the graph, sorted.
    pub requests: Vec<Request>,
    /// Resources this one depends on.
    pub depends_on: BTreeSet<Descriptor>,
}

/// A resolved resource graph.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResourceGraph {
    /// Nodes by descriptor.
    pub nodes: BTreeMap<Descriptor, Node>,
    /// Provisioning layers: every node's dependencies are in earlier layers.
    pub layers: Vec<Vec<Descriptor>>,
}

impl ResourceGraph {
    /// Every edge `(from, to)`, meaning `from` depends on `to`.
    pub fn edges(&self) -> Vec<(&Descriptor, &Descriptor)> {
        self.nodes
            .values()
            .flat_map(|n| n.depends_on.iter().map(move |d| (&n.descriptor, d)))
            .collect()
    }

    /// The provisioning order, one node after another.
    pub fn order(&self) -> impl Iterator<Item = &Descriptor> {
        self.layers.iter().flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CoProvision {
    source: Descriptor,
    co: Descriptor,
    match_dependents: bool,
}

struct Builder<'a> {
    input: GraphInput<'a>,
    nodes: BTreeMap<Descriptor, Node>,
    /// The request that fixed each node's params, for conflict messages.
    explicit: BTreeMap<Descriptor, (String, Value)>,
    queue: VecDeque<Descriptor>,
    co: BTreeSet<CoProvision>,
}

/// Builds the resource graph for a deployment.
pub fn build_graph(input: GraphInput<'_>) -> Result<ResourceGraph, GraphError> {
    let mut b = Builder {
        input,
        nodes: BTreeMap::new(),
        explicit: BTreeMap::new(),
        queue: VecDeque::new(),
        co: BTreeSet::new(),
    };
    let base_env = Descriptor::new(
        ty("base-env"),
        ClassId::default(),
        ResId::new("base-env").expect("valid"),
    );
    b.request(&base_env, "base-env", None)?;
    for (wid, module) in &input.set.modules {
        let w = Descriptor::new(ty("workload"), module.class.clone(), ResId::workload(wid));
        let wpath = format!("modules.{wid}");
        b.request(&w, &wpath, None)?;
        for (key, ext) in &module.externals {
            let res_id = match &ext.id {
                Some(id) => ResId::shared(id),
                None => ResId::private(wid, key),
            }
            .map_err(|source| GraphError::InvalidId {
                path: format!("{wpath}.externals.{key}"),
                source,
            })?;
            let dep = Descriptor::new(ext.ty.clone(), ext.class.clone(), res_id);
            b.request(
                &dep,
                &format!("{wpath}.externals.{key}"),
                Some(ext.params.clone().unwrap_or(Value::Null)),
            )?;
            b.edge(&w, &dep);
        }
    }
    for (id, shared) in &input.set.shared {
        let res_id = ResId::shared(id).map_err(|source| GraphError::InvalidId {
            path: format!("shared.{id}"),
            source,
        })?;
        let d = Descriptor::new(shared.ty.clone(), shared.class.clone(), res_id);
        let params = Some(shared.params.clone().unwrap_or(Value::Null));
        b.request(&d, &format!("shared.{id}"), params)?;
    }
    while let Some(d) = b.queue.pop_front() {
        b.expand(&d)?;
    }
    b.match_dependents();
    let layers = topological_layers(&b.nodes)?;
    Ok(ResourceGraph {
        nodes: b.nodes,
        layers,
    })
}

fn ty(s: &str) -> ResourceTypeId {
    ResourceTypeId::new(s).expect("built-in type IDs are valid")
}

impl Builder<'_> {
    fn resource_type(&self, ty: &ResourceTypeId) -> Option<&ResourceType> {
        self.input.types.iter().find(|t| &t.id == ty)
    }

    fn is_implicit(&self, ty: &ResourceTypeId) -> bool {
        self.resource_type(ty)
            .is_some_and(|t| t.use_ == ResourceUse::Implicit)
    }

    /// Adds a request for `d`. Requests that supply inputs (workloads, shared
    /// resources, co-provisioning) pass `Some`, with `null` when they set no params,
    /// and must all agree. Placeholder references pass `None` and never conflict.
    fn request(
        &mut self,
        d: &Descriptor,
        path: &str,
        params: Option<Value>,
    ) -> Result<(), GraphError> {
        if self.resource_type(&d.ty).is_none() {
            return Err(GraphError::UnknownType {
                path: path.to_owned(),
                ty: d.ty.clone(),
            });
        }
        if !self.input.classes.contains(&d.ty, &d.class) {
            return Err(GraphError::MissingClass {
                path: path.to_owned(),
                ty: d.ty.clone(),
                class: d.class.clone(),
            });
        }
        let canonical = params.as_ref().map(crate::canonical::to_canonical_string);
        if let Some(node) = self.nodes.get_mut(d) {
            if let Some(p) = &params {
                match self.explicit.get(d) {
                    Some((first, existing)) if existing != p => {
                        let mut paths = [first.clone(), path.to_owned()];
                        paths.sort();
                        let [first, second] = paths;
                        return Err(GraphError::Conflict {
                            descriptor: d.clone(),
                            first,
                            second,
                        });
                    }
                    Some(_) => {}
                    None => {
                        node.params = Some(p.clone()).filter(|p| !p.is_null());
                        self.explicit
                            .insert(d.clone(), (path.to_owned(), p.clone()));
                    }
                }
            }
            let req = Request {
                path: path.to_owned(),
                params: canonical,
            };
            if let Err(i) = node.requests.binary_search(&req) {
                node.requests.insert(i, req);
            }
            return Ok(());
        }
        let ctx = ResourceContext {
            org: self.input.org.clone(),
            app: self.input.app.clone(),
            env: self.input.env.clone(),
            env_type: self.input.env_type.clone(),
            res: d.clone(),
        };
        let def = select_definition(self.input.definitions, &ctx).map_err(|source| {
            GraphError::Match {
                path: path.to_owned(),
                source,
            }
        })?;
        if let Some(p) = &params {
            self.explicit
                .insert(d.clone(), (path.to_owned(), p.clone()));
        }
        self.nodes.insert(
            d.clone(),
            Node {
                descriptor: d.clone(),
                definition: def.id.clone(),
                driver_type: def.driver_type.clone(),
                params: params.filter(|p| !p.is_null()),
                requests: vec![Request {
                    path: path.to_owned(),
                    params: canonical,
                }],
                depends_on: BTreeSet::new(),
            },
        );
        self.queue.push_back(d.clone());
        Ok(())
    }

    fn edge(&mut self, from: &Descriptor, to: &Descriptor) -> bool {
        self.nodes
            .get_mut(from)
            .expect("node exists")
            .depends_on
            .insert(to.clone())
    }

    /// Adds the references and co-provisioned resources of `d`'s matched definition.
    fn expand(&mut self, d: &Descriptor) -> Result<(), GraphError> {
        let def_id = self.nodes[d].definition.clone();
        let def = self
            .input
            .definitions
            .iter()
            .find(|x| x.id == def_id)
            .expect("matched definition exists");
        let perr = |source| GraphError::Placeholder {
            definition: def.id.clone(),
            source,
        };
        let mut found = Vec::new();
        let inputs = &def.driver_inputs;
        found.extend(
            collect_placeholders(
                &Value::Object(inputs.values.clone()),
                Dialect::Definition,
                "driver_inputs.values",
            )
            .map_err(perr)?,
        );
        found.extend(
            collect_placeholders(
                &Value::Object(inputs.secrets.clone()),
                Dialect::Definition,
                "driver_inputs.secrets",
            )
            .map_err(perr)?,
        );
        if let Some(acct) = &def.driver_account {
            found.extend(
                collect_placeholders(
                    &Value::String(acct.clone()),
                    Dialect::DriverAccount,
                    "driver_account",
                )
                .map_err(perr)?,
            );
        }
        let path = |loc: &str| format!("definition `{}` {loc} for `{d}`", def.id);
        for (loc, _, expr) in found {
            if let Expr::Resource { target, .. } = expr {
                let to = self.resolve_ref(&target, d);
                self.request(&to, &path(&loc), None)?;
                self.edge(d, &to);
            }
        }
        for (key, prov) in &def.provision {
            let loc = format!("provision.{key}");
            let target = ResourceRef::parse(key).map_err(|e| {
                perr(PlaceholderError {
                    placeholder: key.clone(),
                    location: loc.clone(),
                    kind: crate::placeholder::PlaceholderErrorKind::Syntax(e.to_string()),
                })
            })?;
            let co = self.resolve_ref(&target, d);
            let params = prov.params.clone().unwrap_or(Value::Null);
            self.request(&co, &path(&loc), Some(params.clone()))?;
            if prov.is_dependent {
                self.edge(d, &co);
            } else {
                self.edge(&co, d);
            }
            for (ploc, _, expr) in
                collect_placeholders(&params, Dialect::Definition, &format!("{loc}.params"))
                    .map_err(perr)?
            {
                if let Expr::Resource { target, .. } = expr {
                    let to = self.resolve_ref(&target, d);
                    self.request(&to, &path(&ploc), None)?;
                    if to != co {
                        self.edge(&co, &to);
                    }
                }
            }
            self.co.insert(CoProvision {
                source: d.clone(),
                co,
                match_dependents: prov.match_dependents,
            });
        }
        Ok(())
    }

    fn resolve_ref(&self, target: &ResourceRef, referrer: &Descriptor) -> Descriptor {
        target.resolve(referrer, self.is_implicit(&target.ty))
    }

    /// With `match_dependents`, every consumer of the source also waits for the
    /// co-provisioned resource. Repeats until no edge is added.
    fn match_dependents(&mut self) {
        loop {
            let mut added = false;
            let cos: Vec<CoProvision> = self
                .co
                .iter()
                .filter(|c| c.match_dependents)
                .cloned()
                .collect();
            for c in cos {
                let consumers: Vec<Descriptor> = self
                    .nodes
                    .values()
                    .filter(|n| {
                        n.depends_on.contains(&c.source)
                            && n.descriptor != c.co
                            && n.descriptor != c.source
                    })
                    .map(|n| n.descriptor.clone())
                    .collect();
                for consumer in consumers {
                    added |= self.edge(&consumer, &c.co);
                }
            }
            if !added {
                return;
            }
        }
    }
}

/// Orders nodes into layers, or reports a cycle.
fn topological_layers(
    nodes: &BTreeMap<Descriptor, Node>,
) -> Result<Vec<Vec<Descriptor>>, GraphError> {
    let mut remaining: BTreeMap<&Descriptor, BTreeSet<&Descriptor>> = nodes
        .iter()
        .map(|(d, n)| (d, n.depends_on.iter().collect()))
        .collect();
    let mut layers = Vec::new();
    while !remaining.is_empty() {
        let ready: Vec<&Descriptor> = remaining
            .iter()
            .filter(|(_, deps)| deps.is_empty())
            .map(|(d, _)| *d)
            .collect();
        if ready.is_empty() {
            return Err(GraphError::Cycle(find_cycle(&remaining)));
        }
        for d in &ready {
            remaining.remove(d);
        }
        for deps in remaining.values_mut() {
            for d in &ready {
                deps.remove(d);
            }
        }
        layers.push(ready.into_iter().cloned().collect());
    }
    Ok(layers)
}

/// Finds one cycle among nodes that all have unmet dependencies.
fn find_cycle(remaining: &BTreeMap<&Descriptor, BTreeSet<&Descriptor>>) -> Vec<Descriptor> {
    let start = *remaining.keys().next().expect("non-empty");
    let mut path = vec![start];
    let mut seen = BTreeMap::from([(start, 0usize)]);
    let mut current = start;
    loop {
        let next = *remaining[current]
            .iter()
            .next()
            .expect("every remaining node has a dependency");
        if let Some(&i) = seen.get(next) {
            let mut cycle: Vec<Descriptor> = path[i..].iter().map(|d| (*d).clone()).collect();
            cycle.push(next.clone());
            return cycle;
        }
        seen.insert(next, path.len());
        path.push(next);
        current = next;
    }
}

#[cfg(test)]
mod tests;

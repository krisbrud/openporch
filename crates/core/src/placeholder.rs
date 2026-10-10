//! Placeholder parsing and resolution.
//!
//! A placeholder is written `${...}`; `$\{` is an escape for a literal `${`. A string
//! that is exactly one placeholder takes the referenced value's JSON type. A string
//! mixing text and placeholders inserts strings raw and other values as JSON.
//!
//! Secret values are never evaluated here. A string that refers to a secret becomes a
//! [`SecretExpr`], which the executor renders after authorizing the references, and
//! only into a secret sink.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{AccountId, ClassId, Descriptor, Guresid, IdError, ResId, ResourceTypeId};
use crate::model::SecretRef;

/// A placeholder error, naming the placeholder and where it appears.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind} in `{placeholder}` at {location}")]
pub struct PlaceholderError {
    /// The placeholder text, such as `${secrets.foo}`, or the whole string for syntax errors.
    pub placeholder: String,
    /// Where it appears, such as `definition stg-redis driver_inputs.values.url`.
    pub location: String,
    /// What went wrong.
    pub kind: PlaceholderErrorKind,
}

/// What went wrong with a placeholder.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlaceholderErrorKind {
    /// The string is not valid placeholder syntax.
    #[error("invalid placeholder syntax: {0}")]
    Syntax(String),
    /// The root (`context`, `resources`, ...) is unknown or not allowed here.
    #[error("unknown placeholder root `{0}`")]
    UnknownRoot(String),
    /// The placeholder is well-formed but not allowed in this location.
    #[error("placeholder not allowed here: {0}")]
    NotAllowed(String),
    /// The referenced value does not exist.
    #[error("cannot resolve placeholder: {0}")]
    Unresolvable(String),
    /// A secret flowed into a non-secret input.
    #[error("a secret cannot be used in a non-secret input")]
    SecretInNonSecret,
}

/// One segment of a placeholder path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Segment {
    /// A dotted name: `.host`.
    Name(String),
    /// A bracketed string: `['dns.public#shared.api-dns']`.
    Quoted(String),
    /// A bracketed index: `[0]`.
    Index(usize),
}

impl Segment {
    /// The segment as a map key, if it is a name or a quoted string.
    pub fn key(&self) -> Option<&str> {
        match self {
            Segment::Name(s) | Segment::Quoted(s) => Some(s),
            Segment::Index(_) => None,
        }
    }
}

impl fmt::Display for Segment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Segment::Name(s) => write!(f, ".{s}"),
            Segment::Quoted(s) => write!(f, "['{s}']"),
            Segment::Index(i) => write!(f, "[{i}]"),
        }
    }
}

/// A raw placeholder: its source text and its path segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPlaceholder {
    /// The placeholder as written, including `${` and `}`.
    pub text: String,
    /// Path segments; the first is the root.
    pub segments: Vec<Segment>,
}

impl RawPlaceholder {
    /// The root name, such as `context` or `resources`.
    pub fn root(&self) -> &str {
        self.segments.first().and_then(Segment::key).unwrap_or("")
    }
}

/// A parsed string: literal text and placeholders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// The parts in order.
    pub parts: Vec<Part>,
}

/// A part of a parsed string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    /// Literal text, with escapes already applied.
    Literal(String),
    /// A placeholder.
    Placeholder(RawPlaceholder),
}

impl Template {
    /// Parses a string into literal text and placeholders.
    pub fn parse(s: &str) -> Result<Self, PlaceholderErrorKind> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        let mut rest = s;
        while let Some(i) = rest.find('$') {
            lit.push_str(&rest[..i]);
            let after = &rest[i..];
            if let Some(tail) = after.strip_prefix("$\\{") {
                lit.push_str("${");
                rest = tail;
            } else if let Some(tail) = after.strip_prefix("${") {
                let end = find_placeholder_end(tail)
                    .ok_or_else(|| PlaceholderErrorKind::Syntax("unterminated `${`".to_owned()))?;
                let body = &tail[..end];
                let segments = parse_path(body)?;
                if !lit.is_empty() {
                    parts.push(Part::Literal(std::mem::take(&mut lit)));
                }
                parts.push(Part::Placeholder(RawPlaceholder {
                    text: format!("${{{body}}}"),
                    segments,
                }));
                rest = &tail[end + 1..];
            } else {
                lit.push('$');
                rest = &after[1..];
            }
        }
        lit.push_str(rest);
        if !lit.is_empty() {
            parts.push(Part::Literal(lit));
        }
        Ok(Self { parts })
    }

    /// Whether the string has no placeholders.
    pub fn is_literal(&self) -> bool {
        self.parts.iter().all(|p| matches!(p, Part::Literal(_)))
    }

    /// The placeholders in order.
    pub fn placeholders(&self) -> impl Iterator<Item = &RawPlaceholder> {
        self.parts.iter().filter_map(|p| match p {
            Part::Placeholder(p) => Some(p),
            Part::Literal(_) => None,
        })
    }

    /// The placeholder, if the string is exactly one placeholder.
    pub fn whole(&self) -> Option<&RawPlaceholder> {
        match self.parts.as_slice() {
            [Part::Placeholder(p)] => Some(p),
            _ => None,
        }
    }
}

/// Finds the `}` closing a placeholder body, skipping quoted strings in brackets.
fn find_placeholder_end(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '}') => return Some(i),
            (None, _) => {}
        }
    }
    None
}

fn parse_path(body: &str) -> Result<Vec<Segment>, PlaceholderErrorKind> {
    let syntax = |m: &str| PlaceholderErrorKind::Syntax(m.to_owned());
    let mut segments = Vec::new();
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    let is_name_char = |c: char| !matches!(c, '.' | '[' | ']' | '\'' | '"') && !c.is_whitespace();
    if chars.is_empty() {
        return Err(syntax("empty placeholder"));
    }
    while i < chars.len() {
        match chars[i] {
            '[' => {
                let start = i + 1;
                let close = chars[start..]
                    .iter()
                    .position(|&c| c == ']')
                    .map(|p| p + start)
                    .ok_or_else(|| syntax("unterminated `[`"))?;
                let inner: String = chars[start..close].iter().collect();
                let seg = if let Some(q) = inner
                    .strip_prefix('\'')
                    .and_then(|s| s.strip_suffix('\''))
                    .or_else(|| inner.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
                {
                    Segment::Quoted(q.to_owned())
                } else {
                    Segment::Index(inner.parse().map_err(|_| {
                        syntax("brackets hold a quoted string or a non-negative index")
                    })?)
                };
                segments.push(seg);
                i = close + 1;
            }
            '.' if !segments.is_empty() => {
                i += 1;
                let start = i;
                while i < chars.len() && is_name_char(chars[i]) {
                    i += 1;
                }
                if i == start {
                    return Err(syntax("empty path segment"));
                }
                segments.push(Segment::Name(chars[start..i].iter().collect()));
            }
            c if segments.is_empty() && is_name_char(c) => {
                let start = i;
                while i < chars.len() && is_name_char(chars[i]) {
                    i += 1;
                }
                segments.push(Segment::Name(chars[start..i].iter().collect()));
            }
            c => return Err(syntax(&format!("unexpected `{c}`"))),
        }
    }
    Ok(segments)
}

/// Context values available to definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ContextKey {
    /// `context.org.id`
    OrgId,
    /// `context.app.id`
    AppId,
    /// `context.env.id`
    EnvId,
    /// `context.env.type`
    EnvType,
    /// `context.res.id`
    ResId,
    /// `context.res.class`
    ResClass,
    /// `context.res.type`
    ResType,
    /// `context.res.guresid`
    ResGuresid,
    /// `context.deploy.id`
    DeployId,
    /// `context.deploy.set.id`
    DeploySetId,
}

impl ContextKey {
    /// All context keys.
    pub const ALL: [ContextKey; 10] = [
        Self::OrgId,
        Self::AppId,
        Self::EnvId,
        Self::EnvType,
        Self::ResId,
        Self::ResClass,
        Self::ResType,
        Self::ResGuresid,
        Self::DeployId,
        Self::DeploySetId,
    ];

    /// The dotted path after `context.`.
    pub fn path(self) -> &'static str {
        match self {
            Self::OrgId => "org.id",
            Self::AppId => "app.id",
            Self::EnvId => "env.id",
            Self::EnvType => "env.type",
            Self::ResId => "res.id",
            Self::ResClass => "res.class",
            Self::ResType => "res.type",
            Self::ResGuresid => "res.guresid",
            Self::DeployId => "deploy.id",
            Self::DeploySetId => "deploy.set.id",
        }
    }
}

/// A possibly partial reference to a resource: `<type>[.<class>][#<id>]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResourceRef {
    /// Resource type.
    pub ty: ResourceTypeId,
    /// Class, inherited from the referencing resource when absent.
    pub class: Option<ClassId>,
    /// Resource ID, inherited from the referencing resource when absent.
    pub id: Option<ResId>,
}

impl ResourceRef {
    /// Parses `<type>[.<class>][#<id>]`.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let (head, id) = match s.split_once('#') {
            Some((h, id)) => (h, Some(ResId::new(id)?)),
            None => (s, None),
        };
        let (ty, class) = match head.split_once('.') {
            Some((t, c)) => (t, Some(ClassId::new(c)?)),
            None => (head, None),
        };
        Ok(Self {
            ty: ResourceTypeId::new(ty)?,
            class,
            id,
        })
    }

    /// The full descriptor, taking omitted parts from `referrer`. For implicit types
    /// (such as `k8s-cluster` or `base-env`) an omitted class is `default` and an
    /// omitted ID is the type name, because those resources exist once per environment.
    pub fn resolve(&self, referrer: &Descriptor, implicit: bool) -> Descriptor {
        let (class, id) = if implicit {
            (
                self.class.clone().unwrap_or_default(),
                self.id.clone().unwrap_or_else(|| {
                    ResId::new(self.ty.as_str().replace('/', "-"))
                        .expect("type names are valid resource IDs")
                }),
            )
        } else {
            (
                self.class.clone().unwrap_or_else(|| referrer.class.clone()),
                self.id.clone().unwrap_or_else(|| referrer.id.clone()),
            )
        };
        Descriptor::new(self.ty.clone(), class, id)
    }
}

impl fmt::Display for ResourceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.ty)?;
        if let Some(c) = &self.class {
            write!(f, ".{c}")?;
        }
        if let Some(id) = &self.id {
            write!(f, "#{id}")?;
        }
        Ok(())
    }
}

/// What a resource placeholder reads from the resource.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResourceField {
    /// `.outputs.<path>`
    Outputs(Vec<Segment>),
    /// `.guresid`
    Guresid,
    /// `.account`, only in `driver_account`.
    Account,
}

/// A placeholder interpreted for one dialect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// `${context.<key>}`
    Context(ContextKey),
    /// `${resources.<type>[.<class>][#<id>].<field>}`
    Resource {
        /// The referenced resource.
        target: ResourceRef,
        /// What is read from it.
        field: ResourceField,
    },
    /// `${externals.<key>.<path>}`
    Externals {
        /// The workload's private dependency key.
        key: String,
        /// Output path.
        path: Vec<Segment>,
    },
    /// `${shared.<id>.<path>}`
    Shared {
        /// Shared resource ID.
        id: String,
        /// Output path.
        path: Vec<Segment>,
    },
    /// `${values.<key>[.<path>]}`
    Values {
        /// Shared value key.
        key: String,
        /// Path into the value.
        path: Vec<Segment>,
    },
    /// `${pod.<path>}`, rendered as a Kubernetes field reference.
    Pod(Vec<String>),
}

/// Which placeholders a location accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// Definition driver inputs and provision params: `context` and `resources`.
    Definition,
    /// `driver_account`: like `Definition`, and `resources....account` is allowed.
    DriverAccount,
    /// Workload specs in a deployment set: `externals`, `shared`, `values` and `pod`.
    Set,
}

impl Expr {
    /// Interprets a raw placeholder for `dialect`.
    pub fn interpret(raw: &RawPlaceholder, dialect: Dialect) -> Result<Self, PlaceholderErrorKind> {
        let segs = &raw.segments;
        let root = raw.root();
        let not_here = |m: &str| PlaceholderErrorKind::NotAllowed(m.to_owned());
        let syntax = |m: &str| PlaceholderErrorKind::Syntax(m.to_owned());
        let definition = matches!(dialect, Dialect::Definition | Dialect::DriverAccount);
        match root {
            "context" if definition => {
                let path = names(&segs[1..]).ok_or_else(|| syntax("context paths are names"))?;
                ContextKey::ALL
                    .into_iter()
                    .find(|k| k.path() == path.join("."))
                    .map(Expr::Context)
                    .ok_or_else(|| {
                        PlaceholderErrorKind::Unresolvable(format!(
                            "unknown context value `context.{}`",
                            path.join(".")
                        ))
                    })
            }
            "context" => Err(not_here(
                "workloads cannot use `context` placeholders; use `values` for environment configuration",
            )),
            "resources" if definition => {
                let (target, field) = parse_resource_ref(&segs[1..])?;
                if field == ResourceField::Account && dialect != Dialect::DriverAccount {
                    return Err(not_here("`.account` is only allowed in `driver_account`"));
                }
                Ok(Expr::Resource { target, field })
            }
            "externals" | "shared" if dialect == Dialect::Set => {
                let key = segs
                    .get(1)
                    .and_then(Segment::key)
                    .ok_or_else(|| syntax(&format!("expected `{root}.<id>.<output>`")))?
                    .to_owned();
                let path = segs[2..].to_vec();
                if path.is_empty() {
                    return Err(syntax(&format!("expected `{root}.<id>.<output>`")));
                }
                Ok(if root == "externals" {
                    Expr::Externals { key, path }
                } else {
                    Expr::Shared { id: key, path }
                })
            }
            "values" if dialect == Dialect::Set => {
                let key = segs
                    .get(1)
                    .and_then(Segment::key)
                    .ok_or_else(|| syntax("expected `values.<key>`"))?
                    .to_owned();
                Ok(Expr::Values {
                    key,
                    path: segs[2..].to_vec(),
                })
            }
            "pod" if dialect == Dialect::Set => {
                let path = names(&segs[1..]).ok_or_else(|| syntax("pod paths are names"))?;
                if path.is_empty() {
                    return Err(syntax("expected `pod.<path>`"));
                }
                Ok(Expr::Pod(path))
            }
            other => Err(PlaceholderErrorKind::UnknownRoot(other.to_owned())),
        }
    }
}

fn names(segs: &[Segment]) -> Option<Vec<String>> {
    segs.iter()
        .map(|s| match s {
            Segment::Name(n) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

fn parse_resource_ref(
    segs: &[Segment],
) -> Result<(ResourceRef, ResourceField), PlaceholderErrorKind> {
    let syntax = |m: &str| PlaceholderErrorKind::Syntax(m.to_owned());
    let expected = "expected `resources.<type>[.<class>][#<id>].outputs.<name>`";
    let is_field = |s: &Segment| matches!(s, Segment::Name(n) if n == "outputs" || n == "guresid" || n == "account");
    let (ref_text, rest) = match segs.first() {
        Some(Segment::Quoted(q)) => (q.clone(), &segs[1..]),
        Some(Segment::Name(_)) => {
            // The type comes first; a class may follow as its own segment unless the
            // type already carries `#<id>`, and an ID (which contains dots) runs until
            // the field name.
            let n = segs
                .iter()
                .position(is_field)
                .ok_or_else(|| syntax(expected))?;
            let parts = names(&segs[..n]).ok_or_else(|| syntax(expected))?;
            if parts.is_empty() || (!parts.iter().any(|p| p.contains('#')) && parts.len() > 2) {
                return Err(syntax(expected));
            }
            (parts.join("."), &segs[n..])
        }
        _ => return Err(syntax(expected)),
    };
    let target =
        ResourceRef::parse(&ref_text).map_err(|e| PlaceholderErrorKind::Syntax(e.to_string()))?;
    let field = match rest.split_first() {
        Some((Segment::Name(n), path)) if n == "outputs" && !path.is_empty() => {
            ResourceField::Outputs(path.to_vec())
        }
        Some((Segment::Name(n), [])) if n == "guresid" => ResourceField::Guresid,
        Some((Segment::Name(n), [])) if n == "account" => ResourceField::Account,
        _ => return Err(syntax(expected)),
    };
    Ok((target, field))
}

/// One part of a secret expression.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretPart {
    /// Literal (non-secret) text.
    Literal(String),
    /// A secret, read from its store only by the executor.
    Secret(SecretRef),
}

/// A string that contains secrets, kept unevaluated until the executor renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretExpr {
    /// The parts in order.
    pub parts: Vec<SecretPart>,
}

impl SecretExpr {
    /// A secret expression that is exactly one secret.
    pub fn whole(r: SecretRef) -> Self {
        Self {
            parts: vec![SecretPart::Secret(r)],
        }
    }

    /// The secret references this expression needs.
    pub fn references(&self) -> impl Iterator<Item = &SecretRef> {
        self.parts.iter().filter_map(|p| match p {
            SecretPart::Secret(r) => Some(r),
            SecretPart::Literal(_) => None,
        })
    }

    /// Renders the expression, reading each secret with `read`.
    pub fn render<E>(
        &self,
        mut read: impl FnMut(&SecretRef) -> Result<String, E>,
    ) -> Result<String, E> {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                SecretPart::Literal(s) => out.push_str(s),
                SecretPart::Secret(r) => out.push_str(&read(r)?),
            }
        }
        Ok(out)
    }

    fn push_literal(&mut self, s: &str) {
        if let Some(SecretPart::Literal(last)) = self.parts.last_mut() {
            last.push_str(s);
        } else if !s.is_empty() {
            self.parts.push(SecretPart::Literal(s.to_owned()));
        }
    }
}

/// A resolved JSON value that may contain secret expressions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolved {
    /// A plain JSON value with no secrets anywhere inside.
    Value(Value),
    /// A secret string.
    Secret(SecretExpr),
    /// An array with at least one secret inside.
    Array(Vec<Resolved>),
    /// An object with at least one secret inside, in key order.
    Object(Vec<(String, Resolved)>),
}

impl Resolved {
    /// Whether a secret appears anywhere inside.
    pub fn is_tainted(&self) -> bool {
        !matches!(self, Resolved::Value(_))
    }

    /// The plain value, if nothing inside is secret.
    pub fn as_plain(&self) -> Option<&Value> {
        match self {
            Resolved::Value(v) => Some(v),
            _ => None,
        }
    }

    fn array(items: Vec<Resolved>) -> Self {
        if items.iter().all(|r| !r.is_tainted()) {
            Resolved::Value(Value::Array(
                items
                    .into_iter()
                    .map(|r| r.into_value_unchecked())
                    .collect(),
            ))
        } else {
            Resolved::Array(items)
        }
    }

    fn object(items: Vec<(String, Resolved)>) -> Self {
        if items.iter().all(|(_, r)| !r.is_tainted()) {
            Resolved::Value(Value::Object(
                items
                    .into_iter()
                    .map(|(k, r)| (k, r.into_value_unchecked()))
                    .collect(),
            ))
        } else {
            Resolved::Object(items)
        }
    }

    fn into_value_unchecked(self) -> Value {
        match self {
            Resolved::Value(v) => v,
            _ => unreachable!("only called on untainted values"),
        }
    }
}

/// What a scope returns for one placeholder.
#[derive(Debug, Clone, PartialEq)]
pub enum Lookup {
    /// A non-secret value.
    Value(Value),
    /// A secret, as a reference.
    Secret(SecretRef),
}

/// Supplies values for placeholders.
pub trait Scope {
    /// Looks up one interpreted placeholder. The error is a human-readable reason.
    fn lookup(&self, expr: &Expr) -> Result<Lookup, String>;
}

/// Where a resolved value is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    /// A non-secret input: secrets are rejected.
    Plain,
    /// A secret input: secrets are kept as expressions.
    Secret,
}

/// Resolves every placeholder in `value`. Object keys are not resolved.
/// `location` names where the value comes from; nested paths are appended to it.
pub fn resolve_value(
    value: &Value,
    dialect: Dialect,
    scope: &dyn Scope,
    sink: Sink,
    location: &str,
) -> Result<Resolved, PlaceholderError> {
    match value {
        Value::String(s) => resolve_string(s, dialect, scope, sink, location),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                out.push(resolve_value(
                    item,
                    dialect,
                    scope,
                    sink,
                    &format!("{location}[{i}]"),
                )?);
            }
            Ok(Resolved::array(out))
        }
        Value::Object(map) => {
            let mut out = Vec::with_capacity(map.len());
            for (k, v) in map {
                out.push((
                    k.clone(),
                    resolve_value(v, dialect, scope, sink, &format!("{location}.{k}"))?,
                ));
            }
            Ok(Resolved::object(out))
        }
        other => Ok(Resolved::Value(other.clone())),
    }
}

/// Resolves the placeholders in one string.
pub fn resolve_string(
    s: &str,
    dialect: Dialect,
    scope: &dyn Scope,
    sink: Sink,
    location: &str,
) -> Result<Resolved, PlaceholderError> {
    let err = |placeholder: &str, kind| PlaceholderError {
        placeholder: placeholder.to_owned(),
        location: location.to_owned(),
        kind,
    };
    let template = Template::parse(s).map_err(|k| err(s, k))?;
    let mut lookups = Vec::new();
    for p in template.placeholders() {
        let expr = Expr::interpret(p, dialect).map_err(|k| err(&p.text, k))?;
        let found = scope
            .lookup(&expr)
            .map_err(|reason| err(&p.text, PlaceholderErrorKind::Unresolvable(reason)))?;
        if matches!(found, Lookup::Secret(_)) && sink == Sink::Plain {
            return Err(err(&p.text, PlaceholderErrorKind::SecretInNonSecret));
        }
        lookups.push(found);
    }
    if template.whole().is_some() {
        return Ok(match lookups.pop().expect("one placeholder") {
            Lookup::Value(v) => Resolved::Value(v),
            Lookup::Secret(r) => Resolved::Secret(SecretExpr::whole(r)),
        });
    }
    let mut expr = SecretExpr { parts: Vec::new() };
    let mut lookups = lookups.into_iter();
    for part in &template.parts {
        match part {
            Part::Literal(l) => expr.push_literal(l),
            Part::Placeholder(_) => match lookups.next().expect("one lookup per placeholder") {
                Lookup::Value(Value::String(v)) => expr.push_literal(&v),
                Lookup::Value(v) => expr.push_literal(&v.to_string()),
                Lookup::Secret(r) => expr.parts.push(SecretPart::Secret(r)),
            },
        }
    }
    if expr.references().next().is_some() {
        Ok(Resolved::Secret(expr))
    } else {
        let text = expr
            .render(|_| Ok::<_, ()>(String::new()))
            .expect("no secrets");
        Ok(Resolved::Value(Value::String(text)))
    }
}

/// Every placeholder in `value`, interpreted for `dialect`, with its location.
pub fn collect_placeholders(
    value: &Value,
    dialect: Dialect,
    location: &str,
) -> Result<Vec<(String, RawPlaceholder, Expr)>, PlaceholderError> {
    let mut out = Vec::new();
    collect_into(value, dialect, location, &mut out)?;
    Ok(out)
}

fn collect_into(
    value: &Value,
    dialect: Dialect,
    location: &str,
    out: &mut Vec<(String, RawPlaceholder, Expr)>,
) -> Result<(), PlaceholderError> {
    match value {
        Value::String(s) => {
            let err = |placeholder: &str, kind| PlaceholderError {
                placeholder: placeholder.to_owned(),
                location: location.to_owned(),
                kind,
            };
            let t = Template::parse(s).map_err(|k| err(s, k))?;
            for p in t.placeholders() {
                let e = Expr::interpret(p, dialect).map_err(|k| err(&p.text, k))?;
                out.push((location.to_owned(), p.clone(), e));
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                collect_into(item, dialect, &format!("{location}[{i}]"), out)?;
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                collect_into(v, dialect, &format!("{location}.{k}"), out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The outputs of one provisioned resource, as seen by placeholders.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceOutputs {
    /// Non-secret outputs.
    #[serde(default)]
    pub values: serde_json::Map<String, Value>,
    /// Secret outputs, as references.
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretRef>,
    /// The resource's GUResID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guresid: Option<Guresid>,
    /// The cloud account the resource's definition used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountId>,
}

impl ResourceOutputs {
    /// Reads an output path: the first segment selects a value or secret output.
    pub fn get(&self, path: &[Segment]) -> Result<Lookup, String> {
        let (first, rest) = path.split_first().ok_or("an output name is required")?;
        let name = first.key().ok_or("output names are strings")?;
        if let Some(r) = self.secrets.get(name) {
            return if rest.is_empty() {
                Ok(Lookup::Secret(r.clone()))
            } else {
                Err(format!("secret output `{name}` has no fields"))
            };
        }
        let v = self
            .values
            .get(name)
            .ok_or_else(|| format!("no output `{name}`"))?;
        walk(v, rest).map(Lookup::Value)
    }
}

fn walk(mut v: &Value, path: &[Segment]) -> Result<Value, String> {
    for seg in path {
        v = match (seg, v) {
            (Segment::Index(i), Value::Array(a)) => a.get(*i),
            (Segment::Name(k) | Segment::Quoted(k), Value::Object(m)) => m.get(k),
            _ => None,
        }
        .ok_or_else(|| format!("no field `{seg}`"))?;
    }
    Ok(v.clone())
}

/// A shared value: a plain value or a secret reference.
#[derive(Debug, Clone, PartialEq)]
pub enum SharedValue {
    /// Non-secret value.
    Value(Value),
    /// Secret value as a reference.
    Secret(SecretRef),
}

/// An in-memory scope built from known context and outputs.
#[derive(Debug, Clone, Default)]
pub struct MapScope {
    /// Context values.
    pub context: BTreeMap<ContextKey, Value>,
    /// The resource whose inputs are being resolved, for partial references.
    pub referrer: Option<Descriptor>,
    /// Types whose omitted class and ID default to `default` and the type name.
    pub implicit_types: Vec<ResourceTypeId>,
    /// Outputs of provisioned resources by descriptor.
    pub resources: BTreeMap<Descriptor, ResourceOutputs>,
    /// A workload's private dependency outputs by key.
    pub externals: BTreeMap<String, ResourceOutputs>,
    /// Shared resource outputs by shared ID.
    pub shared: BTreeMap<String, ResourceOutputs>,
    /// Shared values, environment overrides already applied.
    pub values: BTreeMap<String, SharedValue>,
}

impl MapScope {
    /// The descriptor a resource reference names from this scope's referrer.
    pub fn descriptor_for(&self, target: &ResourceRef) -> Result<Descriptor, String> {
        let implicit = self.implicit_types.contains(&target.ty);
        match (&self.referrer, &target.class, &target.id) {
            (Some(r), _, _) => Ok(target.resolve(r, implicit)),
            (None, Some(c), Some(id)) => {
                Ok(Descriptor::new(target.ty.clone(), c.clone(), id.clone()))
            }
            (None, _, _) if implicit => {
                let dummy = Descriptor::new(
                    target.ty.clone(),
                    ClassId::default(),
                    ResId::new("x").expect("valid"),
                );
                Ok(target.resolve(&dummy, true))
            }
            _ => Err(format!("`{target}` needs a class and ID here")),
        }
    }
}

impl Scope for MapScope {
    fn lookup(&self, expr: &Expr) -> Result<Lookup, String> {
        match expr {
            Expr::Context(k) => self
                .context
                .get(k)
                .cloned()
                .map(Lookup::Value)
                .ok_or_else(|| format!("`context.{}` is not available here", k.path())),
            Expr::Resource { target, field } => {
                let d = self.descriptor_for(target)?;
                let outputs = self
                    .resources
                    .get(&d)
                    .ok_or_else(|| format!("resource `{d}` is not in the graph"))?;
                match field {
                    ResourceField::Outputs(path) => outputs.get(path),
                    ResourceField::Guresid => outputs
                        .guresid
                        .as_ref()
                        .map(|g| Lookup::Value(Value::String(g.to_string())))
                        .ok_or_else(|| format!("resource `{d}` has no GUResID yet")),
                    ResourceField::Account => outputs
                        .account
                        .as_ref()
                        .map(|a| Lookup::Value(Value::String(a.to_string())))
                        .ok_or_else(|| format!("resource `{d}` has no cloud account")),
                }
            }
            Expr::Externals { key, path } => self
                .externals
                .get(key)
                .ok_or_else(|| format!("no dependency `{key}`"))?
                .get(path),
            Expr::Shared { id, path } => self
                .shared
                .get(id)
                .ok_or_else(|| format!("no shared resource `{id}`"))?
                .get(path),
            Expr::Values { key, path } => match self.values.get(key) {
                Some(SharedValue::Value(v)) => walk(v, path).map(Lookup::Value),
                Some(SharedValue::Secret(r)) if path.is_empty() => Ok(Lookup::Secret(r.clone())),
                Some(SharedValue::Secret(_)) => Err(format!("secret value `{key}` has no fields")),
                None => Err(format!("no shared value `{key}`")),
            },
            Expr::Pod(path) => Err(format!(
                "`pod.{}` needs a workload target that supports pod references",
                path.join(".")
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn secret(r: &str) -> SecretRef {
        SecretRef {
            store: "openporch".into(),
            reference: r.into(),
            version: None,
        }
    }

    fn scope() -> MapScope {
        let mut s = MapScope::default();
        s.context.insert(ContextKey::AppId, json!("shop"));
        s.context.insert(ContextKey::EnvId, json!("dev"));
        s.context.insert(ContextKey::OrgId, json!("acme"));
        s.referrer = Some(Descriptor::parse("postgres.default#modules.web.externals.db").unwrap());
        s.implicit_types = vec!["k8s-cluster".parse().unwrap(), "base-env".parse().unwrap()];
        let mut inst = ResourceOutputs::default();
        inst.values.insert("host".into(), json!("db.internal"));
        inst.values.insert("port".into(), json!(5432));
        inst.values
            .insert("limits".into(), json!({"cpu": 2, "tags": ["a", "b"]}));
        inst.secrets.insert("password".into(), secret("pg-pass"));
        inst.guresid = Some(Guresid::from_bytes([1; 16]));
        s.resources.insert(
            Descriptor::parse("postgres-instance.default#modules.web.externals.db").unwrap(),
            inst.clone(),
        );
        let mut dns = ResourceOutputs::default();
        dns.values.insert("host".into(), json!("api.example.com"));
        s.resources.insert(
            Descriptor::parse("dns.public#shared.api-dns").unwrap(),
            dns.clone(),
        );
        let cfg = ResourceOutputs {
            account: Some("aws-dev".parse().unwrap()),
            ..Default::default()
        };
        s.resources.insert(
            Descriptor::parse("config.default#aws-account").unwrap(),
            cfg,
        );
        let mut cluster = ResourceOutputs::default();
        cluster.values.insert("name".into(), json!("dev-cluster"));
        s.resources.insert(
            Descriptor::parse("k8s-cluster.default#k8s-cluster").unwrap(),
            cluster,
        );
        s.externals.insert("db".into(), inst);
        s.shared.insert("api-dns".into(), dns);
        s.values
            .insert("page_size".into(), SharedValue::Value(json!(20)));
        s.values
            .insert("api_key".into(), SharedValue::Secret(secret("api-key")));
        s
    }

    fn def(s: &str) -> Result<Resolved, PlaceholderError> {
        resolve_string(s, Dialect::Definition, &scope(), Sink::Plain, "test")
    }

    fn set(s: &str, sink: Sink) -> Result<Resolved, PlaceholderError> {
        resolve_string(s, Dialect::Set, &scope(), sink, "test")
    }

    #[test]
    fn composite_string() {
        assert_eq!(
            def("${context.app.id}-${context.env.id}").unwrap(),
            Resolved::Value(json!("shop-dev"))
        );
    }

    #[test]
    fn unknown_root_names_placeholder_and_location() {
        let e = resolve_string(
            "x-${secrets.foo}",
            Dialect::Definition,
            &scope(),
            Sink::Plain,
            "definition stg-redis driver_inputs.values.url",
        )
        .unwrap_err();
        assert_eq!(e.placeholder, "${secrets.foo}");
        assert_eq!(e.kind, PlaceholderErrorKind::UnknownRoot("secrets".into()));
        let msg = e.to_string();
        assert!(
            msg.contains("${secrets.foo}") && msg.contains("stg-redis"),
            "{msg}"
        );
    }

    #[test]
    fn context_keys() {
        for k in ContextKey::ALL {
            let raw = Template::parse(&format!("${{context.{}}}", k.path())).unwrap();
            let e = Expr::interpret(raw.whole().unwrap(), Dialect::Definition).unwrap();
            assert_eq!(e, Expr::Context(k));
        }
        assert!(matches!(
            def("${context.app.name}").unwrap_err().kind,
            PlaceholderErrorKind::Unresolvable(_)
        ));
        assert_eq!(
            def("${context.org.id}").unwrap(),
            Resolved::Value(json!("acme"))
        );
    }

    #[test]
    fn escape_produces_literal_dollar_brace() {
        assert_eq!(
            def("$\\{context.app.id}").unwrap(),
            Resolved::Value(json!("${context.app.id}"))
        );
        assert_eq!(
            def("cost: $5 and $\\{x} ${context.env.id}").unwrap(),
            Resolved::Value(json!("cost: $5 and ${x} dev"))
        );
    }

    #[test]
    fn resource_outputs_inherit_class_and_id() {
        assert_eq!(
            def("${resources.postgres-instance.outputs.host}").unwrap(),
            Resolved::Value(json!("db.internal"))
        );
        assert_eq!(
            def("${resources.postgres-instance.default#modules.web.externals.db.outputs.port}")
                .unwrap(),
            Resolved::Value(json!(5432))
        );
        assert_eq!(
            def("${resources.postgres-instance#modules.web.externals.db.outputs.host}").unwrap(),
            Resolved::Value(json!("db.internal"))
        );
    }

    #[test]
    fn bracket_form() {
        assert_eq!(
            def("${resources['dns.public#shared.api-dns'].outputs.host}").unwrap(),
            Resolved::Value(json!("api.example.com"))
        );
        assert_eq!(
            def("${resources.postgres-instance.outputs.limits.tags[1]}").unwrap(),
            Resolved::Value(json!("b"))
        );
        assert_eq!(
            def("${resources.postgres-instance.outputs['limits'].cpu}").unwrap(),
            Resolved::Value(json!(2))
        );
    }

    #[test]
    fn implicit_types_resolve_to_their_fixed_ids() {
        assert_eq!(
            def("${resources.k8s-cluster.outputs.name}").unwrap(),
            Resolved::Value(json!("dev-cluster"))
        );
    }

    #[test]
    fn guresid_and_account() {
        assert_eq!(
            def("${resources.postgres-instance.guresid}").unwrap(),
            Resolved::Value(json!("01".repeat(16)))
        );
        let acct = resolve_string(
            "${resources['config.default#aws-account'].account}",
            Dialect::DriverAccount,
            &scope(),
            Sink::Plain,
            "driver_account",
        )
        .unwrap();
        assert_eq!(acct, Resolved::Value(json!("aws-dev")));
        assert!(matches!(
            def("${resources['config.default#aws-account'].account}")
                .unwrap_err()
                .kind,
            PlaceholderErrorKind::NotAllowed(_)
        ));
    }

    #[test]
    fn whole_value_keeps_json_type() {
        assert_eq!(
            def("${resources.postgres-instance.outputs.limits}").unwrap(),
            Resolved::Value(json!({"cpu": 2, "tags": ["a", "b"]}))
        );
        assert_eq!(
            def("l=${resources.postgres-instance.outputs.limits}").unwrap(),
            Resolved::Value(json!(r#"l={"cpu":2,"tags":["a","b"]}"#))
        );
        assert_eq!(
            def("p=${resources.postgres-instance.outputs.port}").unwrap(),
            Resolved::Value(json!("p=5432"))
        );
    }

    #[test]
    fn secret_in_value_input_fails() {
        let e = def("postgres://u:${resources.postgres-instance.outputs.password}@h").unwrap_err();
        assert_eq!(e.kind, PlaceholderErrorKind::SecretInNonSecret);
        assert_eq!(
            e.placeholder,
            "${resources.postgres-instance.outputs.password}"
        );
        assert!(
            e.to_string()
                .contains("a secret cannot be used in a non-secret input")
        );
    }

    #[test]
    fn composite_secret_is_kept_unevaluated() {
        let r = set(
            "postgresql://${externals.db.host}:${externals.db.password}@x",
            Sink::Secret,
        )
        .unwrap();
        let Resolved::Secret(expr) = &r else {
            panic!("{r:?}")
        };
        assert_eq!(
            expr.parts,
            vec![
                SecretPart::Literal("postgresql://db.internal:".into()),
                SecretPart::Secret(secret("pg-pass")),
                SecretPart::Literal("@x".into()),
            ]
        );
        let rendered = expr
            .render(|r| Ok::<_, ()>(format!("<{}>", r.reference)))
            .unwrap();
        assert_eq!(rendered, "postgresql://db.internal:<pg-pass>@x");
    }

    #[test]
    fn taint_propagates_through_objects_and_rejects_plain_sinks() {
        let v = json!({"a": "${values.page_size}", "b": ["${values.api_key}"], "c": 1});
        let r = resolve_value(&v, Dialect::Set, &scope(), Sink::Secret, "spec").unwrap();
        assert!(r.is_tainted());
        let Resolved::Object(items) = &r else {
            panic!()
        };
        assert_eq!(items[0], ("a".to_owned(), Resolved::Value(json!(20))));
        assert!(matches!(&items[1].1, Resolved::Array(_)));
        let e = resolve_value(&v, Dialect::Set, &scope(), Sink::Plain, "spec").unwrap_err();
        assert_eq!(e.location, "spec.b[0]");
        let plain = resolve_value(
            &json!({"a": ["${values.page_size}"]}),
            Dialect::Set,
            &scope(),
            Sink::Plain,
            "s",
        )
        .unwrap();
        assert_eq!(plain, Resolved::Value(json!({"a": [20]})));
    }

    #[test]
    fn shared_dns_host_in_a_variable() {
        assert_eq!(
            set("https://${shared.api-dns.host}/api", Sink::Plain).unwrap(),
            Resolved::Value(json!("https://api.example.com/api"))
        );
    }

    #[test]
    fn set_dialect_rejects_context_and_definition_rejects_set_roots() {
        assert!(matches!(
            set("${context.app.id}", Sink::Plain).unwrap_err().kind,
            PlaceholderErrorKind::NotAllowed(_)
        ));
        assert!(matches!(
            def("${externals.db.host}").unwrap_err().kind,
            PlaceholderErrorKind::UnknownRoot(_)
        ));
        assert!(matches!(
            set("${resources.db.host}", Sink::Plain).unwrap_err().kind,
            PlaceholderErrorKind::UnknownRoot(_)
        ));
    }

    #[test]
    fn pod_references_parse_and_need_target_support() {
        let t = Template::parse("${pod.status.podIP}").unwrap();
        assert_eq!(
            Expr::interpret(t.whole().unwrap(), Dialect::Set).unwrap(),
            Expr::Pod(vec!["status".into(), "podIP".into()])
        );
        assert!(matches!(
            set("${pod.status.podIP}", Sink::Plain).unwrap_err().kind,
            PlaceholderErrorKind::Unresolvable(_)
        ));
    }

    #[test]
    fn syntax_errors() {
        for bad in [
            "${",
            "${}",
            "${a..b}",
            "${a[}",
            "${a[x]}",
            "x ${resources.foo}",
        ] {
            assert!(def(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn collects_references() {
        let v = json!({"host": "${resources.postgres-instance.outputs.host}", "n": ["${context.app.id}"]});
        let found = collect_placeholders(&v, Dialect::Definition, "values").unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, "values.host");
        assert!(
            matches!(&found[0].2, Expr::Resource { target, .. } if target.ty.as_str() == "postgres-instance")
        );
    }

    proptest! {
        #[test]
        fn text_without_placeholders_is_unchanged(s in "[^$]*") {
            prop_assert_eq!(def(&s).unwrap(), Resolved::Value(json!(s)));
        }

        #[test]
        fn escapes_round_trip(parts in proptest::collection::vec("[a-z ]{0,5}", 0..5)) {
            let input = parts.join("$\\{");
            prop_assert_eq!(def(&input).unwrap(), Resolved::Value(json!(parts.join("${"))));
        }

        #[test]
        fn composite_is_concatenation(prefix in "[a-z/:]{0,8}", mid in "[a-z-]{0,8}", suffix in "[a-z]{0,8}") {
            let input = format!("{prefix}${{context.app.id}}{mid}${{context.env.id}}{suffix}");
            prop_assert_eq!(def(&input).unwrap(), Resolved::Value(json!(format!("{prefix}shop{mid}dev{suffix}"))));
        }

        #[test]
        fn secret_taint_is_preserved(prefix in "[a-z]{0,8}", suffix in "[a-z]{0,8}") {
            let input = format!("{prefix}${{values.api_key}}{suffix}");
            let r = set(&input, Sink::Secret).unwrap();
            prop_assert!(r.is_tainted());
            prop_assert!(set(&input, Sink::Plain).is_err());
            let Resolved::Secret(e) = r else { unreachable!() };
            prop_assert_eq!(e.render(|_| Ok::<_, ()>("S".into())).unwrap(), format!("{prefix}S{suffix}"));
        }
    }
}

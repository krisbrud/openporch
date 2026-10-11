//! `openporch/template`: renders Jinja2-style templates from
//! `driver_inputs.values.templates` into outputs, secrets, manifests and a cookie.
//!
//! The `init` template renders first, to a YAML map available to the others as
//! `init`. Then `outputs`, `secrets`, `manifests` and `cookie` render against
//! `driver` (`values` and `secrets`), `resource` (the resolved resource inputs),
//! `init`, `cookie` (the previous cookie, or an empty map) and `guresid`. A value
//! the `cookie` template stores is available to the next deployment, so values
//! generated once, such as passwords, stay stable.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use minijinja::{Environment, Error, ErrorKind, UndefinedBehavior};
use rand::Rng as _;
use rand::distributions::Alphanumeric;
use serde_json::{Map, Value, json};

use crate::{
    Cookie, DeleteOutcome, Driver, DriverError, DriverFuture, DriverInfo, DriverOutputs,
    DriverRequest, Manifest, Observation, Plan, PutOutcome, any_object, stateless,
};

/// The template keys, in render order after `init`.
const KEYS: [&str; 5] = ["init", "outputs", "secrets", "manifests", "cookie"];

/// The cookie format this driver writes: the rendered `cookie` map as JSON.
const COOKIE_VERSION: u32 = 1;

/// Template instructions allowed per render, so a template cannot loop forever.
const FUEL: u64 = 1_000_000;

/// The most a template may render, in bytes.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Manifest locations: the environment namespace, cluster scope, or merged into
/// the workload's pod template.
const LOCATIONS: [&str; 4] = ["namespace", "cluster", "container", "volume"];

/// The longest string `random_alphanumeric` returns.
const MAX_RANDOM_LENGTH: usize = 1024;

/// The error code for template failures.
const CODE: &str = "TPL-001";

/// Renders `driver_inputs.values.templates`.
#[derive(Debug)]
pub struct Template {
    info: DriverInfo,
    env: Environment<'static>,
}

impl Template {
    pub fn new() -> Self {
        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::Strict);
        env.set_fuel(Some(FUEL));
        env.add_function("random_alphanumeric", random_alphanumeric);
        env.add_filter("b64encode", b64encode);
        env.add_filter("b64decode", b64decode);
        let template = |description: &str| json!({"type": "string", "description": description});
        Self {
            info: DriverInfo {
                id: "openporch/template".into(),
                protocol_version: "builtin".into(),
                cookie_version: COOKIE_VERSION,
                input_schema: json!({
                    "type": "object",
                    "required": ["templates"],
                    "properties": {
                        "templates": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "init": template("YAML map available to the other templates as `init`"),
                                "outputs": template("YAML map of output values"),
                                "secrets": template("YAML map of output secrets"),
                                "manifests": template("YAML map of name to `{location, data}`"),
                                "cookie": template("YAML map passed to the next deployment as `cookie`"),
                            }
                        }
                    }
                }),
                output_schema: any_object(),
                capabilities: stateless(),
            },
            env,
        }
    }

    /// Renders every template for `req`.
    fn render(&self, req: &DriverRequest) -> Result<(DriverOutputs, Option<Cookie>), DriverError> {
        let templates = templates(req)?;
        let previous = match &req.cookie {
            None => Map::new(),
            Some(c) => match serde_json::from_str(&c.data) {
                Ok(Value::Object(m)) if c.version == COOKIE_VERSION => m,
                _ => {
                    return Err(invalid(format!(
                        "cannot read the version {} cookie from a previous deployment",
                        c.version
                    )));
                }
            },
        };
        let mut resource = req.resource.clone();
        resource.extend(req.resource_secrets.expose().clone());
        let mut ctx = json!({
            "driver": {"values": req.values, "secrets": req.secrets.expose()},
            "resource": resource,
            "init": {},
            "cookie": previous,
            "guresid": req.guresid.as_str(),
        });
        let mut rendered = Map::new();
        for key in KEYS {
            let Some(source) = templates.get(key) else {
                continue;
            };
            let map = self.render_map(key, source, &ctx)?;
            if key == "init" {
                ctx["init"] = Value::Object(map.clone());
            }
            rendered.insert(key.to_owned(), Value::Object(map));
        }
        let mut take = |key: &str| match rendered.remove(key) {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        let values = take("outputs");
        let secrets = take("secrets");
        let manifests = take("manifests")
            .into_values()
            .enumerate()
            .map(|(i, m)| manifest(i + 1, m))
            .collect::<Result<_, _>>()?;
        let cookie = match templates.contains_key("cookie") {
            true => Some(Cookie::new(
                COOKIE_VERSION,
                Value::Object(take("cookie")).to_string(),
            )?),
            false => None,
        };
        Ok((
            DriverOutputs {
                values,
                secrets: crate::SecretMap::new(secrets),
                manifests,
            },
            cookie,
        ))
    }

    /// Renders one template to a YAML map. Errors name the template and line but
    /// never quote rendered text, which may hold secrets.
    fn render_map(
        &self,
        key: &str,
        source: &str,
        ctx: &Value,
    ) -> Result<Map<String, Value>, DriverError> {
        let mut out = Limited(Vec::new());
        self.env
            .template_from_named_str(key, source)
            .and_then(|t| t.render_captured_to(minijinja::value::Serde(ctx), &mut out))
            .map_err(|e| invalid(describe(key, &e)))?;
        let text = String::from_utf8(out.0)
            .map_err(|_| invalid(format!("the `{key}` template rendered invalid UTF-8")))?;
        if text.trim().is_empty() {
            return Ok(Map::new());
        }
        match serde_yaml_ng::from_str::<Value>(&text) {
            Ok(Value::Object(m)) => Ok(m),
            Ok(Value::Null) => Ok(Map::new()),
            Ok(_) => Err(invalid(format!(
                "the `{key}` template must render a YAML map"
            ))),
            Err(e) => Err(invalid(match e.location() {
                Some(l) => format!(
                    "the `{key}` template rendered invalid YAML at line {}, column {}",
                    l.line(),
                    l.column()
                ),
                None => format!("the `{key}` template rendered invalid YAML"),
            })),
        }
    }
}

impl Default for Template {
    fn default() -> Self {
        Self::new()
    }
}

impl Driver for Template {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn plan<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Plan> {
        Box::pin(async move {
            // Rendering checks the templates; the driver itself creates nothing.
            self.render(req)?;
            Ok(Plan::NoEffects)
        })
    }

    fn plan_delete<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, Plan> {
        Box::pin(async { Ok(Plan::NoEffects) })
    }

    fn put<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, PutOutcome> {
        Box::pin(async move {
            let (outputs, cookie) = self.render(req)?;
            Ok(PutOutcome::Ready { outputs, cookie })
        })
    }

    fn get<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Observation> {
        Box::pin(async move {
            let (outputs, cookie) = self.render(req)?;
            Ok(Observation::Ready { outputs, cookie })
        })
    }

    fn delete<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, DeleteOutcome> {
        Box::pin(async { Ok(DeleteOutcome::Deleted) })
    }
}

/// The `templates` map, checked.
fn templates(req: &DriverRequest) -> Result<BTreeMap<String, String>, DriverError> {
    let templates = match req.values.get("templates") {
        Some(Value::Object(t)) => t.clone(),
        Some(_) => return Err(invalid("`templates` must be a map")),
        None => return Err(invalid("`driver_inputs.values.templates` is required")),
    };
    let mut out = BTreeMap::new();
    for (key, source) in templates {
        if !KEYS.contains(&key.as_str()) {
            return Err(invalid(format!(
                "unknown template `{key}`; templates are {}",
                KEYS.map(|k| format!("`{k}`")).join(", ")
            )));
        }
        let Value::String(source) = source else {
            return Err(invalid(format!("the `{key}` template must be a string")));
        };
        out.insert(key, source);
    }
    Ok(out)
}

/// The `n`th manifest. Errors give its position: its name is rendered text.
fn manifest(n: usize, m: Value) -> Result<Manifest, DriverError> {
    let Value::Object(mut m) = m else {
        return Err(invalid(format!(
            "manifest {n} must be a map with `location` and `data`"
        )));
    };
    let location = match m.remove("location") {
        Some(Value::String(l)) if LOCATIONS.contains(&l.as_str()) => l,
        _ => {
            return Err(invalid(format!(
                "manifest {n} needs a `location` of {}",
                LOCATIONS.map(|l| format!("`{l}`")).join(", ")
            )));
        }
    };
    match m.remove("data") {
        Some(data @ Value::Object(_)) => Ok(Manifest { location, data }),
        _ => Err(invalid(format!("manifest {n} needs a `data` map"))),
    }
}

/// A buffer that refuses to grow past [`MAX_OUTPUT_BYTES`].
struct Limited(Vec<u8>);

impl std::io::Write for Limited {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.0.len() + buf.len() > MAX_OUTPUT_BYTES {
            return Err(std::io::Error::other("template output too large"));
        }
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A template error's kind and position, without the template's values.
fn describe(key: &str, e: &Error) -> String {
    let at = e
        .line()
        .map_or_else(String::new, |l| format!(" at line {l}"));
    let what = match e.kind() {
        ErrorKind::UndefinedError => "uses an undefined value".to_owned(),
        ErrorKind::OutOfFuel => "ran too long".to_owned(),
        ErrorKind::WriteFailure => format!("rendered more than {MAX_OUTPUT_BYTES} bytes"),
        ErrorKind::SyntaxError => "has a syntax error".to_owned(),
        // Only the template functions' own messages are shown: others may quote
        // values.
        ErrorKind::InvalidOperation
            if e.detail().is_some_and(|d| {
                d.starts_with("random_alphanumeric") || d.starts_with("b64decode")
            }) =>
        {
            e.detail().unwrap_or_default().to_owned()
        }
        kind => format!("failed: {kind}"),
    };
    format!("the `{key}` template {what}{at}")
}

fn invalid(message: impl Into<String>) -> DriverError {
    DriverError::Invalid {
        code: Some(CODE.into()),
        message: message.into(),
    }
}

/// `random_alphanumeric(length)`: a random string of letters and digits.
fn random_alphanumeric(length: usize) -> Result<String, Error> {
    if !(1..=MAX_RANDOM_LENGTH).contains(&length) {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            format!("random_alphanumeric takes a length from 1 to {MAX_RANDOM_LENGTH}"),
        ));
    }
    Ok(rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect())
}

fn b64encode(s: String) -> String {
    STANDARD.encode(s)
}

fn b64decode(s: String) -> Result<String, Error> {
    let bytes = STANDARD
        .decode(s.trim())
        .map_err(|_| Error::new(ErrorKind::InvalidOperation, "b64decode: not base64"))?;
    String::from_utf8(bytes)
        .map_err(|_| Error::new(ErrorKind::InvalidOperation, "b64decode: not UTF-8 text"))
}

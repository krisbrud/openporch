# The `openporch/template` driver

The template driver renders templates into a resource's outputs, secrets and
Kubernetes manifests. It creates nothing itself. Use it to derive outputs from other
resources, generate values such as passwords once and keep them, or emit manifests
for a workload.

Templates use the [MiniJinja](https://docs.rs/minijinja) dialect of Jinja2. Humanitec's
template driver uses Go templates, so examples written for it need translating. The
[table below](#translating-go-templates) covers the common constructs.

## Inputs

All templates go under `driver_inputs.values.templates`. Each template is a string
that must render a YAML map. Every template is optional.

| Template | Renders | Becomes |
| --- | --- | --- |
| `init` | A YAML map | `init`, available to the other templates |
| `outputs` | A YAML map | The resource's output values |
| `secrets` | A YAML map | The resource's output secrets |
| `manifests` | A YAML map from name to `{location, data}` | Kubernetes manifests |
| `cookie` | A YAML map | `cookie` in the next deployment of the same resource |

`init` renders first. The other templates render after it, in the order shown, and
can all use these variables:

| Variable | Contents |
| --- | --- |
| `driver.values` | The definition's resolved `driver_inputs.values`, including `templates` |
| `driver.secrets` | The definition's resolved `driver_inputs.secrets` and `secret_refs` |
| `resource` | The resource's resolved inputs, including those that resolved from secrets |
| `init` | The rendered `init` map, or an empty map |
| `cookie` | The `cookie` map from the previous deployment, or an empty map |
| `guresid` | The resource's GUResID, a stable 32-character hexadecimal ID |

Placeholders such as `${resources.db.outputs.host}` are resolved before rendering,
like in any other driver input.

### Manifests

Each entry in the `manifests` map is a manifest. `location` is `namespace` for an
object in the workload's namespace, `cluster` for a cluster-scoped object, or
`container` or `volume` for a container or volume merged into the workload's pod
template. `data` is the object itself. The entry's name only identifies the
manifest within the map. Kubernetes deployment arrives in a later milestone; until
then manifests are returned as outputs but not applied.

```yaml
deployment.yaml:
  location: namespace
  data:
    apiVersion: apps/v1
    kind: Deployment
    metadata:
      name: {{ resource.name }}
```

## Functions and filters

Templates have the MiniJinja built-in filters and tests, such as `default`, `lower`,
`replace`, `join`, `items` and `tojson`, plus:

| Name | Use |
| --- | --- |
| `random_alphanumeric(n)` | A random string of `n` letters and digits, from 1 to 1024 |
| `b64encode` | Base64-encodes a string |
| `b64decode` | Decodes a base64 string to text |

Use `| tojson` to write any value into YAML safely. It quotes strings that would
otherwise change the YAML, such as a password that starts with `*` or contains `: `.

## Keeping generated values

A template that generates a value, such as a password, would generate a new one
every time it renders: on every deployment, and when openporch observes the
resource. To keep it, store it in `cookie` and read it back in `init`:

```yaml
id: shop-db-password
type: postgres
driver_type: openporch/template
driver_inputs:
  values:
    templates:
      init: |
        password: {{ cookie.password | default(random_alphanumeric(24)) | tojson }}
      outputs: |
        host: {{ resource.name }}.db.internal
        port: 5432
      secrets: |
        password: {{ init.password | tojson }}
      cookie: |
        password: {{ init.password | tojson }}
```

The first deployment generates the password and stores it in the cookie. Later
deployments of the same resource read it back, so the password stays the same.
Cookies are limited to 10 KB.

## Errors

A template that fails, or renders something that is not a YAML map, fails the
resource with error code `TPL-001`. The message names the template and the line, but
never quotes rendered text, since it may contain secrets. Manifests are named by
their position for the same reason.

Referring to an undefined variable is an error. Use `default` or `is defined` for
optional values. `default` only covers the last lookup: `cookie.password |
default("x")` works when the cookie has no `password`, but `cookie.db.password |
default("x")` fails when it has no `db`. Booleans render as `True` and `False`,
which YAML reads as booleans; use `| tojson` to get `true` and `false`.

## Limits

Templates are code that administrators write, since only administrators can create
definitions. A template that runs too long, such as nested loops over large ranges,
is stopped, and each template may render at most 1 MB. Neither limit bounds the
memory a template uses while it runs, so treat a definition's templates with the
same care as any other code that runs on the server.

## Translating Go templates

| Go template | MiniJinja |
| --- | --- |
| `{{ .driver.values.name }}` | `{{ driver.values.name }}` |
| `{{ .resource.name }}`, `{{ .init.x }}`, `{{ .cookie.x }}` | `{{ resource.name }}`, `{{ init.x }}`, `{{ cookie.x }}` |
| `{{ .x \| default "y" }}` | `{{ x \| default("y") }}` |
| `{{ randAlphaNum 16 }}` | `{{ random_alphanumeric(16) }}` |
| `{{ .x \| b64enc }}`, `{{ .x \| b64dec }}` | `{{ x \| b64encode }}`, `{{ x \| b64decode }}` |
| `{{ .x \| toRawJson }}` | `{{ x \| tojson }}` |
| `{{ .x \| quote }}` | `{{ x \| string \| tojson }}` |
| `{{ .x \| lower }}`, `{{ .x \| upper }}` | `{{ x \| lower }}`, `{{ x \| upper }}` |
| `{{ printf "%s-%s" .a .b }}` | `{{ a ~ "-" ~ b }}` |
| `{{ .x \| trunc 10 }}` | `{{ x[:10] }}` |
| `{{ hasKey .m "k" }}` | `{{ "k" in m }}` |
| `{{ $x := .a }}` | `{% set x = a %}` |
| `{{ if .x }}…{{ else }}…{{ end }}` | `{% if x %}…{% else %}…{% endif %}`; when `x` may be missing, `{% if x is defined and x %}` |
| `{{ if and .a (not .b) }}` | `{% if a and not b %}` |
| `{{ if eq .a "b" }}` | `{% if a == "b" %}` |
| `{{ range .list }}{{ . }}{{ end }}` | `{% for item in list %}{{ item }}{% endfor %}` |
| `{{ range $k, $v := .map }}` | `{% for k, v in map \| items %}` |
| `{{- x -}}` | `{{- x -}}` (whitespace control is the same) |
| `{{/* comment */}}` | `{# comment #}` |

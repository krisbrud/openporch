# External HTTP drivers

An external driver is an HTTPS service that provisions one kind of resource. Register
it with `POST /orgs/{org}/resources/drivers` (see [the API reference](../api.md)), and
definitions can then use its ID as `driver_type`. openporch calls it with the
`openporch-http/1` protocol described here.

The adapter is tested against the fixtures in
[`crates/drivers/fixtures/http`](../../crates/drivers/fixtures/http).

## Registration

```json
{
  "id": "acme/vault-db",
  "target": "https://drivers.acme.example/vault-db",
  "token": {"value": "drv_7d1e"},
  "protocol_version": "openporch-http/1",
  "cookie_version": 1,
  "inputs_schema": {"type": "object"},
  "outputs_schema": {"type": "object"},
  "capabilities": {
    "targets": {"resources": true, "workloads": false, "triggers": false},
    "observe": true, "recover": true, "preview": false, "cancel": false
  }
}
```

Only `id` and `target` are required. The ID is `<namespace>/<name>`; the `openporch`
namespace is reserved for built-in drivers. The target must be an `https` URL. The
`token` is a secret reference, or `{value}` to store; it is read from the secret
store only when a deployment calls the driver, sent only in the `Authorization`
header, and never shown in logs, errors or API responses. An error about it names
the driver and the reference.

`inputs_schema` and `outputs_schema` are recorded and returned for clients, but
the server does not check against them: it checks a resource's outputs against its
resource type's schema.

The registration pins the protocol version, `openporch-http/1` and the only one
supported, the cookie format version, `1` unless given, and the capabilities. The
targets say what the driver provisions; unless given, it provisions resources other
than workloads only. A definition may use the driver only for a type it targets:
otherwise the definition is refused when it is written, and a deployment before
anything runs. `triggers` is recorded but decides nothing, since triggers are
provisioned with their workload. The other capabilities are `false` unless given:

| Capability | When `true` |
| --- | --- |
| `targets.resources` | The driver provisions resources other than workloads. `true` unless given. |
| `targets.workloads` | The driver provisions workloads. |
| `targets.triggers` | The driver provisions workload triggers. Recorded only. |
| `observe` | `GET` reports the resource's real state. openporch observes a resource whose last outcome is unknown before it changes it again; without this, an operator establishes the outcome. |
| `recover` | A retried operation converges on the same effect, so a `5xx` to a `PUT` or `DELETE` is retried rather than left uncertain. See [below](#responses). |
| `preview` | Must be `false`: this protocol has no preview call. Changes to resources the driver already provisioned therefore have unknown impact and need a permit. |
| `cancel` | `POST {target}/{guresid}/cancel` stops a timed-out operation. |

Each call reads the registration, and its token, as they are then, so a
deployment never uses an earlier registration's target or token. A deployment
whose resources name a driver that is not registered is refused before anything
runs, naming the driver. A driver that cannot observe is reported as such
without its token being read. A registration cannot be deleted while a
definition or an active resource uses it, or while an operation it ran has an
outcome that is not known yet. A registration's token reference cannot name a
secret under `drivers/{id}/` that it did not write itself, so a driver registered
again under a deleted one's ID never inherits its token. openporch calls only
`https` targets, so the token never travels in plain text.

Driver targets are not limited to public addresses: registering one is an
administrator's decision, and drivers commonly run on internal hosts. A target
on a loopback or link-local address, such as a cloud metadata endpoint, is
called like any other. Registrations made before `targets` existed provision
resources other than workloads; register a driver again with
`targets.workloads` to use it for workloads.

The request and response bodies follow the shapes of Humanitec's Driver API 0.9.3, so
existing drivers need few changes. Full wire compatibility is not promised: the
differences are listed [below](#differences-from-humanitecs-driver-api-093).

## Requests

openporch calls the driver's target URL with the resource's GUResID appended:

| Call | Request | Purpose |
| --- | --- | --- |
| Provision | `PUT {target}/{guresid}` with a body | Create or update the resource |
| Observe | `GET {target}/{guresid}` | Report the resource's state |
| Remove | `DELETE {target}/{guresid}` | Delete the resource |
| Cancel | `POST {target}/{guresid}/cancel` | Stop a timed-out operation, for drivers that declare the `cancel` capability |

The target must not contain credentials, a query or a fragment. The GUResID is a
stable 32-character hexadecimal ID. Key everything the driver creates by it.

Every request carries these headers:

| Header | Value |
| --- | --- |
| `Authorization` | `Bearer <token>`, when the driver was registered with a `token` |
| `Openporch-Protocol` | `openporch-http/1` |
| `Openporch-Operation-Id` | The durable ID of the operation. A retry of the same operation has the same ID. |
| `Openporch-Input-Fingerprint` | A hash of the operation's inputs |
| `Openporch-Driver-Cookie` | The cookie from the previous call for this GUResID, if any |

The `PUT` body is:

```json
{
  "type": "postgres",
  "resource": {"name": "shop"},
  "driver": {
    "values": {"version": "16"},
    "secrets": {"master_password": "…"}
  }
}
```

`resource` holds the resource's resolved inputs, including those that resolved from
secrets; a secret input replaces a plain input of the same name. `driver.values` and `driver.secrets` hold the definition's resolved
`driver_inputs`. Secret values are resolved immediately before the call and travel
only in the request body.

## Responses

A ready resource is returned as:

```json
{
  "id": "0123456789abcdef0123456789abcdef",
  "type": "postgres",
  "resource": {
    "values": {"host": "pg.internal", "port": 5432},
    "secrets": {"password": "…"}
  },
  "manifests": [{"location": "namespace", "data": {"kind": "ConfigMap"}}]
}
```

`id` must be the GUResID. `manifests` is optional.

A driver may return a cookie with any response in the
`Set-Openporch-Driver-Cookie` header, and openporch sends it back with the next call
for the same GUResID. A cookie is `<version>.<data>`, where `version` is the cookie
format version the driver declares and `data` is base64url without padding. Cookies
are limited to 10 KB of data. A response may carry `Openporch-Protocol`; if it names
another protocol version, the response is rejected.

A cookie that comes with a failed `PUT` or `DELETE`, such as a `409` or `5xx`, is
kept too. A new cookie shows that the driver accepted the mutation, so the
failure leaves the outcome uncertain; the cookie is stored, sealed, on the
resource, and the `GET` that recovers the outcome and every later call send it.
A failure without a cookie leaves the stored one as it is. A cookie that is
malformed, too large or in another version cannot be kept, so the outcome is
uncertain whatever the status.

| Status | `PUT` | `GET` | `DELETE` |
| --- | --- | --- | --- |
| `200` | Ready, with the body above | Ready, with the body above | Deleted |
| `202` | Accepted; openporch polls `GET` | Still in progress | Accepted; openporch polls `GET` until `404` |
| `204` | Ready, with no outputs | | Deleted |
| `400`, `422` | Invalid inputs; fails without retry | The last operation failed | Fails without retry |
| `401`, `403` | Fails without retry | Fails without retry | Fails without retry |
| `404` | Uncertain | Absent | Already deleted |
| `409` | A conflicting operation; openporch observes before mutating again | A conflict | Same as `PUT` |
| `429` | May be retried after `Retry-After` | May be retried | May be retried |
| `5xx` | Uncertain, unless the driver deduplicates operations | May be retried | Uncertain, unless the driver deduplicates operations |

Any other status to a `PUT` or `DELETE` leaves the outcome uncertain; to a `GET`, it
fails the call. A `404` counts as absent or deleted only when the response carries
`Openporch-Protocol`, so a proxy or a mistyped target cannot make openporch forget a
live resource; without it the outcome is unknown.

`202` and `429` responses may carry `Retry-After` in seconds; a `202` without it is
polled after 10 seconds.

Error responses carry `{"error": "RES-101", "message": "…"}`. A `400` fails the
resource with that code and message, shown in the deployment. Any secret value sent
in the request is replaced with `[REDACTED]` in the code and message, but drivers
should not echo inputs in messages at all.

A `5xx` to a `PUT` or `DELETE` may mean the driver acted before failing. openporch
retries it only when the driver declares that it recovers retried operations: the
same `Openporch-Operation-Id` and fingerprint must converge on the same object, and
a changed fingerprint under a known operation ID must get `409`. Otherwise the
outcome is uncertain and must be established before another mutation. A request
that never connected, including one that could not connect within 30 seconds, is
always safe to retry.

## Differences from Humanitec's Driver API 0.9.3

| Area | openporch |
| --- | --- |
| Request and response bodies | The same shapes |
| Cookies | `Openporch-Driver-Cookie` and `Set-Openporch-Driver-Cookie`, versioned as `<version>.<base64url data>` |
| Operation identity | `Openporch-Operation-Id` and `Openporch-Input-Fingerprint` on every call |
| Protocol version | `Openporch-Protocol: openporch-http/1` on every request |
| Response `id` | Must equal the GUResID |
| Cancellation | `POST {target}/{guresid}/cancel`, for drivers that declare it |
| Cloud credentials | Not sent; a driver authenticates to its cloud itself |
| Redirects | Not followed |

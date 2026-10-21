# External HTTP drivers

An external driver is an HTTPS service that provisions one kind of resource. Register
it with `POST /orgs/{org}/resources/drivers` (see [the API reference](../api.md)), and
definitions can then use its ID as `driver_type`. openporch calls it with the
`openporch-http/1` protocol described here.

The adapter is implemented and tested against the fixtures in
[`crates/drivers/fixtures/http`](../../crates/drivers/fixtures/http), but deployments
do not call drivers until the deployment executor lands.

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

# API reference

openporch serves one organization over a JSON HTTP API. This page lists every
route with the permission it requires, and shows a request and response for each
group. The examples run in order against a fresh dev server in
`crates/server/tests/docs.rs`, so they stay correct as the API changes. See
[security.md](security.md) for roles, tokens, invitations and secret references.

## Conventions

- **Base URL.** The examples use paths only. A dev server started with `just dev`
  listens on `http://127.0.0.1:8080`. Its organization is `acme` in these examples.
- **Authentication.** Send `Authorization: Bearer <token>` with an API token or a
  login session. `$ADMIN_TOKEN` stands for the bootstrap administrator token, which
  the server writes to `<data_dir>/bootstrap-token` on first start.
- **Bodies.** Requests and responses are JSON. Unknown fields in a request are
  rejected with `400`. Times are milliseconds since the Unix epoch.
- **IDs.** Object IDs use lowercase letters, digits and single hyphens, start and
  end with a letter or digit, and are 2 to 50 characters long. IDs that contain a
  slash, such as `acme/queue` or `openporch/echo`, are percent-encoded in paths:
  `acme%2Fqueue`.
- **Visibility.** An application you hold no role on returns `404`, exactly like
  one that does not exist. An object you can see but not change returns `403`.
- **Secrets.** Secret values are accepted on write and never returned. Responses
  show secret references `{store, ref, version}` instead.
- **Contract.** Every response, errors included, carries
  `Openporch-Contract: 1`, the API contract version, as `GET /version` does.
  A request may send the contract it was written for in the same header; a
  server that does not serve it refuses the request with `400` `API-008`
  before doing anything. `porch` and the OpenTofu provider send it, and refuse
  a server whose responses name another contract.
- **Work in progress.** External secret store adapters are not implemented
  yet; where a route registers something the server cannot use yet, its
  section says so.

### Errors

Every error has the same shape:

```http
POST /orgs/acme/apps
Authorization: Bearer $ADMIN_TOKEN

{"id": "Shop"}
```

```http-response
400 Bad Request

{
  "error": "API-002",
  "message": "application ID `Shop` is invalid: IDs use lowercase letters, digits and single hyphens, start and end with a letter or digit, and are 2 to 50 characters long"
}
```

| Code | Status | Meaning |
| --- | --- | --- |
| `API-001` | 404 | Not found, or not visible to you. |
| `API-002` | 400 | An ID in the path or body is invalid. |
| `API-003` | 400 | The request is invalid. The message names the field. |
| `API-004` | 409 | Conflict: the object exists, is in use, or changed concurrently. |
| `API-005` | 405 | The route does not support this method. |
| `API-006` | 413 | The body is larger than `max_body_bytes`. |
| `API-007` | 429 | Too many requests. `Retry-After` gives the seconds to wait. |
| `API-008` | 400 | The request's `Openporch-Contract` header names a contract this server does not serve. Nothing was done. |
| `AUTH-001` | 401 | No valid token or session. |
| `AUTH-002` | 403 | Authenticated, but not allowed. The message names the missing role. |
| `SRV-001` | 500 | Internal error. The log has details under the request ID. |
| `SRV-002` | 503 | Not ready, for example because the master key is missing. |
| `SRV-003` | 502 | The identity provider could not be reached. |
| `DPL-001` | 409 | `approval_required`: a destructive or unknown-impact change has no permit. Preview and request one. |
| `DPL-002` | 409 | `migration_required`: the change moves a resource to another driver, cloud account or state backend. |
| `DPL-003` | 409 | A presented permit is unknown, expired, stale or already used. Preview again. |
| `DPL-004` | 409 | `recovery_required`: a resource's last operation has an unknown outcome. Recover the resource first. |
| `DPL-005` | 503 | A resource could not be observed. Nothing changed; recover it again. |
| `CLD-001` | 422 | The cloud refused a request, such as an account check. |
| `CLD-002` | 502 | The cloud could not be reached. |

### Permissions

The **Permission** column in each table below means:

| Permission | Who |
| --- | --- |
| public | Anyone, without a token. |
| authenticated | Any member. The handler applies further checks. |
| org read | Any member. |
| org read all | Administrators and org viewers. |
| manage members | Administrators and managers. |
| create apps | Administrators and managers. |
| administer | Administrators. |
| app read | Viewers, developers and owners of the application, administrators and org viewers. |
| app configure | Developers and owners of the application, and administrators. |
| app manage | Owners of the application, and administrators. |
| app deploy | App configure, plus `deployer` on the environment's type. |

## Health and version

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/livez` | public |
| GET | `/readyz`, `/healthz` | public |
| GET | `/version` | public |

`/livez` answers while the process runs. `/readyz` returns `503` with the reasons,
such as a missing master key, an unreachable database, no executor or a shutdown
in progress, while the server cannot accept changes; changes then get `503`
`SRV-002` and reads keep working. [Operating openporch](operations.md) lists the
reasons.

```http
GET /livez
```

```http-response
200 OK

{"status": "ok"}
```

```http
GET /readyz
```

```http-response
200 OK

{"status": "ready"}
```

```http
GET /version
```

```http-response
200 OK

{
  "version": "…",
  "contract": "1",
  "capabilities": {
    "authentication": ["token", "session"],
    "identity_providers": [],
    "drivers": []
  }
}
```

## Login

| Method | Path | Permission |
| --- | --- | --- |
| POST | `/auth/github/device` | public |
| POST | `/auth/github/token` | public |
| GET | `/auth/github/authorize` | public |
| GET | `/auth/github/callback` | public |
| POST | `/auth/github/session` | public |
| GET | `/orgs/{org}/current-user` | authenticated |
| DELETE | `/orgs/{org}/sessions/current` | authenticated |

GitHub login uses the device flow. `POST /auth/github/device` returns a
`device_code`, the `user_code` the person enters at `verification_uri`, and the
polling `interval`. The client then polls `POST /auth/github/token` with
`{"device_code": "…"}`, adding `"invitation_code"` on a first login. Polling returns
`202 {"status": "pending", "interval": 5}` until the person decides, then
`{"token", "user", "expires_at"}` for a 24-hour session. The server talks to GitHub
itself, so the GitHub access token never reaches the client. On a server without
GitHub configured, both routes return `404`:

```http
POST /auth/github/device
```

```http-response
404 Not Found

{"error": "API-001", "message": "GitHub login is not configured on this server"}
```

Browser login, for a web portal, uses GitHub's authorization-code flow and is
available when the server configures `[github.browser]` (see
[GitHub sign-in](security.md#browser-login)):

1. The portal sends the browser to `GET /auth/github/authorize?return_to=URL`,
   where `URL` is exactly one of the server's `return_urls`; it may be left out
   when only one is configured. The server answers `302` to GitHub, with a
   `state` and an `HttpOnly` cookie that binds the login to this browser.
2. GitHub returns the browser to `GET /auth/github/callback`. The callback needs
   the login's `state` and the cookie of the browser that started it, and it
   accepts each `state` once, within 10 minutes. Otherwise it answers `401`.
3. The callback answers `302` to the return URL with `login_code`, or with `error`:
   `access_denied` when the person declined, `login_failed` when GitHub did not
   confirm the sign-in, or `unavailable` when GitHub could not be reached.
4. Within 2 minutes, the portal exchanges the code once with
   `POST /auth/github/session` and `{"login_code": "…"}`, adding
   `"invitation_code"` on a first login. The answer is the same session as the
   device flow's. The server sends no CORS headers, so a portal on another
   origin exchanges the code from its own backend, which then keeps the session.

Logins in progress are kept in memory, so a restart abandons them. Without
`[github.browser]` the three routes return `404`. `/version` lists each provider
with its flows, such as `{"id": "github", "flows": ["device", "browser"]}`.

`current-user` shows who a token belongs to and their effective roles:

```http
GET /orgs/acme/current-user
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{
  "id": "bootstrap",
  "type": "service",
  "name": "Bootstrap administrator",
  "role": "administrator",
  "roles": {"org": "administrator", "apps": {}, "deployer": []}
}
```

`DELETE /orgs/{org}/sessions/current` ends the session that makes the request.
Tokens are revoked through the token routes instead.

## Members

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/users` | org read |
| POST | `/orgs/{org}/users` | manage members |
| GET | `/orgs/{org}/users/{user}` | org read |
| PATCH | `/orgs/{org}/users/{user}` | manage members |
| DELETE | `/orgs/{org}/users/{user}` | manage members |
| GET | `/orgs/{org}/invitations` | manage members |
| POST | `/orgs/{org}/invitations` | manage members |

Organization roles are `administrator`, `manager`, `member` and `orgViewer`. People
join through invitations. Service users, for CI and automation, are created
directly:

```http
POST /orgs/acme/users
Authorization: Bearer $ADMIN_TOKEN

{"id": "ci", "type": "service", "name": "CI", "role": "member"}
```

```http-response
201 Created

{
  "id": "ci",
  "type": "service",
  "name": "CI",
  "role": "member",
  "created_at": "…",
  "created_by": "bootstrap"
}
```

```http
GET /orgs/acme/users
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

[
  {"id": "bootstrap", "type": "service", "role": "administrator"},
  {"id": "ci", "type": "service", "role": "member"}
]
```

```http
GET /orgs/acme/users/ci
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{"id": "ci", "type": "service", "name": "CI", "role": "member"}
```

`PATCH` changes a member's `role`, a service user's display `name`, or both; a
person's name comes from their identity. Changing a
member needs authority over their current role, and a role change also over the
new one. The last administrator cannot be demoted or removed. Names are trimmed
and 1 to 100 characters long, on creation too.

```http
PATCH /orgs/acme/users/ci
Authorization: Bearer $ADMIN_TOKEN

{"role": "manager"}
```

```http-response
200 OK

{"id": "ci", "role": "manager"}
```

```http
PATCH /orgs/acme/users/ci
Authorization: Bearer $ADMIN_TOKEN

{"name": "CI deployments"}
```

```http-response
200 OK

{"id": "ci", "name": "CI deployments", "role": "manager"}
```

An invitation names a role and either an email address or an external identity. A
GitHub identity is given as the numeric account ID in `subject`, or as `login` when
GitHub login is configured, in which case the server resolves the login to the
account ID right away. The response carries the one-time `code` that the person
passes on their first login. It is shown only once. Invitations expire after seven
days.

```http
POST /orgs/acme/invitations
Authorization: Bearer $ADMIN_TOKEN

{"role": "member", "provider": "github", "subject": "583231"}
```

```http-response
201 Created

{
  "id": "…",
  "role": "member",
  "provider": "github",
  "subject": "583231",
  "status": "pending",
  "code": "…"
}
```

```http
GET /orgs/acme/invitations
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

[{"role": "member", "provider": "github", "subject": "583231", "status": "pending"}]
```

## API tokens

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/users/{user}/tokens` | authenticated |
| POST | `/orgs/{org}/users/{user}/tokens` | authenticated |
| GET | `/orgs/{org}/users/{user}/tokens/{token}` | authenticated |
| DELETE | `/orgs/{org}/users/{user}/tokens/{token}` | authenticated |

Everyone manages their own tokens. Administrators, and managers within their
delegation ceiling, issue tokens for service users. Nobody issues tokens for another
person. The token value appears only in the creation response. `expires_at` is
optional.

```http
POST /orgs/acme/users/ci/tokens
Authorization: Bearer $ADMIN_TOKEN

{"id": "deploy", "description": "CI deployments"}
```

```http-response
201 Created

{
  "id": "deploy",
  "description": "CI deployments",
  "expires_at": null,
  "created_at": "…",
  "created_by": "bootstrap",
  "last_used_at": null,
  "token": "$CI_TOKEN"
}
```

```http
GET /orgs/acme/users/ci/tokens/deploy
Authorization: Bearer $CI_TOKEN
```

```http-response
200 OK

{"id": "deploy", "description": "CI deployments", "last_used_at": "…"}
```

```http
GET /orgs/acme/users/ci/tokens
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

[{"id": "deploy", "description": "CI deployments"}]
```

## Environment types

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/env-types` | org read |
| POST | `/orgs/{org}/env-types` | administer |
| GET | `/orgs/{org}/env-types/{type}` | org read |
| PATCH | `/orgs/{org}/env-types/{type}` | administer |
| DELETE | `/orgs/{org}/env-types/{type}` | administer |
| GET | `/orgs/{org}/env-types/{type}/users` | org read |
| POST | `/orgs/{org}/env-types/{type}/users` | administer |
| DELETE | `/orgs/{org}/env-types/{type}/users/{user}` | administer |

Every organization has the `development` type. Holding the `deployer` role on a
type lets a developer deploy to, create and delete environments of that type and
change their values.

```http
POST /orgs/acme/env-types
Authorization: Bearer $ADMIN_TOKEN

{"id": "production", "description": "Live traffic"}
```

```http-response
201 Created

{"id": "production", "description": "Live traffic", "protected": false, "created_at": "…"}
```

```http
GET /orgs/acme/env-types
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

[
  {"id": "development", "description": "Default environment type"},
  {"id": "production", "description": "Live traffic"}
]
```

```http
GET /orgs/acme/env-types/production
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{"id": "production", "description": "Live traffic"}
```

`PATCH` changes a type's description:

```http
PATCH /orgs/acme/env-types/production
Authorization: Bearer $ADMIN_TOKEN

{"description": "Live customer traffic"}
```

```http-response
200 OK

{"id": "production", "description": "Live customer traffic"}
```

`protected`, given when the type is created or changed with `PATCH`, protects
the environments of the type. In them, a destructive-action
[permit](#previews-and-permits) for a `delete`, `replace` or `detach` needs an
administrator, even for a developer holding the `deployer` role, and a change of
unknown impact gets no permit at all: deploy it where its driver can plan it
first. Protecting a type rechecks the permits already issued there whenever a
deployment binds or uses one, retries included: a permit whose issuer is not an
administrator, and every `unknown` permit, stops holding (`DPL-003`), which can
fail a deployment already running.

```http
PATCH /orgs/acme/env-types/production
Authorization: Bearer $ADMIN_TOKEN

{"protected": true}
```

```http-response
200 OK

{"id": "production", "description": "Live customer traffic", "protected": true}
```

```http
POST /orgs/acme/env-types/production/users
Authorization: Bearer $ADMIN_TOKEN

{"id": "ci", "role": "deployer"}
```

```http-response
201 Created

{"user": "ci", "role": "deployer"}
```

```http
POST /orgs/acme/env-types/development/users
Authorization: Bearer $ADMIN_TOKEN

{"id": "ci", "role": "deployer"}
```

```http-response
201 Created

{"user": "ci", "role": "deployer"}
```

```http
GET /orgs/acme/env-types/production/users
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

[{"user": "ci", "role": "deployer"}]
```

## Applications

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/apps` | org read |
| POST | `/orgs/{org}/apps` | create apps |
| GET | `/orgs/{org}/apps/{app}` | app read |
| PATCH | `/orgs/{org}/apps/{app}` | app manage |
| DELETE | `/orgs/{org}/apps/{app}` | app manage |
| GET | `/orgs/{org}/apps/{app}/users` | app read |
| POST | `/orgs/{org}/apps/{app}/users` | app manage |
| PATCH | `/orgs/{org}/apps/{app}/users/{user}` | app manage |
| DELETE | `/orgs/{org}/apps/{app}/users/{user}` | app manage |

A new application gets a `development` environment, and its creator becomes its
`owner`. Application roles are `viewer`, `developer` and `owner`. The list shows
only the applications the caller can see.

Deleting an application needs `owner` on it and `deployer` on every
environment type it uses. The optional body is `{"permits": [...], "comment":
"...", "expected_generations": {...}}`, with the destructive-action permits for
deleting the resources of every environment, each from a `delete_env`
[preview](#previews-and-permits) of its environment; a missing one is `409`
`DPL-001`. `expected_generations` maps each environment to the generation its
preview was made at; one that moved on since, or an
environment it leaves out, is `409` `API-004`. Each environment's deletion is accepted only at the
generation its preflight saw. Each environment, `development` included, is then deleted
as by its own deletion (see [Environments](#environments)), and the response
is `202 Accepted` with each environment's deletion and what it leaves in
place. The application's `state` becomes `deleting`: it takes no new
environments or deployments, and it is removed with its sets, deltas, values
and roles once the last environment is gone. `removed` says whether that has
already happened, which it does at once for environments without resources.
If an environment's deletion fails, that environment stays in
`deletion_failed` and the application stays; repeating the request, with the
missing permits if that was the cause, retries every environment that is not
already being deleted.

```http
POST /orgs/acme/apps
Authorization: Bearer $ADMIN_TOKEN

{"id": "shop", "name": "Shop"}
```

```http-response
201 Created

{"id": "shop", "name": "Shop", "created_at": "…", "created_by": "bootstrap"}
```

```http
GET /orgs/acme/apps
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

[{"id": "shop", "name": "Shop"}]
```

```http
GET /orgs/acme/apps/shop
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{"id": "shop", "name": "Shop"}
```

`PATCH` renames an application; the ID stays. Names are trimmed and 1 to 100
characters long, on creation too. Renaming an application, or an environment, that
is being deleted is `409`:

```http
PATCH /orgs/acme/apps/shop
Authorization: Bearer $ADMIN_TOKEN

{"name": "Web shop"}
```

```http-response
200 OK

{"id": "shop", "name": "Web shop"}
```

```http
POST /orgs/acme/apps/shop/users
Authorization: Bearer $ADMIN_TOKEN

{"id": "ci", "role": "viewer"}
```

```http-response
201 Created

{"user": "ci", "role": "viewer"}
```

```http
PATCH /orgs/acme/apps/shop/users/ci
Authorization: Bearer $ADMIN_TOKEN

{"role": "developer"}
```

```http-response
200 OK

{"user": "ci", "role": "developer"}
```

```http
GET /orgs/acme/apps/shop/users
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

[{"user": "bootstrap", "role": "owner"}, {"user": "ci", "role": "developer"}]
```

## Environments

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/apps/{app}/envs` | app read |
| POST | `/orgs/{org}/apps/{app}/envs` | app configure, and `deployer` on the type |
| GET | `/orgs/{org}/apps/{app}/envs/{env}` | app read |
| PATCH | `/orgs/{org}/apps/{app}/envs/{env}` | app configure, and `deployer` on the type |
| DELETE | `/orgs/{org}/apps/{app}/envs/{env}` | app deploy |

An environment shows its runtime summary: the desired and last successful sets,
the latest deployment attempt, and `convergence`, which is `never_deployed`,
`converged`, `partial` or `unknown`. With `from_deploy_id`, creation clones a
deployment of the same application into a draft delta and returns its `delta_id`.
Nothing is deployed. The default `development` environment cannot be deleted while
its application exists (`409`).

Deleting an environment runs like a deployment, so the request needs a body
like a deployment's: `expected_generation`, the `permits` for deleting its
resources from a `delete_env` [preview](#previews-and-permits) and an optional
`comment`, with an optional `Idempotency-Key` header
that returns the same deletion to a retry while the environment exists.
Without the body the request is `400`, and a stale generation, a running
deployment or a resource that needs recovery is `409`. It returns `202` with
the deletion's `deployment`, whose `kind` is `delete_env`, and the resources it
leaves in place as `retained`: external ones, and adopted ones not scheduled
for deletion. An environment without resources is deleted at once, and the
deployment is already `succeeded`. Otherwise the environment is `deleting`
while the deletion deletes every resource it owns, and every adopted one
scheduled for deletion, in reverse dependency order, each behind its permit,
and then the environment with its deployments, values and deltas; the audit
log and tombstones remain, and retained resources get a `released` tombstone.
Once the environment is gone, the request is `404`. If a resource cannot be
deleted, the environment stays in state `deletion_failed` with what is left:
the failed resource has its error in `GET .../resources/graph`, the deletion's
error names it, and nothing can be deployed there. Fixing the cause, scheduling
an adopted resource for deletion or detaching a resource, and sending the
request again with the new generation retries the deletion with the same
resource identities.

```http
POST /orgs/acme/apps/shop/envs
Authorization: Bearer $CI_TOKEN

{"id": "production", "name": "Production", "type": "production"}
```

```http-response
201 Created

{
  "app": "shop",
  "id": "production",
  "name": "Production",
  "type": "production",
  "state": "active",
  "generation": 0,
  "desired_set_id": null,
  "last_successful_set_id": null,
  "last_successful_deploy_id": null,
  "latest_deployment": null,
  "convergence": "never_deployed"
}
```

```http
GET /orgs/acme/apps/shop/envs
Authorization: Bearer $CI_TOKEN
```

```http-response
200 OK

[{"id": "development", "type": "development"}, {"id": "production", "type": "production"}]
```

```http
GET /orgs/acme/apps/shop/envs/production
Authorization: Bearer $CI_TOKEN
```

```http-response
200 OK

{"id": "production", "generation": 0, "convergence": "never_deployed"}
```

`PATCH` renames an environment; its ID and type stay:

```http
PATCH /orgs/acme/apps/shop/envs/production
Authorization: Bearer $CI_TOKEN

{"name": "Production (EU)"}
```

```http-response
200 OK

{"id": "production", "name": "Production (EU)", "type": "production"}
```

## Deployment sets and deltas

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/apps/{app}/sets/{set}` | app read |
| POST | `/orgs/{org}/apps/{app}/sets/{set}` | app configure |
| GET | `/orgs/{org}/apps/{app}/sets/{set}/diff/{other}` | app read |
| GET | `/orgs/{org}/apps/{app}/deltas` | app read |
| POST | `/orgs/{org}/apps/{app}/deltas` | app configure |
| GET | `/orgs/{org}/apps/{app}/deltas/{delta}` | app read |
| PUT | `/orgs/{org}/apps/{app}/deltas/{delta}` | app configure |

A set is the immutable description of what runs in an environment: `modules`
(workloads by ID) and `shared` resources. Its ID is a hash of its content, so equal
sets have equal IDs. The empty set is `0`. Posting a delta to a set applies it and
returns the ID of the resulting set.

A delta has `modules.add` (workloads by ID), `modules.remove` (IDs),
`modules.update` (JSON Patch operations by workload ID) and `shared` (JSON Patch
operations on the shared map). They apply in the order remove, add, update,
shared, so a delta can remove a workload and add it back. Adding a workload that
exists, or removing or updating one that does not, fails with `400` naming it.

```http
POST /orgs/acme/apps/shop/sets/0
Authorization: Bearer $CI_TOKEN

{
  "modules": {
    "add": {
      "web": {
        "class": "default",
        "spec": {"containers": {"web": {
          "image": "registry.acme.example/shop/web:1.4.2",
          "variables": {"PAGE_SIZE": "10"}
        }}}
      }
    }
  }
}
```

```http-response
200 OK

{"id": "$SET_10"}
```

```http
GET /orgs/acme/apps/shop/sets/$SET_10
Authorization: Bearer $CI_TOKEN
```

```http-response
200 OK

{
  "id": "$SET_10",
  "modules": {
    "web": {
      "class": "default",
      "externals": {},
      "spec": {"containers": {"web": {
        "image": "registry.acme.example/shop/web:1.4.2",
        "variables": {"PAGE_SIZE": "10"}
      }}}
    }
  },
  "shared": {}
}
```

```http
POST /orgs/acme/apps/shop/sets/$SET_10
Authorization: Bearer $CI_TOKEN

{
  "modules": {
    "update": {
      "web": [{"op": "replace", "path": "/spec/containers/web/variables/PAGE_SIZE", "value": "20"}]
    }
  }
}
```

```http-response
200 OK

{"id": "$SET_20"}
```

The diff of two sets is the delta that turns the first into the second:

```http
GET /orgs/acme/apps/shop/sets/$SET_10/diff/$SET_20
Authorization: Bearer $CI_TOKEN
```

```http-response
200 OK

{
  "modules": {
    "update": {
      "web": [{"op": "replace", "path": "/spec/containers/web/variables/PAGE_SIZE", "value": "20"}]
    }
  }
}
```

Deltas are stored to be reviewed, changed and deployed later. A delta becomes
frozen when a deployment uses it, after which a `PUT` returns `409`.

```http
POST /orgs/acme/apps/shop/deltas
Authorization: Bearer $CI_TOKEN

{"modules": {"remove": ["web"]}}
```

```http-response
201 Created

{
  "id": "$DELTA",
  "env": null,
  "frozen": false,
  "created_by": "ci",
  "modules": {"remove": ["web"]}
}
```

A `POST` with a `workload` instead converts a workload document: an
openporch `Workload` or a Score file (with an optional `overlay`, a
`WorkloadOverlay` document), as YAML or JSON text. `overrides` supplies
deploy-time values: `images` by container ID (or `main` for an image
function), `artifacts` by function name (`main`), and `properties` by dotted
path into the document. A property value is read as YAML, except that it stays
a string where it replaces one. An image or artifact left as `.` must be
supplied. The delta adds the workload to the environment's desired set, as
here, or updates it there (an empty update when nothing changed, so redeploying the
same document yields the same set). `base_generation` is the environment
generation it was drafted against: deploying it requires that generation, so
a deployment accepted in between makes it stale (`409`) instead of being
overwritten.

```http
POST /orgs/acme/apps/shop/deltas
Authorization: Bearer $CI_TOKEN

{"env": "development", "workload": "apiVersion: openporch.dev/v1\nkind: Workload\nmetadata: {name: web}\ncontainers: {web: {image: .}}\n", "overrides": {"images": {"web": "ghcr.io/acme/web:abc123"}}}
```

```http-response
201 Created

{
  "id": "$WORKLOAD_DELTA",
  "env": "development",
  "base_generation": "…",
  "frozen": false,
  "created_by": "ci",
  "modules": {"add": {"web": {"class": "default", "spec": {"containers": {"web": {"image": "ghcr.io/acme/web:abc123"}}}}}}
}
```

```http
PUT /orgs/acme/apps/shop/deltas/$DELTA
Authorization: Bearer $CI_TOKEN

{"modules": {"update": {"web": [{"op": "replace", "path": "/class", "value": "large"}]}}}
```

```http-response
200 OK

{"id": "$DELTA", "frozen": false, "modules": {"update": {"web": [{"op": "replace", "path": "/class", "value": "large"}]}}}
```

```http
GET /orgs/acme/apps/shop/deltas/$DELTA
Authorization: Bearer $CI_TOKEN
```

```http-response
200 OK

{"id": "$DELTA", "frozen": false}
```

```http
GET /orgs/acme/apps/shop/deltas
Authorization: Bearer $CI_TOKEN
```

```http-response
200 OK

[{"id": "…", "frozen": false}, {"id": "…", "frozen": false}]
```

## Shared values

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/apps/{app}/values` | app read |
| POST | `/orgs/{org}/apps/{app}/values` | app configure, and `deployer` on every environment type in the app |
| GET | `/orgs/{org}/apps/{app}/values/versions` | app read |
| GET | `/orgs/{org}/apps/{app}/values/{key}` | app read |
| PUT | `/orgs/{org}/apps/{app}/values/{key}` | as POST |
| DELETE | `/orgs/{org}/apps/{app}/values/{key}` | as POST |
| GET | `/orgs/{org}/apps/{app}/envs/{env}/values` | app read |
| POST | `/orgs/{org}/apps/{app}/envs/{env}/values` | app configure, and `deployer` on the environment's type |
| GET | `/orgs/{org}/apps/{app}/envs/{env}/values/versions` | app read |
| GET | `/orgs/{org}/apps/{app}/envs/{env}/values/{key}` | app read |
| PUT | `/orgs/{org}/apps/{app}/envs/{env}/values/{key}` | as POST |
| DELETE | `/orgs/{org}/apps/{app}/envs/{env}/values/{key}` | as POST |

Workloads reference values as `${values.KEY}`. An environment value overrides the
application value with the same key, and requires one. A value holds either `value`
(plain text) or `secret_ref`: a reference `{store, ref, version?}`, or `{value}` to
write the secret to the primary store. Keys use letters, digits, `_` and `-`, and
start with a letter or `_`.

```http
POST /orgs/acme/apps/shop/values
Authorization: Bearer $CI_TOKEN

{"key": "API_URL", "description": "Public API", "value": "https://api.dev.acme.example"}
```

```http-response
201 Created

{
  "key": "API_URL",
  "description": "Public API",
  "value": "https://api.dev.acme.example",
  "is_secret": false,
  "updated_at": "…"
}
```

```http
POST /orgs/acme/apps/shop/envs/production/values
Authorization: Bearer $CI_TOKEN

{"key": "API_URL", "value": "https://api.acme.example"}
```

```http-response
201 Created

{"key": "API_URL", "value": "https://api.acme.example", "is_secret": false}
```

The secret value is written to the built-in store under the value's scope. Only the
reference comes back:

```http
POST /orgs/acme/apps/shop/values
Authorization: Bearer $CI_TOKEN

{"key": "API_TOKEN", "secret_ref": {"value": "tok_live_0f3c"}}
```

```http-response
201 Created

{
  "key": "API_TOKEN",
  "description": "",
  "secret_ref": {"store": "openporch", "ref": "apps/shop/values/API_TOKEN", "version": "1"},
  "is_secret": true,
  "updated_at": "…"
}
```

```http
PUT /orgs/acme/apps/shop/values/API_URL
Authorization: Bearer $CI_TOKEN

{"description": "Public API", "value": "https://api.staging.acme.example"}
```

```http-response
200 OK

{"key": "API_URL", "value": "https://api.staging.acme.example"}
```

```http
GET /orgs/acme/apps/shop/values/API_TOKEN
Authorization: Bearer $CI_TOKEN
```

```http-response
200 OK

{"key": "API_TOKEN", "is_secret": true, "secret_ref": {"store": "openporch", "ref": "apps/shop/values/API_TOKEN", "version": "1"}}
```

```http
GET /orgs/acme/apps/shop/envs/production/values
Authorization: Bearer $CI_TOKEN
```

```http-response
200 OK

[{"key": "API_URL", "value": "https://api.acme.example"}]
```

Every change records a value set version of its scope, listed newest first.
Deployments will record the versions they used.

```http
GET /orgs/acme/apps/shop/values/versions
Authorization: Bearer $CI_TOKEN
```

```http-response
200 OK

[
  {
    "id": "3",
    "values": {
      "API_TOKEN": {"description": "", "secret_ref": {"store": "openporch", "ref": "apps/shop/values/API_TOKEN", "version": "1"}},
      "API_URL": {"description": "Public API", "value": "https://api.staging.acme.example"}
    },
    "created_at": "…",
    "created_by": "ci"
  },
  {"id": "2"},
  {"id": "1"}
]
```

An application value that environments override cannot be deleted until the
overrides are:

```http
DELETE /orgs/acme/apps/shop/values/API_URL
Authorization: Bearer $CI_TOKEN
```

```http-response
409 Conflict

{"error": "API-004", "message": "value `API_URL` is overridden in environments `production`; delete the overrides first"}
```

```http
DELETE /orgs/acme/apps/shop/envs/production/values/API_URL
Authorization: Bearer $CI_TOKEN
```

```http-response
204 No Content
```

```http
GET /orgs/acme/apps/shop/envs/production/values/versions
Authorization: Bearer $CI_TOKEN
```

```http-response
200 OK

[{"id": "2", "values": {}}, {"id": "1", "values": {"API_URL": {"value": "https://api.acme.example"}}}]
```

## Resource types and classes

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/resources/types` | org read |
| POST | `/orgs/{org}/resources/types` | administer |
| GET | `/orgs/{org}/resources/types/{type}` | org read |
| PUT | `/orgs/{org}/resources/types/{type}` | administer |
| DELETE | `/orgs/{org}/resources/types/{type}` | administer |
| GET | `/orgs/{org}/resources/types/{type}/classes` | org read |
| POST | `/orgs/{org}/resources/types/{type}/classes` | administer |
| DELETE | `/orgs/{org}/resources/types/{type}/classes/{class}` | administer |
| GET | `/orgs/{org}/resources/classes` | org read |

Built-in types, such as `postgres`, `s3`, `sqs` and `workload`, are listed with
their input and output schemas. Custom types are named `<org>/<name>`. `use` is
`direct` (workloads request it), `indirect` (only other resources reference it) or
`implicit` (one per environment). Output schemas have `values` and `secrets`.
A resource whose driver returns outputs that miss a required output, have the
wrong JSON type, or put a secret output in `values` fails, and the error names
the output. When the driver may have changed infrastructure (it did not plan
"no changes"), the resource's outcome is `unknown` until recovery observes it;
if its outputs still do not match, it waits for an administrator
(`recovery_required`). A deployment checks against
the types as they were when it was accepted; a snapshot replay uses the schemas
it recorded. Snapshots from servers that did not record types cannot be
replayed; deploy them with `"replay": "current"`.
`PUT` replaces the whole type, so schemas left out reset to their defaults.

```http
POST /orgs/acme/resources/types
Authorization: Bearer $ADMIN_TOKEN

{
  "id": "acme/feature-flags",
  "name": "Feature flags",
  "use": "direct",
  "category": "config",
  "inputs_schema": {"type": "object", "properties": {"project": {"type": "string"}}},
  "outputs_schema": {
    "type": "object",
    "properties": {
      "values": {"type": "object", "properties": {"url": {"type": "string"}}},
      "secrets": {"type": "object", "properties": {"sdk_key": {"type": "string"}}}
    }
  }
}
```

```http-response
201 Created

{"id": "acme/feature-flags", "name": "Feature flags", "use": "direct", "category": "config"}
```

```http
PUT /orgs/acme/resources/types/acme%2Ffeature-flags
Authorization: Bearer $ADMIN_TOKEN

{
  "name": "Feature flags",
  "use": "direct",
  "category": "config",
  "inputs_schema": {"type": "object", "properties": {"project": {"type": "string"}}},
  "outputs_schema": {
    "type": "object",
    "properties": {
      "values": {"type": "object", "properties": {"url": {"type": "string"}}},
      "secrets": {"type": "object", "properties": {"sdk_key": {"type": "string"}}}
    }
  }
}
```

```http-response
200 OK

{"id": "acme/feature-flags", "name": "Feature flags"}
```

```http
GET /orgs/acme/resources/types/acme%2Ffeature-flags
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{"id": "acme/feature-flags", "use": "direct"}
```

Every type has the `default` class. More classes let workloads ask for variants:

```http
POST /orgs/acme/resources/types/postgres/classes
Authorization: Bearer $ADMIN_TOKEN

{"id": "large", "description": "More CPU and memory"}
```

```http-response
201 Created

{"id": "large", "resource_type": "postgres", "description": "More CPU and memory"}
```

```http
GET /orgs/acme/resources/types/postgres/classes
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

[
  {"id": "default", "resource_type": "postgres"},
  {"id": "large", "resource_type": "postgres"}
]
```

```http
GET /orgs/acme/resources/classes
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

[{"id": "large", "resource_type": "postgres"}]
```

## Drivers

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/resources/drivers` | org read |
| POST | `/orgs/{org}/resources/drivers` | administer |
| GET | `/orgs/{org}/resources/drivers/{driver}` | org read |
| PUT | `/orgs/{org}/resources/drivers/{driver}` | administer |
| DELETE | `/orgs/{org}/resources/drivers/{driver}` | administer |

The built-in drivers are `openporch/echo`, `openporch/none`, `openporch/template`
and `openporch/opentofu`. An external driver is reached over HTTPS at its
`target`, with its `token` sent as a bearer token; the token is a secret reference
or `{value}`, and is read only when a deployment calls the driver.
[docs/drivers/http.md](drivers/http.md) describes the protocol. Definitions may name
a registered driver as `driver_type`, and deployments call it.

A registration pins what the server relies on when it calls the driver:

| Field | Default | Meaning |
| --- | --- | --- |
| `protocol_version` | `openporch-http/1` | The protocol the driver speaks; `openporch-http/1` is the only one supported |
| `cookie_version` | `1` | The cookie format version the driver writes |
| `inputs_schema` | none | JSON Schema of the driver inputs, recorded for clients; the server does not check definitions' inputs against it |
| `outputs_schema` | none | JSON Schema of the driver's outputs, recorded for clients; the server checks outputs against the resource type's schema, not this one |
| `capabilities.targets.resources` | `true` | The driver provisions resources other than workloads |
| `capabilities.targets.workloads` | `false` | The driver provisions workloads |
| `capabilities.targets.triggers` | `false` | The driver provisions workload triggers; recorded only, since no resource type is a trigger |
| `capabilities.observe` | `false` | `GET` reports the resource's real state, so an unknown outcome can be recovered by observing it |
| `capabilities.recover` | `false` | A retried operation converges, so a `5xx` to a mutation may be retried |
| `capabilities.preview` | `false` | Must be `false`: `openporch-http/1` has no preview call, so changes to existing resources need a permit |
| `capabilities.cancel` | `false` | `POST {target}/{guresid}/cancel` stops a timed-out operation |

The deployment executor reads the registration each time it calls the driver, so
a deployment uses the registration as it is then. A definition whose type the
driver does not target is refused with `400` when it is created or changed, and a
deployment whose resources use a driver that is no longer registered, or no longer
targets their type, is refused with `400` before anything runs.

```http
POST /orgs/acme/resources/drivers
Authorization: Bearer $ADMIN_TOKEN

{"id": "acme/vault-db", "target": "https://drivers.acme.example/vault-db", "token": {"value": "drv_7d1e"}, "capabilities": {"observe": true}}
```

```http-response
201 Created

{
  "id": "acme/vault-db",
  "builtin": false,
  "target": "https://drivers.acme.example/vault-db",
  "token": {"store": "openporch", "ref": "drivers/acme/vault-db/token", "version": "1"},
  "protocol_version": "openporch-http/1",
  "cookie_version": 1,
  "capabilities": {
    "targets": {"resources": true, "workloads": false, "triggers": false},
    "observe": true, "recover": false, "preview": false, "cancel": false
  }
}
```

```http
GET /orgs/acme/resources/drivers/acme%2Fvault-db
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{
  "id": "acme/vault-db",
  "builtin": false,
  "target": "https://drivers.acme.example/vault-db",
  "token": {"store": "openporch", "ref": "drivers/acme/vault-db/token", "version": "1"},
  "protocol_version": "openporch-http/1",
  "cookie_version": 1,
  "capabilities": {
    "targets": {"resources": true, "workloads": false, "triggers": false},
    "observe": true, "recover": false, "preview": false, "cancel": false
  }
}
```

`PUT` replaces a registration in place, so a new token or target does not mean
deleting the definitions that use the driver. It takes the body of `POST` without
`id` (or with the same `id`), checked the same way. `protocol_version`,
`cookie_version` and `capabilities` left out keep their current values; a given
`capabilities` object replaces them, with its own fields' defaults. The schemas
and a `token` left out are removed. `{value}` writes a new version of the token,
and the reference the registration holds now, given back unchanged, keeps it;
any other secret under `drivers/{id}/`, such as an earlier version, must be
entered again (`400`). A registration changed by another request while this one
was handled is not overwritten: the request is refused with `409`, to be retried. Built-in drivers
cannot be replaced (`409`). A replacement is refused with `409` while an
operation the driver ran is still running or has an outcome that is not known
yet, since its cookie and recovery depend on the registration, and a
`cookie_version` change is refused with `409` while the driver has active
resources, whose cookies are of the current version; the error names their
GUResIDs, the first ten and a count of the rest. It is refused with `400`
when the driver would no longer target the type of a definition that uses it. Deployments use the new registration from their next
driver call.

```http
PUT /orgs/acme/resources/drivers/acme%2Fvault-db
Authorization: Bearer $ADMIN_TOKEN

{"target": "https://drivers-v2.acme.example/vault-db", "token": {"value": "drv_93ac"}, "capabilities": {"observe": true}}
```

```http-response
200 OK

{
  "id": "acme/vault-db",
  "builtin": false,
  "target": "https://drivers-v2.acme.example/vault-db",
  "token": {"store": "openporch", "ref": "drivers/acme/vault-db/token", "version": "2"},
  "protocol_version": "openporch-http/1",
  "cookie_version": 1,
  "capabilities": {
    "targets": {"resources": true, "workloads": false, "triggers": false},
    "observe": true, "recover": false, "preview": false, "cancel": false
  }
}
```

```http
GET /orgs/acme/resources/drivers
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

[
  {"id": "openporch/echo", "builtin": true},
  {"id": "openporch/none", "builtin": true},
  {"id": "openporch/template", "builtin": true},
  {"id": "openporch/opentofu", "builtin": true},
  {"id": "acme/vault-db", "builtin": false}
]
```

## Resource definitions

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/resources/defs` | org read |
| POST | `/orgs/{org}/resources/defs` | administer |
| GET | `/orgs/{org}/resources/defs/{def}` | org read |
| PUT | `/orgs/{org}/resources/defs/{def}` | administer |
| DELETE | `/orgs/{org}/resources/defs/{def}` | administer |
| POST | `/orgs/{org}/resources/defs/{def}/criteria` | administer |
| DELETE | `/orgs/{org}/resources/defs/{def}/criteria/{criterion}` | administer |

A definition says how one resource type is provisioned: a driver, its inputs and
the criteria under which the definition matches. `driver_inputs.values` holds plain
inputs, `driver_inputs.secret_refs` holds secret references or `{value}` to write,
and `driver_inputs.secrets` holds placeholder templates for secrets. A reference
to a secret the definition did not write needs a
[grant](security.md#secret-use-grants). A definition without criteria never matches. A criterion matches on `app_id`, `env_type`,
`env_id`, `res_id` and `class`. Without `class`, only the `default` class matches.

The built-in drivers are documented in [drivers/template.md](drivers/template.md)
and [drivers/opentofu.md](drivers/opentofu.md), and external drivers in
[drivers/http.md](drivers/http.md). The server checks
`openporch/opentofu` inputs when a definition is written; other drivers' inputs
are checked when they run.

```http
POST /orgs/acme/resources/defs
Authorization: Bearer $ADMIN_TOKEN

{
  "id": "dev-postgres",
  "name": "Development Postgres",
  "type": "postgres",
  "driver_type": "openporch/echo",
  "driver_inputs": {
    "values": {"host": "db.dev.acme.example", "port": 5432, "name": "shop"},
    "secret_refs": {"password": {"value": "pg-dev-password"}}
  },
  "criteria": [{"env_type": "development"}]
}
```

```http-response
201 Created

{
  "id": "dev-postgres",
  "name": "Development Postgres",
  "type": "postgres",
  "driver_type": "openporch/echo",
  "driver_inputs": {
    "values": {"host": "db.dev.acme.example", "port": 5432, "name": "shop"},
    "secret_refs": {"password": {"store": "openporch", "ref": "defs/dev-postgres/password", "version": "1"}}
  },
  "criteria": [{"id": "…", "env_type": "development"}],
  "version": 1,
  "created_by": "bootstrap"
}
```

`PUT` replaces the name, driver, account, inputs and co-provisioning, and keeps the
type and criteria. A secret reference read back can be sent unchanged:

```http
PUT /orgs/acme/resources/defs/dev-postgres
Authorization: Bearer $ADMIN_TOKEN

{
  "name": "Development Postgres",
  "driver_type": "openporch/echo",
  "driver_inputs": {
    "values": {"host": "db.dev.acme.example", "port": 5433, "name": "shop"},
    "secret_refs": {"password": {"store": "openporch", "ref": "defs/dev-postgres/password", "version": "1"}}
  }
}
```

```http-response
200 OK

{"id": "dev-postgres", "version": 2, "driver_inputs": {"values": {"port": 5433}}}
```

```http
POST /orgs/acme/resources/defs/dev-postgres/criteria
Authorization: Bearer $ADMIN_TOKEN

{"app_id": "shop", "class": "large"}
```

```http-response
201 Created

{"id": "$CRITERION", "app_id": "shop", "class": "large"}
```

```http
GET /orgs/acme/resources/defs/dev-postgres
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{"id": "dev-postgres", "criteria": [{"env_type": "development"}, {"id": "$CRITERION", "app_id": "shop"}]}
```

```http
GET /orgs/acme/resources/defs
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

[{"id": "dev-postgres", "type": "postgres"}]
```

## Cloud accounts

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/resources/accounts` | org read |
| POST | `/orgs/{org}/resources/accounts` | administer |
| GET | `/orgs/{org}/resources/accounts/{account}` | org read |
| PUT | `/orgs/{org}/resources/accounts/{account}` | administer |
| POST | `/orgs/{org}/resources/accounts/{account}/check` | administer |
| DELETE | `/orgs/{org}/resources/accounts/{account}` | administer |

Account types, credential handling and the `check` endpoint, which calls AWS STS,
are described in [cloud-accounts.md](cloud-accounts.md). Secrets go in `secrets`
(values) or `secret_refs` (references or `{value}`) and come back as references.
An account that a definition or secret store names, or that active resources
were provisioned in, cannot be deleted (`409`).

```http
POST /orgs/acme/resources/accounts
Authorization: Bearer $ADMIN_TOKEN

{
  "id": "aws-dev",
  "type": "aws",
  "config": {"access_key_id": "AKIAIOSFODNN7EXAMPLE", "region": "eu-north-1"},
  "secrets": {"secret_access_key": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"}
}
```

```http-response
201 Created

{
  "id": "aws-dev",
  "type": "aws",
  "config": {"access_key_id": "AKIAIOSFODNN7EXAMPLE", "region": "eu-north-1"},
  "secret_refs": {"secret_access_key": {"store": "openporch", "ref": "accounts/aws-dev/secret_access_key", "version": "1"}}
}
```

```http
GET /orgs/acme/resources/accounts
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

[{"id": "aws-dev", "type": "aws"}]
```

`PUT` replaces the account. Send the secret references read back to keep the same
secrets, or `{value}` to write new ones:

```http
PUT /orgs/acme/resources/accounts/aws-dev
Authorization: Bearer $ADMIN_TOKEN

{
  "type": "aws",
  "config": {"access_key_id": "AKIAIOSFODNN7EXAMPLE", "region": "eu-west-1"},
  "secret_refs": {"secret_access_key": {"store": "openporch", "ref": "accounts/aws-dev/secret_access_key", "version": "1"}}
}
```

```http-response
200 OK

{"id": "aws-dev", "config": {"region": "eu-west-1"}}
```

```http
GET /orgs/acme/resources/accounts/aws-dev
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{"id": "aws-dev", "type": "aws", "secret_refs": {"secret_access_key": {"store": "openporch"}}}
```

## Secret stores and grants

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/secretstores` | org read |
| POST | `/orgs/{org}/secretstores` | administer |
| GET | `/orgs/{org}/secretstores/{store}` | org read |
| PATCH | `/orgs/{org}/secretstores/{store}` | administer |
| DELETE | `/orgs/{org}/secretstores/{store}` | administer |
| GET | `/orgs/{org}/secretstores/{store}/grants` | org read all |
| POST | `/orgs/{org}/secretstores/{store}/grants` | administer |
| DELETE | `/orgs/{org}/secretstores/{store}/grants/{grant}` | administer |

The built-in store `openporch` is the primary store, where `{value}` secrets are
written. External stores such as `aws-secrets-manager` can be registered with the
cloud account that reads them, but their adapters are not implemented yet, so
references to them do not resolve. A store that
definitions, accounts, drivers, grants or shared values still reference cannot be
deleted. Grants are described in [security.md](security.md#secret-use-grants).

```http
POST /orgs/acme/secretstores
Authorization: Bearer $ADMIN_TOKEN

{"id": "vault", "type": "aws-secrets-manager", "config": {"region": "eu-north-1", "cloud_account": "aws-dev"}}
```

```http-response
201 Created

{"id": "vault", "type": "aws-secrets-manager", "config": {"region": "eu-north-1", "cloud_account": "aws-dev"}, "primary": false}
```

```http
PATCH /orgs/acme/secretstores/vault
Authorization: Bearer $ADMIN_TOKEN

{"config": {"region": "eu-west-1", "cloud_account": "aws-dev"}}
```

```http-response
200 OK

{"id": "vault", "config": {"region": "eu-west-1"}}
```

```http
GET /orgs/acme/secretstores/vault
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{"id": "vault", "type": "aws-secrets-manager", "primary": false}
```

```http
GET /orgs/acme/secretstores
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

[{"id": "openporch", "type": "openporch", "primary": true}, {"id": "vault", "primary": false}]
```

```http
POST /orgs/acme/secretstores/vault/grants
Authorization: Bearer $ADMIN_TOKEN

{"ref": "shop/stripe-key", "app_id": "shop", "env_id": "production"}
```

```http-response
201 Created

{"id": "$GRANT", "store": "vault", "ref": "shop/stripe-key", "app_id": "shop", "env_id": "production", "org_wide": false}
```

```http
GET /orgs/acme/secretstores/vault/grants
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

[{"id": "$GRANT", "ref": "shop/stripe-key"}]
```

## Deployments

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/apps/{app}/envs/{env}/deploys` | app read |
| POST | `/orgs/{org}/apps/{app}/envs/{env}/deploys` | app deploy |
| GET | `/orgs/{org}/apps/{app}/envs/{env}/deploys/{deploy}` | app read |
| GET | `/orgs/{org}/apps/{app}/envs/{env}/deploys/{deploy}/logs` | app read |

A deployment converges an environment to a set. The request names exactly one
of:

- `delta_id`: a delta, applied to the environment's desired set (the empty set
  before the first deployment). The delta becomes frozen. A draft delta created
  for one environment cannot be deployed to another.
- `set_id`: a stored set, or `0` for the empty set.
- `deploy_id`: an earlier deployment's set. This can be a deployment ID from any
  environment of the application, `+0` for the latest attempt in this environment,
  `+1` for the one before it, and so on up to `+499`, or `last-successful`; one
  that does not exist is `404`. By default (`"replay": "snapshot"`) the
  deployment reuses the configuration recorded by the earlier one: the
  definitions it matched, the shared values it resolved at their value set
  versions, and the version of every built-in secret it read, since acceptance
  pins a reference that names no version to the version it has then. This only
  works in the same environment and while what it used still exists: the
  definitions, the drivers at the same protocol version, and the pinned secret
  versions; otherwise the request is `400`, naming what is missing, before
  anything runs. A driver that reports the version of the code it ran, such
  as the OpenTofu driver's commit and lock file, gets that version back for
  each resource the earlier deployment provisioned; before the replay changes
  anything, each driver checks that its version is still there (the OpenTofu
  driver fetches the commit and installs the providers its lock file selects), and if one is not, that resource fails, every other is left `pending`, and the
  deployment fails with nothing changed. Cloud-account credentials are not pinned; every
  deployment gets fresh ones. Since a preview covers the pinned secret
  versions too, writing a new version of a secret a resource reads makes the
  permits issued for it stale (`DPL-003`); preview again. `"replay": "current"` matches definitions and reads
  values again as they are now. `replay` is
  only valid with `deploy_id`.

`expected_generation` must be the environment's current `generation`. If it is
stale, another deployment is still running, the environment is being deleted, or
a resource in it needs recovery, the request fails with `409` and the
environment, its desired set and the delta are unchanged. With an `Idempotency-Key` header, the same
caller retrying the same request gets the original deployment back, even after
the generation moved on. The same key with a different request gets `409`.
`mode` is `full`; `incremental` is not supported and returns `400`. `permits`
holds destructive-action permits from a [preview](#previews-and-permits), and
`comment` is free text.

Acceptance runs the same preflight as a preview. A change that moves a resource
to another driver, cloud account or state backend is refused with `409`
`DPL-002`. Every deletion, replacement and unknown-impact change the preflight
finds needs one of the presented permits: without it the request fails with
`409` `DPL-001`, and a permit that is unknown, expired, issued at another
generation, already bound, or for a resource whose configuration changed since
the preview fails with `409` `DPL-003`. Nothing is accepted in either case.

Acceptance builds the set's resource graph and rejects a set that cannot be
deployed, such as one with a resource that no definition matches, with `400`.
The graph and the definitions it matched are recorded with the deployment, so
later edits do not affect it. The deployment is returned at once with status
`in_progress`. It then becomes `succeeded` or `failed`, and only success changes
the environment's last successful deployment and set.

A function artifact must have an identity that cannot change. Acceptance pins
one named as an S3 object without a version, `s3://<bucket>/<key>` (literally
or through shared values), to the object's current version, read with the
workload definition's cloud account (`s3:GetObject` on it). The server's own
identity is never used for this. The recorded graph names that version, so the
deployment, and every snapshot replay of it, runs the object as it was then,
while the set keeps the name it was given; a `current` replay pins again. An
object in a bucket that does not keep versions, a missing object, an artifact
that is not an S3 object, or one whose workload definition has no cloud
account known before the deployment runs is refused with `400` before anything
is accepted; naming the version with `?versionId=` avoids the lookup. When S3
cannot be reached, the request fails with `502` (`CLD-002`) and can be sent
again.

Container and function images in Amazon ECR named by tag,
`<account>.dkr.ecr.<region>.amazonaws.com/<repository>[:<tag>]` (`latest`
without a tag; FIPS, China and dual-stack `dkr-ecr.<region>.on.aws` hosts
too), are pinned the same way: acceptance reads the digest the tag
points at with the workload definition's cloud account
(`ecr:DescribeImages`), and the recorded graph names
`<registry>/<repository>@sha256:<digest>` while the set keeps the tag. A
missing repository or tag, or an ECR image whose definition has no cloud
account known before the deployment runs, is refused with `400`; naming the
image by digest avoids the lookup, and a digest that is not `sha256:` with 64
hex digits is refused. Images already named by digest are kept. Images in
other registries named by tag, such as `nginx:1` or `ghcr.io/acme/web:abc123`,
are pinned to `<image>:<tag>@sha256:<digest>` too, read without credentials; a
tag that does not exist, or an image the registry shows only with credentials,
is refused with `400` (see [container registries](operations.md#container-registries)).
An image in another registry named by both tag and digest is stored without
its tag, since only images pinned from a public tag keep one.
Images built from other resources' outputs are not pinned; an ECR image built
from them is refused, since it cannot be pinned before the deployment runs.

Before a deployment changes anything, it checks that every function artifact
its workloads name as an S3 object version, `s3://<bucket>/<key>?versionId=<id>`
(key percent-encoded), still exists, reading it with the workload definition's
cloud account, which needs `s3:GetObjectVersion` on it. It also checks that
every image its workloads name by digest in Amazon ECR is still in its
repository (`ecr:DescribeImages`), and that every image they name by digest in
another registry still exists, where the registry shows it without
credentials. An image pinned from a public tag must still be shown without
credentials; one named by digest that needs them is not checked. If one is gone, as when a replay names a
version or digest that was deleted, the workload fails naming the artifact or
image, every other resource is left `pending`, and the deployment fails with
nothing changed. Checks that fail in a way that may pass are retried like
provisioning. An artifact or image of a workload whose definition has no cloud
account, or one that comes from another resource's outputs, is not checked;
its driver reports a missing one when it runs.

A deployment also has each driver check, before anything runs, that it can
still delete the resources the deployment will delete, such as those of a
removed workload: the OpenTofu driver fetches the commit and lock file it
retained for the resource. If that fails, the resource fails naming the cause,
every other resource is left `pending`, and the deployment fails with nothing
changed. A resource scheduled for deletion is checked too, unless something that
stays still uses it, in which case the deployment keeps it. If the resources to
delete cannot be listed, nothing runs either.

```http
POST /orgs/acme/apps
Authorization: Bearer $ADMIN_TOKEN

{"id": "blog"}
```

```http-response
201 Created

{"id": "blog"}
```

Every graph has a `base-env` node, and every workload is a `workload` node, so
both need a matching definition. A workload's class picks its definition like
any other resource's, and that definition's driver receives the resource
inputs `id`, `class`, `spec` (the workload document) and `dependencies` (the
outputs of its direct dependencies by key; one it did not request itself, such
as a policy a definition co-provisions with `match_dependents`, is keyed by its
descriptor), resolved at deploy time: the
placeholders take its dependencies' outputs, secret outputs stay references
that only the executor reads, and each trigger that names one of its resources
gets that resource's outputs under `outputs`. Content of files marked
`noExpand` is passed on as written, and `${pod.*}` needs a target that supports
pod references. `openporch/none` creates nothing and returns these inputs as
the workload's outputs. `${values.KEY}` in a workload resolves to the
environment's value for `KEY` when it has one, and otherwise to the
application's; the values are read when the deployment is accepted, so later
edits apply to the next deployment. Deployments accepted before workloads
received their inputs, or before deployments recorded their values and pinned
secret versions, cannot be
replayed from their snapshot; redeploy them with `"replay": "current"`.

```http
POST /orgs/acme/resources/defs
Authorization: Bearer $ADMIN_TOKEN

{"id": "blog-env", "type": "base-env", "driver_type": "openporch/none", "criteria": [{"app_id": "blog"}]}
```

```http-response
201 Created

{"id": "blog-env"}
```

```http
POST /orgs/acme/resources/defs
Authorization: Bearer $ADMIN_TOKEN

{"id": "blog-workloads", "type": "workload", "driver_type": "openporch/none", "criteria": [{"app_id": "blog"}]}
```

```http-response
201 Created

{"id": "blog-workloads"}
```

```http
POST /orgs/acme/apps/blog/deltas
Authorization: Bearer $ADMIN_TOKEN

{"modules": {"add": {"web": {"spec": {"containers": {"web": {"image": "registry.acme.example/blog/web:1"}}}}}}}
```

```http-response
201 Created

{"id": "$BLOG_DELTA"}
```

```http
POST /orgs/acme/apps/blog/envs/development/deploys
Authorization: Bearer $ADMIN_TOKEN
Idempotency-Key: release-2026-10-12

{"delta_id": "$BLOG_DELTA", "expected_generation": 0, "comment": "First release"}
```

```http-response
202 Accepted

{
  "id": "$DEPLOY",
  "app": "blog",
  "env": "development",
  "seq": 1,
  "kind": "deploy",
  "actor": "bootstrap",
  "set_id": "$BLOG_SET",
  "delta_id": "$BLOG_DELTA",
  "comment": "First release",
  "generation": 1,
  "status": "in_progress",
  "error": null,
  "created_at": "…",
  "finished_at": null
}
```

Sending the same request with the same key again returns the same deployment:

```http
POST /orgs/acme/apps/blog/envs/development/deploys
Authorization: Bearer $ADMIN_TOKEN
Idempotency-Key: release-2026-10-12

{"delta_id": "$BLOG_DELTA", "expected_generation": 0, "comment": "First release"}
```

```http-response
202 Accepted

{"id": "$DEPLOY"}
```

Without the key, the generation has moved on:

```http
POST /orgs/acme/apps/blog/envs/development/deploys
Authorization: Bearer $ADMIN_TOKEN

{"delta_id": "$BLOG_DELTA", "expected_generation": 0}
```

```http-response
409 Conflict

{"error": "API-004", "message": "…"}
```

```http
POST /orgs/acme/apps/blog/envs/development/deploys
Authorization: Bearer $ADMIN_TOKEN

{"set_id": "0", "expected_generation": 1, "mode": "incremental"}
```

```http-response
400 Bad Request

{"error": "API-003", "message": "mode `incremental` is not supported; use `full`"}
```

History is newest first and holds deployments only; an environment
deletion (`"kind": "delete_env"`) is read by its ID and shows as the
environment's latest attempt; `+n` skips it, and `deploy_id` cannot redeploy it.
`limit` defaults to 50, and `next`, when present, is
an attempt number to pass as `before` for the next page. A single deployment can
be read by ID, `+n` or `last-successful`; the ID of a deployment in another
environment is `404` here. A single deployment also shows `value_versions`, the
value set versions of the application and the environment it resolved (`null`
for a scope without values), which stay as they were when values change later:

```http
GET /orgs/acme/apps/blog/envs/development/deploys
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{"deployments": [{"id": "$DEPLOY", "seq": 1, "status": "…"}]}
```

```http
GET /orgs/acme/apps/blog/envs/development/deploys/+0
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{"id": "$DEPLOY", "set_id": "$BLOG_SET", "value_versions": {"app": null, "env": null}}
```

A deployment's log is read oldest first, by the same references. Each line has
a sequence number, its level, its message, and the resource, GUResID and
operation it is about, which lines about the deployment as a whole do not have.
`limit` defaults to 500 and is at most 5000. Pass `next` as `after` to read the
lines that follow; `finished` is `true` once the deployment has finished and
there are no more lines. Every secret value the executor resolved for a
resource is replaced with `[REDACTED]`, also in its escaped forms, before a
line is stored, and so are the driver errors recorded for the resource. A log
keeps at most 16 MiB of messages; past that, one warning says later lines were
dropped, and only each resource's final status and the deployment's outcome
follow it. If some lines could not be stored, the deployment fails and its error
says how many.

```http
GET /orgs/acme/apps/blog/envs/development/deploys/+0/logs
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{
  "lines": [
    {"seq": 1, "operation_id": null, "descriptor": null, "guresid": null, "at": "…", "level": "info", "message": "…"},
    "…"
  ],
  "next": "…",
  "finished": true
}
```

A page of one line, followed by the next:

```http
GET /orgs/acme/apps/blog/envs/development/deploys/+0/logs?limit=1
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{"lines": [{"seq": 1, "level": "info"}], "next": 1, "finished": false}
```

```http
GET /orgs/acme/apps/blog/envs/development/deploys/+0/logs?limit=1&after=1
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{"lines": [{"seq": 2}], "next": 2, "finished": false}
```

## Previews and permits

| Method | Path | Permission |
| --- | --- | --- |
| POST | `/orgs/{org}/apps/{app}/envs/{env}/plans` | app deploy |
| GET | `/orgs/{org}/apps/{app}/envs/{env}/plans/{plan}` | app read |
| POST | `/orgs/{org}/apps/{app}/envs/{env}/plans/{plan}/permits` | app deploy; `detach`, and any permit in a protected environment, needs administer (`unknown` is refused there) |

A preview runs a deployment's preflight without provisioning anything: it
resolves the set, builds the resource graph and classifies each resource's
change. `kind` is `deploy` (the default, with one of `delta_id`, `set_id` and
`deploy_id` as for a deployment), `delete_env`, or `detach` with the resource's
`descriptor`. `expected_generation` must be the environment's current
generation. A move to another driver, cloud account or state backend is refused
with `409` `DPL-002`.

Each resource's `action` is `create`, `update`, `delete`, `replace`, `unknown`
or `detach`, with a `reason`:

- `update` means its driver plans the change when the deployment runs. A
  replacement it finds then fails the deployment with `approval_required`, and
  the next preview shows that plan as a `replace` with its `driver_plan`,
  until the resource's configuration changes and its driver plans again.
- `unknown` is a change to an existing resource whose driver cannot preview.
- `delete` covers removed workloads, what they owned, and resources scheduled
  for deletion.

`permit_required` marks the changes that need a permit. A preview is never a
complete infrastructure plan, so `complete` is always `false`.

```http
GET /orgs/acme/apps/blog/envs/development
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{"id": "development", "generation": "$BLOG_GENERATION"}
```

```http
POST /orgs/acme/apps/blog/envs/development/plans
Authorization: Bearer $ADMIN_TOKEN

{"deploy_id": "+0", "expected_generation": $BLOG_GENERATION}
```

```http-response
201 Created

{
  "id": "$BLOG_PLAN",
  "kind": "deploy",
  "base_generation": "$BLOG_GENERATION",
  "set_id": "…",
  "created_by": "…",
  "created_at": "…",
  "expires_at": "…",
  "complete": false,
  "resources": [
    {"descriptor": "…", "definition": "…", "action": "update", "known": false,
     "reason": "…", "permit_required": false, "digest": "…"},
    "…"
  ]
}
```

```http
GET /orgs/acme/apps/blog/envs/development/plans/$BLOG_PLAN
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{"id": "$BLOG_PLAN", "kind": "deploy", "complete": false, "resources": ["…"]}
```

`POST .../plans/{plan}/permits` issues permits for changes the preview marked,
each `{"descriptor", "action"}` with the preview's action. A `replace` permit
covers the preview's driver plan, which `fingerprint` may name. The issuer needs
the deploy permission, and an administrator for `detach` or for any permit in an
environment whose type is [protected](#environment-types); there an `unknown`
permit is refused to everyone (`403`). A permit expires an
hour after it is issued unless a deployment binds it first. It binds only at
the preview's generation and while what the change acts on is as previewed:
the resource's configuration, what the environment recorded about it, the
outputs of the resources it depends on and the cloud account it runs in.
Binding and using a permit are recorded in the audit log as `permit.bind` and
`permit.use`, each correlating with the `permit.create` entry that issued it. Once bound, it authorizes that change in that deployment and its
retries, as long as its issuer keeps the authority. A permit issued for a
deployment that failed is not reused: preview again.

For a preview whose database is deleted, the request and its response look
like this:

```text
POST /orgs/acme/apps/shop/envs/development/plans/pln-5f0c2a9d81e4b7c6a3d2e1f0/permits

{"permits": [{"descriptor": "postgres.default#modules.web.externals.db", "action": "delete"}]}

201 Created

{"permits": [{"id": "prm-0d4c…", "plan": "pln-5f0c2a9d81e4b7c6a3d2e1f0",
  "descriptor": "postgres.default#modules.web.externals.db", "guresid": "5a1f…",
  "action": "delete", "plan_fingerprint": null, "base_generation": 4,
  "issuer": "alice", "expires_at": 1760283600000, "bound_to": null}]}
```

The blog's preview has nothing that needs a permit, so asking for one is `400`:

```http
POST /orgs/acme/apps/blog/envs/development/plans/$BLOG_PLAN/permits
Authorization: Bearer $ADMIN_TOKEN

{"permits": [{"descriptor": "workload.default#modules.web", "action": "delete"}]}
```

```http-response
400 Bad Request

{"error": "API-003", "message": "…"}
```

## Active resources

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/apps/{app}/envs/{env}/resources` | app read |
| GET | `/orgs/{org}/apps/{app}/envs/{env}/resources/graph` | app read |
| GET | `/orgs/{org}/apps/{app}/envs/{env}/resources/{type}/{res_id}` | app read |
| DELETE | `/orgs/{org}/apps/{app}/envs/{env}/resources/{type}/{res_id}` | deploy to the environment |
| POST | `/orgs/{org}/apps/{app}/envs/{env}/resources/{type}/{res_id}/detach` | administrator |
| POST | `/orgs/{org}/apps/{app}/envs/{env}/resources/{type}/{res_id}/recover` | deploy to the environment |
| POST | `/orgs/{org}/apps/{app}/envs/{env}/resources/{type}/{res_id}/resolve` | administrator |
| GET | `/orgs/{org}/apps/{app}/envs/{env}/tombstones` | app read |
| GET | `/orgs/{org}/resources/defs/{def}/resources` | org read all |

An active resource is something a deployment provisioned in an environment. It
keeps its `guresid`, a permanent ID that drivers and OpenTofu state are keyed
by, in every later deployment. Each resource shows its definition and driver, its
`status`, its non-secret `outputs`, the names of its `secret_outputs` (never
their values or references), the resources it depends on, and its `usage`:

- `last_deploy_id`: the deployment that last provisioned it.
- `deploys_ago`: how many of the environment's deployments came after that one.
  `0` means the latest deployment provisioned it. After a failed update it
  counts from the last success, and `outputs` are still that success's.
- `referenced`: whether the environment's latest deployment includes it.

While a resource's `status` is `unknown` or `recovery_required`,
`unknown_operation` names the operation whose outcome is unknown; recovering
it is described below.

A resource that a later deployment no longer includes stays tracked, with
`referenced: false`, because it still exists and may cost money. Including it
again reuses it with its data. If a definition switches a provisioned resource to
a different driver or cloud account, the deployment fails with a conflict and the
resource keeps its driver, account and state, since moving it is an explicit
migration. A resource that has never provisioned can still switch. The
definition and version shown are those of the last successful provisioning.

`DELETE` on a resource schedules it for deletion and returns `202` with the
resource, now with `scheduled_deletion: true`. Nothing is deleted yet: the next
deployment that leaves the resource out deletes it with the driver and state it
was provisioned with, and the record disappears, leaving a tombstone in the
audit trail. A deployment that removes a workload deletes the workload the same
way, together with the owned resources provisioned with it that depend on it,
such as its triggers; its dependencies stay unless they are scheduled too. A
removed workload that something kept still uses is not deleted, and the
deployment fails until that resource is scheduled too. Deletion runs only
after everything the deployment includes has provisioned, in reverse dependency
order: a resource is deleted only once nothing that uses it remains. A scheduled
resource that a kept resource still uses stays, with status `skipped` in the
deployment, without failing it. A resource
whose deletion fails keeps its record with status `deletion_failed`, and the
resources it uses stay too; the next deployment retries it with the same
`guresid`. If the driver confirmed a deletion but the record could not be
removed, the resource stays `deletion_failed` and the next deployment that
leaves it out removes the record without deleting in the cloud again. External resources are never deleted and cannot be scheduled
(`409`); adopted resources are deleted only when scheduled. Scheduling advances
the environment's `generation`, except when the resource is already scheduled,
and returns `409` while a deployment is running.

Every deletion needs a destructive-action permit in the deployment's `permits`,
issued from a [preview](#previews-and-permits) of that deployment. Without one
the deployment is refused with `409` `DPL-001`.

Detaching stops openporch from managing a resource without calling its driver,
so the real object keeps running. It needs an administrator, a `reason` and a
matching permit in `permits`, and is refused with `409` while a deployment runs
or while an operation on the resource is open or has an unknown outcome. A
resource that never provisioned has nothing to detach and is refused too; delete
or deploy it instead. So is one whose deletion the driver already confirmed;
the next deployment that leaves it out removes its record. If the resource was
replaced by another with the same descriptor after the permit was checked, the
request is refused with `409`. What a detached resource used stays: a
deployment never deletes it while the detached resource depended on it, and
reports it as kept. The response is the
resource's tombstone. Its `body` holds the definition and the snapshot it was
provisioned from, the driver, cloud account, target, object IDs, state backend,
the driver's recovery data and the outputs, which are enough to adopt the
object elsewhere, plus the reason, which the audit log records too. The driver's
encrypted cookie and its secret-output references are kept with the tombstone
but never returned. Everything in `body` is non-secret, so anyone who can read
the application can list tombstones. Object IDs and
the state backend are those the driver reported when it last provisioned the
resource; drivers that report none leave them empty. A deployment that still includes a detached resource fails for it rather
than creating a second object; bringing it back under management is an explicit
adoption, which is not implemented yet. The permit comes from a `detach`
[preview](#previews-and-permits) and is used with the detach, so a refused
detach keeps it; without one the request fails with `409` `DPL-001`.

`GET .../tombstones` lists what remains of the environment's deleted and
detached resources, oldest first.

### Unknown outcomes

Every driver operation is recorded, with the resource's `guresid` and the
inputs it runs with, before the driver is called. When the server stops during
an operation, or a driver times out or cannot say what it did, the resource's
`status` is `unknown`: it may have changed in the cloud. An operation the
server stopped before its driver was called is known to have changed nothing;
it fails, and the next deployment repeats it with the same `guresid`.

A resource with an unknown outcome blocks every deployment and deletion of its
environment that would touch it, with `409` `DPL-004`, until the outcome is
known. Nothing repeats the operation automatically.

After a restart the server observes each resource whose operation the
restart interrupted with its driver's read-only `get`, using the identity,
driver, cloud account and inputs the operation recorded, and the definition's
own secrets even if the definition no longer matches the resource, as a
deletion does. It observes with the driver's latest cookie: one the uncertain
operation returned before it failed replaces the resource's last cookie, and
does not make that operation's creation count as an update. A later
deployment does treat a resource with a cookie as one that may exist, so a
creation retried after it was found absent is planned like a change: a driver
that cannot preview it needs an unknown-impact permit. One that cannot be settled then, for example because a
deployment started first, and one left unknown by a timeout, waits for an
explicit recovery. `POST .../recover` does the same on request and returns
what the observation established as `outcome`, with the resource as it is now
(absent once deleted) and the observation's `reason`:

- `provisioned`: the creation took effect. The resource is `provisioned`
  with what the driver reports. A resource that existed before the operation
  proves nothing about an update by existing, so an interrupted update is
  `repeatable` instead and keeps its last successful result.
- `deleted`: the deletion took effect. The record is replaced by a tombstone,
  and the next deletion of the environment does not delete it again.
- `repeatable`: the operation can safely be repeated with the same `guresid`,
  because a creation left nothing behind, a deletion left the resource in
  place, an update's outcome is unknown, or the driver reports a failure, and
  the driver has the `recover` capability: repeating with the same identity
  converges instead of duplicating. The
  resource is `failed` or `deletion_failed`, and the next deployment repeats
  the operation.
- `in_progress`: the operation is still running. Nothing changed; recover
  again later.
- `recovery_required`: the driver cannot tell, as for a driver without the
  `observe` capability, or the outcome would be `repeatable` but the driver
  lacks the `recover` capability. The resource stays blocked with status
  `recovery_required`.

`POST .../recover` returns `409` when the resource has no unknown outcome or a
deployment is running in the environment, and `503` `DPL-005` when the driver
could not be reached; nothing changed then, so recover again.

When observation cannot settle an outcome, an administrator checks the cloud
and states that repeating the operation with the same `guresid` is safe:

```text
POST /orgs/acme/apps/shop/envs/development/resources/postgres/modules.web.externals.db/resolve
{"reason": "no database with this GUResID exists in the console"}
```

The resource becomes `failed` (or `deletion_failed` for a deletion) and the
response is the resource; the next deployment repeats the operation. The reason
is recorded with the operation and in the audit log. Resolving needs an
administrator and a `reason`, and is refused with `409` when there is no unknown
outcome or a deployment is running. If the object exists but should no longer be
managed, resolve the outcome first and then detach the resource.

`GET .../resources/graph` returns the graph of the environment's latest
attempt, or `404` before the first one. After an environment deletion, the
graph is empty and its nodes are the resources the deletion deleted, kept or
failed to delete. Each node has its descriptor,
definition, driver, GUResID, status and error; a resource the deployment has
not reached yet is `pending`. Resources the deployment deleted or kept while
removing them are listed too, without a definition. Each edge says that `from`
depends on `to`.

Resources are listed by descriptor. The single-resource path takes the type and
resource ID, such as `postgres/modules.web.externals.db`, and returns `409` if
resources of several classes share them. Listing a definition's
resources across every application needs organization-wide read access.

```http
GET /orgs/acme/apps/blog/envs/development/resources
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{
  "resources": [
    {"descriptor": "base-env.default#base-env", "guresid": "…", "driver_type": "openporch/none",
     "status": "…", "usage": {"deploys_ago": "…", "referenced": true}},
    {"descriptor": "workload.default#modules.web", "guresid": "…", "driver_type": "…",
     "status": "…", "usage": {"deploys_ago": "…", "referenced": true}}
  ]
}
```

```http
DELETE /orgs/acme/apps/blog/envs/development/resources/postgres/modules.api.externals.db
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
404 Not Found

{"error": "…", "message": "resource `postgres/modules.api.externals.db` not found in `blog/development`"}
```

```http
GET /orgs/acme/apps/blog/envs/development/resources/graph
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{
  "deployment_id": "…",
  "status": "…",
  "nodes": [
    {"descriptor": "…", "definition_id": "…", "driver_type": "…", "guresid": "…", "status": "…", "error": "…"},
    "…"
  ],
  "edges": ["…"]
}
```

```http
GET /orgs/acme/apps/blog/envs/development/tombstones
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{"tombstones": []}
```

## Audit log

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/audit-logs` | org read all |
| POST | `/orgs/{org}/audit-logs/expire` | org administer |

Every change and every denied change is recorded with who made it, the request and
the outcome. Entries never contain secret values. Filter with `from`, `to`,
`user_id`, `path_prefix` and `method`. Pages are newest first; pass `next` as
`before` for the next page. `limit` defaults to 100 and is at most 1000.

```http
GET /orgs/acme/audit-logs?path_prefix=/orgs/acme/apps/shop/envs&limit=1
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{
  "entries": [
    {
      "seq": "…",
      "id": "…",
      "at": "…",
      "user_id": "ci",
      "request_id": "…",
      "method": "DELETE",
      "path": "/orgs/acme/apps/shop/envs/production/values/API_URL",
      "action": "value.delete",
      "target": "shop/production/API_URL",
      "outcome": "accepted"
    }
  ],
  "next": "…"
}
```

Entries cannot be changed or deleted through the API: `DELETE` returns `405`.
Nothing expires on its own either. An administrator expires the entries older
than the server's retention period (`[audit] retention_days`, 30 days by
default) with `POST /orgs/{org}/audit-logs/expire`; the expiry is recorded in
the audit log in the same transaction, and a failed expiry removes nothing and
is recorded as `failed`. `dry_run` reports how many entries would expire and
`before`, the time entries written earlier are past retention; export them with
`to` set to `before`, then expire with the same `before`. Expiring without a
`before`, or with one inside the retention period, is refused with `400`, so
what is kept is always reported before anything is removed. `retained` lists what expiry keeps:
active resources, by driver, and those needing recovery or awaiting cleanup,
which may keep costing money until they are deleted.

```http
POST /orgs/acme/audit-logs/expire
Authorization: Bearer $ADMIN_TOKEN
Content-Type: application/json

{"dry_run": true}
```

```http-response
200 OK

{
  "dry_run": true,
  "retention_days": 30,
  "before": "…",
  "expired": "…",
  "remaining": "…",
  "retained": "…",
  "note": "…"
}
```

## Metrics

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/metrics` | org read all |

Returns metrics in the Prometheus text format: queued and running deployments,
deployment and driver call outcomes and durations, resources by status and
storage use. [Operations](operations.md#metrics) lists them.

```http
GET /orgs/acme/metrics
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

"…"
```

## Diagnostics

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/diagnostics` | org read all |

Returns a report to save and share when something goes wrong: the version,
readiness, sanitized configuration, work in progress, storage, resources by
status and the operations of resources that need recovery. With `app`, `env`
and `deployment` together it also describes that deployment: its request ID,
operations, audit entries and the last 200 lines of its log. The report never
holds tokens, key material, secret values, resource inputs or outputs, or
driver state. [Operations](operations.md#diagnostics) describes it.

```http
GET /orgs/acme/diagnostics
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

"…"
```

## Deleting

Deletes return `204` and refuse with `409` while something still uses the object.

```http
DELETE /orgs/acme/secretstores/vault/grants/$GRANT
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
204 No Content
```

```http
DELETE /orgs/acme/secretstores/vault
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
204 No Content
```

```http
DELETE /orgs/acme/resources/accounts/aws-dev
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
204 No Content
```

```http
DELETE /orgs/acme/resources/defs/dev-postgres/criteria/$CRITERION
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
204 No Content
```

```http
DELETE /orgs/acme/resources/types/postgres/classes/large
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
204 No Content
```

```http
DELETE /orgs/acme/resources/defs/dev-postgres
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
204 No Content
```

```http
DELETE /orgs/acme/resources/drivers/acme%2Fvault-db
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
204 No Content
```

```http
DELETE /orgs/acme/resources/types/acme%2Ffeature-flags
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
204 No Content
```

```http
DELETE /orgs/acme/apps/shop/values/API_URL
Authorization: Bearer $CI_TOKEN
```

```http-response
204 No Content
```

Deleting an environment sends the generation the client read:

```http
GET /orgs/acme/apps/shop/envs/production
Authorization: Bearer $CI_TOKEN
```

```http-response
200 OK

{"id": "production", "generation": "$PRODUCTION_GENERATION"}
```

```http
DELETE /orgs/acme/apps/shop/envs/production
Authorization: Bearer $CI_TOKEN

{"expected_generation": $PRODUCTION_GENERATION}
```

```http-response
202 Accepted

{"deployment": {"kind": "delete_env", "status": "succeeded"}, "retained": []}
```

```http
DELETE /orgs/acme/apps/shop/users/ci
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
204 No Content
```

```http
DELETE /orgs/acme/env-types/production/users/ci
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
204 No Content
```

```http
DELETE /orgs/acme/env-types/production
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
204 No Content
```

```http
DELETE /orgs/acme/apps/shop
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
202 Accepted

{"deletions": [{"deployment": {"env": "development", "kind": "delete_env", "status": "succeeded"}, "retained": []}], "removed": true}
```

```http
DELETE /orgs/acme/users/ci/tokens/deploy
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
204 No Content
```

```http
DELETE /orgs/acme/users/ci
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
204 No Content
```

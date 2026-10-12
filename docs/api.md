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
- **Work in progress.** This page covers the configuration API. Deployments,
  driver execution and external secret store adapters are not implemented yet;
  where a route registers something the server cannot use yet, its section says so.

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
| `AUTH-001` | 401 | No valid token or session. |
| `AUTH-002` | 403 | Authenticated, but not allowed. The message names the missing role. |
| `SRV-001` | 500 | Internal error. The log has details under the request ID. |
| `SRV-002` | 503 | Not ready, for example because the master key is missing. |
| `SRV-003` | 502 | The identity provider could not be reached. |
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
such as a missing master key or an unreachable database, while the server cannot
serve requests.

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

Changing a role needs authority over both the current and the new role. The last
administrator cannot be demoted or removed.

```http
PATCH /orgs/acme/users/ci
Authorization: Bearer $ADMIN_TOKEN

{"role": "manager"}
```

```http-response
200 OK

{"id": "ci", "role": "manager"}
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

{"id": "production", "description": "Live traffic", "created_at": "…"}
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
| DELETE | `/orgs/{org}/apps/{app}` | app manage |
| GET | `/orgs/{org}/apps/{app}/users` | app read |
| POST | `/orgs/{org}/apps/{app}/users` | app manage |
| PATCH | `/orgs/{org}/apps/{app}/users/{user}` | app manage |
| DELETE | `/orgs/{org}/apps/{app}/users/{user}` | app manage |

A new application gets a `development` environment, and its creator becomes its
`owner`. Application roles are `viewer`, `developer` and `owner`. The list shows
only the applications the caller can see.

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
| DELETE | `/orgs/{org}/apps/{app}/envs/{env}` | app deploy |

An environment shows its runtime summary: the desired and last successful sets,
the latest deployment attempt, and `convergence`, which is `never_deployed`,
`converged`, `partial` or `unknown`. With `from_deploy_id`, creation clones a
deployment of the same application into a draft delta and returns its `delta_id`.
Nothing is deployed. The default `development` environment cannot be deleted while
its application exists (`409`), and environments that have been deployed cannot be
deleted yet.

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

[{"id": "$DELTA"}]
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
| DELETE | `/orgs/{org}/resources/drivers/{driver}` | administer |

The built-in drivers are `openporch/echo`, `openporch/none`, `openporch/template`
and `openporch/opentofu`. An external driver will be reached over HTTPS, with its
`token` sent as a bearer token; the token is a secret reference or `{value}`.
[docs/drivers/http.md](drivers/http.md) describes the protocol. External drivers
can be registered now, but deployments do not call any driver until the deployment
executor is implemented.

```http
POST /orgs/acme/resources/drivers
Authorization: Bearer $ADMIN_TOKEN

{"id": "acme/vault-db", "target": "https://drivers.acme.example/vault-db", "token": {"value": "drv_7d1e"}}
```

```http-response
201 Created

{
  "id": "acme/vault-db",
  "builtin": false,
  "target": "https://drivers.acme.example/vault-db",
  "token": {"store": "openporch", "ref": "drivers/acme/vault-db/token", "version": "1"}
}
```

```http
GET /orgs/acme/resources/drivers/acme%2Fvault-db
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{"id": "acme/vault-db", "builtin": false}
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
  deployment reuses the configuration recorded by the earlier one, which only
  works in the same environment and while the definitions it used still exist.
  `"replay": "current"` matches definitions again as they are now. `replay` is
  only valid with `deploy_id`.

`expected_generation` must be the environment's current `generation`. If it is
stale, another deployment is still running, the environment is being deleted, or
a resource in it needs recovery, the request fails with `409` and the
environment, its desired set and the delta are unchanged. With an `Idempotency-Key` header, the same
caller retrying the same request gets the original deployment back, even after
the generation moved on. The same key with a different request gets `409`.
`mode` is `full`; `incremental` is not supported and returns `400`. `permits`
holds destructive-action permits, and `comment` is free text.

Acceptance builds the set's resource graph and rejects a set that cannot be
deployed, such as one with a resource that no definition matches, with `400`.
The graph and the definitions it matched are recorded with the deployment, so
later edits do not affect it. The deployment is returned at once with status
`in_progress`. It then becomes `succeeded` or `failed`, and only success changes
the environment's last successful deployment and set.

The server does not provision resources yet, so for now every deployment fails
once accepted.

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
both need a matching definition:

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

History is newest first. `limit` defaults to 50, and `next`, when present, is
an attempt number to pass as `before` for the next page. A single deployment can
be read by ID, `+n` or `last-successful`; the ID of a deployment in another
environment is `404` here:

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

{"id": "$DEPLOY", "set_id": "$BLOG_SET"}
```

## Active resources

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/apps/{app}/envs/{env}/resources` | app read |
| GET | `/orgs/{org}/apps/{app}/envs/{env}/resources/{type}/{res_id}` | app read |
| DELETE | `/orgs/{org}/apps/{app}/envs/{env}/resources/{type}/{res_id}` | deploy to the environment |
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
way; its dependencies stay unless they are scheduled too. Deletion runs only
after everything the deployment includes has provisioned, in reverse dependency
order: a resource is deleted only once nothing that uses it remains. A scheduled
resource that a kept resource still uses stays, with status `skipped` in the
deployment, without failing it. A resource
whose deletion fails keeps its record with status `deletion_failed`, and the
resources it uses stay too; the next deployment retries it with the same
`guresid`. External resources are never deleted and cannot be scheduled
(`409`); adopted resources are deleted only when scheduled. Scheduling advances
the environment's `generation`, except when the resource is already scheduled,
and returns `409` while a deployment is running.

Every deletion needs a destructive-action permit in the deployment's `permits`.
Without one the resource fails with `approval_required` and the deployment
fails. Permits are issued from deployment plans, which are not implemented yet,
so until then deployments that delete resources fail. Detaching resources is not
implemented yet.

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

{"resources": []}
```

```http
DELETE /orgs/acme/apps/blog/envs/development/resources/postgres/modules.api.externals.db
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
404 Not Found

{"error": "…", "message": "resource `postgres/modules.api.externals.db` not found in `blog/development`"}
```

## Audit log

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/orgs/{org}/audit-logs` | org read all |

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

```http
DELETE /orgs/acme/apps/shop/envs/production
Authorization: Bearer $CI_TOKEN
```

```http-response
204 No Content
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
204 No Content
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

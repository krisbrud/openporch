# Security model

This page explains who can do what in openporch and how credentials and secrets
are handled. The examples run in order against a fresh dev server in
`crates/server/tests/docs.rs`. Request and error conventions are in
[api.md](api.md).

## Credentials

Every request except health, version and login carries
`Authorization: Bearer <token>`. There are two kinds of credentials:

- **API tokens** belong to a member and start with `opt_`. They are random, stored
  only as SHA-256 hashes, and shown once when created. They may expire and can be
  revoked at any time.
- **Login sessions** come from GitHub device login and last 24 hours. They are
  signed by the server, and every request rechecks that the session was not
  revoked and that its member still belongs to the organization.

### The bootstrap administrator

On first start the server creates the service user `bootstrap` with the
`administrator` role. It writes a token for it to `<data_dir>/bootstrap-token`, a
new file only the server's user can read. The log names the file, never the token.
The token expires after `bootstrap_token_hours`, 24 by default.

Human members cannot be created directly; they join through an
[invitation](#invitations). To hand over to a human administrator:

1. With the bootstrap token, invite them as an administrator:
   `POST /orgs/{org}/invitations` with
   `{"role": "administrator", "provider": "github", "login": "<their login>"}`
   (or `"subject"` with their numeric GitHub ID). Keep the `code` from the
   response; it is shown only once.
2. They sign in through GitHub device login (see [api.md](api.md#login)),
   passing the code as `invitation_code`. The first login binds their GitHub
   account and makes them a member with the invited role.
3. As the new administrator, remove the bootstrap user with
   `DELETE /orgs/{org}/users/bootstrap`. This revokes its token. To keep the user
   but end the token, use `DELETE /orgs/{org}/users/bootstrap/tokens/bootstrap`.
4. Delete `<data_dir>/bootstrap-token` (or the file `bootstrap_token_file` names).
   The server never reads it again.

The server refuses to listen on a non-loopback address over plain HTTP unless
`tls_terminated_by_proxy = true` says a reverse proxy terminates TLS in front of it.

### GitHub sign-in

Members sign in with GitHub's device flow. Register a GitHub OAuth app (or a
GitHub App) with device flow enabled, and give its client ID to the server in the
configuration file that `openporch serve --config FILE` (or `OPENPORCH_CONFIG`)
names:

```toml
org = "acme"
data_dir = "/var/lib/openporch"

[github]
client_id = "Iv1.0123456789abcdef"
# For GitHub Enterprise Server, also set:
# api_url = "https://github.example.com/api/v3/"
# web_url = "https://github.example.com/"
```

The device flow needs no client secret. Without a `[github]` table, the server
accepts only API tokens.

#### Browser login

A web portal signs people in with GitHub's authorization-code flow instead. It
needs the OAuth app's client secret and its callback URL:

1. In the OAuth app's settings on GitHub, set the **Authorization callback URL**
   to the server's `/auth/github/callback`, such as
   `https://porch.example.com/auth/github/callback`, and generate a client
   secret.
2. Write the secret to a file only the server's user can read. The server
   refuses to start when others may read it:

   ```sh
   (umask 077 && cat > /etc/openporch/github-client-secret)
   ```

3. Name the file, the callback URL, and the portal pages a login may return to:

   ```toml
   [github.browser]
   client_secret_file = "/etc/openporch/github-client-secret"
   callback_url = "https://porch.example.com/auth/github/callback"
   return_urls = ["https://portal.example.com/login/done"]
   ```

The login runs as [api.md](api.md#login) describes. The server checks that each
callback carries a `state` it issued, once and within 10 minutes, and comes from
the browser that started the login. That browser holds an `HttpOnly`,
`SameSite=Lax` cookie scoped to the callback, which is `Secure` when the
callback uses `https`. So a link that carries someone else's login cannot sign a
victim in as them. The browser returns only to a configured page, named exactly,
with a one-time `login_code` that expires after 2 minutes. The session itself
never appears in a URL: the portal exchanges the code for it. GitHub's access
token stays on the server, which drops it once it has read who signed in.
Invitations work as with the device flow: the exchange takes the
`invitation_code` on a first login.

## Roles

| Scope | Role | Grants |
| --- | --- | --- |
| Organization | `administrator` | Everything. |
| Organization | `manager` | Invites members below their own authority, removes any member who is not an administrator, creates applications, and issues tokens for service users within their ceiling. |
| Organization | `orgViewer` | Reads everything except invitations and other members' tokens, including the audit log. |
| Organization | `member` | Sees only applications where they hold a role. |
| Application | `viewer` | Reads the application, its environments, sets, deltas and values. |
| Application | `developer` | Also edits sets, deltas, values and environments. |
| Application | `owner` | Also manages application users and deletes the application, which also needs `deployer` on every environment type the application has environments of. |
| Environment type | `deployer` | Lets developers and owners deploy to, create and delete environments of the type and change their values. Changing application-wide values needs it on every type the application has environments of. |

Every route declares the permission it needs (see the tables in [api.md](api.md)),
and handlers add object-level checks on top. Changes recheck the caller's roles
inside the transaction that commits them, so a role revoked mid-request takes
effect before the change lands.

A member with no role on an application cannot tell it exists. Reads and writes
return `404`:

```http
POST /orgs/acme/apps
Authorization: Bearer $ADMIN_TOKEN

{"id": "shop"}
```

```http-response
201 Created

{"id": "shop"}
```

```http
POST /orgs/acme/users
Authorization: Bearer $ADMIN_TOKEN

{"id": "dev", "type": "service", "role": "member"}
```

```http-response
201 Created

{"id": "dev", "role": "member"}
```

```http
POST /orgs/acme/users/dev/tokens
Authorization: Bearer $ADMIN_TOKEN

{"id": "laptop"}
```

```http-response
201 Created

{"id": "laptop", "token": "$DEV_TOKEN"}
```

```http
GET /orgs/acme/apps/shop
Authorization: Bearer $DEV_TOKEN
```

```http-response
404 Not Found

{"error": "API-001", "message": "application `shop` not found"}
```

Once they hold a role they see the application, and a missing permission is a
`403` that names the role they lack:

```http
POST /orgs/acme/apps/shop/users
Authorization: Bearer $ADMIN_TOKEN

{"id": "dev", "role": "developer"}
```

```http-response
201 Created

{"user": "dev", "role": "developer"}
```

```http
POST /orgs/acme/env-types
Authorization: Bearer $ADMIN_TOKEN

{"id": "production"}
```

```http-response
201 Created

{"id": "production"}
```

```http
POST /orgs/acme/apps/shop/envs
Authorization: Bearer $DEV_TOKEN

{"id": "production", "type": "production"}
```

```http-response
403 Forbidden

{"error": "AUTH-002", "message": "requires the `deployer` role on environment type `production`"}
```

```http
POST /orgs/acme/apps/shop/values
Authorization: Bearer $DEV_TOKEN

{"key": "API_URL", "value": "https://api.acme.example"}
```

```http-response
403 Forbidden

{"error": "AUTH-002", "message": "requires the `deployer` role on environment type `development`, because application values apply to its environments"}
```

Denied changes are recorded in the audit log.

## Delegation

Managers invite, create, change the roles of and issue tokens for members only
when they hold every permission the member has or would get. They cannot create
or promote administrators, and they cannot issue tokens for a service user with
more authority than their own. Removal grants nothing, so a manager may remove
any member who is not an administrator.

```http
POST /orgs/acme/users
Authorization: Bearer $ADMIN_TOKEN

{"id": "lead", "type": "service", "role": "manager"}
```

```http-response
201 Created

{"id": "lead", "role": "manager"}
```

```http
POST /orgs/acme/users/lead/tokens
Authorization: Bearer $ADMIN_TOKEN

{"id": "cli"}
```

```http-response
201 Created

{"id": "cli", "token": "$LEAD_TOKEN"}
```

```http
POST /orgs/acme/users
Authorization: Bearer $LEAD_TOKEN

{"id": "root", "type": "service", "role": "administrator"}
```

```http-response
403 Forbidden

{"error": "AUTH-002"}
```

```http
POST /orgs/acme/users/bootstrap/tokens
Authorization: Bearer $LEAD_TOKEN

{"id": "escalate"}
```

```http-response
403 Forbidden

{"error": "AUTH-002"}
```

## Tokens

A token is returned only by the request that creates it. Listing and reading show
its metadata and when it was last used. Revoking it takes effect on the next
request:

```http
GET /orgs/acme/users/dev/tokens
Authorization: Bearer $DEV_TOKEN
```

```http-response
200 OK

[{"id": "laptop", "created_by": "bootstrap", "expires_at": null, "last_used_at": "…"}]
```

```http
DELETE /orgs/acme/users/dev/tokens/laptop
Authorization: Bearer $DEV_TOKEN
```

```http-response
204 No Content
```

```http
GET /orgs/acme/current-user
Authorization: Bearer $DEV_TOKEN
```

```http-response
401 Unauthorized

{"error": "AUTH-001"}
```

Removing a member revokes all their tokens and sessions.

## Invitations

People join through invitations created by administrators and managers. An
invitation names a role and an email address or an external identity. For GitHub,
the identity is the account's immutable numeric ID. An invitation given a GitHub
`login` resolves it to the ID when it is created, so whoever holds that login later
gains nothing. The response includes a one-time `code`, shown once and stored only
as a hash. The person passes it on their first login. Invitations expire after
seven days.

```http
POST /orgs/acme/invitations
Authorization: Bearer $ADMIN_TOKEN

{"role": "member", "email": "kim@acme.example"}
```

```http-response
201 Created

{"role": "member", "email": "kim@acme.example", "status": "pending", "code": "…"}
```

```http
GET /orgs/acme/invitations
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

[{"email": "kim@acme.example", "status": "pending"}]
```

At the first login the identity is bound to the member. Later logins must present
the same identity, and an email invitation is accepted only for a verified email.

## Secret references

Secrets never travel through the API as values, except on the way in. A secret
reference names a store, a store-specific `ref` and optionally a `version`:

```json
{"store": "openporch", "ref": "apps/shop/values/API_TOKEN", "version": "1"}
```

Wherever the API takes a secret reference (definition and account `secret_refs`,
driver `token`, shared values' `secret_ref`) it accepts either a reference or
`{"value": "..."}`. Cloud accounts also take plain secret strings in `secrets`. A value is written to the primary store, and the object keeps a
reference that pins the written version. Responses, errors, logs and the audit log
show references only. Error messages name the path of a bad input, never its value.

```http
POST /orgs/acme/apps/shop/values
Authorization: Bearer $ADMIN_TOKEN

{"key": "API_TOKEN", "secret_ref": {"value": "tok_live_0f3c"}}
```

```http-response
201 Created

{
  "key": "API_TOKEN",
  "is_secret": true,
  "secret_ref": {"store": "openporch", "ref": "apps/shop/values/API_TOKEN", "version": "1"}
}
```

References are resolved only where the secret is used: by the executor
immediately before a driver runs, and by the cloud account check. The planner and
deployment records hold references only.

### Secret-use grants

Holding a reference is not permission to use it. A secret written through
`{value}` may be used only in the scope of the object that wrote it: an
application's values can use the application's secrets, and an environment's
values can also use that environment's secrets. Anything else needs an
administrator's grant for that exact reference, scoped to an application and
optionally one environment. An org-wide grant is meant for platform definitions.

Grants are checked twice. When an object is assigned a reference, shared values
need a grant covering their application or environment, and definitions, cloud
accounts and drivers need some grant for any secret they did not write
themselves; which application and environment that grant covers is checked only
when the secret is resolved. A definition's own secrets serve the deployments its
criteria match. When a deployment runs, the executor checks every reference again for
the deploying application and environment immediately before reading it, so a
production-only grant never serves development, and removing a grant blocks
every later resolution, including replays of earlier deployments. Deployments
are not implemented yet; the resolution check is in place for them.

A reference copied from another application is refused:

```http
POST /orgs/acme/apps
Authorization: Bearer $ADMIN_TOKEN

{"id": "billing"}
```

```http-response
201 Created

{"id": "billing"}
```

```http
POST /orgs/acme/apps/billing/values
Authorization: Bearer $ADMIN_TOKEN

{"key": "STRIPE_KEY", "secret_ref": {"value": "sk_live_51H"}}
```

```http-response
201 Created

{"secret_ref": {"store": "openporch", "ref": "apps/billing/values/STRIPE_KEY", "version": "1"}}
```

```http
POST /orgs/acme/apps/shop/values
Authorization: Bearer $ADMIN_TOKEN

{"key": "STRIPE_KEY", "secret_ref": {"store": "openporch", "ref": "apps/billing/values/STRIPE_KEY"}}
```

```http-response
403 Forbidden

{"error": "AUTH-002", "message": "no grant lets `shop` use secret `apps/billing/values/STRIPE_KEY` in store `openporch`"}
```

A grant to `shop` allows it:

```http
POST /orgs/acme/secretstores/openporch/grants
Authorization: Bearer $ADMIN_TOKEN

{"ref": "apps/billing/values/STRIPE_KEY", "app_id": "shop"}
```

```http-response
201 Created

{"store": "openporch", "ref": "apps/billing/values/STRIPE_KEY", "app_id": "shop", "org_wide": false}
```

```http
POST /orgs/acme/apps/shop/values
Authorization: Bearer $ADMIN_TOKEN

{"key": "STRIPE_KEY", "secret_ref": {"store": "openporch", "ref": "apps/billing/values/STRIPE_KEY"}}
```

```http-response
201 Created

{"key": "STRIPE_KEY", "is_secret": true}
```

Only administrators and org viewers can list grants. A store that definitions,
cloud accounts, drivers, grants or shared values reference cannot be deleted.

## Transport and rate limits

The server refuses to listen on a non-loopback address over plain HTTP; see
[The bootstrap administrator](#the-bootstrap-administrator). Its own outbound
connections, to GitHub and AWS, verify TLS certificates. A connection to a server with an untrusted certificate fails during
the handshake, before any request or credential is sent, and plain `http://` URLs
are accepted only for loopback addresses.

Login and token creation are rate limited, and every request body is bounded by
`max_body_bytes` (1 MiB by default). The limits are set in the configuration
file, per minute, and `0` turns one off:

```toml
[rate_limits]
login_per_minute = 60   # device login start and poll, per client address
tokens_per_minute = 20  # API tokens created, per calling member
```

A request over a limit gets `429` with code `API-007` and a `Retry-After` header.
Behind a reverse proxy (`tls_terminated_by_proxy = true`), the client address is
the last `X-Forwarded-For` entry, the one the proxy added, so the server must be
reachable only through that proxy. IPv6 clients are limited per /64 network.

## Master keys

The built-in secret store, the session signing key and other protected records
are encrypted with master keys. A new installation generates
`<data_dir>/master.key`, or reads `master_key_file` or the
`OPENPORCH_MASTER_KEY` environment variable. The file must be readable only by
the server's user. It holds one base64 key per line: the first seals new data,
and the others are retired keys that still decrypt older data.

To rotate, run `openporch keygen`, put its output on the first line of the key
file, keep the old keys below it, and restart. Never remove a retired key while
data sealed with it remains: the server does not regenerate missing keys.
Instead `/readyz` reports `master_key_missing:<key id>` and changes are refused
until the key is restored.

## Audit log

Every change, and every change that was denied, is recorded with the principal,
request ID, method, path, action, target and outcome. A change and its audit entry
commit together, so a change that cannot be audited does not happen. Entries never
contain secret values or tokens.

```http
GET /orgs/acme/audit-logs?path_prefix=/orgs/acme/apps/shop/envs&limit=1
Authorization: Bearer $ADMIN_TOKEN
```

```http-response
200 OK

{
  "entries": [
    {
      "user_id": "dev",
      "method": "POST",
      "path": "/orgs/acme/apps/shop/envs",
      "action": "request.denied",
      "outcome": "denied",
      "status_code": 403
    }
  ],
  "next": "…"
}
```

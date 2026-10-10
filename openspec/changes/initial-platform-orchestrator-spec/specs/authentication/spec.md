# Spec Delta

## Purpose

Defines who can call openporch and how they prove it: principals (users and service users), API tokens, short-lived sessions obtained through `porch login`, pluggable external identity providers such as GitHub, and the bootstrap of the first administrator.

## ADDED Requirements

### Requirement: Principals

Every authenticated request SHALL resolve to exactly one principal: a `user` (a human with an email and one linked external identity) or a `service` user (a non-human account). Principals have an ID, a display name, a type and organization membership. Every object created through the API SHALL record the creating principal.

#### Scenario: Actor recorded
- **WHEN** service user `ci` creates a resource definition
- **THEN** the definition's `created_by` is the ID of `ci`

### Requirement: API tokens

A principal MAY hold API tokens created under `POST /orgs/{org}/users/{user}/tokens` with an `id`, optional `description` and optional `expires_at`. The full token SHALL be returned exactly once at creation and stored only as a hash. Tokens inherit the principal's roles and SHALL be revocable; a revoked or expired token SHALL be rejected with `401`.

#### Scenario: Token shown once
- **WHEN** an administrator creates token `deploy` for service user `ci`
- **THEN** the response contains the token value, and later reads of the token show only its ID, description, expiry, creation time and last use

#### Scenario: Revoked token
- **WHEN** token `deploy` is revoked and a request presents it
- **THEN** the server returns `401` and records nothing as that principal

#### Scenario: Who may create tokens
- **WHEN** a user with the Member role tries to create a token for another user
- **THEN** the server returns `403`; users may create tokens for themselves, and administrators for service users; managers may issue service-user tokens only within the delegation ceiling in `organization-membership`

### Requirement: Bearer authentication

Every API request except liveness, readiness, health, version, login and OIDC discovery endpoints SHALL carry `Authorization: Bearer <token>` where the token is an API token or a session token. Missing or invalid credentials SHALL be rejected with `401` and a uniform error body.

#### Scenario: Missing token
- **WHEN** a request carries no `Authorization` header
- **THEN** the server returns `401` without processing the request

### Requirement: Sessions and login

`porch login` SHALL store a signed 24-hour session with principal ID and expiry. Its signature may be verified offline, but every request SHALL check current membership, permissions and revocation. Removal, logout/revocation and role changes SHALL affect the next request, including with caches. A valid signature alone SHALL not authorize a removed principal.

#### Scenario: Login and expiry
- **WHEN** a user completes `porch login` and uses the CLI 25 hours later
- **THEN** the request is rejected with `401` and the CLI asks the user to log in again

### Requirement: External identity providers

Milestone 1 SHALL support `github` (OAuth app credentials, CLI device flow, browser authorization-code flow); milestone 2 adds `oidc` (issuer, client credentials, PKCE). Login SHALL map `github:<immutable-numeric-user-id>` or `<issuer>:<sub>` to a user; login names are display metadata. Unknown identities without a matching invitation SHALL be rejected.

#### Scenario: GitHub device login
- **WHEN** `porch login` runs against a server with a `github` provider
- **THEN** the CLI shows a device code and URL, polls until the user approves, and receives a session token for the user linked to the immutable GitHub user ID returned by the provider

#### Scenario: Unknown identity
- **WHEN** a GitHub user with no openporch user and no pending invitation logs in
- **THEN** the login fails with a message that an invitation is required

#### Scenario: Provider is locked after first login
- **WHEN** a user first logged in through `github` and later tries an `oidc` provider with the same email
- **THEN** the login is rejected; the identity binding is changed only by an administrator

### Requirement: Bootstrap administrator

First start SHALL create the organization and administrator service user `bootstrap`. Its token SHALL expire after 24 hours by default and be written once to a configured owner-only file (default `<data-dir>/bootstrap-token`, Unix `0600`), never normal logs. Explicit interactive display MAY be supported. Documentation SHALL explain creating a human administrator and revoking/removing the bootstrap token/file.

#### Scenario: First start
- **WHEN** `openporch serve` starts with an empty data directory
- **THEN** the restricted bootstrap file contains the token, logs contain only its file location, and `porch get users` with that token lists the `bootstrap` service user as administrator

### Requirement: Identity verification

GitHub identities SHALL use immutable numeric IDs; invitation login names SHALL be resolved to IDs at issuance. Renaming/reusing a login SHALL not transfer membership. Email invitations SHALL require provider-verified email ownership. OIDC login is a milestone 2 integration governed by the validation requirement below.

#### Scenario: Reused GitHub name
- **WHEN** another GitHub account acquires a former member's login name
- **THEN** it gains no membership or invitation associated with the original immutable ID

#### Scenario: Unverified invitation email
- **WHEN** a login presents an invited email address without verified ownership
- **THEN** invitation acceptance is denied

#### Scenario: Removed user with a valid signature
- **WHEN** a removed user presents an otherwise unexpired signed session
- **THEN** the next request returns `401`

### Requirement: Transport and credential handling

Remote API/CLI/provider connections SHALL use verified TLS, either directly or through a documented trusted reverse proxy. Plain HTTP SHALL be allowed only through explicit loopback development configuration. CLI credential files SHALL be owner-only, tokens SHALL not appear in diagnostics, and token entry SHALL support stdin or a hidden prompt without requiring shell-history exposure. Login and token endpoints SHALL have configurable rate limits and bounded request sizes.

#### Scenario: Invalid server certificate
- **WHEN** a remote server presents an invalid TLS certificate
- **THEN** the CLI and provider reject the connection without sending a credential

### Requirement: OIDC login validation

When enabled, OIDC login SHALL validate signature, allowed algorithm, issuer, audience, expiry, state and nonce, use PKCE, and reject reused login transactions. Browser callbacks SHALL be restricted to configured URLs.

#### Scenario: Wrong audience or replayed transaction
- **WHEN** an OIDC response targets another client or repeats a consumed login transaction
- **THEN** login fails without creating a session

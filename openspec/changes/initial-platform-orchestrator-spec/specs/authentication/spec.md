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
- **THEN** the server returns `403`; users may create tokens for themselves, and administrators and managers for service users

### Requirement: Bearer authentication

Every API request except health, version, login and OIDC discovery endpoints SHALL carry `Authorization: Bearer <token>` where the token is an API token or a session token. Missing or invalid credentials SHALL be rejected with `401` and a uniform error body.

#### Scenario: Missing token
- **WHEN** a request carries no `Authorization` header
- **THEN** the server returns `401` without processing the request

### Requirement: Sessions and login

`porch login` SHALL obtain a short-lived session token (24 hours) by authenticating the user through a configured external identity provider and SHALL store it in the CLI config. Session tokens SHALL be signed by the server, carry the principal ID and expiry, and SHALL be verifiable without a database lookup; the server MAY additionally check a revocation list.

#### Scenario: Login and expiry
- **WHEN** a user completes `porch login` and uses the CLI 25 hours later
- **THEN** the request is rejected with `401` and the CLI asks the user to log in again

### Requirement: External identity providers

The server SHALL support configuring identity providers of type `github` (OAuth app client ID and secret, using the device authorization flow for the CLI and the authorization code flow for browsers) and `oidc` (issuer, client ID and secret, PKCE). A successful login SHALL map the external identity (`github:<login>` or `<issuer>:<sub>`) to a user; an unknown identity with no matching invitation SHALL be rejected.

#### Scenario: GitHub device login
- **WHEN** `porch login` runs against a server with a `github` provider
- **THEN** the CLI shows a device code and URL, polls until the user approves, and receives a session token for the user linked to that GitHub login

#### Scenario: Unknown identity
- **WHEN** a GitHub user with no openporch user and no pending invitation logs in
- **THEN** the login fails with a message that an invitation is required

#### Scenario: Provider is locked after first login
- **WHEN** a user first logged in through `github` and later tries an `oidc` provider with the same email
- **THEN** the login is rejected; the identity binding is changed only by an administrator

### Requirement: Bootstrap administrator

On first start with an empty database, the server SHALL create the organization and an initial `administrator` service user named `bootstrap`, and SHALL print a one-time API token for it to its log or write it to a file named in configuration. The token SHALL be usable to create the first human user and SHALL be revocable like any other.

#### Scenario: First start
- **WHEN** `openporch serve` starts with an empty data directory
- **THEN** the log contains a bootstrap token and `porch get users` with it lists the `bootstrap` service user as administrator

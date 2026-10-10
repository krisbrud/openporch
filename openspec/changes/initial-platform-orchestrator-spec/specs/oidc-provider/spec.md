# Spec Delta

## Purpose

openporch acts as an OpenID Connect identity provider so that a cloud account can trust short-lived tokens it issues instead of holding standing credentials, starting with AWS role assumption with web identity. This capability is sequenced after the orchestrator works end to end.

## Delivery scope

This is a later optional integration, not a milestone 1 or milestone 2 release gate. Host identity and `aws-role` are the initial credential path.

## ADDED Requirements

### Requirement: Issuer and discovery

The server SHALL expose an issuer at `https://<public-url>/oidc` with `/.well-known/openid-configuration` and a JWKS endpoint, both unauthenticated. The issuer SHALL sign tokens with RS256 keys stored encrypted in the database, support at least two active keys for rotation, and publish only public keys.

#### Scenario: Discovery
- **WHEN** a client fetches the discovery document
- **THEN** it names the issuer, the JWKS URL, `RS256` and `response_types_supported`

#### Scenario: Key rotation
- **WHEN** an administrator rotates the signing key
- **THEN** new tokens use the new key, the JWKS lists both keys, and the old key is removed after its grace period

### Requirement: Identity tokens for cloud accounts

For a cloud account of type `aws-identity`, the orchestrator SHALL mint an ID token with `iss` the issuer, `sub` `<org>/<cloud-account-id>`, `aud` `sts.amazonaws.com` (overridable per account), a lifetime of at most 15 minutes, and exchange it with `AssumeRoleWithWebIdentity` for the account's `role_arn`. Tokens SHALL never be returned through the API.

#### Scenario: Role assumption without standing credentials
- **WHEN** an AWS role trusts the openporch issuer with condition `sub = acme/aws-prod` and account `aws-prod` is type `aws-identity` with that role
- **THEN** the account check succeeds and drivers receive temporary credentials, while the orchestrator stores no AWS key for that account

#### Scenario: Trust scoped to another account
- **WHEN** the role's condition names `acme/aws-dev` but account `aws-prod` presents its token
- **THEN** STS rejects the exchange and the check reports the STS error

### Requirement: Setup guidance

The account check response and `porch cloud-accounts setup aws-identity <id>` SHALL print the IAM identity provider URL, thumbprint requirements and a trust policy document scoped to the account's subject.

#### Scenario: Print trust policy
- **WHEN** `porch cloud-accounts setup aws-identity aws-prod` runs
- **THEN** it prints a trust policy JSON with the issuer URL and `acme/aws-prod` as the subject condition

### Requirement: Public reachability

The OIDC endpoints SHALL be served over HTTPS at a URL reachable by the cloud provider; the server SHALL refuse to create an `aws-identity` account when no public URL is configured.

#### Scenario: No public URL
- **WHEN** `public_url` is unset and an administrator creates an `aws-identity` account
- **THEN** the server returns `400` explaining the issuer must be publicly reachable

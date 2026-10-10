# Spec Delta

## Purpose

Organization membership determines who belongs to the organization: human users join through invitations, service users are created by administrators, and every member holds exactly one organization role.

## ADDED Requirements

### Requirement: Members and organization roles

Every member SHALL hold exactly one organization role: `administrator`, `manager`, `member` or `orgViewer`. `GET /orgs/{org}/users` SHALL list members with type, role, email or identity, and creation time. The organization SHALL always keep at least one `administrator`; a change that would remove the last one SHALL be rejected with `409`.

#### Scenario: Last administrator
- **WHEN** the only administrator tries to change their own role to `member`
- **THEN** the server returns `409`

### Requirement: Invitations

Administrators/managers SHALL invite by verified email or GitHub login resolved to immutable ID, with role and a one-time 7-day code. Acceptance SHALL require the matching verified identity and code and atomically consume the invitation. Only administrators may invite administrators; all other grants SHALL obey the delegation ceiling.

#### Scenario: Accept by GitHub login
- **WHEN** an invitation for the immutable ID resolved from `octocat` with role `member` exists and that GitHub user logs in with its valid one-time code
- **THEN** a user is created with role `member` and the invitation is consumed

#### Scenario: Expired invitation
- **WHEN** an invitation is 8 days old and the invitee logs in
- **THEN** the login is rejected and the invitation is shown as expired

#### Scenario: Manager invites an administrator
- **WHEN** a manager posts an invitation with role `administrator`
- **THEN** the server returns `403`

### Requirement: Service users

Administrators and managers SHALL create service users with `POST /orgs/{org}/users` and `type: service`, a name and an organization role. Service users SHALL hold roles at every level like users, and their API tokens inherit those roles.

#### Scenario: CI service user
- **WHEN** an administrator creates service user `ci` with role `member` and grants it `developer` on `shop` and `deployer` on `development`
- **THEN** a token of `ci` can deploy to `shop/development` and nothing else

### Requirement: Changing roles and removing members

`PATCH /orgs/{org}/users/{user}` SHALL change the organization role and `DELETE` SHALL remove the member, revoking all their tokens and sessions and removing their application and environment type roles. Managers SHALL only change or remove members whose current and proposed roles are not `administrator`, and every delegated grant SHALL obey the permission ceiling below. Last-administrator checks and membership changes SHALL be atomic.

#### Scenario: Removal revokes access
- **WHEN** user `alice` is removed while holding an API token
- **THEN** the next request with that token returns `401`

#### Scenario: Manager removes an administrator
- **WHEN** a manager deletes an administrator
- **THEN** the server returns `403`

### Requirement: Delegation cannot increase authority

Invites, service-user creation, role changes and token issuance SHALL check current and resulting privileges. Only administrators may create/promote administrators or issue tokens for administrator service users, including bootstrap. Non-administrators SHALL not grant or impersonate permissions beyond their own org/app/type authority. Self-tokens inherit current roles; authoritative role/grant checks SHALL be atomic.

#### Scenario: Manager targets bootstrap
- **WHEN** a manager requests a token for `bootstrap`
- **THEN** the request returns `403` and produces no token

#### Scenario: Manager creates or promotes an administrator
- **WHEN** a manager creates an administrator service user or changes a non-administrator's role to administrator
- **THEN** each request returns `403` without changing membership

#### Scenario: Service user exceeds issuer's authority
- **WHEN** a manager without production deployment permission requests a token for a production CI service user
- **THEN** the request returns `403` even though that service user's organization role is only `member`

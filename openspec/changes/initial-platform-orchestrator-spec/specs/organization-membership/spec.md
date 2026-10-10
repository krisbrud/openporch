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

Administrators and managers SHALL invite humans with `POST /orgs/{org}/invitations` giving `email` or an external identity (`github:<login>`) and a role. The invitation SHALL carry a one-time code valid for 7 days. The invitee accepts by logging in with a matching identity, which creates the user with the invited role. Only administrators MAY invite administrators.

#### Scenario: Accept by GitHub login
- **WHEN** an invitation for `github:octocat` with role `member` exists and that GitHub user logs in
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

`PATCH /orgs/{org}/users/{user}` SHALL change the organization role and `DELETE` SHALL remove the member, revoking all their tokens and sessions and removing their application and environment type roles. Managers SHALL only change or remove members whose role is not `administrator`.

#### Scenario: Removal revokes access
- **WHEN** user `alice` is removed while holding an API token
- **THEN** the next request with that token returns `401`

#### Scenario: Manager removes an administrator
- **WHEN** a manager deletes an administrator
- **THEN** the server returns `403`

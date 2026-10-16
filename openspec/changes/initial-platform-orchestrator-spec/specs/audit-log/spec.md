# Spec Delta

## Purpose

The audit log records who changed what in the organization so that administrators can answer questions about deployments, deletions and configuration changes after the fact.

## ADDED Requirements

### Requirement: Recorded events

Accepted mutations SHALL persist audit intent atomically with domain changes. Record timestamp, org, known actor, request/operation ID, action, target, response/outcome and reason code. Correlate async results, recovery, permits, detach and privilege/credential changes. Denied privileged actions and bounded failed-login events SHALL be recorded. Ordinary reads, bodies, tokens and secret values SHALL be excluded; secret-use events name references and scope only.

#### Scenario: Deployment recorded
- **WHEN** `ci` triggers a deployment to `shop/dev`
- **THEN** an entry with method `POST`, that path and the deployment ID exists with `user_id` of `ci`

### Requirement: Query

`GET /orgs/{org}/audit-logs` SHALL be available to administrators and org viewers, newest first, paginated, with `from`, `to`, `user_id` and `path_prefix` and `method` filters.

#### Scenario: Filter deletions
- **WHEN** an administrator queries with `path_prefix=/orgs/acme/apps/shop` and method `DELETE`
- **THEN** only deletions under that application are returned

### Requirement: Retention

Audit entries SHALL not be API-deletable, except through the explicit administrator expiry of entries past retention described below. Retention defaults to 30 days and SHALL support 365 days or export. Expose retention and export failures. Expiry SHALL be explicit maintenance independent of resource recovery records. Storage exhaustion SHALL reject new mutations rather than silently discard audit events. Tamper-proof storage against a host administrator is not claimed.

#### Scenario: Delete attempt
- **WHEN** any principal sends `DELETE` to an audit log path
- **THEN** the server returns `405`

#### Scenario: Expire past retention
- **WHEN** an administrator expires entries older than the retention period after a dry run
- **THEN** only those entries are removed, the expiry is recorded in the audit log in the same transaction, and entries inside the retention period cannot be expired

### Requirement: Execution outcome correlation

Audit export SHALL support paginated machine-readable records with stable IDs so an operator can correlate the requesting user, accepted operation and final outcome without parsing free-form logs.

#### Scenario: Accepted deployment later fails
- **WHEN** a deployment request returns success but its cloud operation fails
- **THEN** the audit stream contains both the initiating actor and the correlated failed outcome

### Requirement: Audit persistence failure

If required audit intent cannot persist, the mutation SHALL not be accepted. Successful HTTP acceptance and later execution outcomes SHALL remain distinguishable.

#### Scenario: Audit storage unavailable
- **WHEN** a role change cannot persist its audit intent
- **THEN** the role change fails and existing permissions remain unchanged

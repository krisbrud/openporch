# Spec Delta

## Purpose

The audit log records who changed what in the organization so that administrators can answer questions about deployments, deletions and configuration changes after the fact.

## ADDED Requirements

### Requirement: Recorded events

Every successful authenticated create, update or delete request SHALL produce an audit entry with `at`, `org_id`, `user_id`, `request_method`, `request_path`, `response_status` and, where the object has one, its ID. Read requests, failed authentication and request bodies SHALL NOT be recorded.

#### Scenario: Deployment recorded
- **WHEN** `ci` triggers a deployment to `shop/dev`
- **THEN** an entry with method `POST`, that path and the deployment ID exists with `user_id` of `ci`

### Requirement: Query

`GET /orgs/{org}/audit-logs` SHALL be available to administrators and org viewers, newest first, paginated, with `from`, `to`, `user_id` and `path_prefix` filters.

#### Scenario: Filter deletions
- **WHEN** an administrator queries with `path_prefix=/orgs/acme/apps/shop` and method `DELETE`
- **THEN** only deletions under that application are returned

### Requirement: Retention

Audit entries SHALL NOT be deletable through the API and SHALL be retained for at least 365 days.

#### Scenario: Delete attempt
- **WHEN** any principal sends `DELETE` to an audit log path
- **THEN** the server returns `405`

# Spec Delta

## Purpose

Role-based access control decides what each principal may do, at organization, application and environment type level, and is enforced on every API route from the first release rather than added later.

## ADDED Requirements

### Requirement: Role model

Roles SHALL exist at three levels with Humanitec-compatible IDs: organization (`administrator`, `manager`, `member`, `orgViewer`), application (`owner`, `developer`, `viewer`) and environment type (`deployer`). Effective permissions are the union of all roles a principal holds. Applications and environment types SHALL expose role assignment endpoints under `/users`.

#### Scenario: Union of roles
- **WHEN** `bob` is `member` in the organization, `viewer` on `shop` and `developer` on `billing`
- **THEN** he can read `shop`, change configuration in `billing`, and sees no other application

### Requirement: Organization permissions

The orchestrator SHALL grant `administrator` every permission and SHALL reserve management of resource types, classes, definitions, drivers, cloud accounts, secret stores and environment type roles to it. `manager` SHALL be able to invite and remove non-administrators and create applications. `orgViewer` SHALL be able to read everything. `member` SHALL see only applications where they hold a role.

#### Scenario: Manager creates a definition
- **WHEN** a `manager` posts a resource definition
- **THEN** the server returns `403`

### Requirement: Application permissions

`viewer` SHALL be able to read the application. `developer` SHALL additionally be able to edit sets, deltas, values and environments. `owner` SHALL additionally be able to manage application users and delete the application.

#### Scenario: Developer edits definitions
- **WHEN** a `developer` without an organization administrator role posts a resource definition
- **THEN** the server returns `403`

#### Scenario: Viewer deploys
- **WHEN** a `viewer` triggers a deployment
- **THEN** the server returns `403`

#### Scenario: Org viewer reads secrets
- **WHEN** an `orgViewer` reads a resource definition with secret inputs
- **THEN** the definition is returned with secret keys redacted like for any reader

### Requirement: Deployer gate

Creating, deploying to or deleting an environment SHALL require `developer` or `owner` on the application and `deployer` on the environment's type. Deleting an application SHALL require `owner` plus `deployer` on every environment type the application uses.

#### Scenario: Developer without deployer
- **WHEN** a `developer` on `shop` without `deployer` on `production` deploys to `shop/production`
- **THEN** the server returns `403` naming the missing `deployer` role

### Requirement: Creator defaults

The principal that creates an application SHALL become its `owner`. Administrators SHALL hold every role implicitly.

#### Scenario: Create an application
- **WHEN** manager `carol`, with `deployer` on `development`, creates application `shop`
- **THEN** `carol` is listed as `owner` of `shop`

### Requirement: Enforcement on every route

Every API route SHALL declare the permission it requires, and the server SHALL refuse to start if any route lacks one. Authorization failures SHALL return `403` with the uniform error body and SHALL be distinguishable from `404` only when the principal may list the parent scope.

#### Scenario: Hidden application
- **WHEN** a `member` with no role on `billing` reads `/orgs/{org}/apps/billing`
- **THEN** the server returns `404`

#### Scenario: Known application, forbidden action
- **WHEN** a `viewer` on `shop` deletes `shop`
- **THEN** the server returns `403`

### Requirement: Role management surfaces

Roles SHALL be manageable through the API, the CLI (`porch users`, `porch apps users`, `porch env-types users`) and the OpenTofu provider (`openporch_user`, `openporch_application_user`, `openporch_environment_type_user`, `openporch_service_user_token`).

#### Scenario: Grant through the provider
- **WHEN** a configuration declares `openporch_application_user` with user `ci`, app `shop` and role `developer`
- **THEN** `tofu apply` grants the role and a later plan shows no changes

### Requirement: Reference and action authorization

Handlers SHALL supplement route checks with referenced-object scope, secret grants, action permits and delegation ceilings. Foreign set/delta/deployment IDs SHALL not bypass visibility or deployer gates. Environment-value changes require that type's deployer role; application-wide values require deployer on every type in the app. Changed authorization SHALL invalidate affected unused permits and block newly unauthorized execution.

#### Scenario: Cross-application deployment reference
- **WHEN** a developer submits a deployment ID from an application they cannot access
- **THEN** the operation is rejected without disclosing its contents or deploying it

#### Scenario: Development user changes production configuration
- **WHEN** a developer without production deployer permission changes a production override or an application-wide value used by a production environment
- **THEN** the request returns `403`

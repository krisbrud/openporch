# Security policy

## Reporting a vulnerability

Report vulnerabilities privately through
[GitHub's private vulnerability reporting](https://github.com/krisbrud/openporch/security/advisories/new).
Do not open a public issue or pull request for one.

Include the version or commit, how to reproduce it, and what an attacker
gains. You will get an acknowledgement, and a fix or a plan, as soon as the
maintainer can manage. openporch is maintained by one person in their spare
time, so there is no guaranteed response time. Please give a reasonable
period for a fix before you disclose the issue publicly. Fixes are published
as GitHub security advisories and in the release notes.

## Supported versions

openporch is pre-1.0. Only the latest release, and `main`, get security
fixes.

## Scope

In scope:

- authentication, sessions, tokens and RBAC on any route, including a member
  learning that an application exists without the right to see it;
- secret values reaching logs, errors, API responses, audit records or the
  planner;
- secret use without a grant, or across the boundaries a grant sets;
- destruction or deprovisioning without a permit, or that a permit did not
  authorize;
- the OpenTofu driver running code or reaching hosts its configuration does
  not allow;
- backups or restores exposing the master key or secrets.

Out of scope:

- isolation between hostile tenants: an installation serves one organization;
- a compromised host, data directory or master key;
- vulnerabilities in OpenTofu modules, providers or HTTP drivers you
  configure, which run with the access you give them;
- denial of service by an authenticated administrator.

The [security model](docs/security.md) describes what openporch protects and
how.

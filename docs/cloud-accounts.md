# Cloud accounts

A cloud account holds the credentials openporch uses to reach a cloud. Resource
definitions name an account in `driver_account` instead of carrying keys. Only
administrators create, change, check or delete accounts. Members can read them,
but never the secret values.

openporch supports three AWS account types:

| Type | Configuration | Credentials |
| --- | --- | --- |
| `aws` | `access_key_id`, optional `region`; the secret `secret_access_key` | The static key pair. The secret is written to the primary secret store and the account keeps only a reference. |
| `aws-role` | `role_arn`, `external_id`, optional `session_duration` (900 to 43200 seconds, default 3600) and `region` | A role session that openporch assumes with `sts:AssumeRole` from the identity of the host it runs on. |
| `aws-identity` | `role_arn`, optional `audience` (default `sts.amazonaws.com`), `session_duration` and `region` | A role session that openporch obtains with `sts:AssumeRoleWithWebIdentity`, presenting a short-lived token from its own [OIDC issuer](operations.md#oidc-issuer). Neither openporch nor its host holds any AWS credential for it. |

An `aws-identity` account needs the issuer, which AWS must reach over HTTPS:
without `public_url` in the server configuration, creating or changing one
returns `400`.

## How credentials are obtained

Only the executor obtains credentials, immediately before use:

- An `aws` account's secret access key is read from its reference.
- An `aws-role` account assumes the role. The host's own identity comes from the
  standard AWS chain: environment variables, shared config and SSO profiles, web
  identity, or ECS and EC2 instance metadata. The session name is
  `openporch-<account-id>` and appears in CloudTrail.
- An `aws-identity` account signs an identity token with the issuer's current
  key, with `sub` `<org>/<account-id>`, `aud` the account's audience and a
  lifetime of five minutes, and exchanges it for a role session with
  `sts:AssumeRoleWithWebIdentity`. The call is unsigned, so the host needs no
  AWS identity. The token is never stored, logged or returned. The session
  name is `openporch-<account-id>`.

Role sessions are kept in memory and reused while they stay valid for the whole
operation plus one minute. Otherwise openporch assumes the role again. If the
operation needs more time than `session_duration` allows, it fails before any work
starts. Raise the duration, and the role's maximum session duration in IAM, to
match.

Drivers receive exactly four values: `AccessKeyId`, `SecretAccessKey`,
`SessionToken` (empty for static keys) and `Region`. They never receive the host's
own credentials. Credentials are not stored, logged or returned by the API.

## Checking an account

```sh
curl -X POST -H "Authorization: Bearer $TOKEN" \
  https://openporch.example/orgs/acme/resources/accounts/aws-prod/check
```

On success the check returns the identity STS reports:

```json
{
  "arn": "arn:aws:sts::111122223333:assumed-role/openporch/openporch-aws-prod",
  "account": "111122223333",
  "user_id": "AROAEXAMPLE:openporch-aws-prod"
}
```

On failure it returns one of these errors, with the STS message:

| Status | Code | Meaning |
| --- | --- | --- |
| `422` | `CLD-001` | AWS refused the request, for example because of a wrong external ID or an invalid key. |
| `502` | `CLD-002` | AWS could not be reached, or the host has no identity to assume the role with. |
| `400` | `API-003` | The account cannot produce credentials for the check. |

## Setting up a role account

1. Create an IAM role for openporch. Its trust policy allows the host identity and
   requires an external ID:

   ```json
   {
     "Version": "2012-10-17",
     "Statement": [{
       "Effect": "Allow",
       "Principal": {"AWS": "arn:aws:iam::111122223333:role/openporch-host"},
       "Action": "sts:AssumeRole",
       "Condition": {"StringEquals": {"sts:ExternalId": "a-long-random-value"}}
     }]
   }
   ```

2. Give the host identity permission for `sts:AssumeRole` on that role, and
   nothing else it does not need.
3. Attach only the permissions your drivers need to the role. Reference modules
   scope actions and resources to what they declare.
4. Create the account and check it:

   ```json
   {"id": "aws-prod", "type": "aws-role",
    "config": {"role_arn": "arn:aws:iam::111122223333:role/openporch",
               "external_id": "a-long-random-value", "region": "eu-north-1"}}
   ```

## Setting up an identity account

1. Set `public_url` so the issuer is on, and make sure
   `<public_url>/oidc/.well-known/openid-configuration` and `/oidc/jwks` are
   reachable from the internet over HTTPS with a certificate from a public CA.
2. Print the setup for the account. It need not exist yet:

   ```sh
   porch cloud-accounts setup aws-identity aws-prod
   ```

   It prints the identity provider URL, the audience, the subject
   (`acme/aws-prod`), a note on thumbprints, the command that creates the IAM
   OIDC identity provider, and the trust policy for the role:

   ```json
   {
     "Version": "2012-10-17",
     "Statement": [{
       "Effect": "Allow",
       "Principal": {"Federated": "arn:aws:iam::111122223333:oidc-provider/porch.example.com/oidc"},
       "Action": "sts:AssumeRoleWithWebIdentity",
       "Condition": {"StringEquals": {
         "porch.example.com/oidc:sub": "acme/aws-prod",
         "porch.example.com/oidc:aud": "sts.amazonaws.com"
       }}
     }]
   }
   ```

   The `sub` condition is what scopes the role to this one account: another
   account's token names another subject, and STS refuses it. Until the account
   exists, the AWS account in the provider ARN is a placeholder;
   `GET /orgs/{org}/oidc/aws-identity/{account}` returns the same setup as JSON.
   AWS checks the issuer's certificate against the certificate authorities it
   trusts, so no thumbprint needs maintaining.
3. Create the identity provider once per AWS account, create the role with that
   trust policy, and attach only the permissions your drivers need.
4. Create the account and check it:

   ```json
   {"id": "aws-prod", "type": "aws-identity",
    "config": {"role_arn": "arn:aws:iam::111122223333:role/openporch",
               "region": "eu-north-1"}}
   ```

   `porch cloud-accounts check aws-prod` prints the assumed-role identity. The
   check's answer for an `aws-identity` account also carries the setup under
   `setup`, and a refused check names the setup command.

Rotating the issuer's signing key does not interrupt identity accounts: AWS
fetches the new key from the JWKS, and the old one stays published for its
grace period.

## The real account check

Unit tests run against a fake STS. A test against a real AWS account is ignored by
default. To run it:

1. Make the host credentials available, for example with `aws sso login` or
   `AWS_PROFILE`.
2. Create a test role as described above.
3. Run:

   ```sh
   OPENPORCH_TEST_AWS_ROLE_ARN=arn:aws:iam::111122223333:role/openporch-test \
   OPENPORCH_TEST_AWS_EXTERNAL_ID=a-long-random-value \
   cargo test -p openporch-engine real_account_check -- --ignored
   ```

It assumes the role, checks that STS reports the `openporch-aws-prod` assumed-role
session, and checks that a wrong external ID fails with `AccessDenied`.

### Against Floci

CI runs the same test against [Floci](https://floci.io), a local AWS emulator,
in the `account check against Floci` job. Floci is started with
`FLOCI_SERVICES_IAM_ENFORCEMENT_ENABLED=true`, so it evaluates the role's trust
policy and refuses a wrong external ID. To run it locally with Docker and the AWS
CLI:

```sh
docker run -d --name floci -p 4566:4566 \
  -e FLOCI_SERVICES_IAM_ENFORCEMENT_ENABLED=true floci/floci:2.2.0
export AWS_ENDPOINT_URL=http://localhost:4566 AWS_ACCESS_KEY_ID=test \
  AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1
caller=$(aws sts get-caller-identity --query Arn --output text)
aws iam create-role --role-name openporch-test --assume-role-policy-document "{
  \"Version\": \"2012-10-17\",
  \"Statement\": [{\"Effect\": \"Allow\", \"Principal\": {\"AWS\": \"$caller\"},
    \"Action\": \"sts:AssumeRole\",
    \"Condition\": {\"StringEquals\": {\"sts:ExternalId\": \"ext-123\"}}}]}"
OPENPORCH_TEST_AWS_ROLE_ARN=arn:aws:iam::000000000000:role/openporch-test \
OPENPORCH_TEST_AWS_EXTERNAL_ID=ext-123 \
  cargo test -p openporch-engine real_account_check -- --ignored
```

This exercises the AWS SDK path end to end: the host credential chain, AssumeRole
with the external ID, and GetCallerIdentity with the assumed session. It does not
replace a check against a real account, because Floci only emulates IAM.

### An identity account against real AWS

`real_identity_account_check` in `crates/server/src/tests/oidc.rs` is ignored by
default too. It starts a server that serves its issuer on a local address you
expose publicly over HTTPS, for example through a tunnel, creates an
`aws-identity` account for a role whose trust policy `porch cloud-accounts
setup` printed, checks it, and checks that the same role refuses another
account's subject. No AWS credentials are needed on the machine:

```sh
OPENPORCH_TEST_OIDC_PUBLIC_URL=https://porch-test.example.com \
OPENPORCH_TEST_OIDC_LISTEN=127.0.0.1:8443 \
OPENPORCH_TEST_OIDC_DATA_DIR=$HOME/openporch-oidc-test \
OPENPORCH_TEST_AWS_IDENTITY_ROLE_ARN=arn:aws:iam::111122223333:role/openporch-oidc-test \
  cargo test -p openporch-server real_identity_account_check -- --ignored --nocapture
```

The data directory keeps the issuer's key between runs. Point the tunnel at the
listen address and create the identity provider and role from the setup the test
prints, for account `aws-prod` in organization `acme`, before the check runs;
the test waits up to ten minutes for the role to trust it.

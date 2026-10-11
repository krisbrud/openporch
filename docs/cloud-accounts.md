# Cloud accounts

A cloud account holds the credentials openporch uses to reach a cloud. Resource
definitions name an account in `driver_account` instead of carrying keys. Only
administrators create, change, check or delete accounts. Members can read them,
but never the secret values.

Milestone 1 supports two AWS account types:

| Type | Configuration | Credentials |
| --- | --- | --- |
| `aws` | `access_key_id`, optional `region`; the secret `secret_access_key` | The static key pair. The secret is written to the primary secret store and the account keeps only a reference. |
| `aws-role` | `role_arn`, `external_id`, optional `session_duration` (900 to 43200 seconds, default 3600) and `region` | A role session that openporch assumes with `sts:AssumeRole` from the identity of the host it runs on. |

`aws-identity` is reserved for a later OIDC integration. Creating one returns `400`.

## How credentials are obtained

Only the executor obtains credentials, immediately before use:

- An `aws` account's secret access key is read from its reference.
- An `aws-role` account assumes the role. The host's own identity comes from the
  standard AWS chain: environment variables, shared config and SSO profiles, web
  identity, or ECS and EC2 instance metadata. The session name is
  `openporch-<account-id>` and appears in CloudTrail.

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

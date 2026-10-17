# End-to-end tests

`just test` and CI run the in-memory end-to-end tests, which use fake drivers.
CI also applies [`examples/aws-platform`](../examples/aws-platform) and the
modules against Floci, an AWS emulator (see `just example-platform`). Neither
runs a Lambda function. `just e2e-aws` does that, in a real AWS account. It
is opt-in, and it creates resources that AWS bills.

## `just e2e-aws`

[`scripts/e2e-aws.sh`](../scripts/e2e-aws.sh) starts a dev server and takes
these steps:

1. It applies `examples/aws-platform` with a fresh name prefix (`ope2e-<hex>`)
   and checks that openporch can assume the platform's role.
2. It creates the application `shop<hex>`, the environment `e2e` and the secret
   shared value `ORDER_SIGNING_KEY`. The application has the run's hex, so the
   AWS names (`shop<hex>-e2e-...`) of two runs never collide.
3. It uploads the order worker's zip to the artifact bucket, and pushes the
   API's image to a new ECR repository.
4. It deploys [`examples/lambda-order-worker`](../examples/lambda-order-worker)
   and [`examples/lambda-api`](../examples/lambda-api) with
   `porch workload deploy --wait`.
5. It checks these in AWS:
   - both functions;
   - the worker's code hash and its environment: `TABLE`, and
     `SIGNING_KEY_SECRET_REF` without the key itself;
   - the table, the queue, and the queue's enabled event source mapping;
   - the nightly schedule, `cron(0 3 * * ? *)` in `Europe/Oslo`.
6. It sends a signed order to the queue and waits for the worker to store it.
7. It calls the API's function URL at `/`, `/health` and an unknown path.
8. It uploads a second build of the worker under the same key and deploys it,
   then checks that the function runs the second zip and still stores orders.
9. It replays the snapshot of the API's deployment, the last one with the first
   zip, with `porch deploy deploy ID --replay snapshot`. Then it checks that the
   function runs the first zip again: S3 keeps both versions, and the deployment
   pinned the first one. It also checks that orders are still stored.
   Then it checks that the signing key is in neither function's environment
   or CloudWatch log, the server's log, any deployment's log or the active
   resources. It reads each CloudWatch log once it reports every invocation the
   test made, since Lambda logs can arrive minutes late. The deployment
   commands' output is kept in the working directory and printed with any line
   holding the key left out.
10. It deletes the environment with `porch delete env`, acknowledging each
    deletion the preview marks for a permit. It waits until the environment is
    gone, then checks that none of these is left. A resource counts as gone
    only on its service's not-found error; any other error fails the run:
    - the functions, the table, the queue, the schedule and the event source
      mappings;
    - the log groups, the SSM parameters, and the IAM roles and policies.
11. It lists what is kept and what it costs. Then it deletes the application,
    the ECR repository and every artifact version, and destroys the platform.

The results are printed and written to `target/e2e-aws-results.md`, or to
`OPENPORCH_E2E_RESULTS`.

### What it needs

- **An AWS account the test may create and delete resources in.** Use a
  sandbox account, not one with production in it. Name the account in
  `OPENPORCH_E2E_AWS_ACCOUNT`: the script refuses to run unless the
  credentials belong to that account, and it refuses when
  `AWS_ENDPOINT_URL` is set.
- **AWS credentials** in the environment or a profile, with `AWS_REGION` set.
  The identity needs to do the following:
  - create IAM roles and policies, an S3 bucket and an ECR repository;
  - call STS, Lambda, DynamoDB, SQS, EventBridge Scheduler, CloudWatch Logs, SSM
    and the Resource Groups Tagging API.

  `AdministratorAccess` in a sandbox account is the simple choice. The server
  runs with the same credentials and assumes the platform's role with them, so
  the role trusts that identity:
  - for an IAM user, its ARN;
  - for a role session (SSO, `assume-role`), the role, whose ARN with its path
    the script reads with `iam:GetRole`. Without that permission, set
    `OPENPORCH_E2E_TRUSTED_PRINCIPAL` to the role's ARN.
- **The commit pushed to GitHub.** The definitions fetch the modules from
  `https://github.com/krisbrud/openporch` at the commit being tested, and the
  script checks that the commit is there. `OPENPORCH_E2E_MODULES_REV` picks
  another commit, branch or tag. The platform itself is applied from the
  working tree, so the script warns when `examples/` has uncommitted changes.
- **Tools:** `go`, `tofu`, AWS CLI v2, `docker` (with `buildx`), `git`, `zip`,
  `openssl`, `curl`, `jq` and `python3`. The script checks for them before it
  creates anything.

```sh
export AWS_PROFILE=sandbox AWS_REGION=eu-north-1
OPENPORCH_E2E_AWS_ACCOUNT=111122223333 just e2e-aws
```

A run takes roughly 10 to 20 minutes. Most of that is IAM propagation and the
OpenTofu runs.

### Cost

Everything in the test is pay-per-use, and it all exists for less than an
hour. The worker runs three times and the API three times, which is well
inside the Lambda free tier. These cost a fraction of a cent:

- DynamoDB on-demand writes and storage;
- a few SQS requests;
- S3 storage of two small zips;
- ECR storage of one image, about 200 MB;
- log storage.

Nothing is provisioned by the hour. The table is on demand, the functions
have no provisioned concurrency, and there is no NAT gateway or load balancer.
A run that fails and leaves resources behind costs about the same per day,
until they are deleted.

### When it fails

The script deletes what it can before it exits:

- the environment, through openporch, once the first deployment has started,
  after waiting for a deployment that is still running;
- the application and the ECR repository;
- the platform, after emptying the artifact bucket.

Whatever it could not delete is printed after `NOT DELETED`. In that case it
keeps its working directory and prints the path. The directory holds:

- `data/`: the server's database and the OpenTofu state of every resource
  openporch created;
- `platform/`: the platform's OpenTofu state;
- the server log and the deletion preview.

The directory holds the server's master key, an admin token, and OpenTofu
state with the signing key in plain text. Keep it private, and delete it once
the cleanup is done.

To finish the cleanup by hand:

1. Start a server on the directory with the same AWS credentials:
   `OPENPORCH_ORG=acme OPENPORCH_DATA_DIR=<dir>/data openporch serve`.
2. Use the admin token in `data/bootstrap-token`; it expires 24 hours after the
   run started. Set `OPENPORCH_URL`, `OPENPORCH_ORG=acme` and `OPENPORCH_TOKEN`.
3. Delete the environment with `porch delete env e2e --app shop<hex>`. Use the
   `--plan` and `--ack` arguments that its `--preview` prints.
4. Delete the application with `porch delete app shop<hex>`.
5. Empty the artifact bucket, every version included.
6. Run `TF_CLI_CONFIG_FILE=<dir>/tofurc tofu destroy -var external_id=<any 16
   characters>` in `platform/`, with the same `OPENPORCH_*` variables. Its other
   variables are in `platform/e2e.auto.tfvars.json`, and the provider is in
   `<dir>/mirror`.

Resources that the state no longer covers can be found by name:

- functions, roles, schedules and log groups start with `shop<hex>-e2e-` (log
  groups under `/aws/lambda/`);
- SSM parameters are under `/openporch/shop<hex>-e2e-`;
- the table is `shop<hex>.e2e.modules.order-worker.externals.orders`;
- policies, queues and buckets start with the run's prefix `ope2e-<hex>`;
- the ECR repository is `ope2e-<hex>-lambda-api`.

## Results

| Date | Commit | Account, region | Result |
| --- | --- | --- | --- |
| 2026-10-17 | `1134dfc` | A sandbox account, eu-north-1 | Passed in about 16 minutes. Every step ran, the environment's deletion took 7 permits, and nothing the run created was left in the account. |

Add a row for each manual run, with the summary from
`target/e2e-aws-results.md`.

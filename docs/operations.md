# Operating openporch

This page covers running one openporch server: who owns an installation, how
to tell whether the server can accept changes, what happens when it stops, and
how much work it takes on, and how to back up, upgrade and uninstall it.

## One server per installation

An installation is its data directory. The server takes an exclusive lock on
it before it opens the database, and holds the lock until it exits. A second
server started on the same data directory exits at once with

```text
the installation at <data_dir>/... is owned by another openporch process
```

and does no work: it neither migrates the database nor starts or recovers
deployments. Run exactly one server per data directory. A copy of the
directory on another host is a second installation that manages the same
cloud resources, so do not start one while the original runs.

## Health

| Route | Answers |
| --- | --- |
| `/livez` | `200` while the process runs |
| `/readyz`, `/healthz` | `200` when the server can accept changes, otherwise `503` with reason codes |

`/readyz` reports these reasons. They name key IDs only, never key material.

| Reason | Meaning | What to do |
| --- | --- | --- |
| `master_key_missing:<key id>` | The database needs a master key the server does not have | Restore the key file or `OPENPORCH_MASTER_KEY`, then restart |
| `master_key_missing` | No master key was found for an existing installation | As above |
| `database_unavailable` | The database does not answer | Check the disk and the data directory |
| `executor_unavailable` | There is no executor to run deployments with | Restart once the keys load; the executor starts with them |
| `restore_reconciling` | The installation was restored from a backup and its resources are being observed | Wait; see [Backup and restore](#backup-and-restore) |
| `shutting_down` | Shutdown has begun | Wait for the restart |

While the server is not ready, every request that changes something gets
`503` with error `SRV-002` and the same reasons. Reads keep working, so
`porch` can still show deployments, resources and audit logs. Cloud outages do
not affect readiness: they fail the deployments that need the cloud, not the
server.

## Logs

The server writes one JSON object per line to standard output. Each line
carries the fields of the spans it happened in, so lines about the same work
can be found together:

| Field | Where it comes from |
| --- | --- |
| `request_id` | The caller's `x-request-id` header, or one the server generates; it is returned in the same header and stored on audit entries |
| `app`, `env`, `deployment` | The deployment the line belongs to |
| `operation`, `guresid`, `resource`, `driver`, `attempt`, `action` | One driver operation (`provision`, `delete` or `observe`) |
| `outcome` | `succeeded`, `failed`, `retryable` or `unknown` on `driver finished` and `deployment finished` lines |

`driver finished` reports what the driver returned. If the result cannot be
recorded afterwards, a `could not checkpoint an operation` line follows and the
resource's outcome becomes unknown until it is recovered. `deployment finished`
is logged only once the deployment's outcome is recorded.

A deployment keeps the `request_id` of the request that started it, so a
failure reported later can be traced to that request, and the audit entry for
the deployment's outcome carries the same `request_id`. Secret values never
appear in logs: driver output and errors are redacted before they are written.
Set `RUST_LOG` to change the level, for example `RUST_LOG=info,openporch_engine=debug`.
The request, deployment and operation fields stay on every line at any level,
`RUST_LOG=warn` included.

## Metrics

`GET /orgs/{org}/metrics` returns metrics in the Prometheus text format. It
needs a token for an administrator or org viewer, for example a service user
with the `orgViewer` role. The server sends nothing anywhere on its own; a
scraper has to ask.

```yaml
scrape_configs:
  - job_name: openporch
    metrics_path: /orgs/acme/metrics
    authorization:
      credentials_file: /etc/prometheus/openporch-token
    static_configs:
      - targets: ["openporch.example.com:8080"]
```

| Metric | What it measures |
| --- | --- |
| `openporch_deployments_queued`, `openporch_deployments_running`, `openporch_deployments_capacity` | Accepted deployments waiting for room, running now, and how many may run at once |
| `openporch_deployments_finished_total{outcome}` | Finished deployments, `succeeded` or `failed` |
| `openporch_deployment_run_seconds_sum`, `_count` | How long deployments ran |
| `openporch_driver_calls_running`, `openporch_driver_concurrency` | Driver calls running now, and the limit |
| `openporch_driver_call_seconds_sum`, `_count{action,outcome}` | Finished driver calls and their duration; `outcome="retryable"` counts the calls that were retried |
| `openporch_resources{status}` | Active resources by status |
| `openporch_resources_needing_recovery` | Resources with an unknown outcome; see [`porch resources recover`](cli.md#porch-resources-recover) |
| `openporch_database_bytes`, `openporch_database_wal_bytes`, `openporch_database_free_bytes` | The database's files on disk (write-ahead log included), the write-ahead log alone, and the free space inside the database |
| `openporch_audit_entries`, `openporch_deployment_log_lines` | Stored audit entries and deployment log lines |
| `openporch_audit_expirable_entries`, `openporch_audit_oldest_entry_timestamp_seconds` | Audit entries older than the retention period, and when the oldest entry was written |
| `openporch_audit_last_expiry_timestamp_seconds{outcome}`, `openporch_audit_expiry_failures_total` | When audit entries were last expired and whether that succeeded, and failed expiries since the server started |
| `openporch_ready`, `openporch_build_info{version}` | Readiness, as `/readyz` reports it, and the version |

Counters start at zero when the server starts. Alert on
`openporch_resources_needing_recovery > 0`, on deployments that stay queued,
and on `openporch_database_bytes` approaching the free space of the disk.

## Diagnostics

`GET /orgs/{org}/diagnostics` returns a JSON report for administrators and org
viewers to save and attach to a bug report:

```sh
curl -H "Authorization: Bearer $TOKEN" \
  "https://openporch.example.com/orgs/acme/diagnostics?app=shop&env=production&deployment=$DEPLOYMENT" \
  > diagnostics.json
```

| Field | What it holds |
| --- | --- |
| `version`, `contract`, `generated_at` | The server's version, API contract and when the report was made |
| `ready`, `not_ready` | Readiness, as `/readyz` reports it |
| `config` | The configuration, with paths to key and secret files reported only as configured or not |
| `work`, `storage`, `resources` | What the metrics report: queued and running work, database and log storage, resources by status |
| `needing_recovery` | The latest operation of every resource whose outcome is unknown: its ID, deployment, GUResID, attempt, error and the resource's status |
| `audit` | The retention period, how many entries are kept and how many could expire, the oldest entry, the last expiry and failed expiries |
| `deployment` | Only with `app`, `env` and `deployment`: its status and error, the request ID that started it, its operations, its audit entries and the last 200 lines of its log |

The operation, deployment and request IDs are the ones in the [logs](#logs),
so a report and the server's log can be read together. Errors and log lines
are redacted before they are stored, and the report never reads resource
inputs, outputs or driver state, so it holds no secret values.

## Limits

The server bounds the work it takes on, so a burst of deployments cannot use
up the machine or starve the API. Set them in a `[limits]` table:

```toml
[limits]
driver_concurrency = 4         # driver operations at once, OpenTofu runs included
running_deployments = 64       # deployments and deletions running or waiting at once
deployment_minutes = 30        # how long one deployment may run (at most a week)
deployment_log_bytes = 16777216  # log text kept per deployment
graph_nodes = 500              # resources in one deployment
graph_edges = 5000             # dependencies in one deployment
```

| Limit | When it is reached |
| --- | --- |
| `driver_concurrency` | Further driver operations, from any environment, wait for a slot. With the default, at most four OpenTofu processes run at once. A recovery request waits at most 30 seconds for one, then fails and can be repeated. |
| `running_deployments` | New deployments and deletions get `429` with `Retry-After: 30`, and nothing is stored. Deleting an application takes a slot for each environment it starts deleting; one with more environments than the limit needs every slot free, and its other deletions wait for room. |
| `deployment_minutes` | Operations still running fail, and resources whose outcome is unknown need recovery. |
| `deployment_log_bytes` | Later log lines are dropped after a warning line saying so; each resource's final status is still logged. |
| `graph_nodes`, `graph_edges` | The deployment is refused with `400` before anything is stored. |

Request bodies are limited by `max_body_bytes` (default 1 MiB), answered with
`413`. Logins and token creation are rate limited per minute by
`[rate_limits]` `login_per_minute` (default 60) and `tokens_per_minute`
(default 20), answered with `429` and `Retry-After`.

Waiting deployments hold no request open, so status requests, previews and
reads keep answering while the driver slots are full. A store that cannot
write fails the change with `500`; nothing is accepted that could not be
recorded.

## Container registries

Acceptance pins every image a workload names by tag to the digest the tag
points at, and a deployment checks that each pinned digest still exists
before it changes anything. Images in Amazon ECR are read with the workload
definition's cloud account. Images in other registries, such as Docker Hub or
`ghcr.io`, are read over HTTPS without credentials, so the server needs
direct outbound access to them. These reads ignore `HTTPS_PROXY` and the other
proxy variables, since a proxy would reach addresses the server refuses; a
server whose only way out is a proxy sets `pin_other_registries = false`.

A tag that does not exist is refused with `400`. An image the registry shows
only with credentials is refused too; name it by digest
(`<image>@sha256:<digest>`) instead. Its digest is then not checked before a
run, and its driver reports it when it is gone. An image pinned from a public
tag keeps the tag (`<image>:<tag>@sha256:<digest>`), and a run fails before it
changes anything if the registry no longer shows it without credentials.

Registries on loopback, private or link-local addresses are not read, so an
image name cannot make the server probe its own network, and redirects are not
followed. A tag in such a registry is refused with `400`; an image named there
by digest is not checked, nor is one in a registry whose name the server
cannot resolve, such as one known only inside a cluster. List a registry on such an address to read it, as
`host[:port]` exactly as images name it. The listing covers that port only,
and a token service the registry names is read on a private address only when
it is listed too:

```toml
[images]
# Leave images outside ECR named by tag, unpinned and unchecked.
pin_other_registries = false
# Registries on private addresses, read over HTTPS.
private_registries = ["registry.internal:5000"]
# Registries reached over plain HTTP, such as a local test registry. They may
# be on private addresses too.
insecure_registries = ["localhost:5000"]
```

## Stopping the server

On `SIGTERM` or Ctrl-C the server:

1. refuses new changes and logins with `503` and `shutting_down`, while
   status requests keep working;
2. waits up to `shutdown_grace_seconds` (default `60`) for the changes it had
   already accepted, and for running deployments, to finish;
3. logs the deployments still running when the wait ends, gives open
   connections 5 more seconds to close, then exits.

When the server exits, it stops the OpenTofu processes of the deployments
still running, so the resources they were changing may be partly changed.
Every cloud operation is recorded before it starts, so nothing is lost: on the
next start the server marks the interrupted deployments `failed`, and observes
each resource whose last operation has an unknown outcome before any
deployment may change it again.

To give long deployments more time, raise the grace period:

```toml
shutdown_grace_seconds = 600
```

A service manager should allow at least that long before it kills the
process, for example `TimeoutStopSec=` in a systemd unit.

## What the data directory holds

| Path | Holds |
| --- | --- |
| `openporch.db` (with `-wal` and `-shm` while running) | Configuration, deployments, resource records, recovery records, audit log, sealed secrets |
| `openporch.lock` | The ownership lock |
| `master.key` | The master keys, unless `master_key_file` names another file or `OPENPORCH_MASTER_KEY` holds the key itself |
| `opentofu/` | Run directories, the provider cache and, with the default `local` state backend, every resource's OpenTofu state. `[opentofu].root` moves it elsewhere |
| `bootstrap-token` | The first administrator token, until you delete it, unless `bootstrap_token_file` names another place |

Treat the whole directory, and the master key wherever it lives, as secret.
With the `s3` state backend, resources' state is in the bucket instead of
`opentofu/`.

## Backup and restore

Stop the server, then take a backup into a new or empty directory:

```sh
openporch backup --config /etc/openporch/openporch.toml /backups/2026-10-16
```

It refuses while the server runs, since the database and the state files must
match, and refuses a destination inside the data directory or the OpenTofu
root. A backup that fails removes what it wrote. The backup directory, readable by its owner only, holds:

- `backup.json`, the manifest: when and by which version it was taken, the
  schema version, the organization, the number of active resources, where
  OpenTofu state is kept, and the master key IDs a restore needs;
- `data/`, a consistent copy of the database (with its recovery records, audit
  log and sealed secrets) and every other file of the data directory except
  the lock, the master key and the bootstrap token;
- `opentofu-state/`, the OpenTofu root's `state` directory, wherever
  `[opentofu].root` places it: with the `local` backend every resource's
  state, and with either backend each resource's lock file, inline script and
  `retained.json`. Run directories and the provider cache are rebuilt, so they
  are left out.

The backup leaves out, and you keep alongside it:

- the master key, wherever it lives: its file, or the value of
  `OPENPORCH_MASTER_KEY`. Keep it apart from the backup: a backup is only
  useful with it, and anyone holding both can read every stored secret;
- the configuration file, or the environment it is set through;
- the commits Git sources were applied from, and with the `s3` backend the
  state object versions, as below;
- versions of secrets kept in stores outside openporch.

To restore, on the same or a new host, with the same configuration (the
organization and the state backend must match the manifest) and an absent or
empty data directory, and OpenTofu state directory when `[opentofu].root`
places it outside the data directory:

```sh
openporch restore --config /etc/openporch/openporch.toml /backups/2026-10-16
```

Put the master key back where the configuration expects it, then start the
server. A restored server starts read-only: until the driver of every
resource the backup records as provisioned has observed it, `/healthz` reports
`restore_reconciling` and every change is refused with `503` `SRV-002`, while
reads keep working. Each resource is then:

- unchanged, when it still exists: the backup's record of it stands, with its
  GUResID, outputs and status;
- `recovery_required`, when nothing exists for its identity: it was deleted
  after the backup, or its state was not restored. openporch does not create
  it again on its own; check the cloud, then `POST .../resolve` (an
  administrator) lets the next deployment provision it again with the same
  identity;
- `recovery_required` too when its driver cannot observe it or reports a
  failure it cannot repeat safely.

A resource whose observation fails, for example because the cloud is
unreachable, is tried again a few times; after that the server accepts
changes, and that resource stays blocked until `POST .../recover` observes it.
The audit log records each observation as `resource.recover` with reason
`restore`. Without the master keys the server is not ready at all; the
restored resources are observed on the first start that has them.
Resources the backup records as being deleted are not observed: the next
deployment deletes them, which a resource already gone does not fail. A
restore that fails halfway leaves the data directory marked, so a start still
observes what it restored; empty the directory before restoring again.

A backup only knows what existed when it was taken. Resources created after
it are not in the restored records, and a deployment that needs them again
creates new ones beside them, so after restoring an older backup find what
deployments made since then created (by the tags or names your definitions
give cloud objects) and remove or import it before deploying again.

### The drill

`backup_and_restore_drill` in `crates/server/src/tests/restore.rs` runs this
procedure on every `cargo test`: it deploys an application with resources,
refuses a backup while the server runs, backs up the stopped installation
and restores it into a new data directory with the master key copied on its
own, refuses a second restore into it, and starts it with one resource
deleted after the backup and observations held. While they are held,
readiness names `restore_reconciling`, reads work and a change gets `503`.
Once observed, every resource keeps its GUResID, the existing ones keep their
status, the deleted one is `recovery_required` and blocks deployments with
`DPL-004`, and after an administrator resolves it the next deployment
provisions it again with its old GUResID, so nothing is duplicated.

Run the drill against a real installation before relying on backups: restore
the latest backup into a scratch data directory on another host or port,
start it with the master key and the state backend's credentials, and check
that `/healthz` becomes ready and every resource is as expected.

### Git sources and S3 state

The OpenTofu root keeps, for each resource, the lock file, an inline script
and `retained.json`, which records the Git source URL, path and commit the
last apply used. Replays and deletions fetch that commit again, so a backup
of an installation that uses Git sources also needs the commits themselves,
in case a repository is deleted or rewritten later. Keep each repository
`retained.json` names alongside the backup, with every commit it records under
a ref of its own, since a mirror holds only what the remote's branches and
tags still reach:

```sh
find "$OPENTOFU_ROOT" -name retained.json \
  -exec jq -r 'select(.source) | "\(.source.url) \(.source.commit)"' {} + \
  | sort -u | { status=0; while read -r url commit; do
      dir="backup/sources/$(printf %s "$url" | sha256sum | cut -c1-16).git"
      [ -d "$dir" ] || git init -q --bare "$dir"
      printf '%s\n' "$url" > "$dir/openporch-url"
      if ! git -C "$dir" fetch -q "$url" "$commit:refs/openporch/$commit" ||
         ! git -C "$dir" cat-file -e "$commit^{commit}"; then
        echo "missing $commit from $url" >&2
        status=1
      fi
    done; exit "$status"; }
```

It exits non-zero when any commit could not be kept, so the backup is
incomplete. Fetching a private repository needs the same credentials the
definition uses.

openporch fetches a retained commit from the URL it recorded, so a mirror is
restored by serving it there again: recreate the repository at that URL and
push the kept commits into it:

```sh
for dir in backup/sources/*.git; do
  git -C "$dir" push "$(cat "$dir/openporch-url")" 'refs/openporch/*:refs/openporch/*'
done
```

openporch cannot be pointed at a different URL for resources it already
applied.

With the `s3` backend the state lives in the bucket, and later applies
overwrite it. Keep the bucket versioned and record, while the server is
stopped, the version ID of every state object under the key prefix. Use the
region and credentials the server uses. When `[opentofu.state]` sets
`role_arn`, use an AWS CLI profile that assumes that role (`role_arn` and
`source_profile` in `~/.aws/config`) and pass it as `PROFILE`; when it sets
`endpoint`, pass that as `ENDPOINT`:

```sh
aws s3api list-object-versions --region "$REGION" ${PROFILE:+--profile "$PROFILE"} \
  ${ENDPOINT:+--endpoint-url "$ENDPOINT"} \
  --bucket "$BUCKET" --prefix "$PREFIX/" \
  --query '{versions: Versions[?IsLatest].[Key, VersionId], delete_markers: DeleteMarkers[?IsLatest].[Key, VersionId]}' \
  --output json > backup/state-versions.json
```

Current delete markers are part of it: a key whose state was deleted must
stay deleted. A restore puts those versions back as the current ones (or
copies the objects into the backup instead), so the database and the state
it restores match. Secret stores outside openporch keep their own history; record the
versions you rely on where your store has them.


## Upgrades and the schema version

Each release knows the database schema versions up to its newest migration. On
start, a newer release migrates an older database forward. An older release
started on a database a newer release has migrated refuses to start, before it
writes anything:

```text
database schema version <found> is newer than this binary supports (up to <supported>);
run a compatible openporch version or restore a compatible backup
```

Each build runs on one schema version, its newest migration, and migrates
older databases forward to it on start. `openporch version` names the release
and that schema version, as in `openporch 0.1.0 (schema 11)`. A newer schema is
never opened by an older build, even when the newer migrations only added
columns: no downgrade pair is supported. The tested pairs are:

| Schema written by | Upgraded to schema 11 | Opened by the build of the schema before |
| --- | --- | --- |
| 8 (milestone 1 before protected environment types) | yes, from a fixture | refused, simulated |
| 9 (protected environment types) | yes, from a fixture | refused, simulated, and checked once by running the schema 8 build |
| 10 (external driver registrations pin versions and capabilities) | yes, from a fixture | refused, simulated |
| 11 (permits record the audit entry that issued them) | not applicable | refused, simulated |

A refused start writes nothing to the database; it only takes the
installation lock. "Simulated" means a test runs the current build's version
check with the older version, which is the check every build since the schema
guard runs before it migrates.

Every schema version a build ships gets a fixture in
`crates/server/fixtures/schema`: a data directory that build wrote, which every
later build must upgrade while serving the same applications, environments,
resources, deployments and values, reading the same secrets and deploying
again.

So downgrading means either going back to the newer release, or restoring a
backup taken before the upgrade and running the release that made it. Take a
backup before every upgrade. Restoring an older backup forgets every
deployment made since it was taken, including resources it created; check
those by hand before deploying again, as [Backup and
restore](#backup-and-restore) describes.

## Uninstalling

Stopping the server or removing the `openporch` binary never deletes cloud
resources: openporch changes infrastructure only when a deployment or a
deletion you requested runs. Uninstalling is three separate decisions.

1. **Stop managing.** Stop the server and remove the binary. Every resource
   keeps running and keeps costing what it costs. Keep the data directory and
   the master key: starting the same or a newer release on them later resumes
   management where it stopped.
2. **Tear down infrastructure**, if you want it gone. Do this while the server
   still runs, with deletions you preview and confirm, for example `porch
   delete app <app> --preview` and then the command it prints (see [Clean
   up](getting-started.md#clean-up)). Wait until `porch get apps` no longer
   lists the application.
3. **Remove the data.** Delete the data directory, the OpenTofu root if it
   is elsewhere, the master key and, with the `s3` backend, the state
   objects. Without them openporch cannot manage
   or delete what is left, so do this last, and only for resources you have
   torn down or will manage another way. To keep the option of resuming,
   keep a backup instead.

## Retention

Nothing in the data directory expires on its own. Recovery records and
resource state are kept for as long as their resources exist, and expiring
the audit log never touches them. The database grows with the number of
deployments; a backup copies all of it.

The audit log is kept until an administrator expires it, even for deleted
applications. `[audit] retention_days` (30 by default) sets how much expiry
keeps; set it to 365 or more to keep a year:

```toml
[audit]
retention_days = 365
```

To keep entries longer than that, export them before expiring:

```sh
set -e
BEFORE=$(porch expire-audit-logs --dry-run -o json | jq .before)
porch audit-logs --all --to "$BEFORE" -o json > "audit-before-$BEFORE.json"
porch expire-audit-logs --before "$BEFORE"
```

The dry run reports `before`: entries written earlier are past the retention
period. It also lists what expiry keeps: active resources by driver, those
needing recovery and those awaiting cleanup, whose cloud resources keep running
and may keep costing money until they are deleted. Passing the same `before` to the expiry removes exactly what was
exported, even though time has passed; a `before` inside the retention period
is refused. `porch audit-logs --all -o json` follows every page, newest first, and
writes each page as it arrives, so exports of any size fit in memory. If a page
cannot be read the command fails and the file is left as incomplete JSON;
check the exit status before expiring. `porch expire-audit-logs` on its own prints the dry run's report and removes
nothing; add `--yes` to expire what it reports, for example from cron or a
systemd timer. Each
expiry is recorded in the audit log (`audit_log.expire`) in the same
transaction as the removal, so no entry disappears without a record of who
removed it. A failed expiry removes nothing, is recorded as `failed` and shows
in the metrics (`openporch_audit_expiry_failures_total`,
`openporch_audit_last_expiry_timestamp_seconds{outcome="failed"}`) and in
[diagnostics](#diagnostics). Alert on `openporch_audit_expirable_entries > 0`
for longer than your expiry schedule.

When the disk is full, mutations are refused rather than accepted without an
audit entry. The audit log is not tamper-proof against someone with access to
the host and its data directory.

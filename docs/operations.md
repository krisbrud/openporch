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

To back up, stop the server and copy:

- the data directory;
- the OpenTofu root, when `[opentofu].root` places it outside the data
  directory, since with the `local` backend it holds every resource's state;
- the master key, wherever it lives: its file, or the value of
  `OPENPORCH_MASTER_KEY`;
- the bootstrap token file, while it exists and `bootstrap_token_file` places
  it outside the data directory: a restored installation does not issue a new
  one;
- the configuration file, or the environment it is set through.

The OpenTofu root keeps, for each resource, the lock file, an inline script
and `retained.json`, which records the Git source URL, path and commit the
last apply used. Replays and deletions fetch that commit again, so a backup
of an installation that uses Git sources also needs the commits themselves,
in case a repository is deleted or rewritten later. Keep each repository
`retained.json` names alongside the copy, with every commit it records under
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

openporch cannot yet be pointed at a different URL for resources it already
applied; the backup and restore drill in task 14.1 covers that.

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

Copying while the server runs can capture a database that does not match the
state files. Keep the master key apart from the copy: a backup is only useful
with it, and anyone holding both can read every stored secret.

## Upgrades and the schema version

Each release knows the database schema versions up to its newest migration. On
start, a newer release migrates an older database forward. An older release
started on a database a newer release has migrated refuses to start, before it
writes anything:

```text
database schema version <found> is newer than this binary supports (up to <supported>);
run a compatible openporch version or restore a compatible backup
```

So downgrading means either going back to the newer release, or restoring a
backup taken before the upgrade and running the release that made it. Take a
backup before every upgrade. Restoring an older backup forgets every
deployment made since it was taken, including resources it created; check
those by hand before deploying again (the backup and restore drill in task
14.1 covers this).

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

Nothing in the data directory expires on its own. The audit log is kept
indefinitely, even for deleted applications. Recovery records and resource
state are kept for as long as their resources exist. The database grows with
the number of deployments; a backup copies all of it.

# Operating openporch

This page covers running one openporch server: who owns an installation, how
to tell whether the server can accept changes, what happens when it stops, and
how to back up, upgrade and uninstall it.

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
| `master.key` | The master keys, unless `master_key_file` or `OPENPORCH_MASTER_KEY` names them elsewhere |
| `opentofu/` | Run directories, the provider cache and, with the default `local` state backend, every resource's OpenTofu state |
| `bootstrap-token` | The first administrator token, until it is used |

Treat the whole directory, and the master key wherever it lives, as secret.
With the `s3` state backend, resources' state is in the bucket instead of
`opentofu/`.

To back up, stop the server and copy the data directory and the master key.
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
backup before every upgrade.

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
3. **Remove the data.** Delete the data directory, the master key and, with
   the `s3` backend, the state objects. Without them openporch cannot manage
   or delete what is left, so do this last, and only for resources you have
   torn down or will manage another way. To keep the option of resuming,
   keep a backup instead.

## Retention

Nothing in the data directory expires on its own. The audit log is kept
indefinitely, even for deleted applications. Recovery records and resource
state are kept for as long as their resources exist. The database grows with
the number of deployments; a backup copies all of it.

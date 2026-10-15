# Operating openporch

This page covers running one openporch server: who owns an installation, how
to tell whether the server can accept changes, and what happens when it stops.

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

1. refuses new changes with `503` and `shutting_down`, while status requests
   keep working;
2. waits up to `shutdown_grace_seconds` (default `60`) for the changes it had
   already accepted, and for running deployments, to finish;
3. logs the deployments still running when the wait ends, then exits.

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

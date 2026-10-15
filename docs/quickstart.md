# Developer quickstart

This walks through running an openporch server on your machine and deploying a
workload to it with `porch`, without a cloud account: the definitions here use
the built-in `openporch/none` driver, which provisions nothing. Every command
in the `sh` blocks below runs as written in this repository's tests
(`crates/cli/src/tests/quickstart.rs`), against a dev server.

## Start a server

Build the two binaries from this repository, then start the server in a
directory of its own, outside the checkout, so its data stays out of git. The
server listens on `127.0.0.1:8080`, creates organization `acme`, and keeps its
database, master key and a bootstrap administrator token in `.openporch/`:

```bash
cargo install --path crates/server
cargo install --path crates/cli
mkdir -p ~/porch-demo && cd ~/porch-demo
OPENPORCH_ORG=acme OPENPORCH_DATA_DIR=.openporch openporch serve
```

## Log in

In another terminal, in the same `~/porch-demo` directory, point `porch` at the
server and store the bootstrap token. `porch login --token` reads the token from
standard input and keeps it in an owner-only credentials file next to
`~/.config/openporch/config.toml`:

```sh
export OPENPORCH_URL=http://127.0.0.1:8080
export OPENPORCH_ORG=acme
porch login --token < .openporch/bootstrap-token
```

The bootstrap token expires after 24 hours. For a lasting credential, create a
service user with `porch users create-service` and a token for it with
`porch users tokens create`; see the [command reference](cli.md). For an
interactive login with GitHub, configure
[GitHub sign-in](security.md#github-sign-in) on the server and run
`porch login` instead.

## Create an application

An application starts with a `development` environment. `--app` and `--env`
pick the application and environment for later commands; they can also come
from `OPENPORCH_APP` and `OPENPORCH_ENV` or the config file.

```sh
porch create app shop --name Shop
export OPENPORCH_APP=shop
export OPENPORCH_ENV=development
porch get envs
```

## Define how resources are provisioned

A resource definition says which driver provisions a resource type, and where.
Every environment has a `base-env` resource, and every workload is a `workload`
resource. These two definitions match everything (`criteria: [{}]`) and use the
`openporch/none` driver:

```sh
cat > base-env.yaml <<'EOF'
apiVersion: openporch.dev/v1
kind: Definition
metadata:
  id: base-env
entity:
  type: base-env
  driver_type: openporch/none
  criteria:
    - {}
EOF
cat > workloads.yaml <<'EOF'
apiVersion: openporch.dev/v1
kind: Definition
metadata:
  id: workloads
entity:
  type: workload
  driver_type: openporch/none
  criteria:
    - {}
EOF
porch create resource-definition -f base-env.yaml
porch create resource-definition -f workloads.yaml
porch get resource-definitions
```

With AWS, a definition would use `openporch/opentofu` with a module and a
`driver_account`; see [cloud accounts](cloud-accounts.md) and
[the OpenTofu driver](drivers/opentofu.md).

## Deploy a workload

A workload document describes what to run. The image is left as `.` and
supplied at deploy time, as CI would:

```sh
cat > workload.yaml <<'EOF'
apiVersion: openporch.dev/v1
kind: Workload
metadata:
  name: web
containers:
  web:
    image: .
    variables:
      PAGE_SIZE: "10"
EOF
porch workload deploy -f workload.yaml --image web=nginx:1.27 --comment "first deploy" --wait
```

`--wait` prints the deployment ID, follows its log until it finishes, and exits
non-zero if it fails. A Score file deploys the same way with
`porch score deploy -f score.yaml`.

## Look at what happened

```sh
porch get deploys
porch logs deploy +0
porch resources graph
porch get active-resources
```

`+0` is the latest deployment attempt, `+1` the one before it, and
`last-successful` the latest that succeeded.

## Change it and compare

Deploy again with a different value, then compare the two deployment sets:

```sh
porch workload deploy -f workload.yaml --image web=nginx:1.27 --property containers.web.variables.PAGE_SIZE=20 --wait
porch diff sets deploy/+1 deploy/.
```

## Preview a change

`porch workload plan` previews a deployment without running it: each resource's
change, what is not known until the drivers run, and the permits it needs.
A change that deletes or replaces a resource needs a permit for it: the preview
prints the command that carries it out, with one `--ack` per change.

```sh
porch workload plan -f workload.yaml --image web=nginx:1.27
```

## Roll back

Deploy the set of the deployment before the latest (`+1`) again, with the
configuration it recorded; `--replay current` would use the definitions and
values as they are now instead:

```sh
porch deploy deploy +1 --replay snapshot --wait
```

## Reach the rest of the API

`porch api` sends any authenticated request, for operations without a command
of their own:

```sh
porch api get /orgs/acme/resources/types
```

Every command is listed in the [command reference](cli.md), and every API
route in [the API reference](api.md).

# porch command reference

Generated from the help of `porch` itself; do not edit by hand. Regenerate it
with `OPENPORCH_UPDATE_DOCS=1 cargo test -p openporch-cli cli_reference`. The
[quickstart](quickstart.md) shows the commands in use.

## `porch`

```text
The openporch command line

Usage: porch [OPTIONS] <COMMAND>

Commands:
  login              Logs in with GitHub through the server, or stores an API token with `--token`. The server URL and organization are saved to the config file when it has none
  logout             Ends the session and forgets the stored token for the server
  get                Lists objects, or shows one
  create             Creates an object
  delete             Deletes an object. Sets and deployment records are kept for good
  users              Manages the organization's members and their tokens
  apps               Manages application users
  env-types          Manages environment type deployers
  secretstores       Manages secret stores
  audit-logs         Queries the audit log, newest first
  expire-audit-logs  Removes the audit entries older than the server's retention period
  workload           Deploys or previews workload documents
  score              Deploys Score files
  deploy             Deploys a delta, or an earlier deployment again
  logs               Prints logs
  diff               Compares deployment sets
  resources          Shows and recovers an environment's resources
  api                Sends an authenticated request to the API and prints the JSON response
  help               Print this message or the help of the given subcommand(s)

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

      --token <TOKEN>
          An API token, before the command. Prefer `OPENPORCH_TOKEN` or `porch login --token`, which keep it out of the shell history

  -h, --help
          Print help

  -V, --version
          Print version
```

## `porch login`

```text
Logs in with GitHub through the server, or stores an API token with `--token`. The server URL and organization are saved to the config file when it has none

Usage: porch login [OPTIONS]

Options:
      --token
          Store an API token, read from standard input or a hidden prompt

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --invitation <CODE>
          The invitation code, on a first GitHub login

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch logout`

```text
Ends the session and forgets the stored token for the server

Usage: porch logout [OPTIONS]

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch get`

```text
Lists objects, or shows one

Usage: porch get [OPTIONS] <KIND> [ID]

Arguments:
  <KIND>
          What to list

          Possible values:
          - apps:                 Applications
          - envs:                 Environments of an application
          - env-types:            Environment types
          - deploys:              Deployments of an environment
          - deltas:               Deltas of an application
          - sets:                 Deployment sets, by ID
          - active-resources:     Active resources of an environment
          - resource-definitions: Resource definitions
          - resource-types:       Resource types
          - resource-classes:     Resource classes
          - cloud-accounts:       Cloud accounts

  [ID]
          One object's ID instead of the list. Active resources are `TYPE/RES_ID`

Options:
  -o, --output <OUTPUT>
          The output format. Sets default to YAML, everything else to a table

          Possible values:
          - table: Aligned columns
          - json:  JSON
          - yaml:  YAML

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --limit <LIMIT>
          For deploys: at most this many

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --before <SEQ>
          For deploys: those older than this sequence number, to page back

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help (see a summary with '-h')
```

## `porch create`

```text
Creates an object

Usage: porch create [OPTIONS] <COMMAND>

Commands:
  app                  An application. You become its owner
  env                  An environment of the application (`--app`)
  env-type             An environment type
  resource-definition  A resource definition from a `kind: Definition` YAML document
  resource-type        A resource type from a `kind: ResourceType` YAML document
  cloud-account        A cloud account from a `kind: CloudAccount` YAML document
  help                 Print this message or the help of the given subcommand(s)

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch create app`

```text
An application. You become its owner

Usage: porch create app [OPTIONS] <ID>

Arguments:
  <ID>
          The application ID

Options:
      --name <NAME>
          The display name

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch create env`

```text
An environment of the application (`--app`)

Usage: porch create env [OPTIONS] --type <TYPE> <ID>

Arguments:
  <ID>
          The environment ID

Options:
      --type <TYPE>
          The environment type

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --from-deploy <DEPLOY_ID>
          Start from the deployment set of this deployment of the application

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch create env-type`

```text
An environment type

Usage: porch create env-type [OPTIONS] <ID>

Arguments:
  <ID>
          The environment type ID

Options:
      --description <DESCRIPTION>
          A description

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch create resource-definition`

```text
A resource definition from a `kind: Definition` YAML document

Usage: porch create resource-definition [OPTIONS] --file <FILE>

Options:
  -f, --file <FILE>
          The YAML or JSON file, or `-` for standard input

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch create resource-type`

```text
A resource type from a `kind: ResourceType` YAML document

Usage: porch create resource-type [OPTIONS] --file <FILE>

Options:
  -f, --file <FILE>
          The YAML or JSON file, or `-` for standard input

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch create cloud-account`

```text
A cloud account from a `kind: CloudAccount` YAML document

Usage: porch create cloud-account [OPTIONS] --file <FILE>

Options:
  -f, --file <FILE>
          The YAML or JSON file, or `-` for standard input

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch delete`

```text
Deletes an object. Sets and deployment records are kept for good

Usage: porch delete [OPTIONS] <COMMAND>

Commands:
  app                  An application, with every environment and its resources
  env                  An environment of the application (`--app`), with its resources
  env-type             An unused environment type
  resource-definition  An unused resource definition
  resource-type        An unused custom resource type
  cloud-account        An unused cloud account
  help                 Print this message or the help of the given subcommand(s)

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch delete app`

```text
An application, with every environment and its resources

Usage: porch delete app [OPTIONS] <ID>

Arguments:
  <ID>
          The application ID

Options:
      --comment <COMMENT>
          A comment recorded with each environment's deletion
          
          [default: ""]

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --preview
          Preview each environment's deletion and print it, without deleting anything

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --plan <ENV=PLAN>
          The deletion preview of an environment: ENV=PLAN. Environments without one are previewed again

      --ack <ENV:DESCRIPTOR=ACTION>
          Acknowledges a resource an environment's preview deletes: ENV:DESCRIPTOR=ACTION

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch delete env`

```text
An environment of the application (`--app`), with its resources

Usage: porch delete env [OPTIONS] <ID>

Arguments:
  <ID>
          The environment ID

Options:
      --comment <COMMENT>
          A comment recorded with the deletion
          
          [default: ""]

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --preview
          Preview the change and print it, without changing anything

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --plan <PLAN>
          The preview this command carries out: it runs at the preview's environment generation, with a permit for each `--ack`

      --ack <DESCRIPTOR=ACTION>
          Acknowledges a change the preview marks as needing a permit, as the preview names it: DESCRIPTOR=ACTION, with @FINGERPRINT for a replacement's driver plan

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch delete env-type`

```text
An unused environment type

Usage: porch delete env-type [OPTIONS] <ID>

Arguments:
  <ID>
          The environment type ID

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch delete resource-definition`

```text
An unused resource definition

Usage: porch delete resource-definition [OPTIONS] <ID>

Arguments:
  <ID>
          The definition ID

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch delete resource-type`

```text
An unused custom resource type

Usage: porch delete resource-type [OPTIONS] <ID>

Arguments:
  <ID>
          The type ID

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch delete cloud-account`

```text
An unused cloud account

Usage: porch delete cloud-account [OPTIONS] <ID>

Arguments:
  <ID>
          The account ID

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch users`

```text
Manages the organization's members and their tokens

Usage: porch users [OPTIONS] <COMMAND>

Commands:
  list            Lists the organization's members
  invite          Invites a person, by email or GitHub login. Share the printed code with them
  create-service  Creates a service user, such as one for CI
  set-role        Changes a member's organization role
  remove          Removes a member, revoking their tokens, sessions and roles
  tokens          Manages a user's API tokens
  help            Print this message or the help of the given subcommand(s)

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch users list`

```text
Lists the organization's members

Usage: porch users list [OPTIONS]

Options:
  -o, --output <OUTPUT>
          The output format

          Possible values:
          - table: Aligned columns
          - json:  JSON
          - yaml:  YAML
          
          [default: table]

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help (see a summary with '-h')
```

## `porch users invite`

```text
Invites a person, by email or GitHub login. Share the printed code with them

Usage: porch users invite [OPTIONS] --role <ROLE>

Options:
      --role <ROLE>
          Their organization role

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --email <EMAIL>
          Their verified email address

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --github-login <LOGIN>
          Their GitHub login

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch users create-service`

```text
Creates a service user, such as one for CI

Usage: porch users create-service [OPTIONS] <ID>

Arguments:
  <ID>
          The user ID

Options:
      --role <ROLE>
          Its organization role
          
          [default: member]

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --name <NAME>
          A display name

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch users set-role`

```text
Changes a member's organization role

Usage: porch users set-role [OPTIONS] <ID> <ROLE>

Arguments:
  <ID>
          The user ID

  <ROLE>
          The new role

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch users remove`

```text
Removes a member, revoking their tokens, sessions and roles

Usage: porch users remove [OPTIONS] <ID>

Arguments:
  <ID>
          The user ID

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch users tokens`

```text
Manages a user's API tokens

Usage: porch users tokens [OPTIONS] <COMMAND>

Commands:
  list    Lists a user's tokens, without their values
  create  Creates a token and prints its value, once
  revoke  Revokes a token
  help    Print this message or the help of the given subcommand(s)

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch users tokens list`

```text
Lists a user's tokens, without their values

Usage: porch users tokens list [OPTIONS] <USER>

Arguments:
  <USER>
          The user ID

Options:
  -o, --output <OUTPUT>
          The output format

          Possible values:
          - table: Aligned columns
          - json:  JSON
          - yaml:  YAML
          
          [default: table]

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help (see a summary with '-h')
```

## `porch users tokens create`

```text
Creates a token and prints its value, once

Usage: porch users tokens create [OPTIONS] --id <ID> <USER>

Arguments:
  <USER>
          The user ID

Options:
      --id <ID>
          The token ID

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --description <DESCRIPTION>
          A description

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --expires-in <DURATION>
          How long the token is valid, such as `90d`, `12h` or `30m`

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch users tokens revoke`

```text
Revokes a token

Usage: porch users tokens revoke [OPTIONS] <USER> <ID>

Arguments:
  <USER>
          The user ID

  <ID>
          The token ID

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch apps`

```text
Manages application users

Usage: porch apps [OPTIONS] <COMMAND>

Commands:
  users  Manages an application's users
  help   Print this message or the help of the given subcommand(s)

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch apps users`

```text
Manages an application's users

Usage: porch apps users [OPTIONS] <APP> <COMMAND>

Commands:
  list    Lists the application's users and their roles
  grant   Gives a user a role (`owner`, `developer` or `viewer`), replacing any other
  revoke  Removes a user's role
  help    Print this message or the help of the given subcommand(s)

Arguments:
  <APP>
          The application ID

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch apps users list`

```text
Lists the application's users and their roles

Usage: porch apps users <APP> list [OPTIONS]

Options:
  -o, --output <OUTPUT>
          The output format

          Possible values:
          - table: Aligned columns
          - json:  JSON
          - yaml:  YAML
          
          [default: table]

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help (see a summary with '-h')
```

## `porch apps users grant`

```text
Gives a user a role (`owner`, `developer` or `viewer`), replacing any other

Usage: porch apps users <APP> grant [OPTIONS] <USER> <ROLE>

Arguments:
  <USER>
          The user ID

  <ROLE>
          The role

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch apps users revoke`

```text
Removes a user's role

Usage: porch apps users <APP> revoke [OPTIONS] <USER>

Arguments:
  <USER>
          The user ID

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch env-types`

```text
Manages environment type deployers

Usage: porch env-types [OPTIONS] <COMMAND>

Commands:
  users  Manages who may deploy to an environment type
  help   Print this message or the help of the given subcommand(s)

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch env-types users`

```text
Manages who may deploy to an environment type

Usage: porch env-types users [OPTIONS] <ENV_TYPE> <COMMAND>

Commands:
  list    Lists the environment type's deployers
  grant   Gives a user the `deployer` role
  revoke  Removes a user's `deployer` role
  help    Print this message or the help of the given subcommand(s)

Arguments:
  <ENV_TYPE>
          The environment type ID

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch env-types users list`

```text
Lists the environment type's deployers

Usage: porch env-types users <ENV_TYPE> list [OPTIONS]

Options:
  -o, --output <OUTPUT>
          The output format

          Possible values:
          - table: Aligned columns
          - json:  JSON
          - yaml:  YAML
          
          [default: table]

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help (see a summary with '-h')
```

## `porch env-types users grant`

```text
Gives a user the `deployer` role

Usage: porch env-types users <ENV_TYPE> grant [OPTIONS] <USER>

Arguments:
  <USER>
          The user ID

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch env-types users revoke`

```text
Removes a user's `deployer` role

Usage: porch env-types users <ENV_TYPE> revoke [OPTIONS] <USER>

Arguments:
  <USER>
          The user ID

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch secretstores`

```text
Manages secret stores

Usage: porch secretstores [OPTIONS] <COMMAND>

Commands:
  list    Lists secret stores
  get     Shows a secret store
  create  Creates a secret store from a `kind: SecretStore` YAML document
  delete  Deletes an unused secret store
  grants  Lists the grants that let applications use a store's secrets
  help    Print this message or the help of the given subcommand(s)

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch secretstores list`

```text
Lists secret stores

Usage: porch secretstores list [OPTIONS]

Options:
  -o, --output <OUTPUT>
          The output format

          Possible values:
          - table: Aligned columns
          - json:  JSON
          - yaml:  YAML
          
          [default: table]

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help (see a summary with '-h')
```

## `porch secretstores get`

```text
Shows a secret store

Usage: porch secretstores get [OPTIONS] <ID>

Arguments:
  <ID>
          The store ID

Options:
  -o, --output <OUTPUT>
          The output format

          Possible values:
          - table: Aligned columns
          - json:  JSON
          - yaml:  YAML
          
          [default: table]

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help (see a summary with '-h')
```

## `porch secretstores create`

```text
Creates a secret store from a `kind: SecretStore` YAML document

Usage: porch secretstores create [OPTIONS] --file <FILE>

Options:
  -f, --file <FILE>
          The YAML or JSON file, or `-` for standard input

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch secretstores delete`

```text
Deletes an unused secret store

Usage: porch secretstores delete [OPTIONS] <ID>

Arguments:
  <ID>
          The store ID

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch secretstores grants`

```text
Lists the grants that let applications use a store's secrets

Usage: porch secretstores grants [OPTIONS] <ID>

Arguments:
  <ID>
          The store ID

Options:
  -o, --output <OUTPUT>
          The output format

          Possible values:
          - table: Aligned columns
          - json:  JSON
          - yaml:  YAML
          
          [default: table]

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help (see a summary with '-h')
```

## `porch audit-logs`

```text
Queries the audit log, newest first

Usage: porch audit-logs [OPTIONS]

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --user <USER>
          Only this principal's entries

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --path-prefix <PREFIX>
          Only paths starting with this, such as `/orgs/acme/apps/shop`

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --method <METHOD>
          Only this HTTP method

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --from <MS>
          Entries from this time on, in Unix milliseconds

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

      --to <MS>
          Entries before this time, in Unix milliseconds

      --limit <LIMIT>
          At most this many entries

      --before <SEQ>
          Entries older than this sequence number, to page back

      --all
          Every matching entry, following the pages, for export

  -o, --output <OUTPUT>
          The output format

          Possible values:
          - table: Aligned columns
          - json:  JSON
          - yaml:  YAML
          
          [default: table]

  -h, --help
          Print help (see a summary with '-h')
```

## `porch expire-audit-logs`

```text
Removes the audit entries older than the server's retention period

Usage: porch expire-audit-logs [OPTIONS]

Options:
      --dry-run
          Report how many entries would expire without removing any

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --before <MS>
          Expire only entries written before this time, in Unix milliseconds, such as the `before` of a dry run whose entries were exported

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --yes
          Without `--before`, expire what the dry run reports without asking for a second run, as from cron

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

  -o, --output <OUTPUT>
          The output format

          Possible values:
          - table: Aligned columns
          - json:  JSON
          - yaml:  YAML
          
          [default: table]

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help (see a summary with '-h')
```

## `porch workload`

```text
Deploys or previews workload documents

Usage: porch workload [OPTIONS] <COMMAND>

Commands:
  deploy  Converts a workload document or Score file into a delta for the environment (`--app`, `--env`) and deploys it
  plan    Converts a workload document or Score file into a delta and previews deploying it, without deploying: each resource's change, what is not known until the drivers run, and the permits it needs. The same as `workload deploy --preview`
  help    Print this message or the help of the given subcommand(s)

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch workload deploy`

```text
Converts a workload document or Score file into a delta for the environment (`--app`, `--env`) and deploys it

Usage: porch workload deploy [OPTIONS] --file <FILE>

Options:
  -f, --file <FILE>
          The workload document or Score file, or `-` for standard input

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --overlay <FILE>
          A `WorkloadOverlay` document for a Score file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --image <NAME=REF>
          The image of a container, or of the function (`main`)

      --artifact <NAME=URI>
          The artifact of the function (`main`)

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

      --property <PATH=VALUE>
          A value at a dotted path of the document, read as YAML except where it replaces a string

      --comment <COMMENT>
          A comment recorded with the deployment
          
          [default: ""]

      --wait
          Follow the deployment until it finishes, and fail if it does

      --timeout <DURATION>
          Stop following after this long (such as `90s`, `20m` or `1h`) and fail; the deployment carries on

      --preview
          Preview the change and print it, without changing anything

      --plan <PLAN>
          The preview this command carries out: it runs at the preview's environment generation, with a permit for each `--ack`

      --ack <DESCRIPTOR=ACTION>
          Acknowledges a change the preview marks as needing a permit, as the preview names it: DESCRIPTOR=ACTION, with @FINGERPRINT for a replacement's driver plan

  -h, --help
          Print help
```

## `porch workload plan`

```text
Converts a workload document or Score file into a delta and previews deploying it, without deploying: each resource's change, what is not known until the drivers run, and the permits it needs. The same as `workload deploy --preview`

Usage: porch workload plan [OPTIONS] --file <FILE>

Options:
  -f, --file <FILE>
          The workload document or Score file, or `-` for standard input

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --overlay <FILE>
          A `WorkloadOverlay` document for a Score file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --image <NAME=REF>
          The image of a container, or of the function (`main`)

      --artifact <NAME=URI>
          The artifact of the function (`main`)

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

      --property <PATH=VALUE>
          A value at a dotted path of the document, read as YAML except where it replaces a string

  -h, --help
          Print help
```

## `porch score`

```text
Deploys Score files

Usage: porch score [OPTIONS] <COMMAND>

Commands:
  deploy  Imports a Score file and deploys it as `porch workload deploy` would
  help    Print this message or the help of the given subcommand(s)

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch score deploy`

```text
Imports a Score file and deploys it as `porch workload deploy` would

Usage: porch score deploy [OPTIONS] --file <FILE>

Options:
  -f, --file <FILE>
          The workload document or Score file, or `-` for standard input

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --overlay <FILE>
          A `WorkloadOverlay` document for a Score file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --image <NAME=REF>
          The image of a container, or of the function (`main`)

      --artifact <NAME=URI>
          The artifact of the function (`main`)

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

      --property <PATH=VALUE>
          A value at a dotted path of the document, read as YAML except where it replaces a string

      --comment <COMMENT>
          A comment recorded with the deployment
          
          [default: ""]

      --wait
          Follow the deployment until it finishes, and fail if it does

      --timeout <DURATION>
          Stop following after this long (such as `90s`, `20m` or `1h`) and fail; the deployment carries on

      --preview
          Preview the change and print it, without changing anything

      --plan <PLAN>
          The preview this command carries out: it runs at the preview's environment generation, with a permit for each `--ack`

      --ack <DESCRIPTOR=ACTION>
          Acknowledges a change the preview marks as needing a permit, as the preview names it: DESCRIPTOR=ACTION, with @FINGERPRINT for a replacement's driver plan

  -h, --help
          Print help
```

## `porch deploy`

```text
Deploys a delta, or an earlier deployment again

Usage: porch deploy [OPTIONS] <COMMAND>

Commands:
  delta   Deploys a stored delta to an environment of the application
  deploy  Deploys the set of an earlier deployment of the environment (`--env`) again: by ID, `+n` for the nth attempt before the latest (`+0` is the latest), or `last-successful`
  help    Print this message or the help of the given subcommand(s)

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch deploy delta`

```text
Deploys a stored delta to an environment of the application

Usage: porch deploy delta [OPTIONS] <ID> <ENV>

Arguments:
  <ID>
          The delta ID

  <ENV>
          The environment

Options:
      --comment <COMMENT>
          A comment recorded with the deployment
          
          [default: ""]

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --wait
          Follow the deployment until it finishes, and fail if it does

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --timeout <DURATION>
          Stop following after this long (such as `90s`, `20m` or `1h`) and fail; the deployment carries on

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --preview
          Preview the change and print it, without changing anything

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

      --plan <PLAN>
          The preview this command carries out: it runs at the preview's environment generation, with a permit for each `--ack`

      --ack <DESCRIPTOR=ACTION>
          Acknowledges a change the preview marks as needing a permit, as the preview names it: DESCRIPTOR=ACTION, with @FINGERPRINT for a replacement's driver plan

  -h, --help
          Print help
```

## `porch deploy deploy`

```text
Deploys the set of an earlier deployment of the environment (`--env`) again: by ID, `+n` for the nth attempt before the latest (`+0` is the latest), or `last-successful`

Usage: porch deploy deploy [OPTIONS] <REFERENCE>

Arguments:
  <REFERENCE>
          The deployment

Options:
      --replay <REPLAY>
          Run with the configuration the deployment recorded, or with the definitions and values as they are now

          Possible values:
          - snapshot: What the earlier deployment recorded
          - current:  The definitions and values as they are now
          
          [default: snapshot]

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --comment <COMMENT>
          A comment recorded with the deployment
          
          [default: ""]

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --wait
          Follow the deployment until it finishes, and fail if it does

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --timeout <DURATION>
          Stop following after this long (such as `90s`, `20m` or `1h`) and fail; the deployment carries on

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

      --preview
          Preview the change and print it, without changing anything

      --plan <PLAN>
          The preview this command carries out: it runs at the preview's environment generation, with a permit for each `--ack`

      --ack <DESCRIPTOR=ACTION>
          Acknowledges a change the preview marks as needing a permit, as the preview names it: DESCRIPTOR=ACTION, with @FINGERPRINT for a replacement's driver plan

  -h, --help
          Print help (see a summary with '-h')
```

## `porch logs`

```text
Prints logs

Usage: porch logs [OPTIONS] <COMMAND>

Commands:
  deploy  Prints a deployment's log of the environment (`--env`), following it until the deployment finishes
  help    Print this message or the help of the given subcommand(s)

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch logs deploy`

```text
Prints a deployment's log of the environment (`--env`), following it until the deployment finishes

Usage: porch logs deploy [OPTIONS] <REFERENCE>

Arguments:
  <REFERENCE>
          The deployment: an ID, `+n` or `last-successful`

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch diff`

```text
Compares deployment sets

Usage: porch diff [OPTIONS] <COMMAND>

Commands:
  sets  Prints the delta that turns set A into set B. Besides set IDs, takes `deploy/.` (the latest attempt in `--env`), `deploy/+n` (n attempts before it, failures included), `deploy/last-successful` and `deploy/<id>`
  help  Print this message or the help of the given subcommand(s)

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch diff sets`

```text
Prints the delta that turns set A into set B. Besides set IDs, takes `deploy/.` (the latest attempt in `--env`), `deploy/+n` (n attempts before it, failures included), `deploy/last-successful` and `deploy/<id>`

Usage: porch diff sets [OPTIONS] <A> <B>

Arguments:
  <A>
          The set to start from

  <B>
          The set to end at

Options:
  -o, --output <OUTPUT>
          The output format

          Possible values:
          - table: Aligned columns
          - json:  JSON
          - yaml:  YAML
          
          [default: yaml]

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help (see a summary with '-h')
```

## `porch resources`

```text
Shows and recovers an environment's resources

Usage: porch resources [OPTIONS] <COMMAND>

Commands:
  graph    Prints the resource graph of the environment (`--app`, `--env`)
  recover  Observes a resource of the environment whose last operation has an unknown outcome, and records what the observation establishes
  help     Print this message or the help of the given subcommand(s)

Options:
      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help
```

## `porch resources graph`

```text
Prints the resource graph of the environment (`--app`, `--env`)

Usage: porch resources graph [OPTIONS]

Options:
  -o, --output <OUTPUT>
          The output format

          Possible values:
          - table: Aligned columns
          - json:  JSON
          - yaml:  YAML
          
          [default: yaml]

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help (see a summary with '-h')
```

## `porch resources recover`

```text
Observes a resource of the environment whose last operation has an unknown outcome, and records what the observation establishes

Usage: porch resources recover [OPTIONS] <TYPE> <RES_ID>

Arguments:
  <TYPE>
          The resource type

  <RES_ID>
          The resource ID, such as `modules.web.externals.db`

Options:
  -o, --output <OUTPUT>
          The output format

          Possible values:
          - table: Aligned columns
          - json:  JSON
          - yaml:  YAML
          
          [default: yaml]

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help (see a summary with '-h')
```

## `porch api`

```text
Sends an authenticated request to the API and prints the JSON response

Usage: porch api [OPTIONS] <METHOD> <PATH>

Arguments:
  <METHOD>
          The method

          Possible values:
          - get:    GET
          - post:   POST
          - put:    PUT
          - patch:  PATCH
          - delete: DELETE

  <PATH>
          The path, such as `/orgs/acme/apps`

Options:
  -d, --data <BODY>
          A JSON body, `@FILE` to read it from a file, or `@-` for standard input

      --url <URL>
          The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file

      --org <ORG>
          The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file

      --app <APP>
          The application. Defaults to `OPENPORCH_APP`, then `app` in the config file

      --env <ENV>
          The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file

      --config <FILE>
          The config file. Defaults to `OPENPORCH_CONFIG`, then `~/.config/openporch/config.toml`; the credentials file sits next to it

  -h, --help
          Print help (see a summary with '-h')
```

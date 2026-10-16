# Schema fixtures

Each directory is the data directory of a test installation, written by the
build whose schema version names it, with `observed.json` holding what its API
returned then. `every_schema_fixture_upgrades_to_this_build` in
`crates/server/src/tests/upgrades.rs` starts every build on each of them, and
`an_older_build_refuses_every_newer_fixture_unchanged` checks that a build
one schema version older refuses each one without writing to it.

The master keys and tokens here protect nothing but these fixtures. Never use
them, or a copy of these files, for a real installation.

When a migration is added, raise `SCHEMA_VERSION` in `crates/store` and write
the new version's fixture with the build that adds it:

```sh
OPENPORCH_SCHEMA_FIXTURE=crates/server/fixtures/schema/<version> \
    cargo test -p openporch-server write_schema_fixture -- --ignored
```

Fixtures are never rewritten: a fixture records what an older build wrote.

//! `docs/cli.md` is generated from the command line's own help, so it cannot
//! drift from it. Regenerate it with
//! `OPENPORCH_UPDATE_DOCS=1 cargo test -p openporch-cli cli_reference`.

use std::path::PathBuf;

use clap::CommandFactory;

use crate::Cli;

const HEADER: &str = "# porch command reference

Generated from the help of `porch` itself; do not edit by hand. Regenerate it
with `OPENPORCH_UPDATE_DOCS=1 cargo test -p openporch-cli cli_reference`. The
[quickstart](quickstart.md) shows the commands in use.
";

/// Every command's long help, depth first, under a heading per command.
fn reference() -> String {
    let mut cmd = Cli::command();
    cmd.build();
    let mut out = HEADER.to_owned();
    let mut stack = vec![cmd];
    while let Some(mut cmd) = stack.pop() {
        let name = cmd
            .get_bin_name()
            .map(str::to_owned)
            .unwrap_or_else(|| cmd.get_name().to_owned());
        let help = cmd.render_long_help().to_string();
        out.push_str(&format!(
            "\n## `{name}`\n\n```text\n{}\n```\n",
            help.trim_end()
        ));
        let mut subs: Vec<_> = cmd
            .get_subcommands()
            .filter(|s| s.get_name() != "help")
            .cloned()
            .collect();
        subs.reverse();
        stack.extend(subs);
    }
    out
}

#[test]
fn cli_reference_is_current() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/cli.md");
    let expected = reference();
    if std::env::var_os("OPENPORCH_UPDATE_DOCS").is_some() {
        std::fs::write(&path, &expected).unwrap();
        return;
    }
    let actual = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        actual == expected,
        "docs/cli.md is out of date: regenerate it with \
         `OPENPORCH_UPDATE_DOCS=1 cargo test -p openporch-cli cli_reference`"
    );
}

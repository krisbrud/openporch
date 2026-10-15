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

/// Every command's long help, depth first, under a heading per command. The help is
/// not wrapped to the terminal only because clap's `wrap_help` feature is off; if it
/// is ever enabled in the workspace, set a fixed width with `Command::term_width`.
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
    if std::env::var("OPENPORCH_UPDATE_DOCS").as_deref() == Ok("1") {
        std::fs::write(&path, &expected).unwrap();
        return;
    }
    let actual = std::fs::read_to_string(&path).unwrap_or_default();
    if actual != expected {
        let (n, (want, have)) = expected
            .lines()
            .chain(std::iter::repeat(""))
            .zip(actual.lines().chain(std::iter::repeat("")))
            .take(expected.lines().count().max(actual.lines().count()) + 1)
            .enumerate()
            .find(|(_, (a, b))| a != b)
            .unwrap_or((0, ("", "")));
        panic!(
            "docs/cli.md is out of date from line {}: expected `{want}`, found `{have}`. \
             Regenerate it with `OPENPORCH_UPDATE_DOCS=1 cargo test -p openporch-cli cli_reference`",
            n + 1
        );
    }
}

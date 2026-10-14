use std::io::IsTerminal as _;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let env = |key: &str| std::env::var(key).ok();
    let stdin = std::io::stdin();
    let stdin_is_terminal = stdin.is_terminal();
    let mut stdin = stdin.lock();
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    let mut io = openporch_cli::Io {
        env: &env,
        stdin: &mut stdin,
        stdin_is_terminal,
        out: &mut out,
        err: &mut err,
    };
    let code = openporch_cli::run(std::env::args_os(), &mut io).await;
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

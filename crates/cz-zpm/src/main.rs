//! `cz-zpm` standalone binary: works both as `cz-zpm <cmd>` and via multicall symlinks
//! (`ln -s cz-zpm zpm`, `ln -s cz-zpm npm`) or the `cz-zpm zpm ...` subcommand form.

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let code = match cz_zpm::maybe_run_multicall_async().await {
        Some(c) => c,
        // Direct invocation: treat everything after argv[0] as CLI args.
        None => cz_zpm::run_cli_async(&args[1..]).await,
    };
    ExitCode::from(code)
}

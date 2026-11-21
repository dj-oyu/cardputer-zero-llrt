//! `cz-zpm` standalone binary: works both as `cz-zpm <cmd>` and via multicall symlinks
//! (`ln -s cz-zpm zpm`, `ln -s cz-zpm npm`) or the `cz-zpm zpm ...` subcommand form.
//! Phase 14: plain synchronous `fn main()` — the ureq + cz-net stack needs no async runtime.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let code = match cz_zpm::maybe_run_multicall() {
        Some(c) => c,
        // Direct invocation: treat everything after argv[0] as CLI args.
        None => cz_zpm::run_cli(&args[1..]),
    };
    ExitCode::from(code)
}

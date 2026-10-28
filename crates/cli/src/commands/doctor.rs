//! `tenaz doctor`: what a real run still needs on this machine.

use std::process::ExitCode;

use tenaz_core::params::RunParams;

use crate::detect;

/// `tenaz doctor`: prints what is ready and what is missing, with how to
/// fix it; succeeds only when a real run can start.
pub(crate) async fn execute(base_url: Option<&str>) -> ExitCode {
    let checks = tenaz_core::doctor::examine(
        &detect::machine(base_url).await,
        RunParams::default().context_floor,
    );
    print!("{}", tenaz_core::doctor::render(&checks));
    if tenaz_core::doctor::ready(&checks) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

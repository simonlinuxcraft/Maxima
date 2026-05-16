use base64::{engine::general_purpose, Engine};
use lazy_static::lazy_static;
use log::debug;
use regex::Regex;
use serde::Serialize;

use crate::{
    unix::wine::{run_wine_command, CommandType},
    util::native::{module_path, NativeError, SafeParent, SafeStr},
};

lazy_static! {
    static ref PID_PATTERN: Regex = Regex::new(r"wine-helper: PID (.*)").unwrap();
}

#[derive(Default, Serialize)]
pub struct WineGetPidArgs {
    pub launch_id: String,
    pub name: String,
}

#[derive(Default, Serialize)]
pub struct WineInjectArgs {
    pub pid: u32,
    pub path: String,
}

pub async fn wine_get_pid(launch_id: &str, name: &str) -> Result<u32, NativeError> {
    debug!("Searching for wine PID for {}", name);

    let launch_args = WineGetPidArgs {
        launch_id: launch_id.to_owned(),
        name: name.to_owned(),
    };

    let b64 = general_purpose::STANDARD.encode(serde_json::to_string(&launch_args)?);
    // MAXIMA-LINUX-PORT-MOD: when running as Launcher FFI, exe is in bundle/
    // but wine-helper.exe lives in bundle/cli/. Check cli/ first.
    let exe = module_path()?;
    let exe_parent = exe.safe_parent()?;
    let wine_helper = {
        let cli_path = exe_parent.join("cli").join("wine-helper.exe");
        if cli_path.exists() { cli_path } else { exe_parent.join("wine-helper.exe") }
    };
    let output = run_wine_command(
        wine_helper.safe_str()?,
        Some(vec!["get_pid", b64.as_str()]),
        None,
        true,
        CommandType::RunInPrefix,
    )
    .await?;

    if output.contains("Failed to find PID") {
        return Err(NativeError::Pid(b64));
    }

    let pid = match PID_PATTERN.captures(&output) {
        Some(pid) => pid,
        None => return Err(NativeError::PidPattern),
    };

    let pid = match pid.get(1) {
        Some(pid) => pid,
        None => return Err(NativeError::PidPattern),
    };

    Ok(pid.as_str().parse()?)
}

pub async fn request_library_injection(pid: u32, path: &str) -> Result<(), NativeError> {
    debug!("Injecting {}", path);

    let launch_args = WineInjectArgs {
        pid,
        path: path.to_owned(),
    };

    let b64 = general_purpose::STANDARD.encode(serde_json::to_string(&launch_args)?);
    // MAXIMA-LINUX-PORT-MOD: same cli/ fallback as wine_get_pid above.
    let exe = module_path()?;
    let exe_parent = exe.safe_parent()?;
    let wine_helper = {
        let cli_path = exe_parent.join("cli").join("wine-helper.exe");
        if cli_path.exists() { cli_path } else { exe_parent.join("wine-helper.exe") }
    };
    run_wine_command(
        wine_helper.safe_str()?,
        Some(vec!["inject", b64.as_str()]),
        None,
        false,
        CommandType::RunInPrefix,
    )
    .await?;

    Ok(())
}

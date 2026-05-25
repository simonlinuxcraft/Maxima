use std::{
    collections::HashMap,
    env,
    ffi::OsStr,
    fs::{create_dir_all, remove_dir_all, remove_file, File},
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
};

use flate2::read::GzDecoder;
use lazy_static::lazy_static;
use log::{info, warn};
use regex::Regex;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tar::Archive;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::Mutex,
};
use xz2::read::XzDecoder;

use crate::util::{
    github::{fetch_github_release, fetch_github_releases, github_download_asset, GithubRelease},
    native::{maxima_dir, DownloadError, NativeError, SafeParent, SafeStr, WineError},
    registry::RegistryError,
};

lazy_static! {
    static ref PROTON_PATTERN: Regex = Regex::new(r"GE-Proton\d+-\d+\.tar\.gz").unwrap();
    // MAXIMA-LINUX-PORT-MOD 2026-05-24: cache last logged proton choice so
    // that proton_dir() (called 4-5x per game launch) does not spam the log
    // with identical lines. Logs once per unique resolution, including after
    // a hot-switch from the launcher UI.
    static ref LAST_PROTON_LOG: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
}

// MAXIMA-LINUX-PORT-MOD 2026-05-24: emit a single info! line whenever the
// effective PROTONPATH changes (after launcher start or after a custom path
// switch). Idempotent across repeated proton_dir() calls.
fn log_proton_choice(label: &str, path: &Path) {
    let new = format!("{}|{}", label, path.display());
    if let Ok(mut last) = LAST_PROTON_LOG.lock() {
        if last.as_deref() != Some(&new) {
            info!("[custom-proton] {} = {}", label, path.display());
            *last = Some(new);
        }
    }
}

// A Proton verb to use
pub enum CommandType {
    // Set the prefix up and runs the command
    Run,
    // Waits for any hanging wineserver instances and runs the command
    WaitForExitAndRun,
    // Directly calls the command, doesn't setup the prefix (use with caution)
    RunInPrefix,
}

impl std::fmt::Display for CommandType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let display = match self {
            Self::RunInPrefix => "runinprefix",
            Self::Run => "run",
            Self::WaitForExitAndRun => "waitforexitandrun",
        };
        f.write_str(display)
    }
}

const VERSION_FILE: &str = "dependency-versions.toml";

#[derive(Deserialize, Default)]
pub(crate) struct LutrisRuntime {
    name: String,
    created_at: String,
    url: String,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Versions {
    proton: String,
    eac_runtime: String,
    umu: String,
}

/// Returns the auto-managed default wineprefix path. Kept as a separate
/// helper (introduced 2026-05-26) so future routing changes can distinguish
/// the canonical Maxima-managed path from whatever `wine_prefix_dir()`
/// returns at runtime.
pub fn default_wine_prefix_dir() -> Result<PathBuf, NativeError> {
    Ok(maxima_dir()?.join("wine/prefix"))
}

/// Returns internal proton pfx path. Under Option H (the active
/// custom-proton routing strategy), the wineprefix is ALWAYS the default
/// Steam-compatdata-linked one even when a custom Proton is active: the
/// custom-proton swap happens at the `wine/proton` symlink level, not at
/// the prefix level. Keeps Origin / EA Desktop state shared, avoids the
/// pressure-vessel wineserver split that broke per-Proton-prefix routing.
pub fn wine_prefix_dir() -> Result<PathBuf, NativeError> {
    default_wine_prefix_dir()
}

// MAXIMA-LINUX-PORT-MOD 2026-05-24: helper that always returns the default
// auto-managed proton path, regardless of KYBER_PROTON_PATH/sidecar override.
// Used by extract_wine() / install_wine() so that auto-download keeps writing
// to the maxima-owned directory even when the user has set a custom override,
// preventing the user's external proton install from being deleted.
pub fn default_proton_dir() -> Result<PathBuf, NativeError> {
    Ok(maxima_dir()?.join("wine/proton"))
}

// MAXIMA-LINUX-PORT-MOD 2026-05-24: layout check for a candidate proton dir.
// umu-run accepts several upstream layouts: GE-Proton ships `files/bin/wine64`,
// stock Valve ships `dist/bin/wine64`, some Lutris/Heroic builds ship a flat
// `bin/wine64`. Top-level `proton` script is also accepted because that's what
// the umu protocol officially looks for. Permission bit check guards against
// archives extracted with `--no-same-permissions` (Bug-Hunter #13).
//
// MAXIMA-LINUX-PORT-MOD 2026-05-25: also accept the Wine-10 WoW64 single-binary
// layout where only `wine` (no `wine64`) exists. proton-cachyos 11.0 and
// upcoming GE-Proton 11+ ship this layout because Wine 10 merged 32/64-bit
// into one multilib binary.
fn is_executable_file(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

pub fn is_valid_proton_layout(p: &Path) -> bool {
    if !p.is_dir() {
        return false;
    }
    p.join("proton").exists()
        || is_executable_file(&p.join("files/bin/wine64"))
        || is_executable_file(&p.join("dist/bin/wine64"))
        || is_executable_file(&p.join("bin/wine64"))
        || is_executable_file(&p.join("files/bin/wine"))
        || is_executable_file(&p.join("dist/bin/wine"))
        || is_executable_file(&p.join("bin/wine"))
}

// MAXIMA-LINUX-PORT-MOD 2026-05-24: resolve a user-supplied custom proton
// path. Resolution order:
//   1. KYBER_PROTON_PATH env-var (power-user override, takes precedence)
//   2. ~/.local/share/maxima/custom_proton_path sidecar file (UI setting,
//      written by the launcher via the set_custom_proton_path FFI)
//   3. None - fall through to default auto-managed path
// Tilde expansion is done in-house (no shellexpand dep needed).
fn resolve_custom_proton_path() -> Option<PathBuf> {
    if let Ok(val) = env::var("KYBER_PROTON_PATH") {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return Some(expand_tilde(trimmed));
        }
    }
    let sidecar = maxima_dir().ok()?.join("custom_proton_path");
    if sidecar.exists() {
        if let Ok(content) = std::fs::read_to_string(&sidecar) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                return Some(expand_tilde(trimmed));
            }
        }
    }
    None
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    if s == "~" {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(s)
}

// MAXIMA-LINUX-PORT-MOD 2026-05-26: Option H. proton_dir() now always returns
// the stable Maxima-default path. Custom-Proton routing happens at the
// filesystem level via ensure_proton_routing() which makes wine/proton itself
// a symlink to the user-chosen Proton install. Keeping PROTONPATH stable
// means Proton's config_info inside the Steam compatdata never sees a path
// change and never triggers a prefix re-init (Mode B fix).
//
// Callers MUST invoke ensure_proton_routing() before reading proton_dir() to
// guarantee the wine/proton symlink (or backup-restored real dir) is in the
// state implied by the current sidecar. Invalid custom paths surface from
// ensure_proton_routing() (NativeError::Wine::CustomProtonInvalid), not from
// proton_dir() itself.
pub fn proton_dir() -> Result<PathBuf, NativeError> {
    let default = default_proton_dir()?;
    log_proton_choice(
        if resolve_custom_proton_path().is_some() { "custom-symlink" } else { "default" },
        &default,
    );
    Ok(default)
}

// MAXIMA-LINUX-PORT-MOD 2026-05-26: Option H state machine for the
// wine/proton mount point.
//
// 4 valid filesystem states for wine/proton + wine/proton.maxima-backup:
//
//   A: proton = real maxima-managed dir,    backup = absent     [DEFAULT mode]
//   B: proton = symlink to <custom-path>,   backup = real dir   [CUSTOM mode]
//   C: proton = absent,                     backup = absent     [INITIAL pre-install]
//   D: proton = absent,                     backup = real dir   [INCONSISTENT after crash]
//
// State A is reached when sidecar is empty.
// State B is reached when sidecar is set to a valid custom-Proton path.
// State C is the fresh-install case before Maxima has run install_wine.
// State D is the failure mode: we crashed mid-swap. ensure_proton_routing()
// detects and heals it.
//
// Idempotent: safe to call from init_app, start_game, set_custom_proton_path
// FFI without worrying about state accumulation.
pub fn ensure_proton_routing() -> Result<(), NativeError> {
    use std::fs;
    use std::os::unix::fs::symlink;

    let proton = default_proton_dir()?;
    let backup = proton.with_extension("maxima-backup");

    match resolve_custom_proton_path() {
        Some(custom) => {
            if !is_valid_proton_layout(&custom) {
                warn!(
                    "[ensure_proton_routing] custom proton path invalid: {}",
                    custom.display()
                );
                return Err(NativeError::Wine(WineError::CustomProtonInvalid(custom)));
            }
            // Target state: proton -> symlink to custom, backup = real dir.

            // Step 1: ensure backup contains the original Maxima-managed dir
            // (if proton today is a real dir, move it aside; if proton is
            // already a symlink, backup should already exist from a previous
            // switch; if backup is missing in that case, log a warning but
            // continue - the user must have manually deleted the backup, the
            // best we can offer is restoring via install_wine on next reset).
            if let Ok(meta) = fs::symlink_metadata(&proton) {
                if !meta.file_type().is_symlink() && meta.is_dir() {
                    if backup.exists() {
                        warn!(
                            "[ensure_proton_routing] proton is a real dir AND \
                             backup already exists at {}. Refusing to clobber; \
                             investigate manually.",
                            backup.display()
                        );
                        return Err(NativeError::Io(std::io::Error::new(
                            std::io::ErrorKind::AlreadyExists,
                            format!(
                                "wine/proton is a directory and wine/proton.maxima-backup also \
                                 exists; refusing to overwrite. Move one of them aside manually."
                            ),
                        )));
                    }
                    info!(
                        "[ensure_proton_routing] moving Maxima-managed proton aside to {}",
                        backup.display()
                    );
                    fs::rename(&proton, &backup)?;
                }
            }

            // Step 2: replace whatever is at `proton` (nothing, wrong symlink)
            // with a symlink pointing to the custom Proton install.
            if let Ok(meta) = fs::symlink_metadata(&proton) {
                if meta.file_type().is_symlink() {
                    match fs::read_link(&proton) {
                        Ok(target) if target == custom => {
                            // already correct - nothing to do
                            return Ok(());
                        }
                        _ => {
                            fs::remove_file(&proton)?;
                        }
                    }
                }
            }
            info!(
                "[ensure_proton_routing] symlinking wine/proton -> {}",
                custom.display()
            );
            symlink(&custom, &proton)?;
            Ok(())
        }
        None => {
            // Target state: proton = real maxima-managed dir, backup = absent.

            // Step 1: if proton is a symlink, remove it (will be replaced by
            // backup-restore or by install_wine).
            if let Ok(meta) = fs::symlink_metadata(&proton) {
                if meta.file_type().is_symlink() {
                    info!(
                        "[ensure_proton_routing] removing custom-proton symlink at wine/proton"
                    );
                    fs::remove_file(&proton)?;
                }
            }

            // Step 2: if backup exists and proton no longer exists, restore.
            if backup.exists() && !proton.exists() {
                info!(
                    "[ensure_proton_routing] restoring Maxima-managed proton from {}",
                    backup.display()
                );
                fs::rename(&backup, &proton)?;
            }

            // Step 3: if neither proton nor backup exist (state C: fresh
            // install) or proton is a real dir already (state A), do nothing
            // - install_wine will populate / it is already correct.
            Ok(())
        }
    }
}

pub fn wine_dir() -> Result<PathBuf, NativeError> {
    Ok(maxima_dir()?.join("wine"))
}

pub fn eac_dir() -> Result<PathBuf, NativeError> {
    Ok(maxima_dir()?.join("wine/eac_runtime"))
}

pub fn umu_bin() -> Result<PathBuf, NativeError> {
    Ok(maxima_dir()?.join("wine/umu/umu-run"))
}

// MAXIMA-LINUX-PORT-MOD: true if feral `gamemoderun` is on PATH. Used to opt
// the game launch into gamemode only when the user actually has it installed.
fn gamemoderun_in_path() -> bool {
    env::var_os("PATH")
        .map(|paths| {
            env::split_paths(&paths).any(|p| p.join("gamemoderun").is_file())
        })
        .unwrap_or(false)
}

fn versions() -> Result<Versions, NativeError> {
    let file = maxima_dir()?.join(VERSION_FILE);
    if !file.exists() {
        return Ok(Versions::default());
    }

    let data = std::fs::read_to_string(file)?;
    Ok(toml::from_str(&data).unwrap_or_default())
}

fn set_versions(versions: Versions) -> Result<(), NativeError> {
    let file = maxima_dir()?.join(VERSION_FILE);
    std::fs::write(file, toml::to_string(&versions)?)?;
    Ok(())
}

pub(crate) async fn check_wine_validity() -> Result<bool, NativeError> {
    // MAXIMA-LINUX-PORT-MOD 2026-05-24: when a custom proton override is
    // active, skip the version comparison against GE-Proton's GitHub release
    // entirely. The user has opted out of auto-management, so neither
    // dependency-versions.toml nor the upstream release tag are relevant.
    // Layout check has already happened inside proton_dir() (returns Err if
    // invalid), so reaching this point means the path is good.
    if resolve_custom_proton_path().is_some() {
        return Ok(proton_dir().is_ok());
    }

    if !proton_dir()?.exists() {
        return Ok(false);
    }

    let version = versions()?.proton;

    let release = get_wine_release();
    if let Err(err) = release {
        if !version.is_empty() {
            warn!("Failed to check wine release, rate limited?");
            return Ok(true);
        }

        return Err(NativeError::Wine(err));
    }

    Ok(version == release?.tag_name)
}

pub(crate) async fn get_lutris_runtimes() -> Result<Vec<LutrisRuntime>, WineError> {
    let client = reqwest::Client::builder()
        .user_agent("ArmchairDevelopers/Maxima")
        .build()?;
    let res = client.get("https://lutris.net/api/runtimes").send().await?;
    let res = res.error_for_status()?;
    let data = res.json().await?;
    Ok(data)
}

pub(crate) async fn check_runtime_validity(
    key: &str,
    runtimes: &[LutrisRuntime],
) -> Result<bool, NativeError> {
    let versions = versions()?;
    let version = match key {
        "umu" => &versions.umu,
        "eac_runtime" => &versions.eac_runtime,
        _ => {
            return Err(NativeError::Wine(WineError::UnimplementedRuntime(
                key.to_string(),
            )))
        }
    };
    let path = wine_dir()?.join(key);
    if !path.exists() {
        return Ok(false);
    }
    let runtime_version = runtimes.iter().find(|r| r.name == key);

    Ok(runtime_version.is_some_and(|r| &r.created_at == version))
}

pub(crate) async fn install_runtime(
    key: &str,
    runtimes: &[LutrisRuntime],
) -> Result<(), NativeError> {
    info!("Downloading {key}");
    let runtime = runtimes
        .iter()
        .find(|r| r.name == key)
        .ok_or(NativeError::Wine(WineError::UnimplementedRuntime(
            key.to_string(),
        )))?;
    let mut versions = versions()?;
    let path = wine_dir()?.join(key);
    let runtime_ver = match key {
        "umu" => &mut versions.umu,
        "eac_runtime" => &mut versions.eac_runtime,
        _ => {
            return Err(NativeError::Wine(WineError::UnimplementedRuntime(
                key.to_string(),
            )))
        }
    };

    let res = match ureq::get(&runtime.url)
        .set("User-Agent", "ArmchairDevelopers/Maxima")
        .call()
    {
        Err(err) => return Err(NativeError::Download(DownloadError::Request1(err))),
        Ok(res) => res,
    };

    if res.status() != StatusCode::OK {
        return Err(NativeError::Download(DownloadError::Http(key.to_string())));
    }

    let mut body: Vec<u8> = vec![];
    res.into_reader().read_to_end(&mut body)?;

    if path.exists() {
        remove_dir_all(&path)?;
    }

    create_dir_all(&path)?;

    let data: Box<dyn std::io::Read> = if runtime.url.ends_with(".xz") {
        Box::new(XzDecoder::new(&body[..]))
    } else {
        Box::new(&body[..])
    };

    let archive = Archive::new(data);
    extract_archive(path, archive)?;

    let created_at = runtime.created_at.clone();
    *runtime_ver = created_at;
    set_versions(versions)
}

fn get_wine_release() -> Result<GithubRelease, WineError> {
    let releases = fetch_github_releases("GloriousEggroll", "proton-ge-custom")?;

    let mut release = None;
    for r in releases {
        if r.tag_name.ends_with("LoL") {
            continue;
        }

        release = Some(r);
        break;
    }

    release.ok_or(WineError::Fetch)
}

pub async fn run_wine_command<I: IntoIterator<Item = T>, T: AsRef<OsStr>>(
    arg: T,
    args: Option<I>,
    cwd: Option<PathBuf>,
    want_output: bool,
    command_type: CommandType,
) -> Result<String, NativeError> {
    let proton_path = proton_dir()?;
    let proton_prefix_path = wine_prefix_dir()?;
    let eac_path = eac_dir()?;
    let umu_bin = umu_bin()?;

    let wine_path =
        env::var("MAXIMA_WINE_COMMAND").unwrap_or_else(|_| umu_bin.to_string_lossy().to_string());

    // MAXIMA-LINUX-PORT-MOD: wrap the actual game launch in `gamemoderun`
    // when feral gamemode is installed. Only WaitForExitAndRun is the game
    // (reg.exe/DIP/inject use Run/RunInPrefix), so scoping it to the game
    // process keeps gamemode tied to the match lifetime instead of leaking
    // into the long-lived launcher. KYBER_DISABLE_GAMEMODE=1 opts out.
    // The LD_PRELOAD="" set in the env chain below lands on gamemoderun,
    // which then adds only libgamemodeauto.so before exec'ing umu-run, so
    // umu-run still sees no inherited host preloads, just gamemode's own.
    let use_gamemode = matches!(command_type, CommandType::WaitForExitAndRun)
        && !env::var("KYBER_DISABLE_GAMEMODE")
            .map(|v| v == "1")
            .unwrap_or(false)
        && gamemoderun_in_path();

    // Create command with all necessary wine env variables
    let mut binding = if use_gamemode {
        let mut c = Command::new("gamemoderun");
        c.arg(&wine_path);
        c
    } else {
        Command::new(wine_path.clone())
    };
    // MAXIMA-LINUX-PORT-MOD: detach stdin so Wine does not spawn a
    // wineconsole for console-subsystem helpers like wine-helper.exe.
    // Wine treats an inherited terminal-stdin as "interactive user
    // present" and pops up a black console window; Stdio::null keeps
    // those helpers headless.
    binding.stdin(Stdio::null());
    let mut child = binding
        .env("WINEPREFIX", proton_prefix_path)
        .env("GAMEID", "umu-0")
        .env("PROTON_VERB", &command_type.to_string())
        .env("PROTONPATH", proton_path)
        .env("STORE", "ea")
        .env("PROTON_EAC_RUNTIME", eac_path)
        .env("UMU_ZENITY", "1")
        .env("WINEDEBUG", "fixme-all")
        .env("LD_PRELOAD", "") // Fixes some log errors for some games
        // MAXIMA-LINUX-PORT-MOD: force en-US so Wine's prefix initialisation
        // writes Locale=00000409 instead of reading $LANG (e.g. de_DE → 00000407).
        // Both LANG and LC_ALL are set: Wine reads LANG for GetUserDefaultLCID()
        // during prefix init; LC_ALL covers glibc-level locale calls inside
        // pressure-vessel.
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .arg(arg);

    if !wine_path.ends_with("umu-run") {
        // wsock32 is used as a proxy for Northstar (Titanfall 2). TODO: provide user-facing option for this!
        child = child.env(
            "WINEDLLOVERRIDES",
            "CryptBase,wsock32,bcrypt,dxgi,d3d11,d3d12,d3d12core=n,b;winemenubuilder.exe=d",
        );
    }

    // MAXIMA-LINUX-PORT-MOD: opt-in workaround for the
    // `winegstreamer_create_color_converter` Wine abort in GE-Proton 10.x
    // triggered by EA App's embedded browser on systems without complete
    // system GStreamer plugins. Users hit by the abort can set
    // KYBER_DISABLE_WINEGSTREAMER=1 to disable winegstreamer.dll inside
    // the Proton prefix. The EA App login splash video stays silent but
    // the launch flow completes. PROTON_DLL_OVERRIDES is the umu/Proton
    // friendly variant that gets merged with the Proton built-in
    // overrides rather than replacing them.
    if env::var("KYBER_DISABLE_WINEGSTREAMER")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        child = child.env("PROTON_DLL_OVERRIDES", "winegstreamer=d");
    }

    if let Some(arguments) = args {
        child = child.args(arguments);
    }

    if let Some(cwd) = cwd {
        child.current_dir(cwd);
    }

    let status: ExitStatus;
    let mut output_str = String::new();

    if want_output {
        let output = child
            .stdout(Stdio::piped())
            // MAXIMA-LINUX-PORT-MOD: silence stderr so Wine/umu fixme spam
            // does not leak into the parent terminal (matches Windows-side
            // GUI behaviour where helper subprocesses are silent).
            .stderr(Stdio::null())
            .spawn()?
            .wait_with_output()
            .await?;
        output_str = String::from_utf8_lossy(&output.stdout).to_string();
        status = output.status;
    } else {
        // MAXIMA-LINUX-PORT-MOD: capture stderr only for failure diagnostics.
        // Stdout stays null (no useful stdout in the inject path; surfacing
        // Wine fixme spam on systems where this branch succeeds silently
        // would only pollute logs). On systems where the helper crashes
        // (e.g. CachyOS exit 101) the captured stderr is surfaced through
        // WineError::Command::output below so launcher logs show the real
        // Wine error instead of an empty string.
        let output = child
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?
            .wait_with_output()
            .await?;
        status = output.status;
        if !status.success() {
            output_str = String::from_utf8_lossy(&output.stderr).to_string();
        }
    };

    if !status.success() {
        return Err(NativeError::Wine(WineError::Command {
            output: output_str,
            exit: status,
        }));
    }

    Ok(output_str.to_string())
}

pub(crate) async fn install_wine() -> Result<(), NativeError> {
    // MAXIMA-LINUX-PORT-MOD 2026-05-24: skip auto-download entirely when the
    // user has set a custom proton path. The default-managed directory in
    // ~/.local/share/maxima/wine/proton is intentionally left untouched as a
    // recovery fallback (user can clear the override and get back to a
    // working default without a re-download).
    if let Some(custom) = resolve_custom_proton_path() {
        if is_valid_proton_layout(&custom) {
            info!("Using custom proton at {:?}, skipping GE-Proton download", custom);
            return Ok(());
        }
        // Invalid custom path: surface as hard error rather than silently
        // pulling 600MB from GitHub behind the user's back.
        return Err(NativeError::Wine(WineError::CustomProtonInvalid(custom)));
    }

    let release = get_wine_release()?;
    let asset = match release
        .assets
        .iter()
        .find(|x| PROTON_PATTERN.captures(&x.name).is_some())
    {
        Some(asset) => asset,
        None => return Err(NativeError::Wine(WineError::Fetch)),
    };

    let dir = maxima_dir()?.join("downloads");
    create_dir_all(&dir)?;

    let path = dir.join(&asset.name);
    github_download_asset(asset, &path)?;
    extract_wine(&path)?;

    let mut versions = versions()?;
    versions.proton = release.tag_name;
    set_versions(versions)?;

    if let Err(err) = remove_file(&path) {
        warn!("Failed to delete {:?} - {:?}", path, err);
    }

    let _ = run_wine_command("", None::<[&str; 0]>, None, false, CommandType::Run).await;

    Ok(())
}

fn extract_wine(archive_path: &PathBuf) -> Result<(), NativeError> {
    info!("Extracting proton...");

    // MAXIMA-LINUX-PORT-MOD 2026-05-24: always extract into the default
    // auto-managed path, NEVER into a user-supplied custom path. Without
    // this, install_wine would wipe the user's external proton install via
    // the remove_dir_all() below (Bug-Hunter #7). install_wine() already
    // early-returns when custom is active, but extract_wine is defense-in-
    // depth in case extract_wine is ever called directly.
    let dir = default_proton_dir()?;
    if dir.exists() {
        remove_dir_all(&dir)?;
    }

    create_dir_all(&dir)?;

    let archive_file = File::open(archive_path)?;
    let archive_decoder = GzDecoder::new(archive_file);
    let archive = Archive::new(archive_decoder);
    extract_archive(dir, archive)
}

fn extract_archive<R: Read + Sized>(
    dir: PathBuf,
    mut archive: Archive<R>,
) -> Result<(), NativeError> {
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?;

        let next = match entry_path.components().next() {
            Some(next) => next,
            None => {
                return Err(NativeError::PathComponentNext(entry_path.clone().into()));
            }
        };
        let destination_path = dir.join(entry_path.strip_prefix(next)?);
        if let Some(parent_dir) = destination_path.parent() {
            create_dir_all(parent_dir)?;
        }

        entry.unpack(destination_path)?;
    }

    Ok(())
}

/// Sets up the Wine prefix registry entries that EA / Origin / BF2 require
/// before launch.
///
/// Originally this wrote a `.reg` file and invoked `regedit` once. That
/// approach was unreliable on Linux because:
///   1. `regedit` started inside the umu pressure-vessel container did not
///      always run protonfixes, leaving the Origin Games keys partially
///      configured.
///   2. The `.reg` import did not honour the WoW6432Node redirector, so
///      32-bit Origin clients could not find the `Origin Games\1035052`
///      key (BF2's catalog ID).
///   3. Without a `Locale` value under that key, `GetUserDefaultLCID()`
///      reported the host's locale (e.g. `de_DE`) and BF2's entitlement
///      check aborted launch with "The title is installed in a language
///      that you are not entitled to play".
///
/// Each `reg add` invocation is routed through the same wine command
/// runner used for the actual game launch, so it inherits umu-run +
/// protonfixes and writes into the shared Wine prefix.
pub async fn setup_wine_registry() -> Result<(), NativeError> {
    // (key, value-name, value-data, [/reg:32 | /reg:64])
    type RegEntry = (&'static str, &'static str, &'static str, &'static str);
    const ENTRIES: &[RegEntry] = &[
        (
            r"HKLM\Software\Electronic Arts\EA Desktop",
            "InstallSuccessful",
            "true",
            "/reg:64",
        ),
        (
            r"HKLM\Software\Origin",
            "ClientPath",
            "C:/Windows/System32/conhost.exe",
            "/reg:32",
        ),
        (
            r"HKLM\Software\Origin",
            "InstallSuccessful",
            "true",
            "/reg:32",
        ),
        // BF2 catalog ID 1035052: locale + displayname under Origin Games.
        // Both /reg:32 and /reg:64 are needed because the game and the
        // Origin shim disagree on which view they read from.
        (
            r"HKLM\Software\Origin Games\1035052",
            "locale",
            "en_US",
            "/reg:32",
        ),
        (
            r"HKLM\Software\Origin Games\1035052",
            "displayname",
            "STAR WARS Battlefront II",
            "/reg:32",
        ),
        // MAXIMA-LINUX-PORT-MOD: explicitly set the en-US locale values after
        // Wine's own prefix init (which reads $LANG → de_DE → 00000407).
        // Must be the LAST entries so they win over any prior value in user.reg.
        //
        // Locale / LocaleName: fix GetUserDefaultLCID() and GetUserDefaultLocaleName().
        // sLanguage: fix GetSystemDefaultUILanguage() which reads this value
        //   directly (not the numeric Locale field).
        // WoW6432Node duplicate: BF2 is a 32-bit process. /reg:32 should
        //   redirect writes to WoW6432Node, but Wine's redirection is not
        //   perfectly reliable; writing both views guarantees BF2 finds en_US.
        (
            r"HKCU\Control Panel\International",
            "Locale",
            "00000409",
            "/reg:64",
        ),
        (
            r"HKCU\Control Panel\International",
            "LocaleName",
            "en-US",
            "/reg:64",
        ),
        (
            r"HKCU\Control Panel\International",
            "sLanguage",
            "ENU",
            "/reg:64",
        ),
        (
            r"HKLM\Software\WoW6432Node\Origin Games\1035052",
            "locale",
            "en_US",
            "/reg:64",
        ),
        (
            r"HKLM\Software\WoW6432Node\Origin Games\1035052",
            "displayname",
            "STAR WARS Battlefront II",
            "/reg:64",
        ),
        // MAXIMA-LINUX-PORT-MOD: EASteamProxy reads HKCU\Software\Valve\Steam\language
        // and passes it as locale=<lang> to BF2 at launch time. On a German system this
        // key contains "german" (written by Steam or appmanifest_1237950.acf), which
        // causes the "language not entitled" error even after all International patches.
        // Must be last so it overwrites any value Steam or Wine wrote earlier.
        (
            r"HKCU\Software\Valve\Steam",
            "language",
            "english",
            "/reg:64",
        ),
        // MAXIMA-LINUX-PORT-MOD: HKCU\Environment is read by Wine on session
        // start and propagates as a Win32 process environment variable. Setting
        // LANG/LC_ALL here means every wine sub-process the bootstrap → umu →
        // Proton → BF2 chain spawns sees en_US.UTF-8 even if the host process
        // was started with a German locale. Pressure-vessel filters Linux env
        // vars (see memory `feedback_kyber_pressure_vessel_env.md`); this
        // registry path is the documented escape hatch.
        (
            r"HKCU\Environment",
            "LANG",
            "en_US.UTF-8",
            "/reg:64",
        ),
        (
            r"HKCU\Environment",
            "LC_ALL",
            "en_US.UTF-8",
            "/reg:64",
        ),
    ];

    // Critical entries: if any of these fail we abort the whole launch
    // because BF2 will crash with the language-entitlement error and the
    // user has no signal as to why. Non-critical entries (#1-#3 above)
    // continue as warnings.
    const CRITICAL_KEYS: &[&str] = &[
        // BF2 catalog Origin Games\1035052
        "locale",
        "displayname",
        // HKCU\Control Panel\International
        "Locale",
        "LocaleName",
        "sLanguage",
        // HKCU\Software\Valve\Steam
        "language",
    ];

    // Pre-flight: skip the expensive 15-reg-add loop entirely if a quick
    // 4-key verify shows the locale-critical state is already correct.
    // Each reg.exe spawn round-trips through umu-run + pressure-vessel
    // (~3s); skipping turns a ~50s warm-launch overhead into ~12s.
    log::info!(
        "setup_wine_registry: pre-flight verifying {} critical locale keys...",
        CRITICAL_KEYS.len()
    );
    if verify_locale_is_english().await {
        log::info!(
            "setup_wine_registry: all critical locale keys already correct, \
             skipping {} reg-add entries (saves ~50s of umu-run overhead)",
            ENTRIES.len()
        );
        return Ok(());
    }
    log::info!(
        "setup_wine_registry: pre-flight verify found mismatches — \
         applying full set of {} reg add entries",
        ENTRIES.len()
    );

    let mut failures: Vec<&str> = Vec::new();
    let mut critical_failures: Vec<&str> = Vec::new();
    for (key, name, data, view) in ENTRIES {
        // Place /reg:VIEW *before* /f. Some Wine versions of reg.exe accept
        // the trailing form, others ignore the view selector silently and
        // write to the wrong hive.
        let result = run_wine_command(
            "reg",
            Some(vec!["add", key, "/v", name, view, "/d", data, "/f"]),
            None,
            false,
            CommandType::Run,
        )
        .await;

        match result {
            Ok(_) => {
                log::debug!("reg add OK: [{}] {}={} ({})", key, name, data, view);
            }
            Err(err) => {
                log::error!(
                    "reg add FAILED for [{}] {}={} ({}): {err}",
                    key,
                    name,
                    data,
                    view,
                );
                failures.push(name);
                if CRITICAL_KEYS.contains(name) {
                    critical_failures.push(name);
                }
            }
        }
    }

    if !failures.is_empty() {
        log::warn!(
            "{}/{} Wine registry entries failed: {:?}. \
             Non-critical failures continue; critical failures abort below.",
            failures.len(),
            ENTRIES.len(),
            failures,
        );
    }

    if !critical_failures.is_empty() {
        log::error!(
            "Aborting launch: {} CRITICAL registry entries failed: {:?}. \
             BF2 would crash with the language-entitlement error.",
            critical_failures.len(),
            critical_failures,
        );
        return Err(NativeError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "setup_wine_registry: {} critical entries failed: {:?}",
                critical_failures.len(),
                critical_failures,
            ),
        )));
    }

    log::info!("setup_wine_registry: done ({} entries OK)", ENTRIES.len() - failures.len());
    Ok(())
}

/// Just-in-time re-write of the locale-critical keys, run *immediately*
/// before the BF2 spawn — after the full `setup_wine_registry()` block
/// but separated by license/cloud-sync/protonfix activity that may
/// re-initialise parts of the prefix and clobber Steam-language /
/// International values.
///
/// Subset of `setup_wine_registry` ENTRIES: only the four keys BF2 reads
/// at entitlement-check time. Idempotent and fast (`reg add /f` overwrites
/// in place).
pub async fn lock_locale_just_in_time() -> Result<(), NativeError> {
    type RegEntry = (&'static str, &'static str, &'static str, &'static str);
    const CRITICAL: &[RegEntry] = &[
        (r"HKCU\Control Panel\International", "Locale", "00000409", "/reg:64"),
        (r"HKCU\Control Panel\International", "LocaleName", "en-US", "/reg:64"),
        (r"HKCU\Control Panel\International", "sLanguage", "ENU", "/reg:64"),
        (r"HKCU\Software\Valve\Steam", "language", "english", "/reg:64"),
    ];

    log::info!(
        "lock_locale_just_in_time: re-writing {} critical keys before BF2 spawn",
        CRITICAL.len()
    );

    let mut failures: Vec<&str> = Vec::new();
    for (key, name, data, view) in CRITICAL {
        let result = run_wine_command(
            "reg",
            Some(vec!["add", key, "/v", name, view, "/d", data, "/f"]),
            None,
            false,
            CommandType::Run,
        )
        .await;
        if let Err(err) = result {
            log::error!(
                "JIT reg add FAILED [{}] {}={} ({}): {err}",
                key,
                name,
                data,
                view,
            );
            failures.push(name);
        } else {
            log::debug!("JIT reg add OK: [{}] {}={}", key, name, data);
        }
    }

    if !failures.is_empty() {
        log::warn!(
            "lock_locale_just_in_time: {}/{} keys failed to re-write: {:?}. \
             BF2 may abort with the language-entitlement error.",
            failures.len(),
            CRITICAL.len(),
            failures,
        );
    } else {
        log::info!("lock_locale_just_in_time: all {} keys re-written", CRITICAL.len());
    }

    Ok(())
}

/// Probe what BF2 would actually see when it queries the locale-critical
/// keys. Logs the live values of the four most-important keys and warns
/// if any look German/host-locale-derived. Best-effort: a `reg query`
/// failure is treated as a missing/wrong key (returns false) rather
/// than aborting the launch.
///
/// Returns `true` only if all four critical keys resolved cleanly and
/// each value contained the expected English token. Callers use this
/// as a fast pre-flight to skip the expensive `setup_wine_registry()`
/// and `lock_locale_just_in_time()` paths when the prefix is already
/// in the desired state — typically the case on warm launches where
/// nothing tampered with the registry between sessions. Saves ~50s
/// per warm launch (15 reg-add calls @ ~3s each via umu-run).
pub async fn verify_locale_is_english() -> bool {
    type ProbeKey = (&'static str, &'static str, &'static str);
    const PROBES: &[ProbeKey] = &[
        (r"HKCU\Control Panel\International", "Locale", "00000409"),
        (r"HKCU\Control Panel\International", "LocaleName", "en-US"),
        (r"HKCU\Control Panel\International", "sLanguage", "ENU"),
        (r"HKCU\Software\Valve\Steam", "language", "english"),
    ];

    let mut all_match = true;

    for (key, name, expected) in PROBES {
        let result = run_wine_command(
            "reg",
            Some(vec!["query", key, "/v", name]),
            None,
            true,
            CommandType::Run,
        )
        .await;

        match result {
            Ok(output) => {
                let line_with_value = output
                    .lines()
                    .find(|l| l.contains(name))
                    .unwrap_or("(not found)");
                let trimmed = line_with_value.trim();
                if trimmed.contains(expected) {
                    log::info!(
                        "locale verify OK: [{}] {} -> {}",
                        key,
                        name,
                        trimmed,
                    );
                } else {
                    log::warn!(
                        "locale verify MISMATCH: [{}] {} -> {} (expected to contain '{}')",
                        key,
                        name,
                        trimmed,
                        expected,
                    );
                    all_match = false;
                }
            }
            Err(err) => {
                log::warn!(
                    "locale verify failed for [{}] {}: {err}",
                    key,
                    name,
                );
                all_match = false;
            }
        }
    }

    all_match
}

pub type WineRegistry = HashMap<String, String>;

lazy_static! {
    static ref MX_WINE_REGISTRY: Mutex<WineRegistry> = Mutex::new(WineRegistry::new());
}

async fn parse_wine_registry(file_path: &str) -> WineRegistry {
    let mut registry_map = MX_WINE_REGISTRY.lock().await;
    if !registry_map.is_empty() {
        return registry_map.clone();
    }

    let file = tokio::fs::File::open(file_path)
        .await
        .expect("Could not open file");
    let reader = BufReader::new(file);
    let mut current_section = String::new();

    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await.expect("Failed to read file") {
        let trimmed_line = line.trim();

        if trimmed_line.starts_with('[') && trimmed_line.contains(']') {
            if let Some(end) = trimmed_line.find(']') {
                current_section = trimmed_line[1..end].to_string();
            }
        } else if trimmed_line.contains('=') && trimmed_line.starts_with('"') {
            let parts: Vec<&str> = trimmed_line.splitn(2, '=').collect();
            if parts.len() == 2 {
                let key = parts[0].trim_matches('"').to_string();
                let value = parts[1].trim_matches('"').to_string();
                let full_key = format!("{}\\{}", current_section, key).replace("\\\\", "\\");
                registry_map.insert(full_key.to_lowercase(), value);
            }
        }
    }

    registry_map.clone()
}

pub async fn parse_mx_wine_registry() -> Result<WineRegistry, NativeError> {
    // MAXIMA-LINUX-PORT-MOD 2026-05-26: when a custom Proton is active and the
    // per-Proton wineprefix has not been initialised yet (first launch, no
    // system.reg yet), fall back to the default prefix's system.reg. The
    // registry entries we look up here (BF2 install path, EA Games catalog,
    // EA Desktop install state) are environment-level facts seeded by Steam
    // into the default Steam-compatdata-linked prefix and apply identically
    // regardless of which Proton runs the game. Without this fallback,
    // is_installed() returns false on the first launch with a fresh custom
    // prefix and the launcher shows "Game not installed".
    let primary_path = wine_prefix_dir()?.join("system.reg");
    let path = if primary_path.exists() {
        primary_path
    } else {
        let fallback = default_wine_prefix_dir()?.join("system.reg");
        if fallback.exists() {
            fallback
        } else {
            return Ok(HashMap::new());
        }
    };

    Ok(parse_wine_registry(path.safe_str()?).await)
}

pub async fn invalidate_mx_wine_registry() {
    MX_WINE_REGISTRY.lock().await.clear();
}

fn normalize_key(key: &str) -> String {
    let lower_key = key.to_lowercase();
    if lower_key.starts_with("hkey_local_machine\\") {
        lower_key
            .trim_start_matches("hkey_local_machine\\")
            .to_string()
    } else {
        lower_key
    }
}

pub async fn get_mx_wine_registry_value(query_key: &str) -> Result<Option<String>, RegistryError> {
    let registry_map = parse_mx_wine_registry().await?;
    let normalized_query_key = normalize_key(query_key);

    let value = if let Some(value) = registry_map.get(&normalized_query_key) {
        Some(value.clone())
    } else {
        let wow6432_query_key =
            normalized_query_key.replace("software\\", "software\\wow6432node\\");
        registry_map.get(&wow6432_query_key).cloned()
    };

    Ok(value.map(|x| x.replace("Z:", "").replace("\\", "/")))
}

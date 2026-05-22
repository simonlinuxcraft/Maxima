use base64::{engine::general_purpose, Engine};
use derive_getters::Getters;
use log::{error, info};
use std::{env, fmt::Display, path::PathBuf, sync::Arc};
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};
use uuid::Uuid;

use crate::{
    core::{
        auth::{
            context::AuthContext,
            nucleus_auth_exchange,
            storage::{AuthError, TokenError},
        },
        clients::JUNO_PC_CLIENT_ID,
        cloudsync::{CloudSyncError, CloudSyncLockMode},
        library::{LibraryError, OwnedOffer},
        service_layer::ServiceLayerError,
        Maxima,
    },
    ooa::{needs_license_update, request_and_save_license, LicenseAuth, LicenseError},
    util::{
        native::{NativeError, SafeParent, SafeStr},
        registry::bootstrap_path,
        simple_crypto,
    },
};
use thiserror::Error;

#[cfg(unix)]
use crate::unix::fs::case_insensitive_path;

use serde::{Deserialize, Serialize};

#[derive(Error, Debug)]
pub enum LaunchError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    CloudSync(#[from] CloudSyncError),
    #[error(transparent)]
    Library(#[from] LibraryError),
    #[error(transparent)]
    License(#[from] LicenseError),
    #[error(transparent)]
    Native(#[from] NativeError),
    #[error(transparent)]
    ServiceLayer(#[from] ServiceLayerError),
    #[error(transparent)]
    Token(#[from] TokenError),

    #[error("no offer was found for id `{0}`")]
    NoOfferFound(String),
    #[error("offline mode is not yet supported")]
    Offline,
    #[error("game path must be specified when launching in OnlineOffline mode")]
    GamePathOffline,
    #[error("game path not found")]
    GamePath,
    #[error("`{0}` is not installed")]
    NotInstalled(String),
    #[error("bootstrap was not found! Please re-install maxima")]
    BootstrapMissing,
    #[error(
        "content ID (`{0}`) was specified as an offer ID when launching in OnlineOffline mode"
    )]
    ContentIdAsOfferId(String),
}

pub enum StartupStage {
    Launch,
    ConnectionEstablished,
}

pub struct LibraryInjection {
    pub path: PathBuf,
    pub stage: StartupStage,
}

pub struct LaunchOptions {
    pub path_override: Option<String>,
    pub arguments: Vec<String>,
    pub cloud_saves: bool,
}

pub enum LaunchMode {
    /// Completely offline, relies on cached license files and user IDs
    Offline(String), // Offer ID
    /// Online, makes requests about the user and licensing
    Online(String), // Offer ID
    /// Online, but only for license requests; everything else uses dummy offer and user IDs
    /// Content ID, Game executable path, and username/password must be specified
    OnlineOffline(String, String, String), // Content ID, Persona, Password
}

impl LaunchMode {
    // What an awful name
    pub fn is_online_offline(&self) -> bool {
        match self {
            LaunchMode::OnlineOffline(_, _, _) => true,
            _ => false,
        }
    }
}

#[derive(Getters)]
pub struct ActiveGameContext {
    launch_id: String,
    game_path: String,
    /// MAXIMA-LINUX-PORT-MOD: Name of the game executable file (e.g.
    /// `starwarsbattlefrontii.exe`). On Linux/umu the launcher process tree
    /// re-execs through python3/bwrap/proton; the only stable way to find
    /// the Wine PID is to ask `wine-helper.exe get_pid <exe_filename>` with
    /// the original Windows-side .exe name. Walking `/proc/<pid>/cmdline`
    /// of the matched MXLaunchId process is racy because by the time LSX
    /// connects, the process may have already re-exec'd and the exe arg
    /// is gone from argv. Threading the filename through here keeps it
    /// deterministic.
    game_exe_filename: String,
    content_id: String,
    offer: Option<OwnedOffer>,
    mode: LaunchMode,
    injections: Vec<LibraryInjection>,
    cloud_saves: bool,
    process: Child,
    started: bool,
}

impl ActiveGameContext {
    pub fn new(
        launch_id: &str,
        game_path: &str,
        game_exe_filename: &str,
        cloud_saves: bool,
        content_id: &str,
        offer: Option<OwnedOffer>,
        mode: LaunchMode,
        process: Child,
    ) -> Self {
        Self {
            launch_id: launch_id.to_owned(),
            game_path: game_path.to_owned(),
            game_exe_filename: game_exe_filename.to_owned(),
            content_id: content_id.to_owned(),
            offer,
            mode,
            injections: Vec::new(),
            cloud_saves,
            process,
            started: false,
        }
    }

    pub fn set_started(&mut self) {
        self.started = true;
    }

    pub fn process_mut(&mut self) -> &mut Child {
        &mut self.process
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct BootstrapLaunchArgs {
    pub path: String,
    pub args: Vec<String>,
}

impl Display for LaunchMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchMode::Offline(offer_id) => write!(f, "{}", offer_id),
            LaunchMode::Online(offer_id) => write!(f, "{}", offer_id),
            LaunchMode::OnlineOffline(content_id, _, _) => write!(f, "{}", content_id),
        }
    }
}

pub async fn start_game(
    maxima_arc: Arc<Mutex<Maxima>>,
    mode: LaunchMode,
    options: LaunchOptions,
) -> Result<(), LaunchError> {
    let mut maxima = maxima_arc.lock().await;
    info!("Initiating game launch with {}...", mode);

    if let LaunchMode::OnlineOffline(ref content_id, _, _) = mode {
        if options.path_override.is_none() {
            return Err(LaunchError::GamePathOffline);
        }

        if content_id.starts_with("Origin.OFR") {
            return Err(LaunchError::ContentIdAsOfferId(content_id.clone()));
        }
    }

    let (content_id, online_offline, offer, access_token) =
        if let LaunchMode::Online(ref offer_id) = mode {
            let access_token = &maxima.access_token().await?;
            let offer = match maxima.mut_library().game_by_base_offer(offer_id).await? {
                Some(offer) => offer,
                None => return Err(LaunchError::NoOfferFound(offer_id.clone())),
            };

            // an explicit path override bypasses install detection
            if options.path_override.is_none() && !offer.is_installed().await {
                return Err(LaunchError::NotInstalled(offer.offer_id().clone()));
            }

            let content_id = offer.offer().content_id().to_owned();

            (
                content_id,
                false,
                Some(offer.clone()),
                access_token.to_owned(),
            )
        } else if let LaunchMode::OnlineOffline(ref content_id, _, _) = mode {
            (content_id.to_owned(), true, None, String::new())
        } else {
            return Err(LaunchError::Offline);
        };

    // Need to move this into Maxima and have a "current game" system
    let path = if let Some(game_path_override) = options.path_override {
        PathBuf::from(&game_path_override)
    } else if !online_offline {
        match offer {
            Some(ref offer) => offer.execute_path(false).await?.clone(),
            None => return Err(LaunchError::NoOfferFound("Unknown".to_string())),
        }
    } else {
        return Err(LaunchError::GamePath);
    };

    let dir = path.safe_parent()?.safe_str()?;
    // MAXIMA-LINUX-PORT-MOD: capture the .exe filename BEFORE `path` is
    // shadowed below. This name (e.g. starwarsbattlefrontii.exe) is what
    // wine-helper.exe needs to look up the game's Wine PID. Walking
    // /proc/<pid>/cmdline of the MXLaunchId-tagged Linux process is racy:
    // umu-run re-execs through python3 / bwrap / proton and by the time
    // LSX connects the original .exe path may already be gone from argv.
    let game_exe_filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_owned();
    #[cfg(unix)]
    let path = case_insensitive_path(path.clone());
    let path = path.safe_str()?;
    info!("Game path: {}", path);

    #[cfg(unix)]
    mx_linux_setup().await?;

    match mode {
        LaunchMode::Offline(_) => {}
        LaunchMode::Online(_) => {
            let auth = LicenseAuth::AccessToken(maxima.access_token().await?);

            let offer = offer.as_ref().unwrap();
            if needs_license_update(&content_id).await? {
                info!(
                    "Requesting new game license for {}...",
                    offer.offer().display_name()
                );

                request_and_save_license(&auth, &content_id, path.to_owned().into()).await?;
            } else {
                info!("Existing game license is still valid, not updating");
            }

            if options.cloud_saves && offer.offer().has_cloud_save() {
                info!("Syncing with cloud save...");

                let result = maxima
                    .cloud_sync()
                    .obtain_lock(offer, CloudSyncLockMode::Read)
                    .await;
                if let Err(err) = result {
                    error!("Failed to obtain CloudSync read lock: {}", err);
                } else {
                    let lock = result?;

                    let result = lock.sync_files().await;
                    if let Err(err) = result {
                        error!("Failed to sync cloud save: {}", err);
                    } else {
                        info!("Cloud save synced");
                    }

                    lock.release().await?;
                }
            }
        }
        LaunchMode::OnlineOffline(_, ref persona, ref password) => {
            let auth = LicenseAuth::Direct(persona.to_owned(), password.to_owned());

            if needs_license_update(&content_id).await? {
                request_and_save_license(&auth, &content_id, path.to_owned().into()).await?;
            } else {
                info!("Existing game license is still valid, not updating");
            }
        }
    }

    let mut game_args = options.arguments.clone();

    // Append args from env
    if let Ok(args) = env::var("MAXIMA_LAUNCH_ARGS") {
        game_args.append(&mut parse_arguments(args.as_str()));
    }

    if !bootstrap_path()?.exists() {
        return Err(LaunchError::BootstrapMissing);
    }

    let mut child = Command::new(bootstrap_path()?);
    child.arg("launch");

    let bootstrap_args = BootstrapLaunchArgs {
        path: path.to_string(),
        args: game_args,
    };

    let b64 = general_purpose::STANDARD.encode(serde_json::to_string(&bootstrap_args)?);
    child.arg(b64);

    let user = maxima.local_user().await?;
    let launch_id = Uuid::new_v4().to_string();

    child
        .current_dir(PathBuf::from(path).safe_parent()?)
        // MAXIMA-LINUX-PORT-MOD: explicitly propagate en-US locale to the
        // bootstrap → umu-run → Proton → BF2 chain. The bootstrap binary
        // is launched directly via Command::new (not via run_wine_command)
        // and would otherwise inherit LANG=de_DE from the host launcher,
        // causing umu to re-init parts of the prefix with German locale
        // and silently overwrite the locale-critical Wine registry keys
        // that setup_wine_registry just set.
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .env("MXLaunchId", launch_id.to_owned())
        .env("EAAuthCode", "unavailable")
        .env("EAEgsProxyIpcPort", "0")
        // MAXIMA-LINUX-PORT-MOD: read entitlement source from env so the
        // Launcher's in-process FFI path can override to STEAM via set_var().
        .env("EAEntitlementSource", std::env::var("EAEntitlementSource").unwrap_or_else(|_| "EA".to_string()))
        .env("EAExternalSource", std::env::var("EAExternalSource").unwrap_or_else(|_| "EA".to_string()))
        .env("EAFreeTrialGame", "false")
        // MAXIMA-LINUX-PORT-MOD: read game locale from env so the FFI path
        // can override to en_US (German system locale causes entitlement error).
        .env("EAGameLocale", std::env::var("EAGameLocale").unwrap_or_else(|_| maxima.locale.full_str().to_string()))
        .env("EAGenericAuthToken", access_token.to_owned())
        .env("EALaunchCode", "unavailable")
        .env("EALaunchOwner", std::env::var("EALaunchOwner").unwrap_or_else(|_| "EA".to_string()))
        .env(
            "EALaunchEAID",
            user.player()
                .as_ref()
                .ok_or(ServiceLayerError::MissingField)?
                .display_name(),
        )
        .env("EALaunchEnv", "production")
        .env("EALaunchOfflineMode", "false")
        .env("EALsxPort", maxima.lsx_port.to_string())
        .env(
            "EARtPLaunchCode",
            simple_crypto::rtp_handshake().to_string(),
        )
        .env("EASecureLaunchTokenTemp", user.id())
        .env("EASteamProxyIpcPort", "0")
        .env("OriginSessionKey", launch_id.clone())
        .env("ContentId", content_id.clone())
        .env("EAOnErrorExitRetCode", "1");

    match mode {
        LaunchMode::Offline(_) => todo!(),
        LaunchMode::Online(ref offer_id) => {
            let short_token = request_opaque_ooa_token(&access_token).await?;

            child
                .env("EAConnectionId", offer_id.clone())
                .env("EALicenseToken", offer_id.clone())
                .env("EALaunchUserAuthToken", short_token)
                .env("EAAccessTokenJWS", access_token);
        }
        LaunchMode::OnlineOffline(_, ref persona, ref password) => {
            child
                .env("EALaunchOOAUserEmail", persona)
                .env("EALaunchOOAUserPass", password)
                // Given this is probably running headlessly, don't show a UI on error
                .env("EAOnErrorExitRetCode", "1");
        }
    };

    // MAXIMA-LINUX-PORT-MOD: pre-spawn verify is intentionally NOT run here.
    // The pre-flight verify inside `setup_wine_registry()` (called from
    // `mx_linux_setup()` at the top of this fn) already established that
    // the four locale-critical keys hold the expected English values.
    // License-fetch / cloud-sync between that verify and this spawn point
    // do not touch Wine registry, so a second 4-key probe (~14s of umu-run
    // overhead via reg query) is pure waste on the happy path.
    //
    // If the language-entitlement error ever returns the recovery path is
    // to re-introduce a one-key probe of `HKCU\Software\Valve\Steam\language`
    // here (the only key Steam itself sometimes overwrites between launches)
    // and conditionally call `lock_locale_just_in_time()` on mismatch.

    let child = child.spawn().expect("Failed to start child");

    maxima.playing = Some(ActiveGameContext::new(
        &launch_id,
        dir,
        &game_exe_filename,
        options.cloud_saves,
        &content_id,
        offer,
        mode,
        child,
    ));

    Ok(())
}

async fn request_opaque_ooa_token(access_token: &str) -> Result<String, AuthError> {
    let mut context = AuthContext::new()?;
    context.set_access_token(&access_token);
    context.set_token_format("OPAQUE");
    context.set_expires_in(550);

    // These scopes match the token EA Desktop requests for this
    context.add_scope("basic.commerce.cartv2");
    context.add_scope("service.atom");
    context.add_scope("dp.client.default");
    context.add_scope("signin");
    context.add_scope("social_recommendation_user");
    context.add_scope("basic.optin.write");
    context.add_scope("basic.commerce.cartv2.write");
    context.add_scope("basic.billing");
    context.add_scope("external.social_information_ups_admin");

    nucleus_auth_exchange(&context, JUNO_PC_CLIENT_ID, "token").await
}

#[cfg(unix)]
pub async fn mx_linux_setup() -> Result<(), NativeError> {
    use crate::unix::wine::{
        check_runtime_validity, check_wine_validity, get_lutris_runtimes, install_runtime,
        install_wine, setup_wine_registry,
    };

    info!("Verifying wine dependencies...");

    let skip = std::env::var("MAXIMA_DISABLE_WINE_VERIFICATION").is_ok();
    if !skip {
        if !check_wine_validity().await? {
            install_wine().await?;
        }
        let runtimes = get_lutris_runtimes().await?;
        if !check_runtime_validity("eac_runtime", &runtimes).await? {
            install_runtime("eac_runtime", &runtimes).await?;
        }
        if !check_runtime_validity("umu", &runtimes).await? {
            install_runtime("umu", &runtimes).await?;
        }
    }

    setup_wine_registry().await?;

    Ok(())
}

pub fn parse_arguments(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current_arg = String::new();
    let mut in_quotes = false;

    for c in input.chars() {
        match c {
            ' ' if !in_quotes => {
                if !current_arg.is_empty() {
                    args.push(current_arg.clone());
                    current_arg.clear();
                }
            }
            '"' => {
                in_quotes = !in_quotes;
            }
            _ => {
                current_arg.push(c);
            }
        }
    }

    if !current_arg.is_empty() {
        args.push(current_arg);
    }

    args
}

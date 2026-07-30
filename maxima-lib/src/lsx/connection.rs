use derive_getters::Getters;
use lazy_static::lazy_static;
use log::{debug, error, warn};
use quick_xml::DeError;
use regex::Regex;
use std::{
    io::{ErrorKind, Read, Write},
    net::TcpStream,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use sysinfo::{Pid, PidExt, ProcessExt, ProcessRefreshKind, RefreshKind, System, SystemExt};
use thiserror::Error;
use tokio::sync::{MutexGuard, RwLock};

use super::{
    request::{
        account::handle_query_entitlements_request,
        achievements::handle_query_achievements_request,
        auth::handle_auth_code_request,
        challenge::handle_challenge_response,
        config::handle_config_request,
        core::{
            handle_connectivity_request, handle_set_downloader_util_request,
            handle_settings_request,
        },
        game::{handle_all_game_info_request, handle_game_info_request},
        igo::handle_show_igo_window_request,
        license::handle_license_request,
        offer::handle_query_offers_request,
        profile::{
            handle_get_block_list_request, handle_presence_request, handle_profile_request,
            handle_query_friends_request, handle_query_image_request,
            handle_query_presence_request, handle_set_presence_request,
        },
        progressive_install::{handle_pi_availability_request, handle_pi_installed_chunks_request},
        voip::handle_voip_status_request,
    },
    types::{
        create_lsx_message, LSXChallenge, LSXEvent, LSXEventType, LSXMessageType, LSXRequest,
        LSXResponse, LSX,
    },
};
use crate::{
    core::{
        auth::storage::TokenError, launch::ActiveGameContext, LockedMaxima, Maxima, MaximaEvent,
    },
    lsx::{request::LSXRequestError, types::LSXRequestType},
    util::{
        native::NativeError,
        simple_crypto::{simple_decrypt, simple_encrypt},
    },
};

#[derive(Error, Debug)]
pub enum LSXConnectionError {
    #[error(transparent)]
    Xml(#[from] DeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Request(#[from] LSXRequestError),
    #[error(transparent)]
    Native(#[from] NativeError),

    #[error("LSX connection closed")]
    Closed,
    #[error("there is no active game context, LSX connection cannot be established")]
    GameContext,
    #[error("internal error in LSX connection: {0}")]
    Internal(ErrorKind),
}

const CORE_SENDER: &str = "EALS";

const CHALLENGE_BUILD: &str = "release";
const CHALLENGE_KEY: &str = "cacf897a20b6d612ad0c05e011df52bb"; // Need to figure out how to generate this
const CHALLENGE_VERSION: &str = "10,5,30,15625";

lazy_static! {
    static ref LSX_PATTERN: Regex = Regex::new(r"<LSX>.*?</LSX>").unwrap();
}

macro_rules! lsx_message_matcher {
    (
        $connection_var:expr, $message_var:expr, $message_type:ty;
        $($name:ident $handler:ident),* $(,)?
    ) => {
        paste::paste! {
            match $message_var {
                $(
                    $message_type::$name(msg) => $handler($connection_var, msg).await,
                )*
            }?
        }
    };
}

pub enum EncryptionState {
    Disabled,
    Ready([u8; 16]),
    Enabled([u8; 16]),
}

#[derive(Getters)]
pub struct ConnectionState {
    #[getter(skip)]
    maxima: LockedMaxima,
    challenge: String,
    encryption: EncryptionState,
    pid: u32,
    /// Message responses that are waiting to be sent
    queued_messages: Vec<String>,
}

pub type LockedConnectionState = Arc<RwLock<ConnectionState>>;

impl ConnectionState {
    /// Enable encryption on the packet after next
    pub fn enable_encryption(&mut self, encryption_key: [u8; 16]) {
        self.encryption = EncryptionState::Ready(encryption_key);
    }

    pub async fn maxima(&mut self) -> MutexGuard<'_, Maxima> {
        self.maxima.lock().await
    }

    pub fn maxima_arc(&mut self) -> LockedMaxima {
        self.maxima.clone()
    }

    pub async fn access_token(&mut self) -> Result<String, TokenError> {
        self.maxima().await.access_token().await
    }

    pub fn queue_message(&mut self, message: LSX) -> Result<(), LSXConnectionError> {
        let mut str = quick_xml::se::to_string(&message)?;
        debug!("Queuing LSX Message: {}", str);

        if let EncryptionState::Enabled(key) = self.encryption {
            str = simple_encrypt(str.as_bytes(), &key)
        };

        str += "\0";
        self.queued_messages.push(str);
        Ok(())
    }
}

pub fn get_os_pid(context: &ActiveGameContext) -> Result<u32, NativeError> {
    let mut pid = None;

    // MAXIMA-LINUX-PORT-MOD: only refresh processes. System::new_all() is
    // RefreshKind::everything(), which additionally re-scans every hwmon node
    // (on AMD that is an SMU firmware round-trip per sensor), runs statvfs()
    // over every mount incl. FUSE, and reads /proc/<pid>/io per process. None
    // of that is used here; cmd() and environ() are filled regardless of the
    // ProcessRefreshKind.
    let sys = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::new()),
    );
    for e in sys.processes() {
        let (p_pid, process) = e;
        if process.cmd().is_empty() {
            continue;
        }

        // MAXIMA-LINUX-PORT-MOD: under Wine/Proton, cmd[0] is the wine binary
        // (e.g. wine64) while the game exe path appears in cmd[1] or later.
        // Check all args so we find the process regardless of Wine invocation style.
        let game_path_found = process.cmd().iter().any(|arg| {
            let mut s = arg.to_owned();
            if cfg!(unix) && s.starts_with("Z:") {
                s = s.replace("Z:", "").replace('\\', "/");
            }
            s.starts_with(context.game_path())
        });

        if !game_path_found {
            continue;
        }

        for ele in process.environ() {
            let (key, value) = ele.split_once('=').unwrap_or((ele, ""));
            if key != "MXLaunchId" || value != context.launch_id() {
                continue;
            }

            pid = Some(p_pid.as_u32());
            break;
        }
    }

    Ok(pid.unwrap_or(0))
}

#[cfg(target_os = "windows")]
pub async fn get_wine_pid(_launch_id: &str, _name: &str) -> Result<u32, NativeError> {
    Ok(0)
}

#[cfg(target_os = "linux")]
pub async fn get_wine_pid(launch_id: &str, name: &str) -> Result<u32, NativeError> {
    use crate::core::background_service::wine_get_pid;

    wine_get_pid(launch_id, name).await
}

pub struct Connection {
    maxima: LockedMaxima,
    stream: TcpStream,
    state: LockedConnectionState,
}

impl Connection {
    pub async fn new(
        maxima_arc: LockedMaxima,
        stream: TcpStream,
    ) -> Result<Self, LSXConnectionError> {
        stream.set_nodelay(true)?;
        stream.set_nonblocking(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;

        let maxima: MutexGuard<'_, Maxima> = maxima_arc.lock().await;
        let context: &ActiveGameContext = match maxima.playing() {
            Some(context) => context,
            None => {
                stream.shutdown(std::net::Shutdown::Both)?;
                return Err(LSXConnectionError::GameContext);
            }
        };

        // The PID system is mainly for Kyber injection
        let mut pid = get_os_pid(context);
        if cfg!(unix) {
            if let Ok(_os_pid) = pid {
                // MAXIMA-LINUX-PORT-MOD: deterministic Wine-PID lookup.
                //
                // Previously this walked process.cmd()[] of the MXLaunchId-tagged
                // Linux process to find the .exe arg. That race-loses under
                // umu-run/pressure-vessel because by the time LSX connects and
                // we get here, the matched process may already have re-exec'd
                // through python3 / bwrap / proton — its argv no longer
                // contains the original game .exe path. The fallback to cmd()[0]
                // produced wine-helper.exe queries for "python3" or "bwrap",
                // both of which return PID 0, breaking inject silently.
                //
                // ActiveGameContext now carries the .exe filename captured
                // from launch.rs::start_game() at spawn time, before any
                // re-exec can mangle it. Use that directly.
                let filename = context.game_exe_filename();
                if filename.is_empty() {
                    warn!(
                        "ActiveGameContext.game_exe_filename is empty — \
                         wine_get_pid will likely fail. Check launch.rs"
                    );
                } else {
                    pid = get_wine_pid(&context.launch_id(), filename).await;
                }
            }
        }

        if let Err(ref err) = pid {
            warn!("Error while finding game PID: {}", err);
        } else if pid.as_ref().unwrap() == &0 {
            warn!("Failed to find PID through launch ID, things may not work!");
        }

        let state = Arc::new(RwLock::new(ConnectionState {
            maxima: maxima_arc.clone(),
            challenge: CHALLENGE_KEY.to_string(),
            encryption: EncryptionState::Disabled,
            pid: pid.unwrap_or(0),
            queued_messages: Vec::new(),
        }));

        Ok(Self {
            maxima: maxima_arc.clone(),
            stream,
            state,
        })
    }

    // State

    pub async fn maxima(&self) -> MutexGuard<Maxima> {
        self.maxima.lock().await
    }

    // Initialization

    pub async fn send_challenge(&mut self) -> Result<(), LSXConnectionError> {
        let challenge = create_lsx_message(LSXMessageType::Event(LSXEvent {
            sender: CORE_SENDER.to_string(),
            value: LSXEventType::Challenge(LSXChallenge {
                attr_build: CHALLENGE_BUILD.to_string(),
                attr_key: self.state.read().await.challenge.to_owned(),
                attr_version: CHALLENGE_VERSION.to_string(),
            }),
        }));

        self.state.write().await.queue_message(challenge)?;
        Ok(())
    }

    pub async fn listen(&mut self) -> Result<(), LSXConnectionError> {
        let mut buffer = [0; 1024 * 8];

        let n = match self.stream.read(&mut buffer) {
            Ok(n) if n == 0 => {
                // MAXIMA-LINUX-PORT-MOD: Diagnostic for LSX disconnect detection
                warn!("LSX peer closed socket cleanly (FIN received)");
                return Err(LSXConnectionError::Closed);
            }
            Ok(n) => n,
            Err(err) => {
                let kind = err.kind();
                if kind == ErrorKind::WouldBlock {
                    return Ok(());
                }
                // MAXIMA-LINUX-PORT-MOD: Diagnostic for LSX disconnect detection
                warn!("LSX read error: kind={:?}", kind);
                return Err(LSXConnectionError::Internal(kind));
            }
        };

        let state = self.state.write().await;

        let trimmed_buffer = &buffer[..n];
        let message = if let EncryptionState::Enabled(key) = state.encryption {
            simple_decrypt(trimmed_buffer, &key)
        } else {
            String::from_utf8_lossy(trimmed_buffer).trim().to_owned()
        };

        drop(state);

        for mat in LSX_PATTERN.find_iter(message.as_str()) {
            if let Err(err) = self.process_message(mat.as_str()).await {
                error!("Failed to process message: {}", err);
            }
        }

        Ok(())
    }

    pub async fn process_queue(&mut self) -> Result<(), LSXConnectionError> {
        let mut state = self.state.write().await;
        for message in &state.queued_messages {
            if let Err(err) = self.stream.write(message.as_bytes()) {
                error!("Failed to send LSX message: {}", err);
            }
        }

        if !state.queued_messages.is_empty() {
            self.stream.flush()?;
        }

        state.queued_messages.clear();
        Ok(())
    }

    // Message Processing

    async fn process_message(&mut self, message: &str) -> Result<(), LSXConnectionError> {
        debug!("Received LSX Message: {}", message);

        let mut message = message.to_string();
        message.remove_matches("version=\"\" ");
        let lsx_message: LSX = quick_xml::de::from_str(message.as_str())?;

        let state = self.state.clone();
        tokio::spawn(async move {
            let reply: Result<Option<LSXMessageType>, LSXConnectionError> = match lsx_message.value
            {
                LSXMessageType::Event(msg) => Connection::process_event_message(&state, msg).await,
                LSXMessageType::Request(msg) => {
                    Connection::process_request_message(&state, msg).await
                }
                LSXMessageType::Response(_) => unimplemented!(),
            };

            let reply: Option<LSXMessageType> = match reply {
                Ok(reply) => reply,
                Err(err) => {
                    error!("Failed to process LSX message: {}", err);
                    return;
                }
            };

            if let Some(reply) = reply {
                let mut state = state.write().await;
                let result = state.queue_message(LSX { value: reply });

                if let Err(err) = result {
                    error!("Failed to queue LSX message: {}", err);
                    return;
                }
            }

            let mut state = state.write().await;
            if let EncryptionState::Ready(key) = state.encryption {
                state.encryption = EncryptionState::Enabled(key);
            }
        });

        Ok(())
    }

    async fn process_event_message(
        _: &LockedConnectionState,
        _: LSXEvent,
    ) -> Result<Option<LSXMessageType>, LSXConnectionError> {
        Ok(None)
    }

    async fn process_request_message(
        state: &LockedConnectionState,
        message: LSXRequest,
    ) -> Result<Option<LSXMessageType>, LSXConnectionError> {
        {
            let pid = *state.read().await.pid();
            state
                .write()
                .await
                .maxima()
                .await
                .call_event(MaximaEvent::ReceivedLSXRequest(pid, message.value.clone()));
        }

        let result = lsx_message_matcher!(
            state.clone(), message.value, LSXRequestType;

            ChallengeResponse handle_challenge_response,
            GetBlockList handle_get_block_list_request,
            GetConfig handle_config_request,
            GetProfile handle_profile_request,
            GetSetting handle_settings_request,
            RequestLicense handle_license_request,
            GetGameInfo handle_game_info_request,
            GetAllGameInfo handle_all_game_info_request,
            GetInternetConnectedState handle_connectivity_request,
            IsProgressiveInstallationAvailable handle_pi_availability_request,
            AreChunksInstalled handle_pi_installed_chunks_request,
            GetAuthCode handle_auth_code_request,
            GetPresence handle_presence_request,
            SetPresence handle_set_presence_request,
            QueryOffers handle_query_offers_request,
            QueryPresence handle_query_presence_request,
            QueryFriends handle_query_friends_request,
            QueryEntitlements handle_query_entitlements_request,
            QueryImage handle_query_image_request,
            GetVoipStatus handle_voip_status_request,
            ShowIGOWindow handle_show_igo_window_request,
            SetDownloaderUtilization handle_set_downloader_util_request,
            QueryAchievements handle_query_achievements_request,
        );

        Ok(match result {
            Some(result) => Some(LSXMessageType::Response(LSXResponse {
                sender: message.recipient,
                id: message.id,
                value: result,
            })),
            None => None,
        })
    }
}

use lazy_static::lazy_static;
use regex::Regex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;

use crate::core::{auth::storage::AuthError, clients::JUNO_PC_CLIENT_ID};

use super::context::AuthContext;

lazy_static! {
    static ref HTTP_PATTERN: Regex =
        Regex::new(r"^([A-Za-z]+) +(.*) +(HTTP/[0-9][.][0-9])").unwrap();
}

// Browser sign-in must redirect back to the local /auth listener via the
// qrc:// scheme handler. A sandboxed default browser (Flatpak/Snap, e.g.
// Zen) can swallow that callback, leaving the listener waiting forever.
// Bound the whole wait so a missing callback fails with a clear, actionable
// error instead of hanging the login. On timeout the listener is dropped,
// freeing port 31033 for the next attempt.
const LOGIN_CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

// MAXIMA-LINUX-PORT-MOD 2026-06-07: open the EA sign-in URL in the user's real
// browser, robust against the AppImage environment.
//
// Inside the AppImage the GTK apprun-hook prepends $APPDIR/usr/share to
// XDG_DATA_DIRS and exports GTK_PATH/GDK_PIXBUF_*/GI_TYPELIB_PATH etc. Those
// leak into the spawned `xdg-open` and the browser it launches. On SteamOS /
// Steam Deck the default browser is a Flatpak whose .desktop lives under the
// flatpak `exports/share` dirs, which the polluted (or session-short)
// XDG_DATA_DIRS does not contain, so xdg-open cannot resolve the browser and
// KDE falls back to opening the Discover store instead. Rebuild a sane
// XDG_DATA_DIRS (host dirs minus the AppImage's own share, plus the flatpak
// export dirs and the standard system dirs) and drop the AppImage GTK env, so
// the system xdg-open finds and launches the real default browser. Returns Err
// if xdg-open is missing/fails so the caller can fall back to the open crate.
#[cfg(target_os = "linux")]
fn open_login_url(url: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};

    let appdir = std::env::var("APPDIR").unwrap_or_default();
    let mut dirs: Vec<String> = Vec::new();
    if let Some(base) = std::env::var("KYBER_ORIG_XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("XDG_DATA_DIRS").ok())
    {
        for d in base.split(':').filter(|d| !d.is_empty()) {
            if !appdir.is_empty() && d.starts_with(&appdir) {
                continue; // drop the AppImage's own usr/share
            }
            dirs.push(d.to_string());
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(format!("{home}/.local/share/flatpak/exports/share"));
    }
    for d in [
        "/var/lib/flatpak/exports/share",
        "/usr/local/share",
        "/usr/share",
    ] {
        dirs.push(d.to_string());
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));

    Command::new("xdg-open")
        .arg(url)
        .env("XDG_DATA_DIRS", dirs.join(":"))
        .env_remove("GTK_PATH")
        .env_remove("GDK_PIXBUF_MODULE_FILE")
        .env_remove("GTK_IM_MODULE_FILE")
        .env_remove("GI_TYPELIB_PATH")
        .env_remove("GSETTINGS_SCHEMA_DIR")
        .env_remove("GTK_EXE_PREFIX")
        .env_remove("GTK_DATA_PREFIX")
        .env_remove("GTK_THEME")
        .env_remove("GDK_BACKEND")
        .env_remove("LD_LIBRARY_PATH")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .and_then(|s| {
            if s.success() {
                Ok(())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("xdg-open exited with {s}"),
                ))
            }
        })
}

pub async fn begin_oauth_login_flow<'a>(context: &mut AuthContext<'a>) -> Result<(), AuthError> {
    let auth_url = context.nucleus_auth_url(JUNO_PC_CLIENT_ID, "code")?;
    #[cfg(target_os = "linux")]
    {
        if open_login_url(&auth_url).is_err() {
            open::that(&auth_url)?;
        }
    }
    #[cfg(not(target_os = "linux"))]
    open::that(&auth_url)?;
    let listener = TcpListener::bind("127.0.0.1:31033").await?;

    let wait = async {
        loop {
            let (mut socket, _) = listener.accept().await?;

            let (read, _) = socket.split();
            let mut reader = BufReader::new(read);

            let mut line = String::new();
            reader.read_line(&mut line).await?;

            let captures = match HTTP_PATTERN.captures(&line) {
                Some(cap) => cap,
                None => continue,
            };

            let path_and_query = captures.get(2).ok_or(AuthError::Query)?.as_str();
            if path_and_query.starts_with("/auth") {
                let query = path_and_query
                    .split_once("?")
                    .map(|(_, qs)| qs.trim())
                    .map(querystring::querify)
                    .ok_or(AuthError::Query)?;

                for query in query {
                    if query.0 == "code" {
                        context.set_code(query.1);
                        return Ok(());
                    }
                }

                return Err(AuthError::NoAuthCode.into());
            }
        }
    };

    match tokio::time::timeout(LOGIN_CALLBACK_TIMEOUT, wait).await {
        Ok(result) => result,
        Err(_elapsed) => Err(AuthError::LoginTimeout),
    }
}

// Use the OOA API to retrieve an access token without a captcha
#[deprecated(note = "This method of login was patched and this function will be removed soon")]
pub async fn manual_login(_persona: &str, _password: &str) -> Result<String, AuthError> {
    unimplemented!();
}

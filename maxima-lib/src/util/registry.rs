#[cfg(windows)]
extern crate winapi;

use std::path::PathBuf;
use thiserror::Error;

#[cfg(windows)]
use std::ptr;

#[cfg(windows)]
use widestring::U16CString;

#[cfg(windows)]
use winapi::{
    shared::{minwindef::HKEY, winerror::ERROR_CANCELLED},
    um::{
        errhandlingapi::GetLastError,
        shellapi::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SEE_MASK_NO_CONSOLE},
    },
    um::{
        winnt::KEY_QUERY_VALUE,
        winreg::{RegCloseKey, RegOpenKeyExW, RegQueryValueExW},
    },
};

#[cfg(windows)]
use winreg::{
    enums::{HKEY_CLASSES_ROOT, HKEY_LOCAL_MACHINE, KEY_WRITE},
    RegKey,
};

#[cfg(unix)]
use std::{collections::HashMap, env, fs};

#[cfg(unix)]
use crate::unix::fs::case_insensitive_path;

use super::native::{module_path, NativeError, SafeParent, SafeStr};

#[cfg(target_pointer_width = "64")]
pub const REG_ARCH_PATH: &str = "SOFTWARE\\WOW6432Node";
#[cfg(target_pointer_width = "32")]
pub const REG_ARCH_PATH: &str = "SOFTWARE";

pub const REG_EAX32_PATH: &str = "SOFTWARE\\Electronic Arts\\EA Desktop";

#[derive(Error, Debug)]
pub enum RegistryError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Native(#[from] NativeError),
    #[cfg(windows)]
    #[error(transparent)]
    WidestringContainsNul(#[from] widestring::error::ContainsNul<u16>),

    #[error("registry key `{0}` not found")]
    Key(String),
    #[error("failed to get `{value}` of registry key `{key}`")]
    Value { value: String, key: String },
    #[error("install key is invalid")]
    InvalidInstallKey,
    #[error("invalid stored client path")]
    InvalidStoredClientPath,
    #[error("invalid qrc protocol")]
    InvalidQrcProtocol,

    // Linux
    #[error("xdg-mime command is not available. Please install xdg-utils")]
    XdgMime,
    #[error("Failed to set MIME type association for {type}: {error}")]
    MimeSet { r#type: String, error: String },
    #[error("failed to query mime status")]
    XdgQueryFailed,
    #[error("QRC protocol is not registered")]
    QrcUnregistered,
}

#[cfg(windows)]
pub fn check_registry_validity() -> Result<(), RegistryError> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let origin = hklm.open_subkey(format!("{}\\Origin", REG_ARCH_PATH))?;

    let path: String = origin.get_value("ClientPath")?;
    let valid = path == bootstrap_path()?.safe_str()?;
    if !valid {
        return Err(RegistryError::InvalidStoredClientPath);
    }

    let eax32 = hklm.open_subkey(REG_EAX32_PATH)?;
    let install_succesful: String = eax32.get_value("InstallSuccessful")?;
    if install_succesful != "true" {
        return Err(RegistryError::InvalidInstallKey);
    }

    let hkcr = RegKey::predef(HKEY_CLASSES_ROOT);
    let qrc = hkcr.open_subkey("qrc");
    if qrc.is_err() {
        return Err(RegistryError::InvalidQrcProtocol);
    }

    Ok(())
}

#[cfg(windows)]
async fn read_reg_key(path: &str) -> Result<Option<String>, RegistryError> {
    if let (Some(hkey_segment), Some(value_segment)) = (path.find('\\'), path.rfind('\\')) {
        let sub_key = &path[(hkey_segment + 1)..value_segment];
        let value_name = &path[(value_segment + 1)..];

        let hkey = HKEY_LOCAL_MACHINE as HKEY;
        let mut handle = ptr::null_mut();

        unsafe {
            if RegOpenKeyExW(
                hkey,
                U16CString::from_str(sub_key)?.as_ptr(),
                0,
                KEY_QUERY_VALUE,
                &mut handle,
            ) != 0
            {
                return Err(RegistryError::Key(sub_key.to_string()));
            }

            let dw_type = ptr::null_mut();
            let mut dw_size = 0;

            if RegQueryValueExW(
                handle,
                U16CString::from_str(value_name)?.as_ptr(),
                ptr::null_mut(),
                dw_type,
                ptr::null_mut(),
                &mut dw_size,
            ) != 0
            {
                RegCloseKey(handle);
                return Err(RegistryError::Value {
                    value: value_name.to_string(),
                    key: sub_key.to_string(),
                });
            }

            if dw_size <= 0 {
                RegCloseKey(handle);
                return Err(RegistryError::Value {
                    value: value_name.to_string(),
                    key: sub_key.to_string(),
                });
            }

            let mut buf: Vec<u16> = vec![0; dw_size as usize / 2];
            if RegQueryValueExW(
                handle,
                U16CString::from_str(value_name)?.as_ptr(),
                ptr::null_mut(),
                dw_type,
                buf.as_mut_ptr() as *mut u8,
                &mut dw_size,
            ) != 0
            {
                RegCloseKey(handle);
                return Err(RegistryError::Value {
                    value: value_name.to_string(),
                    key: sub_key.to_string(),
                });
            }

            RegCloseKey(handle);
            return Ok(Some(String::from_utf16_lossy(&buf[..buf.len() - 1])));
        }
    }

    Ok(None)
}

#[cfg(unix)]
async fn read_reg_key(path: &str) -> Result<Option<String>, RegistryError> {
    use crate::unix::wine::get_mx_wine_registry_value;
    Ok(get_mx_wine_registry_value(path).await?)
}

pub async fn parse_registry_path(key: &str) -> Result<PathBuf, RegistryError> {
    let mut parts = key
        .split(|c| c == '[' || c == ']')
        .filter(|s| !s.is_empty());

    let path = if let (Some(first), Some(second)) = (parts.next(), parts.next()) {
        let path = match read_reg_key(first).await? {
            Some(path) => path.replace("\\", "/").replace("//", "/"),
            None => return Ok(PathBuf::from(key.to_owned())),
        };

        let second = second.replace("\\", "/");
        let second = second.strip_prefix("/").unwrap_or(&second);

        return Ok([path, second.to_owned()].iter().collect());
    } else {
        PathBuf::from(key.to_owned())
    };

    #[cfg(unix)]
    let path = case_insensitive_path(path);
    Ok(path)
}

pub async fn parse_partial_registry_path(key: &str) -> Result<PathBuf, RegistryError> {
    let mut parts = key
        .split(|c| c == '[' || c == ']')
        .filter(|s| !s.is_empty());

    let path = if let (Some(first), Some(_second)) = (parts.next(), parts.next()) {
        let path = match read_reg_key(first).await? {
            Some(path) => path.replace("\\", "/"),
            None => return Ok(PathBuf::from(key.to_owned())),
        };

        return Ok(PathBuf::from(path.to_owned()));
    } else {
        PathBuf::from(key.to_owned())
    };

    #[cfg(unix)]
    let path = case_insensitive_path(path);
    Ok(path)
}

#[cfg(windows)]
pub fn read_game_path(name: &str) -> Result<PathBuf, RegistryError> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);

    dbg!(name);

    let mut key = hklm.open_subkey(format!("SOFTWARE\\EA Games\\{}", name));
    if key.is_err() {
        key = hklm.open_subkey(format!("SOFTWARE\\WOW6432Node\\EA Games\\{}", name));
    }

    let key = match key {
        Ok(key) => key,
        Err(_) => return Err(RegistryError::Key(format!("SOFTWARE\\EA Games\\{}", name))),
    };

    let path: String = key.get_value("Install Dir")?;
    Ok(PathBuf::from(path))
}

#[cfg(windows)]
pub fn bootstrap_path() -> std::result::Result<PathBuf, NativeError> {
    Ok(module_path()?.safe_parent()?.join("maxima-bootstrap.exe"))
}

#[cfg(windows)]
pub fn launch_bootstrap() -> Result<(), NativeError> {
    let path = bootstrap_path()?;

    let verb = "runas";
    let file = path.safe_str()?;
    let file1 = file.to_string();
    let parameters = "";

    let verb = verb.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let file = file.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let parameters = parameters.encode_utf16().chain(Some(0)).collect::<Vec<_>>();

    let mut shell_execute_info = winapi::um::shellapi::SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<winapi::um::shellapi::SHELLEXECUTEINFOW>() as u32,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: parameters.as_ptr(),
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NO_CONSOLE,
        ..Default::default()
    };

    unsafe {
        ShellExecuteExW(&mut shell_execute_info);

        let err = GetLastError();
        if err == ERROR_CANCELLED {
            return Err(NativeError::Elevation(file1));
        }
    }

    Ok(())
}

#[cfg(windows)]
pub fn set_up_registry() -> Result<(), RegistryError> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let (origin, _) =
        hklm.create_subkey_with_flags(format!("{}\\Origin", REG_ARCH_PATH), KEY_WRITE)?;

    let bootstrap_path = &bootstrap_path()?.safe_str()?.to_string();
    origin.set_value("ClientPath", bootstrap_path)?;

    let (eax_32, _) = hklm.create_subkey_with_flags(REG_EAX32_PATH, KEY_WRITE)?;
    eax_32.set_value("InstallSuccessful", &"true")?;

    // Hijack Qt's protocol for our login redirection
    register_custom_protocol("qrc", "Maxima Protocol", bootstrap_path)?;

    // These are disabled until properly implemented in bootstrap. Epic/Steam-owned games
    // can be launched directly from Maxima until that's done

    // We link2maxima now
    //register_custom_protocol("link2ea", "Maxima Launcher", bootstrap_path)?;

    // maxima2
    //register_custom_protocol("origin2", "Maxima Launcher", bootstrap_path)?;

    Ok(())
}

#[cfg(windows)]
fn register_custom_protocol(
    protocol: &str,
    name: &str,
    executable: &str,
) -> Result<(), RegistryError> {
    let hkcr = RegKey::predef(HKEY_CLASSES_ROOT);
    let (protocol, _) = hkcr.create_subkey_with_flags(protocol, KEY_WRITE)?;

    protocol.set_value("", &format!("URL:{}", name))?;
    protocol.set_value("URL Protocol", &"")?;

    let (command, _) = protocol.create_subkey_with_flags("shell\\open\\command", KEY_WRITE)?;
    command.set_value("", &format!("\"{}\" \"%1\"", executable))?;

    Ok(())
}

#[cfg(target_os = "linux")]
pub fn set_up_registry() -> Result<(), RegistryError> {
    let bootstrap_path = &bootstrap_path()?.safe_str()?.to_string();

    // Hijack Qt's protocol for our login redirection
    register_custom_protocol("qrc", "Maxima Launcher", bootstrap_path)?;

    Ok(())
}

#[cfg(target_os = "macos")]
pub fn set_up_registry() -> Result<(), RegistryError> {
    use std::process::Command;

    use log::warn;

    let bin = bootstrap_path()?;

    if !bin.try_exists()? {
        warn!(
            "{} does not exist. Did you run `cargo bundle` for `maxima-bootstrap`?",
            bin.display()
        );
    }

    Command::new(bin).arg("--noop").spawn()?;

    Ok(())
}

#[cfg(target_os = "linux")]
fn register_custom_protocol(
    protocol: &str,
    name: &str,
    executable: &str,
) -> Result<(), RegistryError> {
    if env::var("MAXIMA_PACKAGED").is_ok_and(|var| var == "1") {
        return Ok(());
    }

    use crate::util::native::maxima_dir;

    let mut parts = HashMap::<&str, String>::new();
    parts.insert("Type", "Application".to_owned());
    parts.insert("Name", name.to_owned());
    parts.insert("MimeType", format!("x-scheme-handler/{}", protocol));
    parts.insert("Exec", format!("{} %u", executable));
    parts.insert("NoDisplay", "true".to_owned());
    parts.insert("StartupNotify", "true".to_owned());

    let mut desktop_file = String::from("[Desktop Entry]\n");
    for part in parts {
        desktop_file += &(part.0.to_owned() + "=" + &part.1 + "\n");
    }

    let maxima_dir = maxima_dir()?;
    let home = maxima_dir.safe_parent()?;
    let desktop_file_name = format!("maxima-{}.desktop", protocol);
    let desktop_file_path = format!("{}/applications/{}", home.safe_str()?, desktop_file_name);
    fs::write(desktop_file_path, desktop_file)?;

    set_mime_type(
        &format!("x-scheme-handler/{}", protocol),
        &desktop_file_name,
    )?;

    Ok(())
}

#[cfg(target_os = "linux")]
fn set_mime_type(mime_type: &str, desktop_file_path: &str) -> Result<(), RegistryError> {
    use std::process::Command;

    let xdg_mime_check = Command::new("xdg-mime").arg("--version").output();
    if xdg_mime_check.is_err() {
        return Err(RegistryError::XdgMime);
    }

    let output = Command::new("xdg-mime")
        .arg("default")
        .arg(desktop_file_path)
        .arg(mime_type)
        .output()?;

    if !output.status.success() {
        return Err(RegistryError::MimeSet {
            r#type: mime_type.to_string(),
            error: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }

    Ok(())
}

#[cfg(unix)]
pub fn check_registry_validity() -> Result<(), RegistryError> {
    if env::var("MAXIMA_DISABLE_QRC").is_ok() {
        return Ok(());
    }

    // Packaged builds register the qrc handler via the installer (under a
    // distro-specific .desktop name), so the in-app check would always fail
    // the hardcoded-name compare and trigger a no-op re-registration each
    // start. Trust the installer when packaged.
    if env::var("MAXIMA_PACKAGED").is_ok_and(|var| var == "1") {
        return Ok(());
    }

    if !verify_protocol_handler("qrc")? {
        return Err(RegistryError::QrcUnregistered);
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_protocol_handler(protocol: &str) -> Result<bool, RegistryError> {
    use std::process::Command;

    let output = Command::new("xdg-mime")
        .arg("query")
        .arg("default")
        .arg(format!("x-scheme-handler/{}", protocol))
        .output()?;

    if !output.status.success() {
        return Err(RegistryError::XdgQueryFailed);
    }

    let output_str = String::from_utf8_lossy(&output.stdout);
    if output_str.is_empty() {
        return Ok(false);
    }

    let expected = format!("maxima-{}.desktop\n", protocol);
    Ok(output_str == expected)
}

#[cfg(target_os = "macos")]
fn verify_protocol_handler(protocol: &str) -> Result<bool, RegistryError> {
    use std::process::Command;

    let output = Command::new("open")
        .args([&format!("{}://", protocol), "--args", "--noop"])
        .output()?;

    Ok(output.status.success())
}

#[cfg(target_os = "linux")]
pub fn read_game_path(name: &str) -> Result<PathBuf, RegistryError> {
    // No EA Games registry on Linux. Resolve the install via Steam
    // metadata instead. Only BF2 supported for now.
    let appid = slug_to_steam_appid(name).ok_or_else(|| {
        RegistryError::Key(format!(
            "Unsupported game slug for Linux Steam detection: `{}`",
            name
        ))
    })?;

    for steam_root in steam_root_candidates() {
        let vdf_path = steam_root
            .join("steamapps")
            .join("libraryfolders.vdf");
        let vdf_text = match fs::read_to_string(&vdf_path) {
            Ok(t) => t,
            Err(_) => continue,
        };

        for library in parse_libraryfolders_vdf(&vdf_text) {
            let acf_path = library
                .join("steamapps")
                .join(format!("appmanifest_{}.acf", appid));
            let acf_text = match fs::read_to_string(&acf_path) {
                Ok(t) => t,
                Err(_) => continue,
            };

            if let Some(installdir) = parse_appmanifest_acf(&acf_text) {
                let game_path = library
                    .join("steamapps")
                    .join("common")
                    .join(&installdir);
                if game_path.exists() {
                    return Ok(case_insensitive_path(game_path));
                }
            }
        }
    }

    Err(RegistryError::Key(format!(
        "No Steam installation found for app id {} (slug `{}`)",
        appid, name
    )))
}

#[cfg(target_os = "macos")]
pub fn read_game_path(_name: &str) -> Result<PathBuf, RegistryError> {
    todo!("read_game_path is not implemented on macOS");
}

#[cfg(target_os = "linux")]
fn slug_to_steam_appid(slug: &str) -> Option<u32> {
    let normalized: String = slug
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect();
    match normalized.as_str() {
        "bf2"
        | "swbf2"
        | "starwarsbattlefrontii"
        | "starwarsbattlefront2"
        | "starwarsbattlefrontii(2017)" => Some(1237950),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn steam_root_candidates() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    // 1. Manual override.
    if let Some(custom) = env::var_os("STEAM_LIBRARY_ROOT") {
        roots.push(PathBuf::from(custom));
    }

    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        // 2. Flatpak Steam (com.valvesoftware.Steam).
        roots.push(
            home.join(".var/app/com.valvesoftware.Steam/.steam/steam"),
        );
        // 3. Native Steam (default).
        roots.push(home.join(".steam/steam"));
        // 4. Snap-style or alternate Steam path.
        roots.push(home.join(".local/share/Steam"));
    }

    // 5. User-configured external library root mirrored from
    // linux_setup.rs hardcoded fallback.
    roots.push(PathBuf::from("/mnt/Games/SteamLibrary"));

    roots
}

/// Extracts every `"path" "<dir>"` value from a Steam
/// `libraryfolders.vdf`. Each Steam library has an entry like:
///
/// ```text
/// "libraryfolders"
/// {
///     "0"
///     {
///         "path"          "/home/user/.steam/steam"
///         "label"         ""
///         ...
///     }
/// }
/// ```
#[cfg(target_os = "linux")]
pub(crate) fn parse_libraryfolders_vdf(text: &str) -> Vec<PathBuf> {
    use regex::Regex;
    lazy_static::lazy_static! {
        static ref PATH_RE: Regex =
            Regex::new(r#""path"\s+"((?:[^"\\]|\\.)*)""#).unwrap();
    }
    PATH_RE
        .captures_iter(text)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str()))
        .map(|raw| raw.replace("\\\\", "\\"))
        .map(PathBuf::from)
        .collect()
}

/// Extracts the `installdir` value from a Steam appmanifest .acf file,
/// e.g. `appmanifest_1237950.acf`.
#[cfg(target_os = "linux")]
fn parse_appmanifest_acf(text: &str) -> Option<String> {
    use regex::Regex;
    lazy_static::lazy_static! {
        static ref INSTALLDIR_RE: Regex =
            Regex::new(r#""installdir"\s+"((?:[^"\\]|\\.)*)""#).unwrap();
    }
    INSTALLDIR_RE
        .captures(text)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str().replace("\\\\", "\\"))
}

#[cfg(unix)]
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn read_game_path(_name: &str) -> Result<PathBuf, RegistryError> {
    todo!("Cannot read game path on this unix variant");
}

#[cfg(target_os = "linux")]
pub fn bootstrap_path() -> Result<PathBuf, NativeError> {
    // Launcher FFI (kyber_launcher in bundle/) points /proc/self/exe at
    // bundle/, bootstrap sits in bundle/cli/. CLI (kyber_cli in
    // bundle/cli/) has bootstrap right next to it.
    let exe = module_path()?;
    let base = exe.safe_parent()?;
    let cli_subdir = base.join("cli").join("maxima-bootstrap");
    if cli_subdir.exists() {
        return Ok(cli_subdir);
    }
    Ok(base.join("maxima-bootstrap"))
}

#[cfg(target_os = "macos")]
pub fn bootstrap_path() -> Result<PathBuf, NativeError> {
    Ok(module_path()?
        .safe_parent()?
        .join("bundle")
        .join("osx")
        .join("MaximaBootstrap.app")
        .join("Contents")
        .join("MacOS")
        .join("maxima-bootstrap"))
}

#[cfg(unix)]
pub fn launch_bootstrap() -> Result<(), RegistryError> {
    todo!()
}

#[cfg(all(test, target_os = "linux"))]
mod linux_steam_detection_tests {
    use super::{
        parse_appmanifest_acf, parse_libraryfolders_vdf, slug_to_steam_appid,
    };
    use std::path::PathBuf;

    #[test]
    fn slug_maps_battlefront_variants_to_appid() {
        assert_eq!(slug_to_steam_appid("bf2"), Some(1237950));
        assert_eq!(slug_to_steam_appid("BF2"), Some(1237950));
        assert_eq!(slug_to_steam_appid("swbf2"), Some(1237950));
        assert_eq!(
            slug_to_steam_appid("STAR WARS Battlefront II"),
            Some(1237950)
        );
        assert_eq!(
            slug_to_steam_appid("starwarsbattlefront2"),
            Some(1237950)
        );
    }

    #[test]
    fn slug_rejects_unknown_games() {
        assert_eq!(slug_to_steam_appid(""), None);
        assert_eq!(slug_to_steam_appid("apex"), None);
        assert_eq!(slug_to_steam_appid("battlefield-1"), None);
    }

    #[test]
    fn parses_libraryfolders_vdf_with_multiple_entries() {
        let vdf = r#"
"libraryfolders"
{
    "0"
    {
        "path"      "/home/user/.local/share/Steam"
        "label"     ""
        "totalsize"     "0"
    }
    "1"
    {
        "path"      "/mnt/Games/SteamLibrary"
        "label"     ""
        "totalsize"     "1000000"
    }
}
"#;
        let libs = parse_libraryfolders_vdf(vdf);
        assert_eq!(
            libs,
            vec![
                PathBuf::from("/home/user/.local/share/Steam"),
                PathBuf::from("/mnt/Games/SteamLibrary"),
            ]
        );
    }

    #[test]
    fn parses_libraryfolders_vdf_returns_empty_for_garbage() {
        let libs = parse_libraryfolders_vdf("not a vdf at all");
        assert!(libs.is_empty());
    }

    #[test]
    fn parses_appmanifest_acf_extracts_installdir() {
        let acf = r#"
"AppState"
{
    "appid"             "1237950"
    "name"              "STAR WARS Battlefront II"
    "installdir"        "STAR WARS Battlefront II"
    "LastUpdated"       "1700000000"
}
"#;
        assert_eq!(
            parse_appmanifest_acf(acf),
            Some("STAR WARS Battlefront II".to_string())
        );
    }

    #[test]
    fn parses_appmanifest_acf_returns_none_when_missing() {
        let acf = r#"
"AppState"
{
    "appid"     "1237950"
    "name"      "STAR WARS Battlefront II"
}
"#;
        assert_eq!(parse_appmanifest_acf(acf), None);
    }
}

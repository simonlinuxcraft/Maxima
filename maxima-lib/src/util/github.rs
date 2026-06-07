use std::path::PathBuf;
use std::sync::Mutex;

use std::io::Read;

use crate::util::native::DownloadError;
use lazy_static::lazy_static;
use log::info;
use reqwest::StatusCode;
use serde::Deserialize;

lazy_static! {
    // MAXIMA-LINUX-PORT-MOD 2026-06-07: latest in-flight download progress,
    // published so the launcher UI can show a percentage during the otherwise
    // silent ~516MB GE-Proton download (the launch flow blocks on this and the
    // window looks frozen). Holds (label, downloaded_bytes, total_bytes) while
    // a download is running, None when idle. Read via download_progress().
    static ref DOWNLOAD_PROGRESS: Mutex<Option<(String, u64, u64)>> = Mutex::new(None);
}

fn set_download_progress(label: &str, downloaded: u64, total: u64) {
    if let Ok(mut p) = DOWNLOAD_PROGRESS.lock() {
        *p = Some((label.to_string(), downloaded, total));
    }
}

fn clear_download_progress() {
    if let Ok(mut p) = DOWNLOAD_PROGRESS.lock() {
        *p = None;
    }
}

/// Snapshot of the current download progress as (label, downloaded_bytes,
/// total_bytes), or None when no download is in flight. Consumed by the
/// launcher's get_proton_download_progress FFI stream.
pub fn download_progress() -> Option<(String, u64, u64)> {
    DOWNLOAD_PROGRESS.lock().ok().and_then(|p| p.clone())
}

#[derive(Deserialize)]
pub struct GithubAsset {
    pub name: String,
    pub size: u64,
    pub browser_download_url: String,
}

#[derive(Deserialize)]
pub struct GithubRelease {
    pub tag_name: String,
    pub assets: Vec<GithubAsset>,
}

pub fn fetch_github_releases(
    author: &str,
    repository: &str,
) -> Result<Vec<GithubRelease>, DownloadError> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases",
        author, repository
    );

    let res = ureq::get(&url)
        .set("User-Agent", "ArmchairDevelopers/Maxima")
        .call()?;
    if res.status() != StatusCode::OK {
        return Err(DownloadError::Http(res.into_string()?));
    }

    let text = res.into_string()?;
    let result = serde_json::from_str(text.as_str())?;
    Ok(result)
}

pub fn fetch_github_release(
    author: &str,
    repository: &str,
    version: &str,
) -> Result<GithubRelease, DownloadError> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases/{}",
        author, repository, version
    );

    let res = ureq::get(&url)
        .set("User-Agent", "ArmchairDevelopers/Maxima")
        .call()?;
    if res.status() != StatusCode::OK {
        return Err(DownloadError::Http(res.into_string()?));
    }

    let text = res.into_string()?;
    let result = serde_json::from_str(text.as_str())?;
    Ok(result)
}

pub fn github_download_asset(asset: &GithubAsset, path: &PathBuf) -> Result<(), DownloadError> {
    // MAXIMA-LINUX-PORT-MOD 2026-06-07: resumable, streamed download.
    // The old implementation read the whole asset (GE-Proton ~516MB) into a
    // RAM Vec and only wrote to disk at the very end. On Steam Deck / slow
    // links the launch flow blocks here with no progress and, if the user
    // closes the seemingly-frozen window, nothing is persisted, so the next
    // Play press re-downloads from byte 0 forever (the proton version is only
    // recorded after extract). This streams to disk in 1MB chunks and resumes
    // a partially-downloaded file via an HTTP Range request, so interrupted
    // downloads continue instead of restarting.
    download_with_resume(&asset.browser_download_url, asset.size, path, &asset.name)
}

fn download_with_resume(
    url: &str,
    expected_size: u64,
    path: &PathBuf,
    label: &str,
) -> Result<(), DownloadError> {
    // Bytes already on disk from a previous interrupted attempt.
    let mut resume_from = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    // A leftover file already at the expected size is complete; larger means
    // corrupt, so restart from scratch in that case.
    if expected_size > 0 && resume_from >= expected_size {
        if resume_from == expected_size {
            return Ok(());
        }
        resume_from = 0;
    }

    // Bracket the actual work with set/clear so progress is published for the
    // UI during the download and never lingers afterwards, regardless of which
    // `?`/early-return path the inner function takes.
    set_download_progress(label, resume_from, expected_size);
    let result = download_inner(url, expected_size, path, label, resume_from);
    clear_download_progress();
    result
}

fn download_inner(
    url: &str,
    expected_size: u64,
    path: &PathBuf,
    label: &str,
    resume_from: u64,
) -> Result<(), DownloadError> {
    use std::fs::OpenOptions;
    use std::io::{BufWriter, Seek, SeekFrom};

    info!(
        "Downloading {}... ({} MB total, resuming from {} MB)",
        label,
        expected_size / 1_048_576,
        resume_from / 1_048_576,
    );

    let req = ureq::get(url).set("User-Agent", "ArmchairDevelopers/Maxima");
    let req = if resume_from > 0 {
        req.set("Range", &format!("bytes={}-", resume_from))
    } else {
        req
    };

    let res = match req.call() {
        Ok(res) => res,
        // 416: requested range past EOF => file already fully downloaded.
        Err(ureq::Error::Status(416, _)) if resume_from > 0 => return Ok(()),
        Err(err) => return Err(DownloadError::Request1(err)),
    };

    // Decide append-vs-restart from the actual response status, NOT from the
    // assumption that the GitHub 302 redirect forwarded our Range header. If
    // the server returns 206 it honoured the range (append); a 200 means it
    // sent the whole body (overwrite from byte 0). This stays correct whether
    // or not Range survives the redirect to release-assets.githubusercontent.com.
    let status = res.status();
    let append = status == 206;
    if status != 200 && status != 206 {
        return Err(DownloadError::Http(format!("{} {}", status, label)));
    }

    let file = OpenOptions::new().create(true).write(true).open(path)?;
    let start = if append { resume_from } else { 0 };
    if append {
        file.set_len(resume_from)?;
        let mut f = file;
        f.seek(SeekFrom::Start(resume_from))?;
        stream_to_writer(res, BufWriter::with_capacity(1 << 20, f), label, start, expected_size)?;
    } else {
        file.set_len(0)?;
        stream_to_writer(res, BufWriter::with_capacity(1 << 20, file), label, start, expected_size)?;
    }

    // Verify the final size; a short file means the connection dropped. Leave
    // the partial file in place so the next attempt resumes, but surface an
    // error so the launch flow does not proceed to extract a truncated archive.
    if expected_size > 0 {
        let final_size = std::fs::metadata(path)?.len();
        if final_size != expected_size {
            return Err(DownloadError::Http(format!(
                "{}: incomplete download ({} of {} bytes), will resume on retry",
                label, final_size, expected_size
            )));
        }
    }

    Ok(())
}

fn stream_to_writer<W: std::io::Write>(
    res: ureq::Response,
    mut writer: W,
    label: &str,
    start_offset: u64,
    total: u64,
) -> Result<(), DownloadError> {
    let mut reader = res.into_reader();
    let mut buf = vec![0u8; 1 << 20];
    let mut written: u64 = start_offset;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        written += n as u64;
        set_download_progress(label, written, total);
    }
    writer.flush()?;
    Ok(())
}

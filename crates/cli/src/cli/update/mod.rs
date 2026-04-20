use std::fs::remove_file;
use std::io::Read;

use flate2::read::GzDecoder;
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use self_replace::self_replace;
use serde::Deserialize;
use tar::Archive;

#[derive(Deserialize, Debug)]
struct LatestRelease {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize, Debug)]
struct Asset {
    name: String,
    browser_download_url: String,
}

pub async fn handle_update_command() -> Result<(), String> {
    let client = reqwest::Client::new();
    let latest_version: LatestRelease = client
        .get("https://api.github.com/repos/solana-foundation/surfpool/releases/latest")
        .header(reqwest::header::USER_AGENT, "surfpool-cli")
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json::<LatestRelease>()
        .await
        .map_err(|e| e.to_string())?;
    let latest_tag_name = latest_version.tag_name.trim_start_matches('v');
    let current_version: &str = env!("CARGO_PKG_VERSION");
    println!("Latest version: {}", latest_tag_name);
    let users_asset = get_asset_name()?;
    let browser_download_url = latest_version
        .assets
        .iter()
        .find(|a| a.name == users_asset)
        .map(|a| a.browser_download_url.as_str())
        .ok_or_else(|| format!("No asset name found matching the users platform"))?;
    println!("Current version: {}", current_version);
    println!("Download URL: {}", browser_download_url);

    if current_version == latest_tag_name {
        println!("Already on the latest version {}", latest_tag_name);
        return Ok(());
    }
    let response = client
        .get(browser_download_url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let total_size = response.content_length().unwrap_or(0);
    let progress_bar = ProgressBar::new(total_size);
    progress_bar.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
            .unwrap()
            .progress_chars("#>-"),
    );

    let mut download: Vec<u8> = Vec::with_capacity(total_size as usize);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        download.extend_from_slice(&chunk);
        progress_bar.set_position(download.len() as u64);
    }
    progress_bar.finish_with_message("Download complete");

    let gz = GzDecoder::new(download.as_slice());
    let mut archive = Archive::new(gz);
    let mut binary_data: Option<Vec<u8>> = None;
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?;
        if path.file_name().and_then(|n| n.to_str()) == Some("surfpool") {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            binary_data = Some(buf);
            break;
        }
    }

    let binary_data = binary_data.ok_or("Could not find 'surfpool' binary in archive")?;

    let temp = std::env::temp_dir().join("surfpool-update");
    std::fs::write(&temp, &binary_data).map_err(|e| e.to_string())?;
    self_replace(&temp).map_err(|e| e.to_string())?;
    remove_file(&temp).ok();
    println!(
        "Surfpool updated from {} to {}",
        current_version, latest_tag_name
    );
    Ok(())
}

fn get_asset_name() -> Result<String, String> {
    let users_os = std::env::consts::OS;
    let users_arch = std::env::consts::ARCH;

    match (users_os, users_arch) {
        ("macos", "aarch64") => Ok("surfpool-darwin-arm64.tar.gz".into()),
        ("macos", "x86_64") => Ok("surfpool-darwin-x64.tar.gz".into()),
        ("linux", "x86_64") => Ok("surfpool-linux-x64.tar.gz".into()),
        _ => Err(format!("Unsupported platform: {users_os}-{users_arch}")),
    }
}

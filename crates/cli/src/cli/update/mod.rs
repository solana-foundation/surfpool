use std::{fs::remove_file, io::Read};

use dialoguer::{Confirm, console::Style, theme::ColorfulTheme};
use flate2::read::GzDecoder;
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use self_replace::self_replace;
use semver::Version;
use serde::Deserialize;
use tar::Archive;

use crate::cli::UpdateCommand;

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

pub async fn handle_update_command(cmd: UpdateCommand) -> Result<(), String> {
    let client = reqwest::Client::new();
    let release_url = match &cmd.version {
        Some(version) => format!(
            "https://api.github.com/repos/solana-foundation/surfpool/releases/tags/v{}",
            version
        ),
        None => {
            "https://api.github.com/repos/solana-foundation/surfpool/releases/latest".to_string()
        }
    };
    let latest_version: LatestRelease = client
        .get(release_url)
        .header(reqwest::header::USER_AGENT, "surfpool-cli")
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json::<LatestRelease>()
        .await
        .map_err(|e| e.to_string())?;
    let latest_tag_name = latest_version.tag_name.trim_start_matches('v');
    let current_version: &str = env!("CARGO_PKG_VERSION");
    let current_semver = Version::parse(current_version)
        .map_err(|e| format!("Failed to parse current version: {e}"))?;
    let target_semver = Version::parse(latest_tag_name)
        .map_err(|e| format!("Failed to parse target version: {e}"))?;
    let users_asset = get_asset_name()?;
    let browser_download_url = latest_version
        .assets
        .iter()
        .find(|a| a.name == users_asset)
        .map(|a| a.browser_download_url.as_str())
        .ok_or_else(|| {
            format!(
                "No asset name found matching the user's platform: {}",
                users_asset
            )
        })?;

    if current_semver == target_semver {
        println!("Already on the latest version {}", current_semver);
        return Ok(());
    }

    if cmd.version.is_none() && current_semver > target_semver {
        println!("Already on the latest version {}", current_semver);
        return Ok(());
    }

    if !cmd.skip_confirm {
        let theme = ColorfulTheme {
            defaults_style: Style::new().for_stderr(),
            ..ColorfulTheme::default()
        };

        let confirm = Confirm::with_theme(&theme)
            .with_prompt(format!(
                "Update surfpool from {} to {}",
                current_version, latest_tag_name
            ))
            .default(true)
            .interact()
            .map_err(|e| format!("Failed to read confirmation: {e}"))?;

        if !confirm {
            println!("Update cancelled");
            return Ok(());
        }
    }
    println!("Download URL: {}", browser_download_url);
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
        if path.file_name().and_then(|n| n.to_str()) == Some(get_binary_name()) {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            binary_data = Some(buf);
            break;
        }
    }

    let binary_data = binary_data
        .ok_or_else(|| format!("Could not find '{}' binary in archive", get_binary_name()))?;

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
        ("windows", "x86_64") => Ok("surfpool-windows-x64.tar.gz".into()),
        _ => Err(format!("Unsupported platform: {users_os}-{users_arch}")),
    }
}

fn get_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "surfpool.exe"
    } else {
        "surfpool"
    }
}

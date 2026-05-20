use std::{fs::remove_file, io::Read};

use dialoguer::{Confirm, console::Style, theme::ColorfulTheme};
use flate2::read::GzDecoder;
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use self_replace::self_replace;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
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
    /// SHA256 digest computed by GitHub server-side at upload time. Format is
    /// `sha256:<hex>`. Older releases or third-party API responses may omit it.
    #[serde(default)]
    digest: Option<String>,
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
    let asset = latest_version
        .assets
        .iter()
        .find(|a| a.name == users_asset)
        .ok_or_else(|| {
            format!(
                "No asset name found matching the user's platform: {}",
                users_asset
            )
        })?;
    let browser_download_url = asset.browser_download_url.as_str();

    if current_semver == target_semver {
        println!("Already on the latest version {}", current_semver);
        return Ok(());
    }

    if cmd.version.is_none() && current_semver > target_semver {
        println!("Already on the latest version {}", current_semver);
        return Ok(());
    }

    let expected_digest: Option<[u8; 32]> = match &asset.digest {
        None => {
            eprintln!(
                "Warning: release asset {users_asset} has no checksum, so the integrity of the release cannot be verified"
            );
            None
        }
        Some(d) => match parse_sha256_digest(d) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                eprintln!("Warning: {e}; the integrity of the release cannot be verified");
                None
            }
        },
    };

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

    // Roots trust in api.github.com's TLS: the digest comes from the same
    // API response as browser_download_url. Stronger guarantees (signed
    // SHASUMS, GitHub Attestations) tracked in #673.
    if let Some(expected) = expected_digest {
        let actual = Sha256::digest(&download);
        if actual.as_slice() != expected {
            return Err(format!(
                "Checksum verification failed for {}.\n  expected: sha256:{}\n  actual:   sha256:{}",
                users_asset,
                hex::encode(expected),
                hex::encode(actual),
            ));
        }
    }

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

/// Parses a GitHub release asset digest string of the form `sha256:<hex>` into
/// the raw 32-byte hash. Returns an error for unknown algorithms, non-hex
/// content, or hex that does not decode to 32 bytes.
fn parse_sha256_digest(digest: &str) -> Result<[u8; 32], String> {
    let hex_part = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| format!("unsupported digest algorithm (expected sha256:...): {digest}"))?;
    let bytes =
        hex::decode(hex_part).map_err(|e| format!("invalid hex in digest {digest}: {e}"))?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| format!("digest {digest} decodes to {} bytes, expected 32", v.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sha256_digest_accepts_valid_input() {
        let expected = [0xabu8; 32];
        let digest = format!("sha256:{}", hex::encode(expected));
        assert_eq!(parse_sha256_digest(&digest).unwrap(), expected);
    }

    #[test]
    fn parse_sha256_digest_rejects_unknown_algorithm() {
        let err = parse_sha256_digest("sha512:00").unwrap_err();
        assert!(err.contains("unsupported digest algorithm"), "{err}");
    }

    #[test]
    fn parse_sha256_digest_rejects_wrong_length() {
        let err = parse_sha256_digest("sha256:abcd").unwrap_err();
        assert!(err.contains("expected 32"), "{err}");
    }

    #[test]
    fn parse_sha256_digest_rejects_non_hex() {
        let err = parse_sha256_digest("sha256:zzzz").unwrap_err();
        assert!(err.contains("invalid hex"), "{err}");
    }

    #[test]
    fn round_trip_hash_matches_parsed_digest() {
        let payload = b"surfpool-test-payload";
        let hash = Sha256::digest(payload);
        let digest = format!("sha256:{}", hex::encode(hash));
        let parsed = parse_sha256_digest(&digest).unwrap();
        assert_eq!(parsed, hash.as_slice());
    }
}

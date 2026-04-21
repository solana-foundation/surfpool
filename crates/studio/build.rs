use std::{env, fs, io, path::PathBuf};

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let asset_dir = out_dir.join("surfpool-studio-ui");

    println!("cargo:rerun-if-env-changed=STUDIO_UI_VERSION");
    println!("cargo:rerun-if-env-changed=STUDIO_UI_DIST");
    println!("cargo:warning=------------ Studio Build Script ------------");

    if !asset_dir.join("_next").exists() {
        if let Ok(zip_path) = env::var("STUDIO_UI_DIST") {
            println!(
                "cargo:warning=Using pre-downloaded Studio UI zip from {}",
                zip_path
            );
            let file = fs::File::open(&zip_path).expect("Failed to open STUDIO_UI_DIST zip file");
            let reader = io::BufReader::new(file);
            let mut zip =
                zip::ZipArchive::new(reader).expect("Failed to read STUDIO_UI_DIST zip file");
            zip.extract(&asset_dir)
                .expect("Failed to extract STUDIO_UI_DIST zip");
        } else {
            let url = match env::var("STUDIO_UI_VERSION") {
                Ok(version) => format!(
                    "https://github.com/solana-foundation/surfpool-web-ui/releases/download/{}/studio-dist.zip",
                    version
                ),
                Err(_) => {
                    "https://github.com/solana-foundation/surfpool-web-ui/releases/latest/download/studio-dist.zip"
                        .to_string()
                }
            };

            println!("cargo:warning=Downloading Surfpool Studio UI from {}", url);

            let client = reqwest::blocking::Client::new();
            let resp = client
                .get(&url)
                .header("User-Agent", "surfpool-studio-build")
                .send()
                .expect("Failed to download dist zip");

            if !resp.status().is_success() {
                panic!(
                    "Failed to download studio UI: HTTP {} from {}",
                    resp.status(),
                    url
                );
            }

            let reader = std::io::Cursor::new(resp.bytes().unwrap());
            let mut zip = zip::ZipArchive::new(reader).unwrap();

            zip.extract(&asset_dir).expect("Failed to extract zip");
        }
    } else {
        println!(
            "cargo:warning=Studio assets already found at {}",
            asset_dir.display()
        );
    }
}

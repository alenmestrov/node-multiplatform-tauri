use eyre::{bail, Result};
use flate2::read::GzDecoder;
use reqwest::get;
use shared_utils::determine_bin_data;
use std::fs;
use std::fs::File;
use std::io::copy;
use std::io::BufReader;
use std::io::Read;
use std::path::Path;
use tar::Archive;
use eyre::anyhow;

fn main() {
    tauri::async_runtime::block_on(setup_binary()).unwrap();
    tauri_build::build()
}

async fn setup_binary() -> Result<()> {
    let (os, arch, target) = determine_bin_data();
    let binary_name = "merod";
    let cache_dir = std::env::temp_dir().join(binary_name);
    std::fs::create_dir_all(&cache_dir).expect("Failed to create cache directory");

    // Get the latest merod release tag
    let latest_release = get_latest_merod_release().await?;
    
    let url = format!(
        "https://github.com/calimero-network/core/releases/download/{}/{}.tar.gz",
        latest_release,
        target
    );
    println!("Downloading from URL: {}", url);

    let cache_bin_path = cache_dir.join(format!("{}.tar.gz", binary_name));
    let bin_dir = std::env::current_dir()?.join("bin").join(os).join(arch);
    let resource_path = bin_dir.join(binary_name);

    if resource_path.exists() {
        return Ok(());
    }

    download_and_extract(&url, &cache_bin_path, &bin_dir).await?;

    Ok(())
}

async fn download_and_extract(url: &str, cache_bin_path: &Path, bin_dir: &Path) -> Result<()> {
    let response = get(url).await.expect("Failed to download binary");
    let mut out = File::create(cache_bin_path).expect("Failed to create file");
    copy(&mut response.bytes().await.unwrap().as_ref(), &mut out).expect("Failed to copy content");

    let tar_gz = File::open(cache_bin_path).expect("Failed to open downloaded file");
    let mut buf_reader = BufReader::new(tar_gz);
    let mut header = [0; 2];
    buf_reader
        .read_exact(&mut header)
        .expect("Failed to read file header");

    if &header != b"\x1f\x8b" {
        bail!("Invalid gzip header for file: {}", cache_bin_path.display());
    }

    let tar_gz = File::open(cache_bin_path).expect("Failed to open .gz file");
    let tar = GzDecoder::new(tar_gz);
    let mut archive = Archive::new(tar);

    fs::create_dir_all(bin_dir).expect("Failed to create directories");
    archive.unpack(bin_dir).expect("Failed to unpack archive");

    Ok(())
}

async fn get_latest_merod_release() -> Result<String> {
    // First try without authentication
    let client = reqwest::Client::new();
    let response = client
        .get("https://api.github.com/repos/calimero-network/core/releases/latest")  // Use /latest instead of fetching all
        .header("User-Agent", "request")
        .send()
        .await?;

    if !response.status().is_success() {
        // If that fails, try with authentication
        let github_token = std::env::var("GITHUB_TOKEN")
            .unwrap_or_else(|_| "".to_string());

        let response = client
            .get("https://api.github.com/repos/calimero-network/core/releases/latest")
            .header("User-Agent", "request")
            .header("Authorization", format!("Bearer {}", github_token))
            .send()
            .await?;

        if !response.status().is_success() {
            // If both methods fail, try a direct approach
            return Ok("merod-0.2.0".to_string());  // Fallback to known working version
        }
    }

    let release: serde_json::Value = response.json().await?;
    let tag_name = release["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow!("Invalid tag name"))?;

    if !tag_name.starts_with("merod") {
        // If the latest release isn't a merod release, fall back to known version
        return Ok("merod-0.2.0".to_string());
    }

    Ok(tag_name.to_string())
}

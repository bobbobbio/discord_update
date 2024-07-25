use futures::stream::TryStreamExt as _;
use indicatif::{MultiProgress, ProgressBar};
use semver::Version;
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr};
use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::tempdir;
use tokio::process::Command;
use tokio_util::compat::FuturesAsyncReadCompatExt as _;

type Error = Box<dyn std::error::Error + 'static>;
type Result<T> = std::result::Result<T, Error>;

#[serde_as]
#[derive(Deserialize)]
struct VersionPayload {
    #[serde(alias = "name")]
    #[serde_as(as = "DisplayFromStr")]
    version: Version,
}

/// Run a bash script
async fn bash(s: &str) -> Result<String> {
    let output = Command::new("/bin/bash").arg("-c").arg(s).output().await?;
    if !output.status.success() {
        Err(format!("script failed: {s:?}").into())
    } else {
        Ok(String::from_utf8(output.stdout)?)
    }
}

/// Get latest discord version from the internet
async fn get_latest_discord_version() -> Result<Version> {
    let r: VersionPayload = reqwest::get("https://discord.com/api/updates/stable?platform=linux")
        .await?
        .json()
        .await?;
    Ok(r.version)
}

/// Discover the path to the currently installed discord
async fn locate_installed_discord() -> Result<PathBuf> {
    let install_path = PathBuf::from(
        bash("source ~/.profile ~/.bashrc ~/.zshrc; which discord")
            .await?
            .trim(),
    );
    Ok(tokio::fs::canonicalize(&install_path)
        .await?
        .parent()
        .ok_or_else(|| Error::from("bad discord install path"))?
        .into())
}

/// Find the version of discord installed at the given path
async fn get_installed_version(install_path: &Path) -> Result<Version> {
    let current_version =
        tokio::fs::read_to_string(install_path.join("resources/build_info.json")).await?;
    let current_version: VersionPayload = serde_json::from_str(&current_version)?;
    Ok(current_version.version)
}

/// Extract a tar file
async fn tar_xf(tar_path: &Path, dest: &Path) -> Result<()> {
    let mut extract_command = Command::new("tar");
    extract_command
        .arg("-xvf")
        .arg(tar_path)
        .arg("-C")
        .arg(dest)
        .arg("--strip-components=1");
    let output = extract_command.output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8(output.stderr)?;
        return Err(format!("tar -xvf failed: {stderr}").into());
    }
    Ok(())
}

/// Download the latest version of discord and extract at given path
async fn update_discord(
    multi_prog: &MultiProgress,
    spinner: &ProgressBar,
    install_path: &Path,
    version: Version,
) -> Result<()> {
    let temp_dir = tempdir()?;
    let download_url =
        format!("https://dl.discordapp.net/apps/linux/{version}/discord-{version}.tar.gz");
    let download_path = temp_dir
        .path()
        .join(format!("discord-{version}.tar.gz"));

    let resp = reqwest::get(&download_url).await?;
    let download_size = resp.content_length().unwrap_or(0);
    let mut download_stream = resp
        .bytes_stream()
        .map_err(|e| futures::io::Error::new(futures::io::ErrorKind::Other, e))
        .into_async_read()
        .compat();

    let pb = multi_prog.add(ProgressBar::new(download_size));
    let mut download_file = pb.wrap_async_write(tokio::fs::File::create(&download_path).await?);
    tokio::io::copy(&mut download_stream, &mut download_file).await?;
    pb.finish_and_clear();

    // Ensure install path exists
    tokio::fs::create_dir_all(&install_path).await?;

    // Extract the downloaded file
    // Assumes that at this point the discord install path is valid
    spinner.set_message(format!("Extracting Discord to {}", install_path.display()));
    tar_xf(&download_path, &install_path).await?;
    spinner.finish_with_message("Discord extracted");

    Ok(())
}

/// The path to the user's home directory
fn home_dir() -> Result<PathBuf> {
    Ok(PathBuf::from(env::var("HOME")?))
}

/// Place to install discord when there isn't an existing location
fn default_discord_path() -> Result<PathBuf> {
    Ok(home_dir()?.join("bin/discord_bin/Discord/Discord"))
}

/// Create a symlink to the given path at <home>/bin/discord
async fn create_home_bin_symlink(source: &Path) -> Result<()> {
    // create a symlink to the discord binary in the user's bin directory
    let home_dir = home_dir()?;
    let bin_dir = home_dir.join("bin");
    tokio::fs::create_dir_all(&bin_dir).await?;
    tokio::fs::symlink(source, bin_dir.join("discord")).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut install_fresh = false;

    let prog = MultiProgress::new();
    let spinner = prog.add(ProgressBar::new_spinner());
    spinner.enable_steady_tick(Duration::from_millis(100));

    // Locate Discord in the system and get the path or use the default path
    let default_install_path = default_discord_path()?;
    let install_path = locate_installed_discord().await.unwrap_or_else(|_| {
        prog.println("Failed to locate Discord. Will use the default path")
            .unwrap();
        default_install_path
    });
    prog.println(format!(
        "Found discord install at {}",
        install_path.display()
    ))?;

    // Create a new Discord instance
    let latest_version = get_latest_discord_version().await?; // Get the latest version
    let current_version = if tokio::fs::try_exists(&install_path).await? {
        get_installed_version(&install_path).await?
    } else {
        install_fresh = true;
        Version::new(0, 0, 0)
    };
    prog.println(format!("Latest version: {latest_version}"))?;
    prog.println(format!("Current version: {current_version}"))?;

    // Check if the latest version is greater than the current version and update if necessary
    if latest_version > current_version {
        prog.println("Update available")?;
        update_discord(&prog, &spinner, &install_path, latest_version).await?;
    } else {
        prog.println("No update available")?;
    }

    // If we installed it fresh, create a symlink in /home/bin/
    if install_fresh {
        create_home_bin_symlink(&default_discord_path()?).await?;
    }

    Ok(()) // Return Ok if everything is fine
}

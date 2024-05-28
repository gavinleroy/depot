use crate::utils;
#[cfg(unix)]
use std::os::unix::prelude::PermissionsExt;
use std::{
  env,
  fs::{File, Permissions},
  io::{BufWriter, Write},
  path::{Path, PathBuf},
};

use anyhow::{bail, ensure, Context, Result};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};

/// Setup Depot for use on this machine
#[derive(clap::Parser)]
pub struct SetupArgs {
  /// Directory for global Depot configuration, defaults to $HOME/.depot
  #[arg(short, long)]
  pub config_dir: Option<PathBuf>,
}

pub struct SetupCommand {
  args: SetupArgs,
}

#[derive(Clone)]
pub struct GlobalConfig {
  root: PathBuf,
  pnpm_path: PathBuf,
}

const HOME_ENV_VAR: &str = "DEPOT_HOME";

impl GlobalConfig {
  fn find_root() -> Result<PathBuf> {
    match env::var(HOME_ENV_VAR) {
      Ok(val) => Ok(PathBuf::from(val)),
      Err(_) => {
        let home_dir = home::home_dir().context("Could not find home directory")?;
        Ok(home_dir.join(".local"))
      }
    }
  }

  pub fn load() -> Result<Self> {
    let root = Self::find_root()?;
    ensure!(
      root.exists(),
      "Depot global config directory does not exist: {}",
      root.display()
    );

    let pnpm_in_root = root.join("bin").join("pnpm");
    let pnpm_path = if pnpm_in_root.exists() {
      pnpm_in_root
    } else {
      let pnpm_fs_opt = pathsearch::find_executable_in_path("pnpm");
      match pnpm_fs_opt {
        Some(path) => path,
        None => bail!("pnpm is not installed"),
      }
    };

    Ok(GlobalConfig { root, pnpm_path })
  }

  pub fn pnpm_path(&self) -> &Path {
    &self.pnpm_path
  }
}

const PNPM_VERSION: &str = "9.1.1";

async fn download_file(url: &str, mut dst: impl Write) -> Result<()> {
  let res = reqwest::get(url).await?;
  let total_size = res
    .content_length()
    .context("Failed to get content length")?;

  log::debug!("Starting download...");
  let bar = ProgressBar::new(total_size);
  bar.set_style(
    ProgressStyle::with_template(
      "{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes}",
    )
    .unwrap()
    .progress_chars("#>-"),
  );

  let mut stream = res.bytes_stream();
  let mut downloaded: u64 = 0;
  while let Some(chunk) = stream.next().await {
    let chunk = chunk?;
    dst.write_all(&chunk)?;
    let new = (downloaded + (chunk.len() as u64)).min(total_size);
    downloaded = new;
    bar.set_position(new);
  }

  bar.finish();

  Ok(())
}

async fn download_pnpm(dst: &Path) -> Result<()> {
  let version = PNPM_VERSION;
  let platform = match env::consts::OS {
    "macos" | "ios" => "macos",
    "windows" => "win",
    _ => "linuxstatic",
  };
  let arch = match env::consts::ARCH {
    "arm" => "arm64",
    _ => "x64",
  };

  let pnpm_url =
    format!("https://github.com/pnpm/pnpm/releases/download/v{version}/pnpm-{platform}-{arch}");

  let mut file = File::create(dst).context("Could not save pnpm binary to file")?;
  download_file(&pnpm_url, BufWriter::new(&mut file)).await?;

  #[cfg(unix)]
  file.set_permissions(Permissions::from_mode(0o555))?;

  Ok(())
}

impl SetupCommand {
  pub fn new(args: SetupArgs) -> Self {
    SetupCommand { args }
  }

  pub async fn run(self) -> Result<()> {
    let config_dir = match self.args.config_dir {
      Some(dir) => dir,
      None => GlobalConfig::find_root()?,
    };
    utils::create_dir_if_missing(&config_dir)?;

    let config = GlobalConfig {
      root: config_dir,
      pnpm_path: PathBuf::new(),
    };
    let bindir = config.root.join("bin");
    utils::create_dir_if_missing(&bindir)?;

    let pnpm_path = bindir.join("pnpm");
    if !pnpm_path.exists() {
      println!("Downloading pnpm from Github...");
      download_pnpm(&pnpm_path).await?;
    }

    println!("Setup complete!");

    Ok(())
  }
}

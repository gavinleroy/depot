use anyhow::{Context, Result};

use crate::workspace::{Command, CoreCommand, PackageCommand, package::Package};

/// Fix biome issues where possible
#[derive(clap::Parser, Debug)]
pub struct FixArgs {
    /// Additional arguments to pass to prettier
    #[arg(last = true)]
    pub biome_args: Option<String>,
}

#[derive(Debug)]
pub struct FixCommand {
    #[allow(unused)]
    args: FixArgs,
}

impl FixCommand {
    pub fn new(args: FixArgs) -> Self {
        FixCommand { args }
    }

    pub fn kind(self) -> Command {
        Command::package(self)
    }
}

impl CoreCommand for FixCommand {
    fn name(&self) -> String {
        "fix".into()
    }
}

#[async_trait::async_trait]
impl PackageCommand for FixCommand {
    async fn run_pkg(&self, pkg: &Package) -> Result<()> {
        let extra = match &self.args.biome_args {
            Some(args) => shlex::split(args).context("Failed to parse prettier args")?,
            None => Vec::new(),
        };

        let _ = pkg
            .exec("biome", |cmd| {
                cmd.arg("check");
                cmd.arg("--fix");
                cmd.args(pkg.source_files());
                cmd.args(extra);
            })
            .await;
        Ok(())
    }
}

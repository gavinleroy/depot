use self::{
    dep_graph::DepGraph,
    fingerprint::Fingerprints,
    package::{PackageGraph, PackageIndex},
    process::Process,
};
use crate::{CommonArgs, shareable, utils};

use anyhow::{Context, Result, anyhow};
use futures::{
    StreamExt,
    stream::{self, TryStreamExt},
};
use log::{debug, warn};
use manifest::DepotManifest;
use package::Package;
use std::{
    cmp::Ordering,
    env,
    fmt::{self, Debug},
    iter,
    path::{Path, PathBuf},
    sync::{Arc, RwLock, RwLockReadGuard},
};

mod dep_graph;
mod fingerprint;
mod manifest;
pub mod package;
pub mod process;
mod runner;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct WorkspaceDepotConfig {
    pub depot_version: String,
}

pub type WorkspaceManifest = DepotManifest<WorkspaceDepotConfig>;

/// Represents an entire Depot workspace.
///
/// This is a central data structure that is held by many parts of the application,
/// wrapped in an [`Arc`] by [`Workspace`].
pub struct WorkspaceInner {
    /// The root directory of the workspace containing `package.json`.
    pub root: PathBuf,

    /// All the packages in the workspace.
    pub packages: Vec<Package>,

    /// The dependencies between packages.
    pub pkg_graph: PackageGraph,

    /// True if this workspace is structured as a monorepo with a `packages/` directory.
    pub monorepo: bool,

    /// CLI arguments that apply to the whole workspace.
    pub common: CommonArgs,

    roots: Vec<Package>,
    package_display_order: Vec<PackageIndex>,
    processes: RwLock<Vec<Arc<Process>>>,
    fingerprints: RwLock<Fingerprints>,
}

shareable!(Workspace, WorkspaceInner);

fn find_workspace_root(max_ancestor: &Path, cwd: &Path) -> Result<PathBuf> {
    let rel_path_to_cwd = cwd.strip_prefix(max_ancestor).unwrap_or_else(|_| {
        panic!(
            "Internal error: Max ancestor `{}` is not a prefix of cwd `{}`",
            max_ancestor.display(),
            cwd.display()
        )
    });
    let components = rel_path_to_cwd.iter().collect::<Vec<_>>();
    (0..=components.len())
        .map(|i| {
            iter::once(max_ancestor.as_os_str())
                .chain(components[..i].iter().copied())
                .collect::<PathBuf>()
        })
        .find(|path| path.join("package.json").exists())
        .with_context(|| {
            format!(
                "Could not find workspace root in working dir: {}",
                cwd.display()
            )
        })
}

pub enum CommandInner {
    Package(Box<dyn PackageCommand>),
    Workspace(Box<dyn WorkspaceCommand>),
}

impl CommandInner {
    pub fn name(&self) -> String {
        match self {
            CommandInner::Package(cmd) => cmd.name(),
            CommandInner::Workspace(cmd) => cmd.name(),
        }
    }

    pub fn deps(&self) -> Vec<Command> {
        match self {
            CommandInner::Package(cmd) => cmd.deps(),
            CommandInner::Workspace(_) => Vec::new(),
        }
    }
}

impl Command {
    pub async fn run_pkg(self, package: Package) -> Result<()> {
        match &*self {
            CommandInner::Package(cmd) => cmd.run_pkg(&package).await,
            CommandInner::Workspace(_) => panic!("run_pkg on workspace command"),
        }
    }

    pub async fn run_ws(self, ws: Workspace) -> Result<()> {
        match &*self {
            CommandInner::Workspace(cmd) => cmd.run_ws(&ws).await,
            CommandInner::Package(_) => panic!("run_ws on package command"),
        }
    }

    pub fn runtime(&self) -> Option<CommandRuntime> {
        match &**self {
            CommandInner::Package(cmd) => Some(cmd.runtime()),
            CommandInner::Workspace(_) => None,
        }
    }
}

impl fmt::Debug for CommandInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandInner::Package(cmd) => write!(f, "{cmd:?}"),
            CommandInner::Workspace(cmd) => write!(f, "{cmd:?}"),
        }
    }
}

shareable!(Command, CommandInner);

impl Command {
    pub fn package(cmd: impl PackageCommand) -> Self {
        Self::new(CommandInner::Package(Box::new(cmd)))
    }

    pub fn workspace(cmd: impl WorkspaceCommand + 'static) -> Self {
        Self::new(CommandInner::Workspace(Box::new(cmd)))
    }
}

pub trait CoreCommand {
    fn name(&self) -> String;
}

#[derive(Clone, Copy)]
pub enum CommandRuntime {
    WaitForDependencies,
    RunImmediately,
    RunForever,
}

#[async_trait::async_trait]
pub trait PackageCommand: CoreCommand + Debug + Send + Sync + 'static {
    async fn run_pkg(&self, package: &Package) -> Result<()>;

    fn pkg_key(&self, package: &Package) -> String {
        format!("{}-{}", self.name(), package.name)
    }

    fn deps(&self) -> Vec<Command> {
        Vec::new()
    }

    fn runtime(&self) -> CommandRuntime {
        CommandRuntime::RunImmediately
    }
}

#[async_trait::async_trait]
pub trait WorkspaceCommand: CoreCommand + Debug + Send + Sync + 'static {
    async fn run_ws(&self, ws: &Workspace) -> Result<()>;

    fn ws_key(&self) -> String {
        self.name()
    }

    fn input_files(&self, _ws: &Workspace) -> Option<Vec<PathBuf>> {
        None
    }
}

pub const DEPOT_VERSION: &str = env!("CARGO_PKG_VERSION");

impl Workspace {
    pub async fn load(cwd: Option<PathBuf>, common: CommonArgs) -> Result<Self> {
        let cwd = match cwd {
            Some(cwd) => cwd,
            None => env::current_dir()?,
        };
        let fs_root = cwd.ancestors().last().unwrap().to_path_buf();
        let git_root = utils::get_git_root(&cwd);
        let max_ancestor: &Path = git_root.as_deref().unwrap_or(&fs_root);
        let root = find_workspace_root(max_ancestor, &cwd)?;
        debug!("Workspace root: `{}`", root.display());

        let pkg_dir = root.join("packages");
        let monorepo = pkg_dir.exists();
        debug!("Workspace is monorepo: {monorepo}");

        let manifest = WorkspaceManifest::load(&root.join("package.json"))?;
        let created_version = &manifest.config.depot_version;
        if DEPOT_VERSION != created_version {
            warn!(
        "Depot binary is v{DEPOT_VERSION} but workspace was created with v{created_version}.

Double-check that this workspace is compatible and update depot.depot_version in package.json."
      );
        }

        let pkg_roots = if monorepo {
            pkg_dir
                .read_dir()?
                .map(|entry| Ok(entry?.path()))
                .collect::<Result<Vec<_>>>()?
        } else {
            vec![root.clone()]
        };

        let packages: Vec<_> = stream::iter(pkg_roots)
            .enumerate()
            .then(|(index, pkg_root)| async move { Package::load(&pkg_root, index) })
            .try_collect()
            .await?;

        let roots = match &common.package {
            Some(name) => {
                let pkg = packages
                    .iter()
                    .find(|pkg| &pkg.name == name)
                    .with_context(|| format!("Could not find package with name: {name}"))?;
                vec![pkg.clone()]
            }
            None => packages.clone(),
        };

        let pkg_graph = package::build_package_graph(&packages, &roots)?;

        let package_display_order = {
            let mut order = pkg_graph.nodes().map(|pkg| pkg.index).collect::<Vec<_>>();

            order.sort_by(|n1, n2| {
                if pkg_graph.is_dependent_on(&packages[*n2], &packages[*n1]) {
                    Ordering::Less
                } else if pkg_graph.is_dependent_on(&packages[*n1], &packages[*n2]) {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                }
            });

            order.sort_by(|n1, n2| {
                if pkg_graph.is_dependent_on(&packages[*n2], &packages[*n1]) {
                    Ordering::Less
                } else if pkg_graph.is_dependent_on(&packages[*n1], &packages[*n2]) {
                    Ordering::Greater
                } else {
                    packages[*n1].name.cmp(&packages[*n2].name)
                }
            });

            order
        };

        let fingerprints = RwLock::new(Fingerprints::load(&root)?);

        let ws = Workspace::new(WorkspaceInner {
            root,
            packages,
            package_display_order,
            monorepo,
            pkg_graph,
            common,
            roots,
            processes: RwLock::default(),
            fingerprints,
        });

        for pkg in &ws.packages {
            pkg.set_workspace(&ws);
        }

        Ok(ws)
    }
}

impl WorkspaceInner {
    pub fn package_display_order(&self) -> impl Iterator<Item = &Package> {
        self.package_display_order
            .iter()
            .map(|idx| &self.packages[*idx])
    }

    pub fn start_process(
        &self,
        script: &'static str,
        configure: impl FnOnce(&mut tokio::process::Command),
    ) -> Result<Arc<Process>> {
        log::trace!("Starting process: {script}");

        let pnpm = utils::find_pnpm(Some(&self.root))
            .ok_or(anyhow!("could not find pnpm on your system"))?;

        let mut cmd = tokio::process::Command::new(pnpm);
        cmd.current_dir(&self.root);
        cmd.env("NODE_PATH", self.root.join("node_modules"));

        if script != "pnpm" {
            cmd.args(["exec", script]);
        }

        configure(&mut cmd);

        Ok(Arc::new(Process::new(script.to_owned(), cmd)?))
    }

    pub async fn exec(
        &self,
        script: &'static str,
        configure: impl FnOnce(&mut tokio::process::Command),
    ) -> Result<()> {
        let process = self.start_process(script, configure)?;
        self.processes.write().unwrap().push(process.clone());
        process.wait_for_success().await
    }

    pub fn processes(&self) -> RwLockReadGuard<'_, Vec<Arc<Process>>> {
        self.processes.read().unwrap()
    }

    pub fn all_files(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.packages.iter().flat_map(|pkg| pkg.all_files())
    }
}

pub type CommandGraph = DepGraph<Command>;

pub fn build_command_graph(root: &Command) -> CommandGraph {
    DepGraph::build(vec![root.clone()], |_| unreachable!(), |cmd| cmd.deps()).unwrap()
}

#[cfg(test)]
mod test {
    use crate::commands::test::{TestArgs, TestCommand};

    use super::*;

    #[test]
    fn test_command_graph() {
        let root = TestCommand::new(TestArgs::default()).kind();
        let _cmd_graph = build_command_graph(&root);
        // TODO: finish this test
    }
}

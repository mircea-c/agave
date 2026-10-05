use {
    anyhow::{Context, Result, bail},
    clap::{ArgGroup, Args},
    log::{info, warn},
    serde::Deserialize,
    std::{
        env, fs,
        path::{Path, PathBuf},
        process::Command,
        time::Instant,
    },
};

#[derive(Deserialize)]
struct CargoManifest {
    workspace: Workspace,
}

#[derive(Deserialize)]
struct Workspace {
    metadata: Metadata,
}

#[derive(Deserialize)]
struct Metadata {
    #[serde(rename = "agave-build-lists")]
    build_lists: BuildLists,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct BuildLists {
    dev: Vec<String>,
    end_user: Vec<String>,
    val_op: Vec<String>,
    dcou: Vec<String>,
    deprecated: Vec<String>,
}

#[derive(Args)]
#[command(group(ArgGroup::new("profile").args(["debug", "release_with_debug", "release_with_lto"])))]
pub struct CommandArgs {
    #[arg(help = "Install directory; not needed with --dcou-check-only")]
    pub install_dir: Option<PathBuf>,

    #[arg(
        long,
        help = "Build using this toolchain instead of the one in rust-toolchain.toml"
    )]
    pub toolchain: Option<String>,

    #[arg(
        long,
        help = "Only check that dcou feature activation is correct and exit (no build)"
    )]
    pub dcou_check_only: bool,

    #[arg(long, help = "Build with debug profile instead of release profile")]
    pub debug: bool,

    #[arg(
        long,
        help = "Build with release-with-debug profile instead of release profile"
    )]
    pub release_with_debug: bool,

    #[arg(
        long,
        help = "Build with release-with-lto profile instead of release profile"
    )]
    pub release_with_lto: bool,

    #[arg(long, help = "Do not build DCOU binaries")]
    pub no_build_dcou_bins: bool,

    #[arg(long, help = "Do not build deprecated binaries")]
    pub no_build_deprecated_bins: bool,

    #[arg(long, help = "Do not build development binaries")]
    pub no_build_dev_bins: bool,

    #[arg(long, help = "Do not build end user binaries")]
    pub no_build_end_user_bins: bool,

    #[arg(long, help = "Do not build solana-platform-tools")]
    pub no_build_platform_tools: bool,

    #[arg(long, help = "Do not build validator binaries")]
    pub no_build_validator_bins: bool,

    #[arg(
        long,
        help = "Do not fetch and install SPL-Token (not using this flag requires internet at \
                build time)"
    )]
    pub no_spl_token: bool,

    #[arg(long, hide = true)]
    pub no_perf_libs: bool,

    #[arg(long, hide = true)]
    pub validator_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    Debug,
    Release,
    ReleaseWithDebug,
    ReleaseWithLto,
}

impl Profile {
    pub fn name(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
            Self::ReleaseWithDebug => "release-with-debug",
            Self::ReleaseWithLto => "release-with-lto",
        }
    }

    // cargo rejects `--profile debug`; debug is the default profile
    fn args(self) -> Vec<String> {
        match self {
            Self::Debug => vec![],
            profile => vec![String::from("--profile"), profile.name().to_string()],
        }
    }
}

/// Where a build looks for its bins. Production bins build from the root
/// workspace, dcou bins from dev-bins so their features do not unify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Workspace,
    DevBins,
}

impl Scope {
    fn args(self) -> Vec<String> {
        match self {
            Self::Workspace => vec![String::from("--workspace")],
            Self::DevBins => vec![
                String::from("--manifest-path"),
                String::from("dev-bins/Cargo.toml"),
            ],
        }
    }

    fn target_dir(self, repo_root: &Path, profile: Profile) -> PathBuf {
        let target = match self {
            Self::Workspace => repo_root.join("target"),
            Self::DevBins => repo_root.join("dev-bins/target"),
        };
        target.join(profile.name())
    }
}

/// Arguments after the cargo subcommand, shared by every build of the bins
/// we ship so checks and builds cannot drift.
pub fn bin_build_args(profile: Profile, scope: Scope, bins: &[String]) -> Vec<String> {
    let mut args = profile.args();
    args.extend(scope.args());
    for bin in bins {
        args.push(String::from("--bin"));
        args.push(bin.clone());
    }
    args
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Selection {
    dcou: bool,
    deprecated: bool,
    dev: bool,
    end_user: bool,
    validator: bool,
}

impl Selection {
    fn from_args(args: &CommandArgs) -> Self {
        // dcou and validator bins are not built on Windows
        let windows = cfg!(windows);
        Self {
            dcou: !args.no_build_dcou_bins && !windows,
            deprecated: !args.no_build_deprecated_bins && !args.validator_only,
            dev: !args.no_build_dev_bins && !args.validator_only,
            end_user: !args.no_build_end_user_bins,
            validator: !args.no_build_validator_bins && !windows,
        }
    }

    fn bins(&self, lists: BuildLists) -> (Vec<String>, Vec<String>) {
        let BuildLists {
            dev,
            end_user,
            val_op,
            dcou,
            deprecated,
        } = lists;

        let mut prod_bins = vec![];
        if self.deprecated {
            prod_bins.extend(deprecated);
        }
        if self.dev {
            prod_bins.extend(dev);
        }
        if self.end_user {
            prod_bins.extend(end_user);
        }
        if self.validator {
            prod_bins.extend(val_op);
        }

        let dcou_bins = if self.dcou { dcou } else { vec![] };

        (prod_bins, dcou_bins)
    }
}

#[derive(Deserialize)]
struct ToolchainManifest {
    toolchain: Toolchain,
}

#[derive(Deserialize)]
struct Toolchain {
    channel: String,
}

struct Cargo {
    toolchain: Option<String>,
}

impl Cargo {
    /// Same toolchain choice as the repo's `./cargo` wrapper.
    fn new(repo_root: &Path, toolchain: Option<String>) -> Result<Self> {
        if toolchain.is_some() {
            return Ok(Self { toolchain });
        }
        if env::var_os("NO_RUSTUP_OVERRIDE").is_some_and(|v| !v.is_empty()) {
            return Ok(Self { toolchain: None });
        }
        if let Some(version) = env::var("RUST_STABLE_VERSION")
            .ok()
            .filter(|v| !v.is_empty())
        {
            return Ok(Self {
                toolchain: Some(version),
            });
        }
        let manifest = fs::read_to_string(repo_root.join("rust-toolchain.toml"))
            .context("failed to read rust-toolchain.toml")?;
        let manifest: ToolchainManifest =
            toml::from_str(&manifest).context("failed to parse rust-toolchain.toml")?;
        Ok(Self {
            toolchain: Some(manifest.toolchain.channel),
        })
    }

    fn command(&self, repo_root: &Path) -> Command {
        let mut cmd = match &self.toolchain {
            Some(toolchain) => {
                let mut cmd = Command::new("rustup");
                cmd.args(["run", toolchain, "cargo"]);
                cmd
            }
            None => Command::new("cargo"),
        };
        // `cargo run` hands xtask its own package env. Build scripts that
        // watch these (ring watches CARGO_MANIFEST_DIR) would otherwise see a
        // change and rebuild against a plain `cargo build` of the same tree.
        for (name, _) in env::vars_os() {
            if name.to_str().is_some_and(is_cargo_run_env) {
                cmd.env_remove(&name);
            }
        }
        cmd.current_dir(repo_root);
        cmd
    }

    fn run(&self, repo_root: &Path, args: &[String]) -> Result<()> {
        let mut cmd = self.command(repo_root);
        cmd.args(args);
        info!("+ cargo {}", args.join(" "));
        let status = cmd
            .status()
            .with_context(|| format!("failed to run cargo {}", args.join(" ")))?;
        if !status.success() {
            bail!("cargo {} failed with {status}", args.join(" "));
        }
        Ok(())
    }

    /// Whether any unit in the build plan activates dev-context-only-utils.
    fn activates_dcou(&self, repo_root: &Path, build_args: &[String]) -> Result<bool> {
        let mut cmd = self.command(repo_root);
        // RUSTC_BOOTSTRAP avoids needing a nightly toolchain for the
        // unstable unit graph
        cmd.env("RUSTC_BOOTSTRAP", "1")
            .args(["build", "-Z", "unstable-options", "--unit-graph"])
            .args(build_args);
        info!("+ cargo build --unit-graph {}", build_args.join(" "));
        let output = cmd.output().context("failed to run cargo --unit-graph")?;
        if !output.status.success() {
            bail!(
                "cargo --unit-graph failed with {}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        unit_graph_activates_dcou(&output.stdout)
    }
}

fn is_cargo_run_env(name: &str) -> bool {
    matches!(
        name,
        "CARGO_MANIFEST_DIR"
            | "CARGO_MANIFEST_PATH"
            | "CARGO_CRATE_NAME"
            | "CARGO_BIN_NAME"
            | "CARGO_PRIMARY_PACKAGE"
    ) || name.starts_with("CARGO_PKG_")
}

#[derive(Deserialize)]
struct UnitGraph {
    units: Vec<Unit>,
}

#[derive(Deserialize)]
struct Unit {
    #[serde(default)]
    features: Vec<String>,
}

fn unit_graph_activates_dcou(unit_graph: &[u8]) -> Result<bool> {
    let unit_graph: UnitGraph =
        serde_json::from_slice(unit_graph).context("failed to parse cargo unit graph")?;
    Ok(unit_graph
        .units
        .iter()
        .any(|unit| unit.features.iter().any(|f| f == "dev-context-only-utils")))
}

fn parse_build_lists(manifest: &str) -> Result<BuildLists> {
    let manifest: CargoManifest = toml::from_str(manifest)
        .context("failed to parse [workspace.metadata.agave-build-lists] in Cargo.toml")?;
    Ok(manifest.workspace.metadata.build_lists)
}

pub fn run(args: CommandArgs) -> Result<()> {
    let started = Instant::now();
    let repo_root = repo_root();

    if args.no_perf_libs {
        warn!(
            "--no-perf-libs has been deprecated and is now a no-op. perf-libs are no longer \
             applicable to agave."
        );
    }
    if args.validator_only {
        warn!(
            "--validator-only has been deprecated, use a combination of \
             --no-build-{{dev,deprecated}}-bins and --no-build-platform-tools instead."
        );
    }

    let profile = if args.debug {
        Profile::Debug
    } else if args.release_with_debug {
        Profile::ReleaseWithDebug
    } else if args.release_with_lto {
        Profile::ReleaseWithLto
    } else {
        Profile::Release
    };

    let install_dir = if args.dcou_check_only {
        info!("(dcou check mode: ignore install dir)");
        None
    } else {
        let Some(install_dir) = &args.install_dir else {
            bail!("install directory not specified");
        };
        fs::create_dir_all(install_dir.join("bin/deps"))
            .with_context(|| format!("failed to create {}", install_dir.display()))?;
        let install_dir = install_dir
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", install_dir.display()))?;
        info!(
            "install location: {} ({})",
            install_dir.display(),
            profile.name()
        );
        Some(install_dir)
    };

    let manifest =
        fs::read_to_string(repo_root.join("Cargo.toml")).context("failed to read Cargo.toml")?;
    let (prod_bins, dcou_bins) = Selection::from_args(&args).bins(parse_build_lists(&manifest)?);
    info!("building binaries: {prod_bins:?} {dcou_bins:?}");

    let cargo = Cargo::new(&repo_root, args.toolchain.clone())?;
    let prod_args = bin_build_args(profile, Scope::Workspace, &prod_bins);
    let dcou_args = bin_build_args(profile, Scope::DevBins, &dcou_bins);

    // Without `--workspace`, a cargo bug unifies dev-context-only-utils into
    // the build even when none of the requested bins depend on it. Checking
    // both directions keeps the check from silently passing if the unit
    // graph format changes.
    if !prod_bins.is_empty() && cargo.activates_dcou(&repo_root, &prod_args)? {
        bail!("dcou feature activation is incorrectly activated!");
    }
    if !dcou_bins.is_empty() && !cargo.activates_dcou(&repo_root, &dcou_args)? {
        bail!("dcou feature activation is incorrectly deactivated!");
    }

    let Some(install_dir) = install_dir else {
        info!("dcou feature activation check passed.");
        return Ok(());
    };

    if !prod_bins.is_empty() {
        cargo.run(&repo_root, &prefixed("build", prod_args))?;
    }
    if !dcou_bins.is_empty() {
        cargo.run(&repo_root, &prefixed("build", dcou_args))?;
    }

    if !args.no_spl_token {
        let version = pinned_version(&repo_root, "spl-token-cli-version.sh", "splTokenCliVersion")?;
        cargo_install(&cargo, &repo_root, "spl-token-cli", &install_dir, version)?;
    }

    let bin_dir = install_dir.join("bin");
    copy_bins(
        &Scope::Workspace.target_dir(&repo_root, profile),
        &prod_bins,
        &bin_dir,
    )?;
    copy_bins(
        &Scope::DevBins.target_dir(&repo_root, profile),
        &dcou_bins,
        &bin_dir,
    )?;

    if !args.no_build_platform_tools && !args.validator_only {
        cargo.run(
            &repo_root,
            &prefixed(
                "build",
                vec![
                    String::from("--manifest-path"),
                    String::from("syscalls/gen-syscall-list/Cargo.toml"),
                ],
            ),
        )?;
        // cargo-build-sbf v4.1.0+ also installs cargo-test-sbf
        let version = pinned_version(
            &repo_root,
            "cargo-build-sbf-version.sh",
            "cargoBuildSbfVersion",
        )?;
        cargo_install(&cargo, &repo_root, "cargo-build-sbf", &install_dir, version)?;
    }

    copy_program_deps(
        &Scope::Workspace
            .target_dir(&repo_root, profile)
            .join("deps"),
        &bin_dir.join("deps"),
    )?;

    info!("done after {} seconds", started.elapsed().as_secs());
    info!("to use these binaries:");
    info!("  export PATH=\"{}\"/bin:\"$PATH\"", install_dir.display());

    Ok(())
}

fn prefixed(subcommand: &str, args: Vec<String>) -> Vec<String> {
    let mut prefixed = vec![subcommand.to_string()];
    prefixed.extend(args);
    prefixed
}

fn cargo_install(
    cargo: &Cargo,
    repo_root: &Path,
    package: &str,
    install_dir: &Path,
    version: Option<String>,
) -> Result<()> {
    let mut args = vec![
        String::from("install"),
        String::from("--locked"),
        package.to_string(),
        String::from("--root"),
        install_dir.display().to_string(),
    ];
    if let Some(version) = version {
        args.push(String::from("--version"));
        args.push(version);
    }
    cargo.run(repo_root, &args)
}

/// Reads a `name=value` pin from one of the scripts/*-version.sh files, which
/// RELEASE.md has release managers populate on the stable branch.
fn pinned_version(repo_root: &Path, file: &str, name: &str) -> Result<Option<String>> {
    let path = repo_root.join("scripts").join(file);
    let script =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    parse_pinned_version(&script, name).with_context(|| format!("in {}", path.display()))
}

fn parse_pinned_version(script: &str, name: &str) -> Result<Option<String>> {
    let prefix = format!("{name}=");
    let Some(value) = script
        .lines()
        .find_map(|line| line.trim().strip_prefix(&prefix))
    else {
        bail!("no {name}= line");
    };
    let value = value.trim().trim_matches(['"', '\'']);
    Ok((!value.is_empty()).then(|| value.to_string()))
}

fn copy_bins(target_dir: &Path, bins: &[String], bin_dir: &Path) -> Result<()> {
    for bin in bins {
        let file = format!("{bin}{}", env::consts::EXE_SUFFIX);
        copy(&target_dir.join(&file), &bin_dir.join(&file))?;
    }
    Ok(())
}

// deps dir can be empty or missing
fn copy_program_deps(deps_dir: &Path, dest: &Path) -> Result<()> {
    let Ok(entries) = fs::read_dir(deps_dir) else {
        return Ok(());
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| is_program_dep(name))
        .collect();
    names.sort();
    for name in names {
        copy(&deps_dir.join(&name), &dest.join(&name))?;
    }
    Ok(())
}

/// Matches the `libsolana*program.*` glob
fn is_program_dep(name: &str) -> bool {
    name.strip_prefix("libsolana")
        .is_some_and(|rest| rest.contains("program."))
}

fn copy(from: &Path, to: &Path) -> Result<()> {
    // remove first so a running binary is replaced rather than overwritten,
    // like `cp -f`
    let _ = fs::remove_file(to);
    fs::copy(from, to)
        .with_context(|| format!("failed to copy {} to {}", from.display(), to.display()))?;
    info!("'{}' -> '{}'", from.display(), to.display());
    Ok(())
}

fn repo_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.canonicalize().unwrap_or(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lists() -> BuildLists {
        BuildLists {
            dev: vec![String::from("dev-bin")],
            end_user: vec![String::from("end-user-bin")],
            val_op: vec![String::from("val-op-bin")],
            dcou: vec![String::from("dcou-bin")],
            deprecated: vec![String::from("deprecated-bin")],
        }
    }

    fn all() -> Selection {
        Selection {
            dcou: true,
            deprecated: true,
            dev: true,
            end_user: true,
            validator: true,
        }
    }

    #[test]
    fn selects_bins_in_script_order() {
        let (prod_bins, dcou_bins) = all().bins(lists());

        assert_eq!(
            prod_bins,
            ["deprecated-bin", "dev-bin", "end-user-bin", "val-op-bin"]
        );
        assert_eq!(dcou_bins, ["dcou-bin"]);
    }

    #[test]
    fn skips_unselected_bins() {
        let selection = Selection {
            dev: false,
            deprecated: false,
            dcou: false,
            ..all()
        };
        let (prod_bins, dcou_bins) = selection.bins(lists());

        assert_eq!(prod_bins, ["end-user-bin", "val-op-bin"]);
        assert!(dcou_bins.is_empty());
    }

    #[test]
    fn builds_bin_args() {
        let bins = [String::from("a"), String::from("b")];

        assert_eq!(
            bin_build_args(Profile::Release, Scope::Workspace, &bins),
            [
                "--profile",
                "release",
                "--workspace",
                "--bin",
                "a",
                "--bin",
                "b"
            ]
        );
        assert_eq!(
            bin_build_args(Profile::Debug, Scope::DevBins, &bins[..1]),
            ["--manifest-path", "dev-bins/Cargo.toml", "--bin", "a"]
        );
    }

    #[test]
    fn reads_repo_manifest() {
        let manifest = fs::read_to_string(repo_root().join("Cargo.toml")).unwrap();
        let (prod_bins, dcou_bins) = all().bins(parse_build_lists(&manifest).unwrap());

        assert!(prod_bins.iter().any(|bin| bin == "agave-validator"));
        assert!(dcou_bins.iter().any(|bin| bin == "agave-ledger-tool"));
    }

    #[test]
    fn parses_pinned_versions() {
        assert_eq!(parse_pinned_version("v=\n", "v").unwrap(), None);
        assert_eq!(
            parse_pinned_version("# x\nv=5.1.0\n", "v")
                .unwrap()
                .as_deref(),
            Some("5.1.0")
        );
        assert_eq!(
            parse_pinned_version("v=\"5.1.0\"", "v").unwrap().as_deref(),
            Some("5.1.0")
        );
        assert!(parse_pinned_version("other=1", "v").is_err());
    }

    #[test]
    fn reads_repo_pinned_versions() {
        let root = repo_root();
        pinned_version(&root, "spl-token-cli-version.sh", "splTokenCliVersion").unwrap();
        pinned_version(&root, "cargo-build-sbf-version.sh", "cargoBuildSbfVersion").unwrap();
    }

    #[test]
    fn detects_dcou_in_unit_graph() {
        let graph = |features: &str| {
            format!(
                r#"{{"version":1,"units":[{{"features":[]}},{{"features":[{features}]}}],"roots":[]}}"#
            )
        };

        assert!(
            unit_graph_activates_dcou(graph(r#""dev-context-only-utils""#).as_bytes()).unwrap()
        );
        assert!(!unit_graph_activates_dcou(graph(r#""default""#).as_bytes()).unwrap());
        assert!(unit_graph_activates_dcou(b"not json").is_err());
    }

    #[test]
    fn scrubs_only_cargo_run_env() {
        assert!(is_cargo_run_env("CARGO_MANIFEST_DIR"));
        assert!(is_cargo_run_env("CARGO_PKG_VERSION"));
        assert!(!is_cargo_run_env("CARGO_TARGET_DIR"));
        assert!(!is_cargo_run_env("CARGO_HOME"));
        assert!(!is_cargo_run_env("CARGO_BUILD_JOBS"));
    }

    #[test]
    fn matches_program_dep_glob() {
        assert!(is_program_dep("libsolana_noop_program.so"));
        assert!(is_program_dep("libsolanaprogram.so"));
        assert!(!is_program_dep("libsolana_runtime.rlib"));
        assert!(!is_program_dep("solana_noop_program.so"));
    }
}

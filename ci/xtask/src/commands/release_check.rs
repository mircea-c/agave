use {
    super::install_all::{Profile, Scope, Selection, bin_build_args, parse_build_lists},
    anyhow::{Context, Result, bail, ensure},
    clap::Args,
    log::info,
    std::{
        env, fs,
        path::{Path, PathBuf},
        process::Command,
    },
};

#[derive(Args)]
pub struct CommandArgs {
    #[arg(
        long,
        value_enum,
        default_value = "release",
        help = "Cargo profile to check against"
    )]
    pub profile: Profile,

    #[arg(long, help = "Override the computed job count")]
    pub jobs: Option<usize>,
}

pub fn run(args: CommandArgs) -> Result<()> {
    let CommandArgs { profile, jobs } = args;
    let repo_root = repo_root();
    let jobs = match jobs {
        Some(jobs) => jobs,
        None => xtask_shared::commands::jobs::jobs()?,
    };

    let manifest =
        fs::read_to_string(repo_root.join("Cargo.toml")).context("failed to read Cargo.toml")?;
    let (prod_bins, dcou_bins) = bin_sets(&manifest)?;

    // Same arguments as the two builds in `cargo xtask install-all`
    let profile_name = profile.name();
    info!("checking {profile_name} production bins across {jobs} jobs: {prod_bins:?}");
    cargo_check(
        &repo_root,
        jobs,
        &bin_build_args(profile, Scope::Workspace, &prod_bins),
    )?;

    info!("checking {profile_name} dcou bins across {jobs} jobs: {dcou_bins:?}");
    cargo_check(
        &repo_root,
        jobs,
        &bin_build_args(profile, Scope::DevBins, &dcou_bins),
    )?;

    Ok(())
}

fn cargo_check(repo_root: &Path, jobs: usize, build_args: &[String]) -> Result<()> {
    // RUSTFLAGS stays unset so .cargo/config.toml keeps -Ctarget-cpu.
    let mut cmd = Command::new(cargo_bin());
    cmd.current_dir(repo_root)
        .arg("check")
        .args(build_args)
        .args(["--jobs", &jobs.to_string()]);

    let status = cmd.status().context("failed to run cargo check")?;
    if !status.success() {
        bail!("cargo check failed with {status}");
    }

    Ok(())
}

/// Production and dcou bins, grouped the same way `cargo xtask install-all`
/// builds them.
fn bin_sets(manifest: &str) -> Result<(Vec<String>, Vec<String>)> {
    let (prod_bins, dcou_bins) = Selection::all().bins(parse_build_lists(manifest)?);
    ensure!(!prod_bins.is_empty(), "no production bins in Cargo.toml");
    ensure!(!dcou_bins.is_empty(), "no dcou bins in Cargo.toml");

    Ok((prod_bins, dcou_bins))
}

fn cargo_bin() -> PathBuf {
    env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"))
}

fn repo_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.canonicalize().unwrap_or(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
        [workspace.metadata.agave-build-lists]
        dev = ["dev-bin"]
        end-user = ["end-user-bin"]
        val-op = ["val-op-bin"]
        dcou = ["dcou-bin"]
        deprecated = ["deprecated-bin"]
        dcou-tainted-packages = ["tainted-package"]
    "#;

    #[test]
    fn groups_bins_like_cargo_install_all() {
        let (prod_bins, dcou_bins) = bin_sets(MANIFEST).unwrap();

        assert_eq!(
            prod_bins,
            ["deprecated-bin", "dev-bin", "end-user-bin", "val-op-bin"]
        );
        assert_eq!(dcou_bins, ["dcou-bin"]);
    }

    #[test]
    fn reads_repo_manifest() {
        let manifest = fs::read_to_string(repo_root().join("Cargo.toml")).unwrap();
        let (prod_bins, dcou_bins) = bin_sets(&manifest).unwrap();

        assert!(prod_bins.iter().any(|bin| bin == "agave-validator"));
        assert!(dcou_bins.iter().any(|bin| bin == "agave-ledger-tool"));
    }

    #[test]
    fn rejects_missing_and_empty_lists() {
        for manifest in [
            "",
            "[workspace.metadata]",
            &MANIFEST.replace("val-op", "valop"),
        ] {
            let error = bin_sets(manifest).unwrap_err();
            assert_eq!(
                error.to_string(),
                "failed to parse [workspace.metadata.agave-build-lists] in Cargo.toml"
            );
        }

        let error = bin_sets(&MANIFEST.replace(r#"["dcou-bin"]"#, "[]")).unwrap_err();
        assert_eq!(error.to_string(), "no dcou bins in Cargo.toml");
    }
}

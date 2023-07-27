#[macro_use]
extern crate anyhow;

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;

use anyhow::Context;
use anyhow::Result;
use cargo_metadata::Artifact;
use cargo_metadata::ArtifactDebuginfo;
use cargo_metadata::Message;
use cargo_metadata::MetadataCommand;
use cargo_metadata::Package;
use clap::Args;
use clap::Parser;

#[derive(Debug, Parser)]
#[clap(version)]
struct HeaptrackOpts {
    /// Output file
    #[clap(short, long)]
    output: Option<PathBuf>,

    /// Only record raw data, do not interpret it.
    #[clap(short, long, default_value = "true")]
    raw: bool,
}

// Taken from cargo-flamegraph, since we need to support more or less
// the same arguments
#[derive(Args, Debug)]
struct Opts {
    /// Build with the dev profile
    #[clap(long)]
    dev: bool,

    /// Build with the specified profile
    #[clap(long)]
    profile: Option<String>,

    /// package with the binary to run
    #[clap(short, long)]
    package: Option<String>,

    /// Binary to run
    #[clap(short, long, group = "exec-args")]
    bin: Option<String>,

    /// Example to run
    #[clap(long, group = "exec-args")]
    example: Option<String>,

    /// Test binary to run
    #[clap(long, group = "exec-args")]
    test: Option<String>,

    /// Crate target to unit test, <unit-test> may be omitted if crate only has one target
    /// (currently profiles the test harness and all tests in the binary; test selection
    /// can be passed as trailing arguments after `--` as separator)
    #[clap(long, group = "exec-args")]
    unit_test: Option<Option<String>>,

    /// Benchmark to run
    #[clap(long, group = "exec-args")]
    bench: Option<String>,

    /// Path to Cargo.toml
    #[clap(long)]
    manifest_path: Option<PathBuf>,

    /// Build features to enable
    #[clap(short, long)]
    features: Option<String>,

    /// Disable default features
    #[clap(long)]
    no_default_features: bool,

    /// No-op. For compatibility with `cargo run --release`.
    #[clap(short, long)]
    release: bool,

    #[clap(flatten)]
    heaptrack: HeaptrackOpts,

    /// Trailing arguments passed to the binary being profiled.
    #[clap(last = true)]
    trailing_arguments: Vec<String>,
}

#[derive(Parser, Debug)]
#[clap(bin_name = "cargo")]
enum Cli {
    /// A cargo subcommand for profiling executables with heaptrack
    #[clap(version)]
    Heaptrack(Opts),
}

fn build(opt: &Opts, kind: impl IntoIterator<Item = String>) -> Result<Vec<Artifact>> {
    let mut cmd = Command::new("cargo");
    // This will build benchmarks with the `bench` profile. This is needed
    // because the `--profile` argument for `cargo build` is unstable.
    if !opt.dev && opt.bench.is_some() {
        cmd.args(["bench", "--no-run"]);
    } else if opt.unit_test.is_some() {
        cmd.args(["test", "--no-run"]);
    } else {
        cmd.arg("build");
    }

    if let Some(profile) = &opt.profile {
        cmd.arg("--profile").arg(profile);
    } else if !opt.dev && opt.bench.is_none() {
        // do not use `--release` when we are building for `bench`
        cmd.arg("--release");
    }

    if let Some(ref package) = opt.package {
        cmd.arg("--package");
        cmd.arg(package);
    }

    if let Some(ref bin) = opt.bin {
        cmd.arg("--bin");
        cmd.arg(bin);
    }

    if let Some(ref example) = opt.example {
        cmd.arg("--example");
        cmd.arg(example);
    }

    if let Some(ref test) = opt.test {
        cmd.arg("--test");
        cmd.arg(test);
    }

    if let Some(ref bench) = opt.bench {
        cmd.arg("--bench");
        cmd.arg(bench);
    }

    if let Some(Some(ref unit_test)) = opt.unit_test {
        if kind.into_iter().any(|k| k == "lib") {
            cmd.arg("--lib");
        } else {
            cmd.args(["--bin", unit_test]);
        }
    }

    if let Some(ref manifest_path) = opt.manifest_path {
        cmd.arg("--manifest-path");
        cmd.arg(manifest_path);
    }

    if let Some(ref features) = opt.features {
        cmd.arg("--features");
        cmd.arg(features);
    }

    if opt.no_default_features {
        cmd.arg("--no-default-features");
    }

    cmd.arg("--message-format=json-render-diagnostics");

    let Output { status, stdout, .. } = cmd
        .stderr(Stdio::inherit())
        .output()
        .context("failed to execute cargo build command")?;

    if !status.success() {
        bail!("cargo build failed");
    }

    Message::parse_stream(&*stdout)
        .filter_map(|m| match m {
            Ok(Message::CompilerArtifact(artifact)) => Some(Ok(artifact)),
            Ok(_) => None,
            Err(e) => Some(Err(e).context("failed to parse cargo build output")),
        })
        .collect()
}

fn workload(opt: &Opts, artifacts: &[Artifact]) -> Result<Vec<String>> {
    if artifacts.iter().all(|a| a.executable.is_none()) {
        bail!("build artifacts do not contain any executable to profile");
    }

    let (kind, target): (&[&str], _) = match opt {
        Opts { bin: Some(t), .. } => (&["bin"], t),
        Opts {
            example: Some(t), ..
        } => (&["example"], t),
        Opts { test: Some(t), .. } => (&["test"], t),
        Opts { bench: Some(t), .. } => (&["bench"], t),
        Opts {
            unit_test: Some(Some(t)),
            ..
        } => (&["lib", "bin"], t),
        _ => bail!("no target for profiling"),
    };

    // `target.kind` is a `Vec`, but it always seems to contain exactly one element.
    let (debug_level, binary_path) = artifacts
        .iter()
        .find_map(|a| {
            a.executable
                .as_deref()
                .filter(|_| {
                    a.target.name == *target
                        && a.target.kind.iter().any(|k| kind.contains(&k.as_str()))
                })
                .map(|e| (&a.profile.debuginfo, e))
        })
        .ok_or_else(|| {
            let targets: Vec<_> = artifacts
                .iter()
                .map(|a| (&a.target.kind, &a.target.name))
                .collect();
            anyhow!(
                "could not find desired target ({kind:?}, {target:?}) in the targets for this crate: {targets:?}",
            )
        })?;

    if !opt.dev && debug_level == &ArtifactDebuginfo::None {
        let profile = match opt
            .example
            .as_ref()
            .or(opt.bin.as_ref())
            .or_else(|| opt.unit_test.as_ref().unwrap_or(&None).as_ref())
        {
            // binaries, examples and unit tests use release profile
            Some(_) => "release",
            // tests use the bench profile in release mode.
            _ => "bench",
        };

        eprintln!("\nWARNING: profiling without debuginfo. Enable symbol information by adding the following lines to Cargo.toml:\n");
        eprintln!("[profile.{}]", profile);
        eprintln!("debug = true\n");
        eprintln!("Or set this environment variable:\n");
        eprintln!("CARGO_PROFILE_{}_DEBUG=true\n", profile.to_uppercase());
    }

    let mut command = Vec::with_capacity(1 + opt.trailing_arguments.len());
    command.push(binary_path.to_string());
    command.extend(opt.trailing_arguments.iter().cloned());
    Ok(command)
}

#[derive(Clone, Debug)]
struct BinaryTarget {
    package: String,
    target: String,
    kind: Vec<String>,
}

impl std::fmt::Display for BinaryTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "target {} in package {}", self.target, self.package)
    }
}

pub fn find_crate_root(manifest_path: Option<&Path>) -> Result<PathBuf> {
    match manifest_path {
        Some(path) => {
            let path = path.parent().ok_or_else(|| {
                anyhow!(
                    "the manifest path '{}' must point to a Cargo.toml file",
                    path.display()
                )
            })?;

            path.canonicalize().with_context(|| {
                anyhow!(
                    "failed to canonicalize manifest parent directory '{}'\nHint: make sure your manifest path is exists and points to a Cargo.toml file",
                    path.display()
                )
            })
        }
        None => {
            let cargo_toml = "Cargo.toml";
            let cwd = std::env::current_dir().context("failed to determine working directory")?;

            for current in cwd.ancestors() {
                if current.join(cargo_toml).exists() {
                    return Ok(current.to_path_buf());
                }
            }

            Err(anyhow!(
                "could not find '{cargo_toml}' in '{}' or any parent directory",
                cwd.display()
            ))
        }
    }
}

fn find_unique_target(
    kind: &[&str],
    pkg: Option<&str>,
    manifest_path: Option<&Path>,
    target_name: Option<&str>,
) -> Result<BinaryTarget> {
    let mut metadata_command = MetadataCommand::new();
    metadata_command.no_deps();
    if let Some(ref manifest_path) = manifest_path {
        metadata_command.manifest_path(manifest_path);
    }

    let crate_root = find_crate_root(manifest_path)?;

    let mut packages = metadata_command
        .exec()
        .context("failed to access crate metadata")?
        .packages
        .into_iter()
        .filter(|p| match pkg {
            Some(pkg) => pkg == p.name,
            None => p.manifest_path.starts_with(&crate_root),
        })
        .peekable();

    if packages.peek().is_none() {
        return Err(match pkg {
            Some(pkg) => anyhow!("workspace has no package named {pkg}"),
            None => anyhow!(
                "failed to find any package in '{}' or below",
                crate_root.display()
            ),
        });
    }

    let mut num_packages = 0;
    let mut is_default = false;

    let mut targets: Vec<_> = packages
        .flat_map(|p| {
            let Package {
                targets,
                name,
                default_run,
                ..
            } = p;
            num_packages += 1;
            if default_run.is_some() {
                is_default = true;
            }
            targets.into_iter().filter_map(move |t| {
                // Keep only targets that are of the right kind.
                if !t.kind.iter().any(|s| kind.contains(&s.as_str())) {
                    return None;
                }

                // When `default_run` is set, keep only the target with that name.
                match &default_run {
                    Some(name) if name != &t.name => return None,
                    _ => {}
                }

                match target_name {
                    Some(name) if name != t.name => return None,
                    _ => {}
                }

                Some(BinaryTarget {
                    package: name.clone(),
                    target: t.name,
                    kind: t.kind,
                })
            })
        })
        .collect();

    match targets.as_slice() {
        [_] => {
            let target = targets.remove(0);
            // If the selected target is the default_run of the only package, do not print a message.
            if num_packages != 1 || !is_default {
                eprintln!(
                    "automatically selected {} as it is the only valid target",
                    target
                );
            }
            Ok(target)
        }
        [] => bail!(
            "crate has no automatically selectable target:\nHint: try passing `--example <example>` \
                or similar to choose a binary"
        ),
        _ => bail!(
            "several possible targets found: {:?}, please pass an explicit target.",
            targets
        ),
    }
}

fn main() -> Result<()> {
    let Cli::Heaptrack(mut opt) = Cli::parse();

    let kind = if opt.bin.is_none()
        && opt.bench.is_none()
        && opt.example.is_none()
        && opt.test.is_none()
        && opt.unit_test.is_none()
    {
        let target = find_unique_target(
            &["bin"],
            opt.package.as_deref(),
            opt.manifest_path.as_deref(),
            None,
        )?;
        opt.bin = Some(target.target);
        opt.package = Some(target.package);
        target.kind
    } else if let Some(unit_test) = opt.unit_test {
        let target = find_unique_target(
            &["bin", "lib"],
            opt.package.as_deref(),
            opt.manifest_path.as_deref(),
            unit_test.as_deref(),
        )?;
        opt.unit_test = Some(Some(target.target));
        opt.package = Some(target.package);
        target.kind
    } else {
        Vec::new()
    };

    let artifacts = build(&opt, kind)?;
    let mut workload = workload(&opt, &artifacts)?;
    println!("{workload:?}");
    let args = workload.split_off(1);
    run_heaptrack(workload.into_iter().next().unwrap(), args, opt.heaptrack)
}


fn run_heaptrack(target: String, args: Vec<String>, opts: HeaptrackOpts) -> Result<()> {
    let mut cmd = Command::new("heaptrack");
    if let Some(output) = opts.output {
        cmd.args(["--output".as_ref(), output.as_os_str()]);
    }
    if opts.raw {
        cmd.arg("--raw");
    }
    cmd.arg(target);
    cmd.args(args);

    let mut child = cmd.spawn()
        .context("failed to execute heaptrack command")?;

    let status = child.wait()
        .context("failed to wait on heaptrack process")?;

    if !status.success() {
        bail!("heaptrack failed");
    }
    Ok(())
}

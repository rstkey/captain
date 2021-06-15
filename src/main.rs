//! Fleet entrypoint

mod command;
mod config;
mod workspace;

use crate::config::Config;
use crate::config::Network;
use anyhow::{anyhow, format_err, Result};
use clap::{crate_authors, crate_description, crate_version, AppSettings, Clap};
use colored::*;
use rand::rngs::OsRng;
use semver::Version;
use solana_sdk::signature::Signer;
use std::env;
use std::fs::File;
use std::io::Write;
use std::process::Command;
use strum::VariantNames;
use tempfile::NamedTempFile;

#[derive(Debug, Clap)]
pub enum SubCommand {
    #[clap(about = "Initializes a new Fleet workspace.")]
    Init,
    #[clap(about = "Builds all programs. (Uses Anchor)")]
    Build,
    #[clap(about = "Deploys a program.")]
    Deploy {
        #[clap(short, long)]
        version: Option<Version>,
        #[clap(short, long)]
        #[clap(about = "Name of the program in target/deploy/<id>.so")]
        program: String,
        #[clap(short, long)]
        #[clap(about = "Network to deploy to")]
        #[clap(
            default_value = Network::Devnet.into(),
            possible_values = Network::VARIANTS
        )]
        network: Network,
    },
    #[clap(about = "Upgrades a program.")]
    Upgrade {
        #[clap(short, long)]
        version: Option<Version>,
        #[clap(short, long)]
        #[clap(about = "Name of the program in target/deploy/<id>.so")]
        program: String,
        #[clap(short, long)]
        #[clap(about = "Network to deploy to")]
        #[clap(
            default_value = Network::Devnet.into(),
            possible_values = Network::VARIANTS
        )]
        network: Network,
    },
}

#[derive(Debug, Clap)]
#[clap(about = crate_description!())]
#[clap(version = crate_version!())]
#[clap(author = crate_authors!())]
#[clap(setting = AppSettings::ColoredHelp)]
pub struct Opts {
    #[clap(subcommand)]
    command: SubCommand,
}

fn main_with_result() -> Result<()> {
    let opts: Opts = Opts::parse();

    // Gets a value for config if supplied by user, or defaults to "default.conf"
    println!("Value for config: {:?}", opts.command);

    match opts.command {
        SubCommand::Init => {
            if !std::env::current_dir()?.join("Cargo.toml").exists() {
                println!(
                    "{}",
                    "Cargo.toml does not exist in the current working directory. Ensure that you are at the Cargo workspace root.".red()
                );
                std::process::exit(1);
            }
            let cfg = Config::default();
            let toml = toml::to_string(&cfg)?;
            let mut file = File::create("Fleet.toml")?;
            file.write_all(toml.as_bytes())?;
        }
        SubCommand::Build => {
            let (_, _, root) = Config::discover()?;
            if root.join("Anchor.toml").exists() {
                println!("{}", "Anchor found! Running `anchor build -v`.".green());
                command::exec(Command::new("anchor").arg("build").arg("-v"))?;
            } else {
                println!(
                    "{}",
                    "Anchor.toml not found in workspace root. Running `cargo build-bpf`.".yellow()
                );
                command::exec(Command::new("cargo").arg("build-bpf"))?;
            }
        }
        SubCommand::Deploy {
            version,
            program,
            ref network,
        } => {
            let workspace = &workspace::init(program.as_str(), version, network.clone())?;
            println!(
                "Deploying program {} with version {}",
                program, workspace.deploy_version
            );

            println!("Address: {}", workspace.program_key);

            if workspace.show_program()? {
                println!("Program already deployed. Use `fleet upgrade` if you want to upgrade the program.");
                std::process::exit(0);
            }

            output_header("Deploying program");

            command::exec(
                std::process::Command::new("solana")
                    .arg("program")
                    .arg("deploy")
                    .arg(&workspace.program_paths.bin)
                    .arg("--keypair")
                    .arg(&workspace.deployer_path)
                    .arg("--program-id")
                    .arg(&workspace.program_paths.id),
            )?;

            output_header("Setting upgrade authority");

            command::exec(
                std::process::Command::new("solana")
                    .arg("program")
                    .arg("set-upgrade-authority")
                    .arg(&workspace.program_paths.id)
                    .arg("--keypair")
                    .arg(&workspace.deployer_path)
                    .arg("--new-upgrade-authority")
                    .arg(&workspace.network_config.upgrade_authority),
            )?;

            workspace.show_program()?;

            if workspace.has_anchor() {
                output_header("Initializing IDL");
                command::exec(
                    std::process::Command::new("anchor")
                        .arg("idl")
                        .arg("init")
                        .arg(&workspace.program_key.to_string())
                        .arg("--filepath")
                        .arg(&workspace.program_paths.idl)
                        .arg("--provider.cluster")
                        .arg(workspace.network.to_string())
                        .arg("--provider.wallet")
                        .arg(&workspace.deployer_path),
                )?;

                output_header("Setting IDL authority");
                command::exec(
                    std::process::Command::new("anchor")
                        .arg("idl")
                        .arg("set-authority")
                        .arg("--program-id")
                        .arg(workspace.program_key.to_string())
                        .arg("--new-authority")
                        .arg(&workspace.network_config.upgrade_authority)
                        .arg("--provider.cluster")
                        .arg(workspace.network.as_ref())
                        .arg("--provider.wallet")
                        .arg(&workspace.deployer_path),
                )?;
            }

            output_header("Copying artifacts");
            workspace.copy_artifacts()?;

            println!("Deployment success!");
        }
        SubCommand::Upgrade {
            version,
            program,
            ref network,
        } => {
            let upgrade_authority_keypair =
                env::var("UPGRADE_AUTHORITY_KEYPAIR").map_err(|_| {
                    format_err!("Must set UPGRADE_AUTHORITY_KEYPAIR environment variable.")
                })?;

            let workspace = workspace::init(program.as_str(), version, network.clone())?;
            println!(
                "Upgrading program {} with version {}",
                program, workspace.deploy_version
            );

            if workspace.artifact_paths.exist() {
                return Err(anyhow!("Program artifacts already exist for this version. Make sure to bump your Cargo.toml."));
            }

            if !workspace.show_program()? {
                println!("Program does not exist. Use `fleet deploy` if you want to deploy the program for the first time.");
                std::process::exit(1);
            }

            output_header("Writing buffer");

            let buffer_kp = solana_sdk::signer::keypair::Keypair::generate(&mut OsRng);
            let buffer_key = buffer_kp.pubkey();
            println!("Buffer Pubkey: {}", buffer_key);

            let mut buffer_file = NamedTempFile::new()?;
            solana_sdk::signer::keypair::write_keypair(&buffer_kp, &mut buffer_file)
                .map_err(|_| format_err!("could not generate temp buffer keypair"))?;

            command::exec(
                std::process::Command::new("solana")
                    .arg("program")
                    .arg("write-buffer")
                    .arg(&workspace.program_paths.bin)
                    .arg("--keypair")
                    .arg(&workspace.deployer_path)
                    .arg("--output")
                    .arg("json")
                    .arg("--buffer")
                    .arg(&buffer_file.path()),
            )?;

            output_header("Setting buffer authority");

            command::exec(
                std::process::Command::new("solana")
                    .arg("program")
                    .arg("set-buffer-authority")
                    .arg(buffer_key.to_string())
                    .arg("--keypair")
                    .arg(&workspace.deployer_path)
                    .arg("--new-buffer-authority")
                    .arg(&workspace.network_config.upgrade_authority),
            )?;

            output_header("Switching to new buffer (please connect your wallet)");

            command::exec(
                std::process::Command::new("solana")
                    .arg("program")
                    .arg("deploy")
                    .arg("--buffer")
                    .arg(buffer_key.to_string())
                    .arg("--keypair")
                    .arg(&upgrade_authority_keypair)
                    .arg("--program-id")
                    .arg(workspace.program_key.to_string()),
            )?;

            workspace.show_program()?;

            if workspace.has_anchor() {
                output_header("Uploading new IDL");
                command::exec(
                    std::process::Command::new("anchor")
                        .arg("idl")
                        .arg("write-buffer")
                        .arg(workspace.program_key.to_string())
                        .arg("--filepath")
                        .arg(&workspace.program_paths.idl)
                        .arg("--provider.cluster")
                        .arg(workspace.network.to_string())
                        .arg("--provider.wallet")
                        .arg(&workspace.deployer_path),
                )?;

                println!(
                    "WARNING: please manually run `anchor idl set-buffer {} --buffer <BUFFER>`",
                    workspace.program_key.to_string()
                );
                println!("TODO: need to be able to hook into anchor for this");
            }

            output_header("Copying artifacts");
            workspace.copy_artifacts()?;

            println!("Deployment success!");
        }
    }

    Ok(())
}

fn output_header(header: &'static str) {
    println!();
    println!("{}", "===================================".bold());
    println!();
    println!("    {}", header.bold());
    println!();
    println!("{}", "===================================".bold());
    println!();
}

fn main() {
    if let Err(err) = main_with_result() {
        println!("Error: {}", err);
        std::process::exit(1);
    }
}

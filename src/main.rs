use std::{
    error::Error,
    io::Read,
    path::Path,
    process::{Command, Stdio},
};

use clap::Parser;
use k8s_openapi::api::core::v1::Namespace;
use kube::{
    Api, Client,
    api::ListParams,
    config::{Context, Kubeconfig},
};
use skim::prelude::*;
use tempfile::NamedTempFile;

use crate::config::Generator;

mod config;
mod store;

#[derive(Parser)]
struct Opts {
    #[clap(subcommand)]
    cmd: Cmd,
}

#[derive(Parser)]
enum Cmd {
    #[clap(alias = "ls", about = "List contexts")]
    List,
    #[clap(alias = "ns", about = "Switch namespace")]
    Namespace { namespace: Option<String> },
    #[clap(alias = "ctx", about = "Use a context")]
    Use { name: Option<String> },
}

fn pick_item(items: Vec<String>) -> Result<String, Box<dyn Error>> {
    let options = SkimOptionsBuilder::default().height("50%").build()?;
    let output = Skim::run_items(options, items)?;

    if output.is_abort {
        return Err("no item chosen".into());
    }

    output
        .current
        .map(|cur| cur.item.to_string())
        .ok_or_else(|| "no item selected".into())
}

fn generate(
    store: &store::Store,
    name: &str,
    generator: &config::Generator,
) -> Result<(), Box<dyn Error>> {
    let file = NamedTempFile::new()?;
    let (mut child, writes_to_file) = match generator {
        Generator::Gcloud { project, location } => (
            Command::new("gcloud")
                .arg("container")
                .arg("clusters")
                .arg("get-credentials")
                .arg("--location")
                .arg(location)
                .arg("--project")
                .arg(project)
                .arg(name)
                .env("KUBECONFIG", file.path())
                .spawn()?,
            true,
        ),
        Generator::Aks {
            subscription,
            resource_group,
        } => (
            Command::new("az")
                .arg("aks")
                .arg("get-credentials")
                .arg("--subscription")
                .arg(subscription)
                .arg("--name")
                .arg(name)
                .arg("--resource-group")
                .arg(resource_group)
                .env("KUBECONFIG", file.path())
                .spawn()?,
            true,
        ),
        Generator::Tcloud { organisation } => (
            Command::new("tcloud")
                .stdout(Stdio::piped())
                .arg("--organisation")
                .arg(organisation)
                .arg("kubernetes")
                .arg("kubeconfig")
                .arg(name)
                .spawn()?,
            false,
        ),
    };

    if writes_to_file {
        let status = child.wait()?;

        if !status.success() {
            return Err(format!(
                "generator failed with exit code: {}",
                status.code().unwrap_or_default()
            )
            .into());
        }

        let mut file = file.into_file();
        let mut buff = vec![];

        file.read_to_end(&mut buff)?;
        store.store(name, buff)?;
    } else {
        let output = child.wait_with_output()?;

        if !output.status.success() {
            return Err(format!(
                "generator failed with exit code: {}",
                output.status.code().unwrap_or_default()
            )
            .into());
        }

        store.store(name, output.stdout)?;
    }

    Ok(())
}

fn spawn_shell(kubeconfig: impl AsRef<Path>) -> Result<(), Box<dyn Error>> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

    Command::new(shell)
        .env("KUBECONFIG", kubeconfig.as_ref())
        .env("TACK_ENABLED", "1")
        .spawn()?
        .wait()?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let opts = Opts::parse();
    let mut config = config::load_default_config()?;
    let kubeconfig_dir = config
        .kubeconfig_dir
        .take()
        .or_else(|| dirs::home_dir().map(|home| home.join(".config").join("tack")))
        .ok_or("unable to determine kubeconfig dir")?;
    let store = store::Store::new(kubeconfig_dir);

    match opts.cmd {
        Cmd::List => {
            for (name, _) in config.context {
                println!("{name}");
            }
        }
        Cmd::Namespace { namespace } => {
            let namespace = match namespace {
                Some(namespace) => namespace,
                None => pick_item({
                    let client = Client::try_default().await?;
                    let ns = Api::<Namespace>::all(client);
                    let namespaces = ns.list(&ListParams::default()).await?;

                    Ok::<_, Box<dyn Error>>(
                        namespaces
                            .items
                            .into_iter()
                            .filter_map(|ns| ns.metadata.name)
                            .collect::<Vec<_>>(),
                    )
                }?)?,
            };

            let mut config = Kubeconfig::from_env()?.ok_or("unable to load kubeconfig")?;

            if let Some(name) = &config.current_context
                && let Some(named_ctx) = config.contexts.iter_mut().find(|ctx| &ctx.name == name)
            {
                if named_ctx.context.is_none() {
                    named_ctx.context = Some(Context::default());
                }

                if let Some(context) = named_ctx.context.as_mut() {
                    context.namespace = Some(namespace);
                }
            }

            let path = std::env::var("KUBECONFIG")
                .map_err(|_| "KUBECONFIG is not set, use `tack use` first")?;

            let yaml = serde_yaml::to_string(&config)?;

            if path.ends_with(".tmp.yml") {
                store::write_restricted(&path, yaml.as_bytes())?;
            } else {
                let file = NamedTempFile::with_suffix(".tmp.yml")?;
                store::write_restricted(file.path(), yaml.as_bytes())?;
                let (_, path) = file.keep()?;

                let result = spawn_shell(&path);
                let _ = std::fs::remove_file(&path);
                result?;
            }
        }
        Cmd::Use { name } => {
            let name = match name {
                Some(name) => name,
                None => pick_item(config.context.keys().cloned().collect::<Vec<_>>())?,
            };

            let ctx = config.context.get(&name).ok_or("non-existing context")?;

            if !store.contains(&name) {
                match &ctx.generator {
                    Some(generator) => {
                        println!("# generating kubeconfig for {name}");
                        generate(&store, &name, generator)?
                    }
                    None => return Err("kubeconfig not found and no generator configured".into()),
                };
            }

            let kubeconfig = store
                .kubeconfig(&name)
                .ok_or("unable to determine kubeconfig")?;

            spawn_shell(kubeconfig)?;
        }
    }

    Ok(())
}

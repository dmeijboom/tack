use std::{error::Error, ffi::OsString, path::Path, process::Command};

use clap::Parser;
use k8s_openapi::api::core::v1::Namespace;
use kube::{
    Api,
    api::ListParams,
    config::{Context, Kubeconfig},
};
use skim::prelude::*;
use tempfile::NamedTempFile;

use crate::recovery::Recovery;

mod config;
mod recovery;
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

fn spawn_shell(
    kubeconfig: impl AsRef<Path>,
    shell: Option<&Path>,
    context: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let shell = shell
        .map(|shell| shell.as_os_str().to_os_string())
        .or_else(|| std::env::var_os("SHELL"))
        .unwrap_or_else(|| OsString::from("/bin/sh"));

    let mut command = Command::new(&shell);
    command
        .env("KUBECONFIG", kubeconfig.as_ref())
        .env("TACK_ENABLED", "1");
    if let Some(context) = context {
        command.env("TACK_CONTEXT", context);
    }
    command.spawn()?.wait()?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let opts = Opts::parse();
    let mut config = config::load_default_config()?;
    let shell = config.shell.clone();
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
            let path = std::env::var("KUBECONFIG")
                .map_err(|_| "KUBECONFIG is not set, use `tack use` first")?;
            let path = Path::new(&path);
            let name = std::env::var("TACK_CONTEXT")
                .ok()
                .filter(|name| config.context.contains_key(name))
                .or_else(|| {
                    config.context.keys().find_map(|name| {
                        (store.kubeconfig(name).as_deref() == Some(path)).then(|| name.clone())
                    })
                });
            let generator = name
                .as_ref()
                .and_then(|name| config.context.get(name))
                .and_then(|context| context.generator.as_ref());
            let mut recovery = Recovery::new(&store, name.as_deref(), generator, path);
            recovery.check_access().await?;
            let namespace = match namespace {
                Some(namespace) => namespace,
                None => pick_item({
                    let namespaces = recovery
                        .with_client(|client| async move {
                            Api::<Namespace>::all(client)
                                .list(&ListParams::default())
                                .await
                        })
                        .await?;

                    Ok::<_, Box<dyn Error>>(
                        namespaces
                            .items
                            .into_iter()
                            .filter_map(|ns| ns.metadata.name)
                            .collect::<Vec<_>>(),
                    )
                }?)?,
            };

            let mut config = Kubeconfig::read_from(path)?;

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

            let yaml = serde_yaml::to_string(&config)?;

            if path.as_os_str().as_encoded_bytes().ends_with(b".tmp.yml") {
                store::write_restricted(path, yaml.as_bytes())?;
            } else {
                let file = NamedTempFile::with_suffix(".tmp.yml")?;
                store::write_restricted(file.path(), yaml.as_bytes())?;
                let (_, path) = file.keep()?;

                let result = spawn_shell(&path, shell.as_deref(), name.as_deref());
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

            let kubeconfig = store
                .kubeconfig(&name)
                .ok_or("unable to determine kubeconfig")?;
            let mut recovery =
                Recovery::new(&store, Some(&name), ctx.generator.as_ref(), &kubeconfig);
            if !store.contains(&name) {
                if ctx.generator.is_none() {
                    return Err("kubeconfig not found and no generator configured".into());
                }
                println!("# generating kubeconfig for {name}");
                recovery.generate()?;
            }
            recovery.check_access().await?;

            spawn_shell(kubeconfig, shell.as_deref(), Some(&name))?;
        }
    }

    Ok(())
}

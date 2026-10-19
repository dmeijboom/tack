use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::IsTerminal,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

use clap::{CommandFactory, FromArgMatches, Parser, error::ErrorKind};
use k8s_openapi::api::core::v1::Namespace;
use kube::{Api, api::ListParams, config::Kubeconfig};
use skim::prelude::*;
use tempfile::NamedTempFile;

use crate::{
    error::{AppError, Result},
    output::{Data, Mode},
    recovery::Recovery,
};

mod config;
mod error;
mod output;
mod recovery;
mod store;

#[derive(Debug, Parser)]
#[command(version, about = "Isolated Kubernetes contexts and namespaces")]
struct Opts {
    #[arg(
        long,
        global = true,
        help = "Emit one JSON result without prompts or shells"
    )]
    json: bool,
    #[clap(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Parser)]
enum Cmd {
    #[clap(alias = "ls", about = "List contexts")]
    List,
    #[clap(alias = "ns", about = "Switch namespace")]
    Namespace {
        #[arg(value_parser = namespace_name)]
        namespace: Option<String>,
    },
    #[clap(alias = "ctx", about = "Use a context")]
    Use { name: Option<String> },
}

enum Invocation {
    Run(Opts),
    Display(Data),
}

fn parse(args: Vec<OsString>) -> (Mode, Result<Invocation>) {
    let json = args
        .iter()
        .skip(1)
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--json");
    let mode = if json { Mode::Json } else { Mode::Human };
    let command = Opts::command().color(if json {
        clap::ColorChoice::Never
    } else {
        clap::ColorChoice::Auto
    });
    let result = match command.try_get_matches_from(args) {
        Ok(matches) => Opts::from_arg_matches(&matches)
            .map(Invocation::Run)
            .map_err(output::invalid_arguments),
        Err(error) => match error.kind() {
            ErrorKind::DisplayHelp => Ok(Invocation::Display(Data::Help {
                text: error.to_string(),
            })),
            ErrorKind::DisplayVersion => Ok(Invocation::Display(Data::Version {
                version: env!("CARGO_PKG_VERSION").into(),
            })),
            _ => Err(output::invalid_arguments(error)),
        },
    };
    (mode, result)
}

fn namespace_name(value: &str) -> std::result::Result<String, String> {
    let alphanumeric = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let bytes = value.as_bytes();
    if value.len() <= 63
        && bytes.first().is_some_and(alphanumeric)
        && bytes.last().is_some_and(alphanumeric)
        && bytes.iter().all(|byte| alphanumeric(byte) || *byte == b'-')
    {
        Ok(value.into())
    } else {
        Err("namespace must be 1–63 lowercase letters, digits, or hyphens, starting and ending with a letter or digit".into())
    }
}

fn pick_item(items: Vec<String>) -> Result<String> {
    if items.is_empty() {
        return Err(AppError::new(
            "selection_empty",
            "No items are available to select",
        ));
    }
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(AppError::new(
            "selection_required",
            "Interactive selection requires a terminal",
        )
        .help("Provide an explicit context/namespace, or use --json to list choices."));
    }
    let options = SkimOptionsBuilder::default()
        .height("50%")
        .build()
        .map_err(|source| {
            AppError::new("selection_failed", "Unable to initialize selection").caused_by(source)
        })?;
    let output = Skim::run_items(options, items).map_err(|source| {
        AppError::new("selection_failed", "Unable to display selection")
            .detail(format!("{source:#}"))
    })?;
    if output.is_abort {
        return Err(AppError::new("selection_cancelled", "Selection cancelled").with_exit_code(130));
    }
    output
        .current
        .map(|cur| cur.item.to_string())
        .ok_or_else(|| AppError::new("selection_cancelled", "No item selected").with_exit_code(130))
}

fn spawn_shell(kubeconfig: &Path, shell: Option<&Path>, context: Option<&str>) -> Result<()> {
    let shell = shell
        .map(|shell| shell.as_os_str().to_os_string())
        .or_else(|| std::env::var_os("SHELL"))
        .unwrap_or_else(|| OsString::from("/bin/sh"));
    let mut command = Command::new(&shell);
    command
        .env("KUBECONFIG", kubeconfig)
        .env("TACK_ENABLED", "1");
    if let Some(context) = context {
        command.env("TACK_CONTEXT", context);
    }
    let status = command
        .status()
        .map_err(|source| AppError::command_start(&command, source))?;
    if !status.success() {
        let mut error = AppError::command_failed(&command, status, String::new());
        error.code = "shell_failed";
        error.exit_code = status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or(1);
        return Err(error);
    }
    Ok(())
}

fn read_kubeconfig(path: &Path) -> Result<Kubeconfig> {
    Kubeconfig::read_from(path).map_err(|source| AppError::kubeconfig(path, source))
}

fn active_context(config: &Kubeconfig) -> Result<&kube::config::Context> {
    config
        .contexts
        .iter()
        .find(|context| Some(&context.name) == config.current_context.as_ref())
        .and_then(|context| context.context.as_ref())
        .ok_or_else(|| {
            AppError::new(
                "kubeconfig_invalid",
                "Kubeconfig has no valid current context",
            )
            .help("Select a valid context with `tack use <context>`.")
        })
}

fn environment(path: &Path, context: Option<&str>, cleanup: bool) -> Result<Data> {
    let config = read_kubeconfig(path)?;
    let namespace = active_context(&config)?
        .namespace
        .clone()
        .unwrap_or_else(|| "default".into());
    let absolute = path.canonicalize().map_err(|source| {
        AppError::io(
            "kubeconfig_read_failed",
            "Unable to resolve kubeconfig path",
            path,
            source,
        )
    })?;
    let kubeconfig = absolute
        .to_str()
        .ok_or_else(|| {
            AppError::new(
                "path_invalid",
                "JSON output requires a UTF-8 kubeconfig path",
            )
            .at_path(path)
        })?
        .to_owned();
    let mut env = BTreeMap::from([
        ("KUBECONFIG".into(), kubeconfig.clone()),
        ("TACK_ENABLED".into(), "1".into()),
    ]);
    if let Some(context) = context {
        env.insert("TACK_CONTEXT".into(), context.into());
    }
    Ok(Data::Environment {
        context: context.map(String::from),
        namespace,
        cleanup: if cleanup {
            vec![kubeconfig.clone()]
        } else {
            vec![]
        },
        kubeconfig,
        env,
    })
}

fn temporary_kubeconfig(config: &Kubeconfig) -> Result<NamedTempFile> {
    let yaml = serde_yaml::to_string(config).map_err(|source| {
        AppError::new(
            "kubeconfig_invalid",
            "Unable to serialize namespace kubeconfig",
        )
        .caused_by(source)
    })?;
    let file = NamedTempFile::with_suffix(".tmp.yml").map_err(|source| {
        AppError::new(
            "temporary_file_failed",
            "Unable to create a namespace kubeconfig",
        )
        .help("Check that the temporary directory exists and is writable.")
        .caused_by(source)
    })?;
    store::write_restricted(file.path(), yaml.as_bytes()).map_err(|source| {
        AppError::io(
            "kubeconfig_write_failed",
            "Unable to write namespace kubeconfig",
            file.path(),
            source,
        )
    })?;
    Ok(file)
}

async fn run(opts: Opts) -> Result<Data> {
    let mode = if opts.json { Mode::Json } else { Mode::Human };
    let mut config = config::load_default_config()?;
    let mut contexts = config.context.keys().cloned().collect::<Vec<_>>();
    contexts.sort();
    if matches!(opts.cmd, Cmd::List)
        || (mode.is_json() && matches!(opts.cmd, Cmd::Use { name: None }))
    {
        return Ok(Data::Contexts { contexts });
    }
    let shell = config.shell.clone();
    let kubeconfig_dir = config
        .kubeconfig_dir
        .take()
        .or_else(|| dirs::home_dir().map(|home| home.join(".config").join("tack")))
        .ok_or_else(|| {
            AppError::new(
                "home_unavailable",
                "Unable to determine the kubeconfig directory",
            )
        })?;
    let store = store::Store::new(kubeconfig_dir);
    match opts.cmd {
        Cmd::List => Ok(Data::Contexts { contexts }),
        Cmd::Use { name } => {
            let name = match name {
                Some(name) => name,
                None => pick_item(contexts)?,
            };
            let kubeconfig = store.kubeconfig(&name).ok_or_else(|| {
                AppError::new("context_invalid", format!("Invalid context name {name:?}")).help(
                    "Context names must contain only letters, numbers, hyphens, and underscores.",
                )
            })?;
            let ctx = config.context.get(&name).ok_or_else(|| {
                AppError::new(
                    "context_not_found",
                    format!("Context {name:?} is not configured"),
                )
                .help("Run `tack list` or `tack --json list` to see configured contexts.")
            })?;
            let mut recovery = Recovery::new(
                &store,
                Some(&name),
                ctx.generator.as_ref(),
                &kubeconfig,
                mode,
            );
            if !store.contains(&name) {
                if ctx.generator.is_none() {
                    return Err(AppError::new(
                        "kubeconfig_missing",
                        format!("No kubeconfig or generator for {name}"),
                    )
                    .at_path(&kubeconfig)
                    .help("Configure a generator or provide the kubeconfig at this path."));
                }
                mode.progress(format_args!("# generating kubeconfig for {name}"));
                recovery.generate()?;
            }
            recovery.check_access().await?;
            if mode.is_json() {
                return environment(&kubeconfig, Some(&name), false);
            }
            spawn_shell(&kubeconfig, shell.as_deref(), Some(&name))?;
            Ok(Data::Completed)
        }
        Cmd::Namespace { namespace } => {
            let path = std::env::var_os("KUBECONFIG").map(PathBuf::from).ok_or_else(|| {
                AppError::new("context_required", "KUBECONFIG is not set")
                    .help("Use `tack use <context>` first, or supply KUBECONFIG from `tack --json use <context>`.")
            })?;
            if std::env::split_paths(&path).count() != 1 || path.as_os_str().is_empty() {
                return Err(AppError::new(
                    "kubeconfig_invalid",
                    "KUBECONFIG must name a single kubeconfig file",
                )
                .help("Select a tack context or set KUBECONFIG to one file."));
            }
            let name = std::env::var("TACK_CONTEXT")
                .ok()
                .filter(|name| config.context.contains_key(name))
                .or_else(|| {
                    config.context.keys().find_map(|name| {
                        (store.kubeconfig(name).as_deref() == Some(path.as_path()))
                            .then(|| name.clone())
                    })
                });
            let generator = name
                .as_ref()
                .and_then(|name| config.context.get(name))
                .and_then(|context| context.generator.as_ref());
            let mut recovery = Recovery::new(&store, name.as_deref(), generator, &path, mode);
            recovery.check_access().await?;
            let namespace = match namespace {
                Some(namespace) => namespace,
                None => {
                    let namespaces = recovery
                        .with_client(|client| async move {
                            Api::<Namespace>::all(client)
                                .list(&ListParams::default())
                                .await
                        })
                        .await?;
                    let mut namespaces = namespaces
                        .items
                        .into_iter()
                        .filter_map(|namespace| namespace.metadata.name)
                        .collect::<Vec<_>>();
                    namespaces.sort();
                    if mode.is_json() {
                        let kubeconfig = read_kubeconfig(&path)?;
                        return Ok(Data::Namespaces {
                            context: name,
                            current_namespace: active_context(&kubeconfig)?
                                .namespace
                                .clone()
                                .unwrap_or_else(|| "default".into()),
                            namespaces,
                        });
                    }
                    pick_item(namespaces)?
                }
            };
            let mut kubeconfig = read_kubeconfig(&path)?;
            active_context(&kubeconfig)?;
            if let Some(context) = kubeconfig
                .contexts
                .iter_mut()
                .find(|context| Some(&context.name) == kubeconfig.current_context.as_ref())
                .and_then(|context| context.context.as_mut())
            {
                context.namespace = Some(namespace);
            }
            if !mode.is_json() && path.as_os_str().as_encoded_bytes().ends_with(b".tmp.yml") {
                let yaml = serde_yaml::to_string(&kubeconfig).map_err(|source| {
                    AppError::new(
                        "kubeconfig_invalid",
                        "Unable to serialize namespace kubeconfig",
                    )
                    .caused_by(source)
                })?;
                store::write_restricted(&path, yaml.as_bytes()).map_err(|source| {
                    AppError::io(
                        "kubeconfig_write_failed",
                        "Unable to switch namespace",
                        &path,
                        source,
                    )
                })?;
                return Ok(Data::Completed);
            }
            let file = temporary_kubeconfig(&kubeconfig)?;
            if mode.is_json() {
                let data = environment(file.path(), name.as_deref(), true)?;
                file.keep().map_err(|source| {
                    AppError::new(
                        "temporary_file_failed",
                        "Unable to retain namespace kubeconfig",
                    )
                    .caused_by(source.error)
                })?;
                return Ok(data);
            }
            spawn_shell(file.path(), shell.as_deref(), name.as_deref())?;
            Ok(Data::Completed)
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let (mode, invocation) = parse(std::env::args_os().collect());
    let result = match invocation {
        Ok(Invocation::Run(opts)) => run(opts).await,
        Ok(Invocation::Display(data)) => Ok(data),
        Err(error) => Err(error),
    };
    mode.finish(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn json_is_global_and_argument_errors_stay_machine_readable() {
        for argv in [
            vec!["tack", "--json", "list"],
            vec!["tack", "list", "--json"],
            vec!["tack", "use", "cluster", "--json"],
        ] {
            let (mode, result) = parse(args(&argv));
            assert_eq!(mode, Mode::Json);
            assert!(matches!(
                result.unwrap(),
                Invocation::Run(Opts { json: true, .. })
            ));
        }
        let (mode, result) = parse(args(&["tack", "--json", "--invalid"]));
        assert_eq!(mode, Mode::Json);
        let error = result.err().unwrap();
        assert_eq!(error.code, "invalid_arguments");
        assert_eq!(error.exit_code, 2);
        assert!(!error.json().to_string().contains("\\u001b"));
    }

    #[test]
    fn json_help_and_version_are_successful_results() {
        for flag in ["--help", "--version"] {
            let (mode, result) = parse(args(&["tack", "--json", flag]));
            assert_eq!(mode, Mode::Json);
            assert!(matches!(result.unwrap(), Invocation::Display(_)));
        }
    }

    #[test]
    fn positional_json_after_separator_is_not_a_flag() {
        let (mode, result) = parse(args(&["tack", "use", "--", "--json"]));
        assert_eq!(mode, Mode::Human);
        assert!(
            matches!(result.unwrap(), Invocation::Run(Opts { cmd: Cmd::Use { name: Some(name) }, .. }) if name == "--json")
        );
    }

    #[test]
    fn environment_result_contains_paths_not_credentials_and_identifies_cleanup() {
        use std::os::unix::fs::PermissionsExt;

        let config = Kubeconfig::from_yaml(
            r#"
apiVersion: v1
kind: Config
current-context: cluster
contexts:
- name: cluster
  context: {cluster: cluster, user: user, namespace: kube-system}
clusters:
- name: cluster
  cluster: {server: 'https://example.invalid'}
users:
- name: user
  user: {token: secret-token}
"#,
        )
        .unwrap();
        let file = temporary_kubeconfig(&config).unwrap();
        let data =
            serde_json::to_value(environment(file.path(), Some("cluster"), true).unwrap()).unwrap();
        assert_eq!(data["kind"], "environment");
        assert_eq!(data["namespace"], "kube-system");
        assert_eq!(data["env"]["TACK_CONTEXT"], "cluster");
        assert_eq!(data["env"]["KUBECONFIG"], data["kubeconfig"]);
        assert_eq!(data["cleanup"][0], data["kubeconfig"]);
        assert!(!data.to_string().contains("secret-token"));
        assert_eq!(
            file.as_file().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn invalid_namespace_is_rejected_before_accessing_configuration_or_network() {
        for namespace in [
            "",
            "UPPERCASE",
            "has space",
            "-prefix",
            "suffix-",
            "under_score",
        ] {
            let (_, result) = parse(args(&["tack", "--json", "namespace", namespace]));
            assert_eq!(result.err().unwrap().code, "invalid_arguments");
        }
        assert!(namespace_name("kube-system").is_ok());
        assert!(namespace_name(&"a".repeat(64)).is_err());
    }

    #[test]
    fn invalid_current_context_is_an_explicit_error() {
        assert_eq!(
            active_context(&Kubeconfig::default()).unwrap_err().code,
            "kubeconfig_invalid"
        );
    }
}

use std::{
    error::Error,
    future::Future,
    path::Path,
    process::{Command, Stdio},
};

use k8s_openapi::api::authentication::v1::SelfSubjectReview;
use kube::{
    Api, Client,
    api::PostParams,
    config::{ExecInteractiveMode, Kubeconfig},
};
use tempfile::NamedTempFile;

use crate::{config::Generator, store};

#[derive(Debug, PartialEq)]
enum Action {
    Login,
    Regenerate,
}

#[derive(Debug, Default, PartialEq)]
struct AzureLogin {
    tenant: Option<String>,
    scope: Option<String>,
}

impl AzureLogin {
    fn from_message(message: &str) -> Self {
        message
            .lines()
            .find_map(|line| {
                let args = line.split_whitespace().collect::<Vec<_>>();
                if !args.starts_with(&["az", "login"]) {
                    return None;
                }
                Some(Self {
                    tenant: argument_value(&args, "--tenant"),
                    scope: argument_value(&args, "--scope"),
                })
            })
            .unwrap_or_default()
    }
}

fn argument_value(args: &[&str], flag: &str) -> Option<String> {
    args.iter().enumerate().find_map(|(index, arg)| {
        let value = if *arg == flag {
            args.get(index + 1).copied()
        } else {
            arg.strip_prefix(flag)
                .and_then(|value| value.strip_prefix('='))
        };
        value.map(|value| value.trim_matches(['\'', '"']).to_owned())
    })
}

fn prepare_client_config(config: &mut Kubeconfig, generator: Option<&Generator>) -> AzureLogin {
    let mut azure = AzureLogin::default();
    if !matches!(
        generator,
        Some(Generator::Aks { .. } | Generator::Gcloud { .. })
    ) {
        return azure;
    }
    let user = config
        .contexts
        .iter()
        .find(|context| Some(&context.name) == config.current_context.as_ref())
        .and_then(|context| context.context.as_ref())
        .and_then(|context| context.user.as_ref());
    if let Some(exec) = config
        .auth_infos
        .iter_mut()
        .find(|auth| Some(&auth.name) == user)
        .and_then(|auth| auth.auth_info.as_mut())
        .and_then(|auth| auth.exec.as_mut())
    {
        // kube-rs inherits stderr for interactive plugins, hiding login failures from recovery.
        exec.interactive_mode = Some(ExecInteractiveMode::Never);
        if matches!(generator, Some(Generator::Aks { .. })) {
            let args = exec
                .args
                .iter()
                .flatten()
                .map(String::as_str)
                .collect::<Vec<_>>();
            azure.tenant = argument_value(&args, "--tenant-id");
            azure.scope = argument_value(&args, "--server-id")
                .map(|server| format!("{}/.default", server.trim_end_matches('/')));
        }
    }
    azure
}

pub struct Recovery<'a> {
    store: &'a store::Store,
    name: Option<&'a str>,
    generator: Option<&'a Generator>,
    kubeconfig: &'a Path,
    regenerated: bool,
    logged_in: bool,
    azure_login: AzureLogin,
}

impl<'a> Recovery<'a> {
    pub fn new(
        store: &'a store::Store,
        name: Option<&'a str>,
        generator: Option<&'a Generator>,
        kubeconfig: &'a Path,
    ) -> Self {
        Self {
            store,
            name,
            generator,
            kubeconfig,
            regenerated: false,
            logged_in: false,
            azure_login: AzureLogin::default(),
        }
    }

    fn action(&mut self, error: &(dyn Error + 'static)) -> Option<Action> {
        let generator = self.generator?;
        self.name?;
        let mut message = String::new();
        let mut denied = false;
        let mut source = Some(error);
        while let Some(error) = source {
            message.push_str(&error.to_string());
            message.push('\n');
            if let Some(kube::client::AuthError::AuthExecRun { out, .. }) =
                error.downcast_ref::<kube::client::AuthError>()
            {
                message.push_str(&String::from_utf8_lossy(&out.stderr));
                message.push('\n');
            }
            if let Some(kube::Error::Api(status)) = error.downcast_ref::<kube::Error>() {
                denied |= matches!(status.code, 401 | 403);
            }
            source = error.source();
        }
        let lowercase = message.to_lowercase();

        if login_required(generator, &lowercase) {
            if self.logged_in {
                return None;
            }
            self.logged_in = true;
            if matches!(generator, Generator::Aks { .. }) {
                let suggested = AzureLogin::from_message(&message);
                if suggested.tenant.is_some() {
                    self.azure_login.tenant = suggested.tenant;
                }
                if suggested.scope.is_some() {
                    self.azure_login.scope = suggested.scope;
                }
            }
            return Some(Action::Login);
        }

        denied |= [
            "forbidden",
            "access denied",
            "accessdenied",
            "permission denied",
            "permission_denied",
            "authorizationfailed",
            "unauthorized",
            "unauthenticated",
            "you must be logged in to the server",
            "the server has asked for the client to provide credentials",
        ]
        .iter()
        .any(|pattern| lowercase.contains(pattern));
        if denied && !self.regenerated {
            self.regenerated = true;
            return Some(Action::Regenerate);
        }
        None
    }

    fn login(&self) -> Result<(), Box<dyn Error>> {
        let generator = self.generator.ok_or("no generator configured")?;
        let mut command =
            login_command(generator, &self.azure_login).ok_or("generator has no login command")?;
        eprintln!("# authentication required; running {command:?} in the console");
        let status = command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|error| format!("unable to run {command:?}: {error}"))?;
        if !status.success() {
            return Err(format!("cloud login command {command:?} failed: {status}").into());
        }
        Ok(())
    }

    pub fn generate(&mut self) -> Result<(), Box<dyn Error>> {
        let name = self.name.ok_or("unable to determine tack context")?;
        let generator = self.generator.ok_or("no generator configured")?;
        loop {
            match generate_once(self.store, name, generator) {
                Ok(()) => return Ok(()),
                Err(error) => match self.action(error.as_ref()) {
                    Some(Action::Login) => self.login()?,
                    Some(Action::Regenerate) => {
                        eprintln!("# access denied; regenerating kubeconfig for {name} once");
                    }
                    None => return Err(error),
                },
            }
        }
    }

    fn regenerate(&mut self) -> Result<(), Box<dyn Error>> {
        let previous = Kubeconfig::read_from(self.kubeconfig)?;
        let namespace = current_namespace(&previous);
        eprintln!("# access denied; regenerating kubeconfig once");
        self.generate()?;
        let source = self
            .name
            .and_then(|name| self.store.kubeconfig(name))
            .ok_or("unable to determine kubeconfig")?;
        let mut config = Kubeconfig::read_from(source)?;
        if let Some(context) = config
            .contexts
            .iter_mut()
            .find(|context| Some(&context.name) == config.current_context.as_ref())
            .and_then(|context| context.context.as_mut())
        {
            context.namespace = namespace;
        }
        store::write_restricted(self.kubeconfig, serde_yaml::to_string(&config)?.as_bytes())?;
        Ok(())
    }

    pub async fn check_access(&mut self) -> Result<(), Box<dyn Error>> {
        self.with_client(|client| async move {
            let review = Api::<SelfSubjectReview>::all(client.clone())
                .create(&PostParams::default(), &SelfSubjectReview::default())
                .await?;
            verify_identity(&review)?;
            client.list_core_api_versions().await?;
            client.list_api_groups().await?;
            Ok(())
        })
        .await
    }

    pub async fn with_client<T, F, Fut>(&mut self, mut operation: F) -> Result<T, Box<dyn Error>>
    where
        F: FnMut(Client) -> Fut,
        Fut: Future<Output = Result<T, kube::Error>>,
    {
        loop {
            let result = async {
                let mut kubeconfig = Kubeconfig::read_from(self.kubeconfig)?;
                self.azure_login = prepare_client_config(&mut kubeconfig, self.generator);
                let config =
                    kube::Config::from_custom_kubeconfig(kubeconfig, &Default::default()).await?;
                let client = Client::try_from(config)?;
                Ok::<_, Box<dyn Error>>(operation(client).await?)
            }
            .await;
            match result {
                Ok(value) => return Ok(value),
                Err(error) => match self.action(error.as_ref()) {
                    Some(Action::Login) => self.login()?,
                    Some(Action::Regenerate) => self.regenerate()?,
                    None => return Err(error),
                },
            }
        }
    }
}

fn verify_identity(review: &SelfSubjectReview) -> Result<(), kube::Error> {
    if let Some(user) = review
        .status
        .as_ref()
        .and_then(|status| status.user_info.as_ref())
        && user
            .username
            .as_deref()
            .is_some_and(|name| !name.is_empty() && name != "system:anonymous")
        && !user
            .groups
            .iter()
            .flatten()
            .any(|group| group == "system:unauthenticated")
    {
        return Ok(());
    }
    Err(kube::Error::Api(
        kube::core::Status::failure(
            "Kubernetes did not authenticate the configured credentials",
            "Unauthorized",
        )
        .with_code(401)
        .boxed(),
    ))
}

fn current_namespace(config: &Kubeconfig) -> Option<String> {
    config
        .contexts
        .iter()
        .find(|context| Some(&context.name) == config.current_context.as_ref())
        .and_then(|context| context.context.as_ref())
        .and_then(|context| context.namespace.clone())
}

fn login_required(generator: &Generator, message: &str) -> bool {
    let patterns: &[&str] = match generator {
        Generator::Gcloud { .. } => &[
            "gcloud auth login",
            "reauthentication is needed",
            "reauthentication is required",
            "reauthentication required",
            "invalid_grant",
            "refresh token has expired",
            "do not currently have an active account",
        ],
        Generator::Aks { .. } => &[
            "az login",
            "interaction_required",
            "aadsts50058",
            "aadsts50076",
            "aadsts50079",
            "aadsts700082",
            "aadsts70043",
        ],
        Generator::Tcloud { .. } => &[],
    };
    patterns.iter().any(|pattern| message.contains(pattern))
}

fn gcloud_settings(command: &mut Command, generator: &Generator) {
    if let Generator::Gcloud {
        project,
        account,
        configuration,
        ..
    } = generator
    {
        command.arg("--project").arg(project);
        if let Some(account) = account {
            command.arg("--account").arg(account);
        }
        if let Some(configuration) = configuration {
            command.arg("--configuration").arg(configuration);
        }
    }
}

fn login_command(generator: &Generator, azure: &AzureLogin) -> Option<Command> {
    match generator {
        Generator::Gcloud { .. } => {
            let mut command = Command::new("gcloud");
            command.args(["auth", "login", "--force"]);
            gcloud_settings(&mut command, generator);
            Some(command)
        }
        Generator::Aks {
            subscription,
            tenant,
            ..
        } => {
            let mut command = Command::new("az");
            command.arg("login").arg("--subscription").arg(subscription);
            if let Some(tenant) = tenant.as_ref().or(azure.tenant.as_ref()) {
                command.arg("--tenant").arg(tenant);
            }
            if let Some(scope) = &azure.scope {
                command.arg("--scope").arg(scope);
            }
            Some(command)
        }
        Generator::Tcloud { .. } => None,
    }
}

fn generator_command(generator: &Generator, name: &str, path: &Path) -> (Command, bool) {
    match generator {
        Generator::Gcloud { location, .. } => {
            let mut command = Command::new("gcloud");
            command
                .args(["container", "clusters", "get-credentials"])
                .arg("--location")
                .arg(location)
                .arg(name)
                .env("KUBECONFIG", path)
                .env("CLOUDSDK_CORE_DISABLE_PROMPTS", "1");
            gcloud_settings(&mut command, generator);
            (command, false)
        }
        Generator::Aks {
            subscription,
            resource_group,
            ..
        } => {
            let mut command = Command::new("az");
            command
                .args(["aks", "get-credentials"])
                .arg("--subscription")
                .arg(subscription)
                .arg("--name")
                .arg(name)
                .arg("--resource-group")
                .arg(resource_group)
                .arg("--file")
                .arg(path);
            (command, false)
        }
        Generator::Tcloud { organisation } => {
            let mut command = Command::new("tcloud");
            command
                .arg("--organisation")
                .arg(organisation)
                .args(["kubernetes", "kubeconfig"])
                .arg(name);
            (command, true)
        }
    }
}

fn generate_once(
    store: &store::Store,
    name: &str,
    generator: &Generator,
) -> Result<(), Box<dyn Error>> {
    let file = NamedTempFile::new()?;
    let (mut command, stdout_config) = generator_command(generator, name, file.path());
    let output = command
        .output()
        .map_err(|error| format!("unable to run {command:?}: {error}"))?;
    if !output.status.success() {
        let stdout = if stdout_config {
            String::new()
        } else {
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        return Err(format!(
            "generator command {command:?} failed ({}): {}{}",
            output.status,
            stdout,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    let content = if stdout_config {
        output.stdout
    } else {
        print!("{}", String::from_utf8_lossy(&output.stdout));
        std::fs::read(file.path())?
    };
    if content.is_empty() {
        return Err("generator returned an empty kubeconfig".into());
    }
    Kubeconfig::from_yaml(&String::from_utf8_lossy(&content))?;
    store.store(name, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsStr, os::unix::process::ExitStatusExt, process::Output};

    fn gcloud() -> Generator {
        Generator::Gcloud {
            project: "project-id".into(),
            location: "europe-west1".into(),
            account: Some("user@example.com".into()),
            configuration: Some("work".into()),
        }
    }

    fn aks() -> Generator {
        Generator::Aks {
            subscription: "subscription-id".into(),
            resource_group: "resource-group".into(),
            tenant: Some("tenant-id".into()),
        }
    }

    fn azure_kubeconfig() -> Kubeconfig {
        Kubeconfig::from_yaml(
            r#"
apiVersion: v1
kind: Config
current-context: cluster
contexts:
- name: cluster
  context:
    cluster: cluster
    user: user
clusters:
- name: cluster
  cluster:
    server: http://127.0.0.1:1
users:
- name: user
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      command: kubelogin
      interactiveMode: IfAvailable
      args: [get-token, --login, azurecli, --tenant-id, kube-tenant, --server-id, kube-server]
"#,
        )
        .unwrap()
    }

    #[test]
    fn cloud_checks_capture_stderr_without_changing_stored_exec_settings() {
        let original = azure_kubeconfig();
        let mut prepared = original.clone();
        let settings = prepare_client_config(&mut prepared, Some(&aks()));
        assert_eq!(settings.tenant.as_deref(), Some("kube-tenant"));
        assert_eq!(settings.scope.as_deref(), Some("kube-server/.default"));
        assert_eq!(
            prepared.auth_infos[0]
                .auth_info
                .as_ref()
                .unwrap()
                .exec
                .as_ref()
                .unwrap()
                .interactive_mode,
            Some(ExecInteractiveMode::Never)
        );
        assert_eq!(
            original.auth_infos[0]
                .auth_info
                .as_ref()
                .unwrap()
                .exec
                .as_ref()
                .unwrap()
                .interactive_mode,
            Some(ExecInteractiveMode::IfAvailable)
        );
        let mut unmanaged = original.clone();
        prepare_client_config(&mut unmanaged, None);
        assert_eq!(
            serde_yaml::to_string(&unmanaged).unwrap(),
            serde_yaml::to_string(&original).unwrap()
        );
    }

    #[tokio::test]
    async fn azure_exec_failure_preserves_login_hints_and_retries_only_once() {
        let mut generator = aks();
        if let Generator::Aks { tenant, .. } = &mut generator {
            *tenant = None;
        }
        let mut kubeconfig = azure_kubeconfig();
        let settings = prepare_client_config(&mut kubeconfig, Some(&generator));
        let exec = kubeconfig.auth_infos[0]
            .auth_info
            .as_mut()
            .unwrap()
            .exec
            .as_mut()
            .unwrap();
        exec.command = Some("/bin/sh".into());
        exec.args = Some(vec![
            "-c".into(),
            "printf '%s\\n' 'Error: AzureCLICredential: AADSTS70043: The refresh token has expired' 'az logout' 'az login --tenant \"error-tenant\" --scope \"CaseSensitiveResource/.default\"' >&2; exit 1".into(),
        ]);
        let config = kube::Config::from_custom_kubeconfig(kubeconfig, &Default::default())
            .await
            .unwrap();
        let error = Client::try_from(config).err().unwrap();
        let store = store::Store::new("unused".into());
        let mut recovery = Recovery::new(
            &store,
            Some("cluster"),
            Some(&generator),
            Path::new("unused"),
        );
        recovery.azure_login = settings;
        assert_eq!(recovery.action(&error), Some(Action::Login));
        assert_eq!(recovery.azure_login.tenant.as_deref(), Some("error-tenant"));
        assert_eq!(
            recovery.azure_login.scope.as_deref(),
            Some("CaseSensitiveResource/.default")
        );
        let login = login_command(&generator, &recovery.azure_login).unwrap();
        assert_eq!(
            args(&login),
            [
                "login",
                "--subscription",
                "subscription-id",
                "--tenant",
                "error-tenant",
                "--scope",
                "CaseSensitiveResource/.default"
            ]
        );
        assert_eq!(recovery.action(&error), None);
    }

    #[test]
    fn azure_login_uses_configured_tenant_and_only_supported_suggested_flags() {
        let generator = aks();
        let settings = AzureLogin::from_message(
            "az logout\naz login --tenant=other-tenant --scope='Resource/.default' --username ignored",
        );
        let login = login_command(&generator, &settings).unwrap();
        assert_eq!(
            args(&login),
            [
                "login",
                "--subscription",
                "subscription-id",
                "--tenant",
                "tenant-id",
                "--scope",
                "Resource/.default"
            ]
        );
    }

    #[test]
    fn preflight_requires_an_authenticated_identity() {
        use k8s_openapi::api::authentication::v1::{SelfSubjectReviewStatus, UserInfo};

        let store = store::Store::new("unused".into());
        let generator = Generator::Tcloud {
            organisation: "org".into(),
        };
        for (username, groups, authenticated) in [
            (None, vec![], false),
            (Some(""), vec![], false),
            (Some("system:anonymous"), vec![], false),
            (
                Some("system:anonymous"),
                vec!["system:unauthenticated"],
                false,
            ),
            (Some("user"), vec!["system:unauthenticated"], false),
            (Some("user"), vec!["system:authenticated"], true),
            (Some("user"), vec![], true),
            (Some("system:serviceaccount:default:app"), vec![], true),
        ] {
            let review = SelfSubjectReview {
                status: Some(SelfSubjectReviewStatus {
                    user_info: Some(UserInfo {
                        username: username.map(String::from),
                        groups: Some(groups.into_iter().map(String::from).collect()),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            };
            let result = verify_identity(&review);
            assert_eq!(result.is_ok(), authenticated, "{username:?}");
            if let Err(error) = result {
                assert!(matches!(&error, kube::Error::Api(status) if status.code == 401));
                let mut recovery = Recovery::new(
                    &store,
                    Some("cluster"),
                    Some(&generator),
                    Path::new("unused"),
                );
                assert_eq!(recovery.action(&error), Some(Action::Regenerate));
                assert_eq!(recovery.action(&error), None);
            }
        }
        assert!(verify_identity(&SelfSubjectReview::default()).is_err());
        assert!(
            verify_identity(&SelfSubjectReview {
                status: Some(SelfSubjectReviewStatus::default()),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn recognizes_provider_login_requirements() {
        for (generator, message) in [
            (gcloud(), "Please run `gcloud auth login`"),
            (gcloud(), "Reauthentication is needed. Cannot prompt."),
            (gcloud(), "invalid_grant: Token has been revoked"),
            (
                gcloud(),
                "You do not currently have an active account selected",
            ),
            (aks(), "Please run 'az login' to setup account."),
            (aks(), "AADSTS700082: The refresh token has expired"),
            (
                aks(),
                "AADSTS70043: The refresh token has expired due to conditional access",
            ),
            (aks(), "AADSTS50076: Multi-factor authentication required"),
        ] {
            assert!(
                login_required(&generator, &message.to_lowercase()),
                "{message}"
            );
        }
        assert!(!login_required(&gcloud(), "az login"));
        assert!(!login_required(&aks(), "gcloud auth login"));
        assert!(!login_required(&gcloud(), "connection refused"));
    }

    #[test]
    fn recovery_is_bounded_and_login_takes_priority() {
        let store = store::Store::new("unused".into());
        let generator = gcloud();
        let mut recovery = Recovery::new(
            &store,
            Some("cluster"),
            Some(&generator),
            Path::new("unused"),
        );
        let login = std::io::Error::other("Forbidden: run gcloud auth login");
        let denied = std::io::Error::other("ACCESS DENIED");
        assert_eq!(recovery.action(&login), Some(Action::Login));
        assert_eq!(recovery.action(&login), None);
        assert_eq!(recovery.action(&denied), Some(Action::Regenerate));
        assert_eq!(recovery.action(&denied), None);
    }

    #[test]
    fn recognizes_denials_and_ignores_unrelated_errors() {
        let store = store::Store::new("unused".into());
        let generator = aks();
        for message in [
            "Forbidden",
            "AccessDenied",
            "Permission denied",
            "PERMISSION_DENIED",
            "AuthorizationFailed",
            "Unauthorized",
            "You must be logged in to the server",
            "the server has asked for the client to provide credentials",
        ] {
            let mut recovery = Recovery::new(
                &store,
                Some("cluster"),
                Some(&generator),
                Path::new("unused"),
            );
            assert_eq!(
                recovery.action(&std::io::Error::other(message)),
                Some(Action::Regenerate)
            );
            assert_eq!(recovery.action(&std::io::Error::other(message)), None);
        }
        let mut recovery = Recovery::new(
            &store,
            Some("cluster"),
            Some(&generator),
            Path::new("unused"),
        );
        assert_eq!(
            recovery.action(&std::io::Error::other("connection refused")),
            None
        );
        assert_eq!(
            recovery.action(&std::io::Error::other("malformed kubeconfig")),
            None
        );
    }

    #[test]
    fn inspects_api_status_and_nested_exec_plugin_errors() {
        let store = store::Store::new("unused".into());
        let generator = aks();
        for code in [401, 403] {
            let mut recovery = Recovery::new(
                &store,
                Some("cluster"),
                Some(&generator),
                Path::new("unused"),
            );
            let error = kube::Error::Api(Box::new(kube::core::Status::default().with_code(code)));
            assert_eq!(recovery.action(&error), Some(Action::Regenerate));
        }
        let mut recovery = Recovery::new(
            &store,
            Some("cluster"),
            Some(&generator),
            Path::new("unused"),
        );
        let error = kube::Error::Service(Box::new(kube::Error::Auth(
            kube::client::AuthError::AuthExecRun {
                cmd: "kubelogin".into(),
                status: std::process::ExitStatus::from_raw(256),
                out: Output {
                    status: std::process::ExitStatus::from_raw(256),
                    stdout: Vec::new(),
                    stderr: b"Please run 'az login' to setup account.".to_vec(),
                },
            },
        )));
        assert_eq!(recovery.action(&error), Some(Action::Login));
    }

    #[test]
    fn does_not_recover_without_a_generator() {
        let store = store::Store::new("unused".into());
        let mut recovery = Recovery::new(&store, Some("cluster"), None, Path::new("unused"));
        assert_eq!(
            recovery.action(&std::io::Error::other("Forbidden: az login")),
            None
        );
        let generator = Generator::Tcloud {
            organisation: "org".into(),
        };
        assert!(login_command(&generator, &AzureLogin::default()).is_none());
        assert!(!login_required(&generator, "gcloud auth login or az login"));
    }

    fn args(command: &Command) -> Vec<&str> {
        command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect()
    }

    #[test]
    fn commands_preserve_cloud_settings_and_isolate_kubeconfig() {
        let generator = gcloud();
        let login = login_command(&generator, &AzureLogin::default()).unwrap();
        assert_eq!(login.get_program(), OsStr::new("gcloud"));
        assert_eq!(
            args(&login),
            [
                "auth",
                "login",
                "--force",
                "--project",
                "project-id",
                "--account",
                "user@example.com",
                "--configuration",
                "work"
            ]
        );
        let (command, stdout_config) =
            generator_command(&generator, "cluster", Path::new("/tmp/kubeconfig"));
        assert!(!stdout_config);
        assert_eq!(
            args(&command),
            [
                "container",
                "clusters",
                "get-credentials",
                "--location",
                "europe-west1",
                "cluster",
                "--project",
                "project-id",
                "--account",
                "user@example.com",
                "--configuration",
                "work"
            ]
        );
        assert!(command.get_envs().any(
            |(key, value)| key == "KUBECONFIG" && value == Some(OsStr::new("/tmp/kubeconfig"))
        ));
        let generator = aks();
        let login = login_command(&generator, &AzureLogin::default()).unwrap();
        assert_eq!(login.get_program(), OsStr::new("az"));
        assert_eq!(
            args(&login),
            [
                "login",
                "--subscription",
                "subscription-id",
                "--tenant",
                "tenant-id"
            ]
        );
        let (command, stdout_config) =
            generator_command(&generator, "cluster", Path::new("/tmp/kubeconfig"));
        assert!(!stdout_config);
        assert_eq!(
            args(&command),
            [
                "aks",
                "get-credentials",
                "--subscription",
                "subscription-id",
                "--name",
                "cluster",
                "--resource-group",
                "resource-group",
                "--file",
                "/tmp/kubeconfig"
            ]
        );
    }
}

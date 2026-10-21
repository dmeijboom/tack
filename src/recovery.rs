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

use crate::{
    config::Generator,
    error::{AppError, Result as AppResult},
    output::Mode,
    store,
};

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

fn prepare_client_config(
    config: &mut Kubeconfig,
    generator: Option<&Generator>,
    mode: Mode,
    device_login: bool,
) -> AzureLogin {
    let mut azure = AzureLogin::default();
    if !mode.is_json()
        && !matches!(
            generator,
            Some(Generator::Aks { .. } | Generator::Gcloud { .. })
        )
    {
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
        let args = exec
            .args
            .iter()
            .flatten()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let is_azure = matches!(generator, Some(Generator::Aks { .. }));
        let login = argument_value(&args, "--login").or_else(|| argument_value(&args, "-l"));
        let device_code = is_azure && matches!(login.as_deref(), None | Some("devicecode"));
        let interactive_login =
            device_code || (is_azure && login.as_deref() == Some("interactive"));
        // kube-rs captures stderr in Never mode, which hides device codes until login times out.
        if mode.is_json() {
            exec.interactive_mode = Some(if device_login && device_code {
                ExecInteractiveMode::IfAvailable
            } else {
                ExecInteractiveMode::Never
            });
        } else if !interactive_login {
            exec.interactive_mode = Some(ExecInteractiveMode::Never);
        }
        if is_azure {
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
    mode: Mode,
    device_login: bool,
}

impl<'a> Recovery<'a> {
    pub fn new(
        store: &'a store::Store,
        name: Option<&'a str>,
        generator: Option<&'a Generator>,
        kubeconfig: &'a Path,
        mode: Mode,
    ) -> Self {
        Self {
            store,
            name,
            generator,
            kubeconfig,
            regenerated: false,
            logged_in: false,
            azure_login: AzureLogin::default(),
            mode,
            device_login: true,
        }
    }

    pub fn with_device_login(mut self, enabled: bool) -> Self {
        self.device_login = enabled;
        self
    }

    fn action(&mut self, error: &(dyn Error + 'static)) -> Option<Action> {
        if let Some(error) = error.downcast_ref::<AppError>()
            && !matches!(
                error.code,
                "command_failed"
                    | "auth_failed"
                    | "auth_required"
                    | "access_denied"
                    | "kubernetes_failed"
            )
        {
            return None;
        }
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
            if let Some(status) = error.downcast_ref::<kube::core::Status>() {
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

    fn login(&self, cause: AppError) -> AppResult<()> {
        let mut command = self
            .generator
            .and_then(|generator| login_command(generator, &self.azure_login))
            .ok_or_else(|| AppError::auth_required(None))?;
        if self.mode.is_json() {
            return Err(AppError::auth_required(Some(&command))
                .at_path(self.kubeconfig)
                .caused_by(cause));
        }
        self.mode.progress(format_args!(
            "# authentication required; running {command:?} in the console"
        ));
        let status = command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|source| AppError::command_start(&command, source))?;
        if !status.success() {
            return Err(AppError::command_failed(&command, status, String::new())
                .help("Complete cloud authentication successfully, then retry."));
        }
        Ok(())
    }

    pub fn generate(&mut self) -> AppResult<()> {
        let name = self.name.ok_or_else(|| {
            AppError::new("context_required", "Unable to determine the tack context")
                .help("Select a context with `tack use <context>` first.")
        })?;
        let generator = self.generator.ok_or_else(|| {
            AppError::new(
                "generator_missing",
                format!("No generator configured for {name}"),
            )
            .help("Configure a generator or provide a kubeconfig in tack's store.")
        })?;
        loop {
            match generate_once(self.store, name, generator, self.mode) {
                Ok(()) => return Ok(()),
                Err(error) => match self.action(&error) {
                    Some(Action::Login) => self.login(error)?,
                    Some(Action::Regenerate) => {
                        self.mode.progress(format_args!(
                            "# access denied; regenerating kubeconfig for {name} once"
                        ));
                    }
                    None => return Err(error),
                },
            }
        }
    }

    fn regenerate(&mut self) -> AppResult<()> {
        let previous = Kubeconfig::read_from(self.kubeconfig)
            .map_err(|source| AppError::kubeconfig(self.kubeconfig, source))?;
        let namespace = current_namespace(&previous);
        self.mode
            .progress("# access denied; regenerating kubeconfig once");
        self.generate()?;
        let source = self
            .name
            .and_then(|name| self.store.kubeconfig(name))
            .ok_or_else(|| {
                AppError::new("context_invalid", "Unable to determine kubeconfig path")
            })?;
        let mut config =
            Kubeconfig::read_from(&source).map_err(|error| AppError::kubeconfig(&source, error))?;
        if let Some(context) = config
            .contexts
            .iter_mut()
            .find(|context| Some(&context.name) == config.current_context.as_ref())
            .and_then(|context| context.context.as_mut())
        {
            context.namespace = namespace;
        }
        let yaml = serde_yaml::to_string(&config).map_err(|source| {
            AppError::new(
                "kubeconfig_invalid",
                "Unable to serialize regenerated kubeconfig",
            )
            .caused_by(source)
        })?;
        store::write_restricted(self.kubeconfig, yaml.as_bytes()).map_err(|source| {
            AppError::io(
                "kubeconfig_write_failed",
                "Unable to save regenerated kubeconfig",
                self.kubeconfig,
                source,
            )
        })?;
        Ok(())
    }

    pub async fn check_access(&mut self) -> AppResult<()> {
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

    pub async fn with_client<T, F, Fut>(&mut self, mut operation: F) -> AppResult<T>
    where
        F: FnMut(Client) -> Fut,
        Fut: Future<Output = Result<T, kube::Error>>,
    {
        loop {
            let result = async {
                let mut kubeconfig = Kubeconfig::read_from(self.kubeconfig)
                    .map_err(|source| AppError::kubeconfig(self.kubeconfig, source))?;
                self.azure_login = prepare_client_config(
                    &mut kubeconfig,
                    self.generator,
                    self.mode,
                    self.device_login,
                );
                let config = kube::Config::from_custom_kubeconfig(kubeconfig, &Default::default())
                    .await
                    .map_err(|source| AppError::kubeconfig(self.kubeconfig, source))?;
                let client = Client::try_from(config)
                    .map_err(|source| AppError::kubernetes(source).at_path(self.kubeconfig))?;
                operation(client)
                    .await
                    .map_err(|source| AppError::kubernetes(source).at_path(self.kubeconfig))
            }
            .await;
            match result {
                Ok(value) => return Ok(value),
                Err(error) => match self.action(&error) {
                    Some(Action::Login) => self.login(error)?,
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
    mode: Mode,
) -> AppResult<()> {
    let file = NamedTempFile::new().map_err(|source| {
        AppError::new(
            "temporary_file_failed",
            "Unable to create a temporary kubeconfig",
        )
        .help("Check that the temporary directory exists and is writable.")
        .caused_by(source)
    })?;
    let (mut command, stdout_config) = generator_command(generator, name, file.path());
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|source| AppError::command_start(&command, source))?;
    if !output.status.success() {
        let stdout = if stdout_config {
            String::new()
        } else {
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        return Err(AppError::command_failed(
            &command,
            output.status,
            format!("{}{}", stdout, String::from_utf8_lossy(&output.stderr)),
        ));
    }
    mode.child_output(&output.stderr);
    if !stdout_config {
        mode.child_output(&output.stdout);
    }
    let content = if stdout_config {
        output.stdout
    } else {
        std::fs::read(file.path()).map_err(|source| {
            AppError::io(
                "kubeconfig_read_failed",
                "Unable to read generated kubeconfig",
                file.path(),
                source,
            )
        })?
    };
    if content.is_empty() {
        return Err(AppError::new(
            "kubeconfig_invalid",
            "Generator returned an empty kubeconfig",
        )
        .with_command(&command));
    }
    Kubeconfig::from_yaml(&String::from_utf8_lossy(&content))
        .map_err(|source| AppError::kubeconfig(file.path(), source))?;
    let path = store.kubeconfig(name).ok_or_else(|| {
        AppError::new("context_invalid", format!("Invalid context name {name:?}"))
    })?;
    store.store(name, content).map_err(|source| {
        AppError::io(
            "kubeconfig_write_failed",
            "Unable to store generated kubeconfig",
            &path,
            source,
        )
    })?;
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
        let settings = prepare_client_config(&mut prepared, Some(&aks()), Mode::Human, true);
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
        prepare_client_config(&mut unmanaged, None, Mode::Human, true);
        assert_eq!(
            serde_yaml::to_string(&unmanaged).unwrap(),
            serde_yaml::to_string(&original).unwrap()
        );
    }

    #[test]
    fn azure_login_prompts_respect_output_and_activation_modes() {
        for login in [
            vec![],
            vec!["--login", "devicecode"],
            vec!["-l", "devicecode"],
            vec!["--login=devicecode"],
            vec!["-l=devicecode"],
            vec!["--login", "interactive"],
        ] {
            for (mode, device_login) in
                [(Mode::Human, true), (Mode::Json, true), (Mode::Json, false)]
            {
                let mut config = azure_kubeconfig();
                let exec = config.auth_infos[0]
                    .auth_info
                    .as_mut()
                    .unwrap()
                    .exec
                    .as_mut()
                    .unwrap();
                exec.args = Some(
                    std::iter::once("get-token")
                        .chain(login.iter().copied())
                        .map(String::from)
                        .collect(),
                );
                let original = serde_yaml::to_string(&config).unwrap();
                prepare_client_config(&mut config, Some(&aks()), mode, device_login);
                let exec = config.auth_infos[0]
                    .auth_info
                    .as_ref()
                    .unwrap()
                    .exec
                    .as_ref()
                    .unwrap();
                assert_eq!(
                    exec.interactive_mode,
                    Some(
                        if mode.is_json() && (!device_login || login.contains(&"interactive")) {
                            ExecInteractiveMode::Never
                        } else {
                            ExecInteractiveMode::IfAvailable
                        }
                    ),
                    "{login:?} {mode:?} device_login={device_login}"
                );
                if !mode.is_json() {
                    assert_eq!(serde_yaml::to_string(&config).unwrap(), original);
                }
            }
        }
    }

    #[tokio::test]
    async fn azure_exec_failure_preserves_login_hints_and_retries_only_once() {
        let mut generator = aks();
        if let Generator::Aks { tenant, .. } = &mut generator {
            *tenant = None;
        }
        let mut kubeconfig = azure_kubeconfig();
        let settings = prepare_client_config(&mut kubeconfig, Some(&generator), Mode::Human, true);
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
            Mode::Human,
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
                    Mode::Human,
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
            Mode::Human,
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
                Mode::Human,
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
            Mode::Human,
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
                Mode::Human,
            );
            let error = kube::Error::Api(Box::new(kube::core::Status::default().with_code(code)));
            assert_eq!(recovery.action(&error), Some(Action::Regenerate));
        }
        let mut recovery = Recovery::new(
            &store,
            Some("cluster"),
            Some(&generator),
            Path::new("unused"),
            Mode::Human,
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
    fn local_io_errors_do_not_trigger_cloud_recovery() {
        let store = store::Store::new("unused".into());
        let generator = aks();
        let mut recovery = Recovery::new(
            &store,
            Some("cluster"),
            Some(&generator),
            Path::new("unused"),
            Mode::Json,
        );
        for code in [
            "kubeconfig_write_failed",
            "command_not_found",
            "temporary_file_failed",
        ] {
            let error = AppError::new(code, "Local operation failed").detail("Permission denied");
            assert_eq!(recovery.action(&error), None);
        }
        assert!(!recovery.regenerated);
        assert!(!recovery.logged_in);
    }

    #[test]
    fn structured_api_errors_preserve_the_regeneration_signal() {
        let store = store::Store::new("unused".into());
        let generator = aks();
        for code in [401, 403] {
            let mut recovery = Recovery::new(
                &store,
                Some("cluster"),
                Some(&generator),
                Path::new("unused"),
                Mode::Json,
            );
            let error = AppError::kubernetes(kube::Error::Api(
                kube::core::Status::default().with_code(code).boxed(),
            ));
            assert_eq!(recovery.action(&error), Some(Action::Regenerate));
            assert_eq!(recovery.action(&error), None);
        }
    }

    #[test]
    fn json_checks_make_unmanaged_exec_plugins_noninteractive() {
        let mut config = azure_kubeconfig();
        prepare_client_config(&mut config, None, Mode::Json, true);
        assert_eq!(
            config.auth_infos[0]
                .auth_info
                .as_ref()
                .unwrap()
                .exec
                .as_ref()
                .unwrap()
                .interactive_mode,
            Some(ExecInteractiveMode::Never)
        );
    }

    #[test]
    fn does_not_recover_without_a_generator() {
        let store = store::Store::new("unused".into());
        let mut recovery = Recovery::new(
            &store,
            Some("cluster"),
            None,
            Path::new("unused"),
            Mode::Human,
        );
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

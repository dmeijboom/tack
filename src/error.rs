use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

use miette::{Diagnostic, LabeledSpan, NamedSource, SourceCode, SourceSpan};
use serde::Serialize;

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, Serialize, PartialEq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandSpec {
    pub fn from_command(command: &Command) -> Self {
        Self {
            program: command.get_program().to_string_lossy().into_owned(),
            args: command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
        }
    }

    pub fn display(&self) -> String {
        std::iter::once(&self.program)
            .chain(self.args.iter())
            .map(|arg| format!("{arg:?}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct AppError {
    pub code: &'static str,
    pub message: String,
    pub exit_code: u8,
    #[source]
    source: Option<Box<dyn Error + Send + Sync>>,
    context: Box<ErrorContext>,
}

#[derive(Debug, Default)]
struct ErrorContext {
    help: Option<String>,
    command: Option<CommandSpec>,
    path: Option<PathBuf>,
    status: Option<i32>,
    http_status: Option<u16>,
    source_code: Option<NamedSource<String>>,
    span: Option<SourceSpan>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Cause(String);

impl AppError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            exit_code: 1,
            source: None,
            context: Box::default(),
        }
    }

    pub fn help(mut self, help: impl Into<String>) -> Self {
        self.context.help = Some(help.into());
        self
    }

    pub fn caused_by(mut self, source: impl Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    pub fn detail(self, detail: impl Into<String>) -> Self {
        self.caused_by(Cause(detail.into()))
    }

    pub fn at_path(mut self, path: impl AsRef<Path>) -> Self {
        self.context.path = Some(path.as_ref().to_owned());
        self
    }

    pub fn with_command(mut self, command: &Command) -> Self {
        self.context.command = Some(CommandSpec::from_command(command));
        self
    }

    pub fn with_exit_code(mut self, code: u8) -> Self {
        self.exit_code = code;
        self
    }

    pub fn with_source_code(
        mut self,
        path: &Path,
        content: String,
        span: Option<std::ops::Range<usize>>,
    ) -> Self {
        self.context.source_code = Some(NamedSource::new(path.display().to_string(), content));
        self.context.span = span.map(|span| (span.start, span.len()).into());
        self
    }

    pub fn io(
        code: &'static str,
        message: impl Into<String>,
        path: &Path,
        source: std::io::Error,
    ) -> Self {
        Self::new(code, format!("{} ({})", message.into(), path.display()))
            .at_path(path)
            .caused_by(source)
    }

    pub fn kubeconfig(path: &Path, source: kube::config::KubeconfigError) -> Self {
        Self::new(
            "kubeconfig_invalid",
            format!("Unable to load kubeconfig {}", path.display()),
        )
        .at_path(path)
        .help("Check the kubeconfig file, or regenerate it with `tack use <context>`.")
        .caused_by(source)
    }

    pub fn command_start(command: &Command, source: std::io::Error) -> Self {
        let code = if source.kind() == std::io::ErrorKind::NotFound {
            "command_not_found"
        } else {
            "command_start_failed"
        };
        Self::new(
            code,
            format!(
                "Unable to run {}",
                CommandSpec::from_command(command).display()
            ),
        )
        .with_command(command)
        .caused_by(source)
    }

    pub fn command_failed(command: &Command, status: ExitStatus, detail: String) -> Self {
        let mut error = Self::new(
            "command_failed",
            format!(
                "Command {} failed ({status})",
                CommandSpec::from_command(command).display()
            ),
        )
        .with_command(command);
        error.context.status = status.code();
        if !detail.trim().is_empty() {
            error = error.detail(detail.trim());
        }
        error
    }

    pub fn auth_required(command: Option<&Command>) -> Self {
        let error = Self::new(
            "auth_required",
            "Cloud authentication requires interactive login",
        );
        match command {
            Some(command) => error
                .help(format!(
                    "Run {} in a terminal, then retry.",
                    CommandSpec::from_command(command).display()
                ))
                .with_command(command),
            None => error.help("Authenticate with your cloud provider in a terminal, then retry."),
        }
    }

    pub fn kubernetes(source: kube::Error) -> Self {
        let mut current: Option<&(dyn Error + 'static)> = Some(&source);
        while let Some(error) = current {
            if let Some(kube::client::AuthError::AuthExecStart(source)) =
                error.downcast_ref::<kube::client::AuthError>()
            {
                return Self::new(
                    "auth_plugin_start_failed",
                    "Unable to start the Kubernetes credential plugin",
                )
                .help("Check the executable configured in the kubeconfig's user.exec section.")
                .detail(source.to_string());
            }
            if let Some(kube::client::AuthError::AuthExecRun { out, .. }) =
                error.downcast_ref::<kube::client::AuthError>()
            {
                let mut error = Self::new("auth_failed", "Kubernetes credential plugin failed")
                    .help("Authenticate with your cloud provider and retry the operation.");
                if !out.stderr.is_empty() {
                    error = error.detail(String::from_utf8_lossy(&out.stderr));
                }
                return error;
            }
            current = error.source();
        }
        let (code, message, help) = match &source {
            kube::Error::Api(status) if status.code == 401 => (
                "auth_required",
                "Kubernetes rejected the configured credentials",
                "Authenticate with your cloud provider, then retry.",
            ),
            kube::Error::Api(status) if status.code == 403 => (
                "access_denied",
                "Kubernetes denied access",
                "Check that your account has permission for this cluster and operation.",
            ),
            kube::Error::Auth(_) => (
                "auth_failed",
                "Unable to authenticate with Kubernetes",
                "Check your cloud credentials and kubeconfig authentication settings.",
            ),
            _ => (
                "kubernetes_failed",
                "Kubernetes request failed",
                "Check the cluster connection, kubeconfig, and network access.",
            ),
        };
        let error = Self::new(code, message).help(help);
        match source {
            kube::Error::Api(status) => {
                let mut error = error;
                error.context.http_status = Some(status.code);
                error.caused_by(*status)
            }
            source => error.caused_by(source),
        }
    }

    pub fn json(&self) -> serde_json::Value {
        let mut causes = Vec::new();
        let mut source = self.source();
        while let Some(error) = source {
            causes.push(error.to_string());
            source = error.source();
        }
        serde_json::json!({
            "code": self.code,
            "message": self.message,
            "help": self.context.help,
            "causes": causes,
            "path": self.context.path.as_ref().map(|path| path.to_string_lossy()),
            "command": self.context.command,
            "status": self.context.status,
            "http_status": self.context.http_status,
            "exit_code": self.exit_code,
            "span": self.context.span.map(|span| serde_json::json!({
                "offset": span.offset(),
                "length": span.len(),
            })),
        })
    }
}

impl Diagnostic for AppError {
    fn code<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        Some(Box::new(format!("tack::{}", self.code)))
    }

    fn help<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        self.context.help.as_ref().map(|help| Box::new(help) as _)
    }

    fn source_code(&self) -> Option<&dyn SourceCode> {
        self.context.source_code.as_ref().map(|source| source as _)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        self.context.span.map(|span| {
            Box::new(std::iter::once(LabeledSpan::new_with_span(
                Some("invalid configuration here".into()),
                span,
            ))) as _
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::process::ExitStatusExt, process::Output};

    #[test]
    fn auth_errors_never_include_exec_stdout_or_command_environment() {
        let error = AppError::kubernetes(kube::Error::Auth(kube::client::AuthError::AuthExecRun {
            cmd: "SECRET_ENV=secret kubelogin".into(),
            status: ExitStatus::from_raw(256),
            out: Output {
                status: ExitStatus::from_raw(256),
                stdout: b"secret-token".to_vec(),
                stderr: b"Please run az login".to_vec(),
            },
        }));
        let json = error.json().to_string();
        assert_eq!(error.code, "auth_failed");
        assert!(json.contains("Please run az login"));
        assert!(!json.contains("secret"));
        assert!(!format!("{:?}", miette::Report::new(error)).contains("secret"));
    }

    #[test]
    fn auth_required_contains_executable_arguments_not_shell_code() {
        let mut command = Command::new("az");
        command.args([
            "login",
            "--tenant",
            "tenant",
            "--scope",
            "resource/.default",
        ]);
        let error = AppError::auth_required(Some(&command));
        assert_eq!(error.json()["code"], "auth_required");
        assert_eq!(error.json()["command"]["program"], "az");
        assert_eq!(error.json()["command"]["args"][2], "tenant");
    }
}

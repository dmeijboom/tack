use std::ffi::OsString;

use crate::error::{AppError, Result};

pub fn validate_read_only(command: &[OsString]) -> Result<()> {
    let denied = || {
        AppError::new(
            "read_only_violation",
            "Command is not permitted in read-only mode",
        )
        .help("Use an allowed kubectl diagnostic, or --allow-write for unrestricted command execution.")
    };
    let args = command
        .iter()
        .map(|arg| arg.to_str().ok_or_else(denied))
        .collect::<Result<Vec<_>>>()?;
    if args.first() != Some(&"kubectl") {
        return Err(denied());
    }
    let verb = *args.get(1).ok_or_else(denied)?;
    let mut index = match verb {
        "get" | "describe" | "logs" | "api-resources" | "api-versions" | "version"
        | "port-forward" | "proxy" => 2,
        "top" if matches!(args.get(2), Some(&"pods" | &"nodes")) => 3,
        _ => return Err(denied()),
    };
    while let Some(arg) = args.get(index) {
        if !arg.starts_with('-') {
            index += 1;
            continue;
        }
        let (flag, value) = arg
            .split_once('=')
            .map_or((*arg, None), |(flag, value)| (flag, Some(value)));
        let takes_value = option_takes_value(verb, flag).ok_or_else(denied)?;
        if takes_value {
            if value.is_none() {
                index += 1;
                if args
                    .get(index)
                    .is_none_or(|value| value.starts_with('-') && *value != "-1")
                {
                    return Err(denied());
                }
            }
        } else if value.is_some_and(|value| !matches!(value, "true" | "false")) {
            return Err(denied());
        }
        index += 1;
    }
    Ok(())
}

fn option_takes_value(verb: &str, flag: &str) -> Option<bool> {
    match (verb, flag) {
        (_, "--namespace" | "-n" | "--request-timeout") => Some(true),
        (_, "--help" | "-h") => Some(false),
        ("get" | "describe" | "top", "--selector" | "-l") => Some(true),
        ("get" | "top", "--all-namespaces" | "-A" | "--no-headers") => Some(false),
        ("get", "--output" | "-o" | "--field-selector" | "--sort-by" | "--chunk-size") => {
            Some(true)
        }
        ("get", "--watch" | "-w" | "--watch-only" | "--show-labels" | "--ignore-not-found") => {
            Some(false)
        }
        ("describe", "--show-events") => Some(false),
        (
            "logs",
            "--container"
            | "-c"
            | "--tail"
            | "--since"
            | "--since-time"
            | "--limit-bytes"
            | "--max-log-requests"
            | "--pod-running-timeout"
            | "--selector"
            | "-l",
        ) => Some(true),
        (
            "logs",
            "--follow" | "-f" | "--previous" | "-p" | "--timestamps" | "--prefix"
            | "--all-containers" | "--all-pods" | "--ignore-errors",
        ) => Some(false),
        ("top", "--sort-by") => Some(true),
        ("top", "--containers" | "--sum" | "--use-protocol-buffers" | "--show-capacity") => {
            Some(false)
        }
        ("api-resources", "--api-group" | "--output" | "-o" | "--sort-by" | "--verbs") => {
            Some(true)
        }
        ("api-resources", "--cached" | "--namespaced") => Some(false),
        ("version", "--output" | "-o") => Some(true),
        ("version", "--client") => Some(false),
        ("port-forward", "--address" | "--pod-running-timeout") => Some(true),
        (
            "proxy",
            "--address" | "--port" | "-p" | "--accept-hosts" | "--accept-paths" | "--reject-paths"
            | "--reject-methods" | "--unix-socket" | "-u" | "--www" | "-w" | "--www-prefix" | "-P"
            | "--api-prefix" | "--keepalive",
        ) => Some(true),
        ("proxy", "--disable-filter" | "--append-server-path") => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(args: &[&str]) -> Result<()> {
        validate_read_only(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn permits_targeted_diagnostics() {
        for args in [
            vec!["kubectl", "get", "pods", "--namespace", "default"],
            vec!["kubectl", "get", "pods", "-A", "--output=json"],
            vec![
                "kubectl",
                "get",
                "secrets",
                "--namespace",
                "default",
                "-o",
                "json",
            ],
            vec!["kubectl", "describe", "secret", "credentials"],
            vec![
                "kubectl",
                "port-forward",
                "service/app",
                "8080:80",
                "--address=0.0.0.0",
            ],
            vec!["kubectl", "proxy"],
            vec!["kubectl", "proxy", "--port", "8001", "--accept-hosts=.*"],
            vec!["kubectl", "describe", "pod", "app", "-n=default"],
            vec!["kubectl", "logs", "app", "--tail", "-1", "--follow=false"],
            vec!["kubectl", "top", "pods", "--containers"],
            vec!["kubectl", "top", "nodes"],
            vec!["kubectl", "api-resources", "--namespaced=false"],
            vec!["kubectl", "api-versions"],
            vec!["kubectl", "version", "--client"],
        ] {
            assert!(check(&args).is_ok(), "{args:?}");
        }
    }

    #[test]
    fn rejects_writes_bypasses_and_unknown_options() {
        for args in [
            vec![],
            vec!["kubectl"],
            vec!["kubectl", "delete", "pods", "--all"],
            vec!["kubectl", "apply", "--dry-run=client", "-f", "app.yaml"],
            vec!["kubectl", "exec", "app", "--", "ls"],
            vec!["kubectl", "config", "view"],
            vec!["kubectl", "unknown-plugin"],
            vec!["kubectl", "proxy", "--context=other"],
            vec![
                "kubectl",
                "port-forward",
                "app",
                "8080:80",
                "--kubeconfig=other",
            ],
            vec!["kubectl", "top", "unknown-plugin"],
            vec!["sh", "-c", "kubectl delete pods --all"],
            vec!["/usr/bin/kubectl", "get", "pods"],
            vec!["kubectl", "--namespace", "default", "get", "pods"],
            vec!["kubectl", "get", "--raw=/api"],
            vec!["kubectl", "get", "pods", "--context=other"],
            vec!["kubectl", "get", "pods", "--kubeconfig", "other"],
            vec!["kubectl", "get", "pods", "--server=https://other"],
            vec!["kubectl", "get", "pods", "--as=admin"],
            vec!["kubectl", "get", "pods", "--token=secret"],
            vec!["kubectl", "get", "pods", "--insecure-skip-tls-verify"],
            vec!["kubectl", "get", "pods", "--unknown"],
            vec!["kubectl", "get", "pods", "--namespace"],
            vec!["kubectl", "get", "pods", "--namespace", "--raw=/api"],
            vec!["kubectl", "get", "pods", "--watch=unexpected"],
            vec!["kubectl", "get", "pods", "--"],
        ] {
            assert_eq!(
                check(&args).unwrap_err().code,
                "read_only_violation",
                "{args:?}"
            );
        }
    }

    #[test]
    fn rejects_non_utf8_arguments() {
        use std::os::unix::ffi::OsStringExt;

        let args = vec![
            "kubectl".into(),
            "get".into(),
            OsString::from_vec(vec![0xff]),
        ];
        assert_eq!(
            validate_read_only(&args).unwrap_err().code,
            "read_only_violation"
        );
    }
}

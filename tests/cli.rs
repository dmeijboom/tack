use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;

fn cli(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tack"))
        .args(args)
        .env("HOME", home)
        .env("NO_COLOR", "1")
        .env_remove("KUBECONFIG")
        .env_remove("TACK_CONTEXT")
        .output()
        .unwrap()
}

fn configure(home: &Path, content: &str) {
    let dir = home.join(".config/tack");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.toml"), content).unwrap();
}

fn json(output: &Output) -> Value {
    let text = std::str::from_utf8(&output.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(!text.contains('\x1b'));
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_str(text).unwrap();
    assert_eq!(value["schema_version"], 1);
    value
}

#[test]
fn json_list_and_unselected_use_return_sorted_choices() {
    let home = TempDir::new().unwrap();
    configure(home.path(), "[context.zeta]\n[context.alpha]\n");
    for args in [["--json", "list"], ["use", "--json"]] {
        let output = cli(home.path(), &args);
        assert!(output.status.success());
        let value = json(&output);
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["kind"], "contexts");
        assert_eq!(
            value["data"]["contexts"],
            serde_json::json!(["alpha", "zeta"])
        );
    }
}

#[test]
fn json_errors_cover_configuration_arguments_and_contexts() {
    let home = TempDir::new().unwrap();
    let missing = cli(home.path(), &["--json", "list"]);
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(json(&missing)["error"]["code"], "config_not_found");
    let invalid_args = cli(home.path(), &["--json", "--unknown"]);
    assert_eq!(invalid_args.status.code(), Some(2));
    assert_eq!(json(&invalid_args)["error"]["code"], "invalid_arguments");
    configure(home.path(), "[context.local]\n");
    for (args, code) in [
        (vec!["--json", "use", "missing"], "context_not_found"),
        (vec!["--json", "use", "local"], "kubeconfig_missing"),
        (vec!["--json", "namespace"], "context_required"),
        (vec!["--json", "use", "../bad"], "context_invalid"),
        (vec!["--json", "use", ""], "context_invalid"),
    ] {
        let output = cli(home.path(), &args);
        assert!(!output.status.success());
        assert_eq!(json(&output)["error"]["code"], code);
    }
}

#[test]
fn help_and_version_work_in_json_without_configuration() {
    let home = TempDir::new().unwrap();
    for (args, kind) in [
        (vec!["--json", "--help"], "help"),
        (vec!["--json", "use", "--help"], "help"),
        (vec!["--version", "--json"], "version"),
    ] {
        let output = cli(home.path(), &args);
        assert!(output.status.success());
        assert_eq!(json(&output)["data"]["kind"], kind);
    }
}

#[test]
fn malformed_toml_has_a_diagnostic_and_a_machine_readable_span() {
    let home = TempDir::new().unwrap();
    configure(home.path(), "[context\n");
    let output = cli(home.path(), &["--json", "list"]);
    let value = json(&output);
    assert_eq!(value["error"]["code"], "config_invalid");
    assert!(value["error"]["span"]["offset"].is_number());
    let human = cli(home.path(), &["list"]);
    assert!(human.stdout.is_empty());
    let error = String::from_utf8(human.stderr).unwrap();
    assert!(error.contains("tack::config_invalid"));
    assert!(error.contains("config.toml"));
    assert!(!error.contains('\x1b'));
}

#[test]
fn read_only_guard_rejects_commands_before_loading_configuration() {
    let home = TempDir::new().unwrap();
    for args in [
        vec!["--use", "gke", "--", "kubectl", "delete", "pods", "--all"],
        vec!["--use", "gke", "--", "gcloud"],
    ] {
        let output = cli(home.path(), &args);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("tack::read_only_violation"));
    }
}

#[test]
fn noninteractive_auth_required_never_starts_login_a_shell_or_a_command() {
    let home = TempDir::new().unwrap();
    configure(
        home.path(),
        "[context.gke]\ngenerator = 'gcloud'\nproject = 'project'\nlocation = 'location'\naccount = 'user@example.com'\n",
    );
    fs::write(
        home.path().join(".config/tack/gke.yml"),
        r#"
apiVersion: v1
kind: Config
current-context: gke
contexts:
- name: gke
  context: {cluster: gke, user: gke}
clusters:
- name: gke
  cluster: {server: 'http://127.0.0.1:1'}
users:
- name: gke
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      command: /bin/sh
      args: [-c, "echo 'Please run gcloud auth login' >&2; exit 1"]
"#,
    )
    .unwrap();
    let marker = home.path().join("login-was-run");
    let fake_gcloud = home.path().join("gcloud");
    fs::write(
        &fake_gcloud,
        "#!/bin/sh\necho unexpected > \"$TACK_TEST_MARKER\"\nexit 99\n",
    )
    .unwrap();
    fs::set_permissions(&fake_gcloud, fs::Permissions::from_mode(0o700)).unwrap();
    for args in [
        vec!["--json", "use", "gke"],
        vec!["--allow-write", "--use", "gke", "--", "gcloud"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tack"))
            .args(&args)
            .env("HOME", home.path())
            .env("PATH", home.path())
            .env("SHELL", &fake_gcloud)
            .env("TACK_TEST_MARKER", &marker)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        if args[0] == "--json" {
            let value = json(&output);
            assert_eq!(value["error"]["code"], "auth_required");
            assert_eq!(value["error"]["command"]["program"], "gcloud");
            assert_eq!(
                value["error"]["command"]["args"],
                serde_json::json!([
                    "auth",
                    "login",
                    "--force",
                    "--project",
                    "project",
                    "--account",
                    "user@example.com"
                ])
            );
        } else {
            assert!(output.stdout.is_empty());
            assert!(String::from_utf8_lossy(&output.stderr).contains("tack::auth_required"));
        }
        assert!(!marker.exists());
    }
}

fn configure_azure_device_login(home: &Path, server: &str) -> String {
    configure(
        home,
        "[context.aks]\ngenerator = 'aks'\nsubscription = 'subscription'\nresource-group = 'resource-group'\n",
    );
    let plugin = home.join("kubelogin");
    fs::write(
        &plugin,
        r#"#!/bin/sh
if [ ! -f "$0.cache" ]; then
    printf '%s\n' 'Open https://login.microsoft.com/device and enter TEST-CODE' >&2
    read -r code || exit 1
    [ "$code" = done ] || exit 1
    : > "$0.cache"
fi
printf '%s\n' '{"apiVersion":"client.authentication.k8s.io/v1beta1","kind":"ExecCredential","status":{"token":"test-plugin-token"}}'
"#,
    )
    .unwrap();
    fs::set_permissions(&plugin, fs::Permissions::from_mode(0o700)).unwrap();
    let kubeconfig = format!(
        r#"
apiVersion: v1
kind: Config
current-context: aks
contexts:
- name: aks
  context: {{cluster: aks, user: aks}}
clusters:
- name: aks
  cluster:
    server: '{server}'
    # An explicit empty CA bundle keeps this HTTP fixture independent of host trust roots.
    certificate-authority-data: ''
users:
- name: aks
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      command: '{}'
      interactiveMode: IfAvailable
      args: [get-token, --login, devicecode]
"#,
        plugin.display()
    );
    fs::write(home.join(".config/tack/aks.yml"), &kubeconfig).unwrap();
    kubeconfig
}

fn complete_azure_device_login(home: &Path, args: &[&str], response: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tack"))
        .args(args)
        .env("HOME", home)
        .env("NO_COLOR", "1")
        .env("SSL_CERT_FILE", home.join("missing-ca.pem"))
        .env("SSL_CERT_DIR", home.join("missing-certs"))
        .env_remove("KUBECONFIG")
        .env_remove("TACK_CONTEXT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        sender.send(line).unwrap();
        let mut rest = String::new();
        reader.read_to_string(&mut rest).unwrap();
        rest
    });
    let prompt = receiver.recv_timeout(Duration::from_secs(10));
    let waiting_for_login = child.try_wait().unwrap().is_none();
    let _ = child.stdin.take().unwrap().write_all(response);
    if prompt.is_err() {
        child.kill().unwrap();
    }
    let mut output = child.wait_with_output().unwrap();
    let rest = reader.join().unwrap();
    let prompt = prompt.unwrap();
    assert_eq!(
        prompt,
        "Open https://login.microsoft.com/device and enter TEST-CODE\n"
    );
    assert!(waiting_for_login);
    output.stderr = format!("{prompt}{rest}").into_bytes();
    output
}

#[test]
fn azure_device_code_prompt_is_live_in_human_and_json_modes() {
    let home = TempDir::new().unwrap();
    let kubeconfig = configure_azure_device_login(home.path(), "http://127.0.0.1:1");
    for args in [vec!["use", "aks"], vec!["--json", "use", "aks"]] {
        let output = complete_azure_device_login(home.path(), &args, b"fail\n");
        assert!(!output.status.success());
        if args[0] == "--json" {
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert_eq!(stdout.lines().count(), 1);
            let value: Value = serde_json::from_str(&stdout).unwrap();
            assert_eq!(value["schema_version"], 1);
            assert_eq!(value["ok"], false);
            assert_eq!(value["error"]["code"], "auth_failed");
            assert_eq!(
                String::from_utf8(output.stderr).unwrap(),
                "Open https://login.microsoft.com/device and enter TEST-CODE\n"
            );
        } else {
            assert!(output.stdout.is_empty());
            assert!(
                String::from_utf8(output.stderr)
                    .unwrap()
                    .contains("tack::auth_failed")
            );
        }
        assert_eq!(
            fs::read_to_string(home.path().join(".config/tack/aks.yml")).unwrap(),
            kubeconfig
        );
    }

    let output = cli(
        home.path(),
        &["--use", "aks", "--", "kubectl", "get", "pods"],
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("tack::auth_failed"));
    assert!(!stderr.starts_with("Open https://"));
}

#[test]
fn json_activation_completes_after_device_login_without_leaking_credentials() {
    let home = TempDir::new().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    configure_azure_device_login(home.path(), &server);
    let api = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        for (path, body) in [
            (
                "/apis/authentication.k8s.io/v1/selfsubjectreviews",
                r#"{"apiVersion":"authentication.k8s.io/v1","kind":"SelfSubjectReview","status":{"userInfo":{"username":"test-user"}}}"#,
            ),
            (
                "/api",
                r#"{"versions":["v1"],"serverAddressByClientCIDRs":[]}"#,
            ),
            ("/apis", r#"{"groups":[]}"#),
        ]
        .into_iter()
        .cycle()
        .take(6)
        {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "timed out waiting for {path}");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut reader = BufReader::new(&stream);
            let mut headers = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                headers.push_str(&line);
            }
            assert!(headers.lines().next().unwrap().contains(path));
            assert!(
                headers
                    .to_lowercase()
                    .contains("authorization: bearer test-plugin-token")
            );
            reader.read_exact(&mut vec![0; length]).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    let output = complete_azure_device_login(home.path(), &["--json", "use", "aks"], b"done\n");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1);
    assert!(!stdout.contains("test-plugin-token"));
    assert!(!stdout.contains("TEST-CODE"));
    let value: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["kind"], "environment");
    assert_eq!(value["data"]["context"], "aks");
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Open https://login.microsoft.com/device and enter TEST-CODE\n"
    );
    let kubectl = home.path().join("kubectl");
    fs::write(
        &kubectl,
        "#!/bin/sh\nprintf '%s\\n' 'cached authentication works'\n",
    )
    .unwrap();
    fs::set_permissions(&kubectl, fs::Permissions::from_mode(0o700)).unwrap();
    let command = Command::new(env!("CARGO_BIN_EXE_tack"))
        .args(["--use", "aks", "--", "kubectl", "get", "pods"])
        .env("HOME", home.path())
        .env("PATH", home.path())
        .env("SSL_CERT_FILE", home.path().join("missing-ca.pem"))
        .env("SSL_CERT_DIR", home.path().join("missing-certs"))
        .env_remove("KUBECONFIG")
        .env_remove("TACK_CONTEXT")
        .output()
        .unwrap();
    assert!(command.status.success(), "{command:?}");
    assert_eq!(command.stdout, b"cached authentication works\n");
    assert!(command.stderr.is_empty());
    api.join().unwrap();
}

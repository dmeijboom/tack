use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
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
fn json_auth_required_never_starts_login_or_a_shell() {
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
    let output = Command::new(env!("CARGO_BIN_EXE_tack"))
        .args(["--json", "use", "gke"])
        .env("HOME", home.path())
        .env("PATH", home.path())
        .env("SHELL", &fake_gcloud)
        .env("TACK_TEST_MARKER", &marker)
        .output()
        .unwrap();
    let value = json(&output);
    assert_eq!(output.status.code(), Some(1));
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
    assert!(!marker.exists());
}

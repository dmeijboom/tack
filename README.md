# tack

A Kubernetes context and namespace switcher. An alternative to [kubie](https://github.com/sbstp/kubie) that uses isolated sub-shells and supports automatic kubeconfig generation for GKE, AKS, and Tcloud clusters.

## How it works

Unlike tools that mutate your global `~/.kube/config`, tack spawns a **new sub-shell** or runs a single command with an isolated `KUBECONFIG` for each context. Exiting the shell or command returns you to your previous context. This means you can have multiple terminals connected to different clusters without conflicts.

When switching to a context for the first time, tack can automatically generate the kubeconfig using `gcloud`, `az`, or `tcloud` CLI tools.

Before switching contexts or namespaces, tack checks Kubernetes authentication with a SelfSubjectReview (Kubernetes 1.28+), rejecting anonymous or missing identities, and probes the `/api` and `/apis` discovery endpoints used by kubectl. During these checks, kubeconfig generation, or namespace listing, an authentication failure or forbidden/access-denied response triggers one kubeconfig regeneration and retry if a generator is configured. Namespace-scoped shells retain their selected namespace when credentials are regenerated.

Tack runs GCP/Azure exec authentication plugins non-interactively during its own checks so login errors can be captured; the stored kubeconfig is unchanged. If GCP or Azure reports that login is required, tack pauses and runs `gcloud auth login` or `az login` interactively in the current console, then retries the failed operation. Login is attempted at most once per invocation, separately from the regeneration retry. Failed login or persistent errors stop the operation. Commands run inside the shell are not intercepted.

## Installation

```sh
cargo install --path .
```

## Usage

```sh
# List all configured contexts
tack list    # or: tack ls

# Switch to a context (spawns a sub-shell)
tack use             # interactive fuzzy picker
tack use my-cluster  # directly by name

# Run one command in a context without opening a shell or prompting for login
tack --use my-cluster -- kubectl get pods --namespace kube-system

# Switch namespace within a context
tack namespace          # or: tack ns (interactive fuzzy picker)
tack namespace kube-system  # directly by name
```

## Diagnostics

Human-readable failures are rendered by `miette` on stderr, with an error code, cause chain, and actionable help. Invalid TOML includes a highlighted source location. Color is detected automatically; set `NO_COLOR=1` to disable it. Failed subprocesses identify the executable and arguments without dumping their environment or credential-plugin stdout.

Exit status is `0` on success, `1` for runtime failures, `2` for invalid arguments, and `130` for cancelled interactive selection. A spawned shell or command's nonzero exit status is propagated.

## One-shot commands

`tack --use <context> -- <command> [args...]` validates access and runs the command directly with isolated `KUBECONFIG`, `TACK_CONTEXT`, and `TACK_ENABLED` overrides. The `--` separator and a command are required. Arguments after `--` are passed unchanged, without shell evaluation; stdout and stderr are inherited.

Activation is non-interactive, using the same checks and credential recovery as JSON mode. If cloud login is needed, tack reports the error without launching login or the command. Authenticate in a terminal and retry. The child command runs once and is not intercepted or retried. This mode cannot be combined with `--json` or a tack subcommand.

## JSON / LLM automation

`--json` is global and may appear before or after a subcommand. It emits exactly one JSON object on stdout, including for errors, help, and version output. Progress and successful child-process output are suppressed. Errors still return a nonzero exit status.

JSON mode never opens a shell, fuzzy picker, or interactive cloud login. It may still contact Kubernetes and generate/regenerate cached kubeconfigs. When login is needed, it returns `auth_required` with a command represented as an executable plus an argument array; run that command interactively, then retry.

| Command | `data.kind` | Result |
| --- | --- | --- |
| `tack --json list` | `contexts` | Sorted configured context names |
| `tack --json use` | `contexts` | Context choices instead of a picker |
| `tack --json use <context>` | `environment` | Validated kubeconfig path, namespace, and environment overrides; no shell |
| `tack --json namespace` | `namespaces` | Sorted accessible namespaces and the current namespace; requires `KUBECONFIG` |
| `tack --json namespace <namespace>` | `environment` | A separate namespace-scoped kubeconfig and environment overrides; no shell |
| `tack --json --help` | `help` | Plain help text |
| `tack --json --version` | `version` | Version string |

Success:

```json
{"schema_version":1,"ok":true,"data":{"kind":"contexts","contexts":["production-gke","staging-aks"]}}
```

Failure:

```json
{
  "schema_version": 1,
  "ok": false,
  "error": {
    "code": "auth_required",
    "message": "Cloud authentication requires interactive login",
    "help": "Authenticate in a terminal, then retry.",
    "causes": ["Kubernetes credential plugin failed"],
    "path": "/home/user/.config/tack/staging-aks.yml",
    "command": {
      "program": "az",
      "args": ["login", "--subscription", "subscription-id", "--tenant", "tenant-id", "--scope", "resource/.default"]
    },
    "status": null,
    "http_status": null,
    "exit_code": 1,
    "span": null
  }
}
```

Branch on `error.code`, not the human-readable message. Codes include `config_not_found`, `config_invalid`, `context_not_found`, `context_required`, `kubeconfig_missing`, `auth_required`, `auth_failed`, `access_denied`, `kubernetes_failed`, and `command_failed`. Optional error metadata is `null` when unavailable. `status` is a subprocess exit status; `http_status` is a Kubernetes HTTP status. TOML `span` offsets and lengths are in bytes.

An `environment` result contains `context`, `namespace`, `kubeconfig`, `env`, and `cleanup`. Pass `data.env` as environment overrides to subsequent commands; do not evaluate it as shell code. No tokens, certificates, or kubeconfig contents are returned. Use the returned `KUBECONFIG` and `TACK_CONTEXT` for subsequent namespace operations.

For namespace selection, `cleanup` lists the private temporary kubeconfig to delete when finished. JSON mode retains this file after tack exits and does not modify the input kubeconfig's namespace. Cached context kubeconfigs have an empty cleanup list and should not be deleted.

## Agent Skill

The portable [`tack` skill](.agents/skills/tack/SKILL.md) lets agents validate a context once with `tack --json use <context>`, then inspect Kubernetes through `tack --use <context> -- kubectl ...` with a strict read-only policy. Namespaces are selected with kubectl's `--namespace` flag. Every command explicitly selects its tack context. It never uses interactive subshells or changes the global kubeconfig.

Pi discovers it from `.agents/skills/tack/` in this repository; run `/reload` in an existing session, then use `/skill:tack`. To use it from other projects, copy that directory into your agent's user skills directory (for Pi, `~/.pi/agent/skills/tack/`). The skill requires the installed `tack` and `kubectl` executables, configured contexts, and any necessary cloud authentication.

The skill permits non-mutating kubectl diagnostics, including loopback-only port forwarding and proxying for read-only access. It prohibits Kubernetes and application writes, container execution, plugins, raw kubectl API requests, and credential overrides. This is an agent policy, **not an enforcement boundary**: use Kubernetes RBAC with read-only credentials to enforce resource and namespace restrictions. Activation can still refresh local kubeconfigs and credential caches and perform tack's non-persisted authentication review.

## Configuration

Configuration lives at `~/.config/tack/config.toml`. Generated kubeconfigs are stored as YAML files in `~/.config/tack/`.

You can override the kubeconfig storage directory with the `kubeconfig_dir` option.
You can also set `shell` to choose which shell tack spawns. When omitted, tack falls back to `$SHELL`, then `/bin/sh`.

### Example

```toml
# Optional: override where generated kubeconfigs are stored
# kubeconfig_dir = "/home/user/.config/tack"

# Optional: override which shell `tack use` and `tack namespace` spawn
# shell = "/bin/zsh"

# GKE cluster via gcloud
[context.production-gke]
generator = "gcloud"
project = "my-gcp-project"
location = "europe-west1"
# Optional: use the same account and named configuration for generation and login
# account = "user@example.com"
# configuration = "work"

# AKS cluster via az
[context.staging-aks]
generator = "aks"
subscription = "my-azure-subscription-id"
resource-group = "my-resource-group"
# Optional: explicitly target the tenant during az login
# tenant = "my-azure-tenant-id"

# Tcloud cluster
[context.dev-tcloud]
generator = "tcloud"
organisation = "my-org"

# Context without a generator (kubeconfig must already exist in the store)
[context.local-kind]
```

GCP login receives the configured project and optional account/configuration. Azure login receives the configured subscription and tenant, falling back to Azure's login recommendation or the active kubeconfig for the tenant when not configured. Its scope comes from Azure's recommendation or the AKS exec plugin's server ID. Tack only reads tenant/scope arguments from recommendations; it does not execute suggested shell commands or automatically log out of Azure. Credential generation also uses the configured resource group. Azure credentials are written to an explicit isolated file, not the global kubeconfig.

## Shell integration

tack exports `TACK_CONTEXT` with the configured context name so namespace operations can find its generator, including inside namespace-scoped shells.

tack sets `TACK_ENABLED=1` in the sub-shell. You can use this to show the active tack session in your prompt:

```sh
# zsh example
if [[ -n "$TACK_ENABLED" ]]; then
  PROMPT="(tack) $PROMPT"
fi
```

## License

MIT

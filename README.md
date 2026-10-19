# tack

A Kubernetes context and namespace switcher. An alternative to [kubie](https://github.com/sbstp/kubie) that uses isolated sub-shells and supports automatic kubeconfig generation for GKE, AKS, and Tcloud clusters.

## How it works

Unlike tools that mutate your global `~/.kube/config`, tack spawns a **new sub-shell** with an isolated `KUBECONFIG` for each context. Exiting the shell returns you to your previous context. This means you can have multiple terminals connected to different clusters without conflicts.

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

# Switch namespace within a context
tack namespace          # or: tack ns (interactive fuzzy picker)
tack namespace kube-system  # directly by name
```

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

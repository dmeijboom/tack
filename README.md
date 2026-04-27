# tack

A Kubernetes context and namespace switcher. An alternative to [kubie](https://github.com/sbstp/kubie) that uses isolated sub-shells and supports automatic kubeconfig generation for GKE, AKS, and Tcloud clusters.

## How it works

Unlike tools that mutate your global `~/.kube/config`, tack spawns a **new sub-shell** with an isolated `KUBECONFIG` for each context. Exiting the shell returns you to your previous context. This means you can have multiple terminals connected to different clusters without conflicts.

When switching to a context for the first time, tack can automatically generate the kubeconfig using `gcloud`, `az`, or `tcloud` CLI tools.

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

### Example

```toml
# Optional: override where generated kubeconfigs are stored
# kubeconfig_dir = "/home/user/.config/tack"

# GKE cluster via gcloud
[context.production-gke]
generator = "gcloud"
project = "my-gcp-project"
location = "europe-west1"

# AKS cluster via az
[context.staging-aks]
generator = "aks"
subscription = "my-azure-subscription-id"
resource-group = "my-resource-group"

# Tcloud cluster
[context.dev-tcloud]
generator = "tcloud"
organisation = "my-org"

# Context without a generator (kubeconfig must already exist in the store)
[context.local-kind]
```

## Shell integration

tack sets `TACK_ENABLED=1` in the sub-shell. You can use this to show the active tack session in your prompt:

```sh
# zsh example
if [[ -n "$TACK_ENABLED" ]]; then
  PROMPT="(tack) $PROMPT"
fi
```

## License

MIT

# Migrate Inference Routing

Guide for migrating from the old multi-route inference system (`nemoclaw inference create/update/delete/list` + `inference.allowed_routes` policy) to the new `inference.local` architecture.

## What Changed

The old system used:
- **Top-level CRUD commands**: `nemoclaw inference create --routing-hint local --base-url https://api.openai.com/v1 --model-id gpt-4 --api-key sk-...`
- **Implicit catch-all policy**: An `inference:` section in sandbox policy YAML with `allowed_routes: [local]` that caused *all* unmatched connections to be TLS-intercepted and inspected for inference API patterns
- **Tri-state OPA action**: `allow`, `inspect_for_inference`, or `deny`
- **Per-route endpoint/key storage**: Each route stored its own `base_url`, `api_key`, `protocols`

The new system uses:
- **Cluster-scoped config**: `nemoclaw cluster inference set --provider <name> --model <id>`
- **Explicit routing**: Agents connect to `inference.local` (a virtual host) -- no implicit catch-all
- **Binary OPA policy**: `allow` or `deny` only -- inference interception is a separate pre-OPA path
- **Provider-backed resolution**: Only `provider_name` and `model_id` are stored; endpoint, API key, protocols, and auth style are resolved dynamically from the provider record

## Migration Steps

Follow these steps in order. Each step includes diagnostics to verify success.

### Step 1: Inventory existing inference routes

Before destroying anything, capture the current state.

```bash
# If you still have the old cluster running:
nemoclaw inference list
```

Record each route's:
- `routing_hint` (e.g., `local`, `frontier`)
- `base_url` (e.g., `https://api.openai.com/v1`)
- `protocols` (e.g., `openai_chat_completions`)
- `model_id`
- Whether it used an API key

**Categorize each route:**

| Category | Description | Migration action |
|----------|-------------|------------------|
| **Managed inference** | Routes used by agents via the implicit catch-all (the common case) | Migrate to `inference.local` via provider + cluster config |
| **External API** | Routes to third-party APIs that agents call directly (e.g., a custom model endpoint) | Add explicit `network_policies` entry + configure the client with the real endpoint |

### Step 2: Recreate the cluster

The schema change is not backward-compatible. You must destroy and recreate the cluster.

```bash
# Record current cluster info
nemoclaw cluster admin info

# Destroy the old cluster
nemoclaw cluster admin destroy

# Deploy a fresh cluster (same flags you used originally)
nemoclaw cluster admin deploy [--name <name>] [--remote <host>] [--ssh-key <key>]

# Verify
nemoclaw cluster status
```

For remote clusters, ensure you pass the same `--remote` and `--ssh-key` flags.

### Step 3: Recreate provider records

Provider records are stored in the cluster database and must be recreated.

```bash
# Example: OpenAI provider
nemoclaw provider create \
  --name openai \
  --type openai \
  --credential OPENAI_API_KEY

# Example: Anthropic provider
nemoclaw provider create \
  --name anthropic \
  --type anthropic \
  --credential ANTHROPIC_API_KEY

# Example: NVIDIA provider
nemoclaw provider create \
  --name nvidia \
  --type nvidia \
  --credential NVIDIA_API_KEY

# Verify
nemoclaw provider list
```

If you used `--from-existing` before, you can use it again to auto-discover credentials from your local environment.

### Step 4: Configure cluster inference

Replace the old `nemoclaw inference create` with:

```bash
nemoclaw cluster inference set \
  --provider <provider-name> \
  --model <model-id>

# Verify
nemoclaw cluster inference get
```

**Mapping from old to new:**

| Old command | New equivalent |
|-------------|----------------|
| `nemoclaw inference create --routing-hint local --base-url https://api.openai.com/v1 --model-id gpt-4 --api-key sk-...` | `nemoclaw provider create --name openai --type openai --credential OPENAI_API_KEY` then `nemoclaw cluster inference set --provider openai --model gpt-4` |
| `nemoclaw inference update my-route --model-id gpt-4.1` | `nemoclaw cluster inference update --model gpt-4.1` |
| `nemoclaw inference delete my-route` | Not needed -- there is only one managed route, reconfigure with `set` |
| `nemoclaw inference list` | `nemoclaw cluster inference get` |

### Step 5: Update sandbox policies

Remove the `inference:` section and add explicit `network_policies` for any external endpoints.

**Before (old format):**

```yaml
version: 1
network_policies:
  github:
    name: github
    endpoints:
      - { host: api.github.com, port: 443 }
    binaries:
      - { path: /usr/bin/gh }

inference:
  allowed_routes:
    - local
```

**After (new format):**

```yaml
version: 1
network_policies:
  github:
    name: github
    endpoints:
      - { host: api.github.com, port: 443 }
    binaries:
      - { path: /usr/bin/gh }
```

The `inference:` section is simply removed. Inference routing via `inference.local` works automatically when cluster inference is configured -- no policy entry is needed.

**If you had external API routes** (not going through `inference.local`), add them as explicit network policies:

```yaml
network_policies:
  # Agent calls this endpoint directly (not through inference.local)
  custom_model:
    name: custom-model-endpoint
    endpoints:
      - host: my-model-server.example.com
        port: 443
    binaries:
      - { path: /usr/local/bin/python3 }
```

### Step 6: Update agent code (if needed)

Agents using the **implicit catch-all** need a one-line change: point their SDK at `inference.local` instead of the real API endpoint.

**Before (implicit catch-all):**

```python
# Old: agent connects to api.openai.com, proxy intercepts transparently
client = openai.OpenAI(api_key="dummy-key")
```

**After (explicit inference.local):**

```python
# New: agent connects to inference.local explicitly
client = openai.OpenAI(
    base_url="https://inference.local/v1",
    api_key="dummy-key",  # stripped by proxy, real key injected from provider
)
```

For agents that already targeted `inference.local`, no code changes are needed.

For agents calling external APIs directly (not through inference routing), ensure they use the real endpoint and have a matching `network_policies` entry.

### Step 7: Recreate sandboxes and verify

```bash
# Create a sandbox with the updated policy
nemoclaw sandbox create --name test --policy updated-policy.yaml -- python my_agent.py

# Watch logs for any blocked connections
nemoclaw sandbox logs test --tail --source sandbox
```

## Diagnosing Blocked Connections

If an agent's requests are being denied, use sandbox logs to identify the issue.

### Reading deny logs

```bash
# Stream logs, filter for sandbox-level events
nemoclaw sandbox logs <name> --tail --source sandbox --level info
```

**Deny log structure:**

```
CONNECT src_addr=... dst_host=api.example.com dst_port=443 binary=/usr/bin/python3
  action=deny reason="network connections not allowed by policy"
```

Key fields:

| Field | Meaning |
|-------|---------|
| `action=deny` | The connection was blocked |
| `dst_host` + `dst_port` | What the process tried to reach |
| `binary` | Which binary initiated the connection |
| `reason` | Why it was denied |

### Common deny scenarios after migration

| Symptom | Log pattern | Fix |
|---------|-------------|-----|
| Agent can't reach `inference.local` | No deny log (pre-OPA path) but inference interception denied log | Check `nemoclaw cluster inference get` -- is inference configured? |
| Agent can't reach external API | `action=deny dst_host=api.example.com` | Add `network_policies` entry for that endpoint + binary |
| Wrong binary blocked | `action=deny binary=/usr/bin/curl` but policy allows `/usr/local/bin/python3` | Add the correct binary to the policy's `binaries` list |
| Connection allowed but L7 denied | L7 request log with `action=deny` | Check `access` preset or `rules` in the endpoint config |

### Inference interception deny log

```
Inference interception denied action=deny reason="cluster inference context not configured" host=inference.local
```

This means the agent connected to `inference.local` but no cluster inference is configured. Fix: run `nemoclaw cluster inference set`.

## Quick Reference

| Task | Command |
|------|---------|
| Check old routes (before migration) | `nemoclaw inference list` |
| Destroy old cluster | `nemoclaw cluster admin destroy` |
| Deploy fresh cluster | `nemoclaw cluster admin deploy` |
| Create provider | `nemoclaw provider create --name <n> --type <t> --credential <KEY>` |
| Set cluster inference | `nemoclaw cluster inference set --provider <n> --model <m>` |
| Update model only | `nemoclaw cluster inference update --model <m>` |
| Check cluster inference | `nemoclaw cluster inference get` |
| Apply updated policy | `nemoclaw sandbox policy set <name> --policy <path>` |
| Diagnose blocked connections | `nemoclaw sandbox logs <name> --tail --source sandbox` |

## Related Skills

| Skill | When to use |
|-------|-------------|
| `generate-sandbox-policy` | Generate new policies from scratch for the new format |
| `nemoclaw-cli` | General CLI usage, cluster management, sandbox operations |
| `debug-navigator-cluster` | Cluster health issues after redeployment |

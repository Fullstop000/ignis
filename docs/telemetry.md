# Telemetry (OpenTelemetry)

Ignis can export session traces and token-usage metrics over OTLP so you can
answer questions like:

- How much did this session cost?
- Which tools fail most often?
- Why was this turn slow — model latency or tool execution?
- How many tokens are we burning per day across the team?

Telemetry is **off by default**. The data never leaves your machine unless you
configure an OTLP endpoint that points off-box.

## Enable

Two equivalent switches; the env var takes precedence:

```bash
# Per-shell
export IGNIS_ENABLE_TELEMETRY=1
```

```toml
# ~/.ignis/config.toml
[telemetry]
enabled = true
```

Once enabled, configure the destination with the standard OTel env vars (Ignis
intentionally does not duplicate them in TOML — they're the universal vocabulary):

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317   # default
export OTEL_EXPORTER_OTLP_PROTOCOL=grpc                    # or http/protobuf
export OTEL_EXPORTER_OTLP_HEADERS="x-api-key=…"            # for hosted backends
export OTEL_RESOURCE_ATTRIBUTES="team.id=ml,cost_center=research"
```

Inspect runtime state any time inside the TUI with `/telemetry`.

## View the metrics — pick a tier

| Tier | What | Setup time | Use when |
|---|---|---|---|
| **`otel-tui`** (recommended) | Single-binary terminal UI, embedded OTLP receiver | 1 min | Personal use, dogfooding |
| Console / file exporter | Built-in OTel SDK exporter to stdout / JSONL | 0 min | Debugging instrumentation |
| Docker Grafana stack | Persistent Grafana dashboards | 5 min | Want history + saved queries |
| Hosted (Honeycomb / Datadog / Grafana Cloud / SigNoz) | Cloud OTLP backend | Pre-existing | Team-wide aggregation |

### Tier 1 — `otel-tui` (default)

Single binary; nothing else to install.

```bash
brew install ymtdzzz/tap/otel-tui     # or download from GitHub releases
otel-tui                              # listens on :4317
```

In another shell:

```bash
IGNIS_ENABLE_TELEMETRY=1 ignis "what files are in this directory?"
```

You'll see the session trace appear in `otel-tui` in real time, with the
`ignis.session` → `ignis.turn` → `ignis.llm_request` / `ignis.tool.execution`
hierarchy and the metric points panel.

### Tier 2 — console / file exporter (debug)

```bash
IGNIS_ENABLE_TELEMETRY=1 \
OTEL_TRACES_EXPORTER=console \
OTEL_METRICS_EXPORTER=console \
ignis "..."
```

Pretty-prints OTLP payloads to stdout. Use for inspecting exactly what Ignis emits.

### Tier 3 — Docker Grafana stack (optional, persistent)

For a long-running dashboard with history:

```bash
docker run -d --name lgtm -p 3000:3000 -p 4317:4317 -p 4318:4318 \
  grafana/otel-lgtm:latest
```

Then open `http://localhost:3000` (admin/admin). Traces appear under
**Explore → Tempo**; metrics under **Explore → Prometheus**.

### Tier 4 — hosted backend

Point at any OTLP-compliant ingestion endpoint:

```bash
# Honeycomb
export OTEL_EXPORTER_OTLP_ENDPOINT=https://api.honeycomb.io
export OTEL_EXPORTER_OTLP_HEADERS="x-honeycomb-team=<KEY>"

# Grafana Cloud / Datadog / SigNoz / New Relic — same pattern, vendor's URL + key.
```

## Privacy

Sensitive content is redacted by default. Opt in tier-by-tier:

| Field | Default | Override env var |
|---|---|---|
| Prompt content | Redacted (length only on the span) | `IGNIS_LOG_USER_PROMPTS=1` |
| Tool arguments / results | Redacted (name + success only) | `IGNIS_LOG_TOOL_DETAILS=1` |
| LLM request / response bodies | Never exported | (no flag — out of scope) |

Existing API-key regex (`sk-…`) is applied to any string that *is* exported, as
defense in depth.

## What ships

### Spans

| Name | When | Attributes |
|---|---|---|
| `ignis.session` | `Session::open` | `session.id`, `provider`, `cwd` |
| `ignis.turn` | Each user prompt | `session.id`, `prompt.length` (+ `prompt.text` iff opted in) |
| `ignis.llm_request` | Each LLM API call | `provider`, `model`, `input_tokens`, `output_tokens`, `reasoning_tokens`, `success` |
| `ignis.tool.execution` | Each tool call | `tool.name`, `tool.call_id`, `success`, `is_error` (+ `tool.arguments` iff opted in) |

### Metrics

| Metric | Kind | Labels |
|---|---|---|
| `ignis.session.count` | Counter | `provider`, `model` |
| `ignis.token.usage` | Counter | `type` ∈ {input, output, reasoning, cache_read, cache_write}, `provider`, `model` |
| `gen_ai.client.token.usage` | Counter (OTel GenAI semconv mirror) | `gen_ai.system`, `gen_ai.request.model`, `gen_ai.token.type` |
| `ignis.tool.calls` | Counter | `tool.name`, `success` |
| `ignis.tool.call.duration_ms` | Histogram | `tool.name` |
| `ignis.llm.request.duration_ms` | Histogram | `provider`, `model`, `success` |

### Resource attributes

`service.name=ignis`, `service.version=<crate version>`, plus anything from
`OTEL_RESOURCE_ATTRIBUTES`.

## Known limitations (v1)

- **Anthropic provider does not emit token usage** at all today (separate
  pre-existing gap in the provider's stream parser). Anthropic users will see
  spans but zero token-usage metric points. Follow-up issue:
  add `message_delta` event parsing to `provider/anthropic.rs`.
- **No distributed tracing** into bash/MCP subprocesses (`traceparent`
  propagation). Deferred to v2; rare need for the current use cases.
- **Cost is not computed inside ignis.** Compute it backend-side from
  `ignis.token.usage` × your model price table; ignis ships the labeled token
  counts and the model identifier.
- **No log signal** in v1 — only traces + metrics. `~/.ignis/logs/ignis.log`
  remains the file-based diagnostic log via `simplelog`.

## Building without telemetry

The default binary includes the OTel stack (~3-5 MB). To strip it:

```bash
cargo build --release --no-default-features
```

The `record_*` calls in the agent loop become no-ops; the `tracing` macros
expand to nothing without a subscriber. All public API stays callable.

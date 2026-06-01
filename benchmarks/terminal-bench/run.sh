#!/usr/bin/env bash
# Reproducible TB runner for the ignis harbor adapter.
#
# Two modes:
#   * Interactive (TTY, no env overrides) — prompts for benchmark, model, env,
#     prints a summary, asks to confirm, then runs.
#   * Non-interactive (env-driven) — same defaults; nothing prompts.
#     Useful for CI / scripted re-runs.
#
# Hard-won defaults baked in:
#   * --max-retries 2            : a transient cloud blip retries the trial
#                                  instead of crashing the whole job.
#   * --agent-timeout-multiplier : compute-heavy tasks (builds, ML training)
#                                  need more than the stock budget.
#   * disk + concurrency preset  : torch/cudnn wheels need real disk, or the
#                                  *verifier* dies with "No space left on device"
#                                  and never tests the agent's (correct) work.
#                                  On Daytona, disk × concurrency must fit the
#                                  ~50 GB account quota, so bigger disk => lower -n.
#
# Keys are read from ~/.ignis/config.toml (provider api_key + optional
# [web_search]); DAYTONA_API_KEY is read from the env. Nothing is printed.
#
# Usage:
#   ./run.sh                                  # interactive picker
#   MODEL=anthropic/claude-haiku-4-5 ./run.sh # skip the model prompt
#   ENV=docker ./run.sh                       # skip the env prompt
#   DRY_RUN=1 ./run.sh                        # print what would run, don't run
#
# Env overrides (any subset): MODEL, ENV (daytona|docker|novita), DATASET,
#                             NCONC, STORAGE_MB, TIMEOUT_MULT, MAX_RETRIES,
#                             JOB_NAME, DRY_RUN, NO_PROMPT.
set -eo pipefail
cd "$(dirname "$0")"

CONFIG="$HOME/.ignis/config.toml"
[ -f "$CONFIG" ] || { echo "run.sh: $CONFIG not found (need provider keys)" >&2; exit 2; }

# ---------- interactive prompts -----------------------------------------------
# Only fire when stdin+stdout are a terminal AND the value wasn't pre-set in env.
# NO_PROMPT=1 forces non-interactive even on a TTY (lets you script with full defaults).
_interactive() { [ -t 0 ] && [ -t 1 ] && [ "${NO_PROMPT:-0}" != "1" ]; }

# Render a numbered menu on stderr, read selection on stdin, echo the picked
# *key* on stdout. Args: prompt, default-key, then key:label pairs.
_pick() {
    local label="$1" default_key="$2"; shift 2
    local choices=("$@")
    {
        echo
        echo "$label"
        local i=1
        for c in "${choices[@]}"; do
            local key="${c%%:*}" desc="${c#*:}"
            local mark="  "; [ "$key" = "$default_key" ] && mark=" *"
            printf "%s%d) %s\n" "$mark" "$i" "$desc"
            i=$((i+1))
        done
        echo "  c) custom"
        echo "     (* = default; press Enter to accept)"
    } >&2
    local sel; read -rp "> " sel || sel=""
    if [ -z "$sel" ]; then
        echo "$default_key"
    elif [ "$sel" = c ] || [ "$sel" = C ]; then
        local v; read -rp "value: " v || v=""
        echo "${v:-$default_key}"
    elif [[ "$sel" =~ ^[0-9]+$ ]] && [ "$sel" -ge 1 ] && [ "$sel" -le ${#choices[@]} ]; then
        local picked="${choices[$((sel-1))]}"
        echo "${picked%%:*}"
    else
        echo "$default_key"
    fi
}

if _interactive; then
    [ -z "${DATASET:-}" ] && DATASET="$(_pick "Benchmark:" \
        "terminal-bench/terminal-bench-2-1" \
        "terminal-bench/terminal-bench-2-1:Terminal-Bench 2.1 (89 tasks)" \
        "terminal-bench/terminal-bench-2:Terminal-Bench 2.0 (89 tasks, older)")"

    [ -z "${MODEL:-}" ] && MODEL="$(_pick "Model (provider/name[@effort]):" \
        "deepseek/deepseek-v4-flash@max" \
        "deepseek/deepseek-v4-flash@max:deepseek/deepseek-v4-flash@max" \
        "anthropic/claude-haiku-4-5:anthropic/claude-haiku-4-5" \
        "anthropic/claude-sonnet-4-6:anthropic/claude-sonnet-4-6" \
        "openai/gpt-5:openai/gpt-5" \
        "gemini/gemini-2-5-pro:gemini/gemini-2-5-pro" \
        "kimi-code/k2-thinking:kimi-code/k2-thinking")"

    [ -z "${ENV:-}" ] && ENV="$(_pick "Sandbox env:" \
        "daytona" \
        "daytona:daytona  (cloud, needs DAYTONA_API_KEY, ~50 GB quota, n=3)" \
        "novita:novita   (cloud, cheapest, n=8)" \
        "docker:docker   (local, no cloud quota, n=4)")"
fi

# ---------- defaults (applied to whatever the prompts / env left unset) -------
MODEL="${MODEL:-deepseek/deepseek-v4-flash@max}"
ENV="${ENV:-daytona}"
DATASET="${DATASET:-terminal-bench/terminal-bench-2-1}"
TIMEOUT_MULT="${TIMEOUT_MULT:-2.0}"
MAX_RETRIES="${MAX_RETRIES:-2}"

# ---------- provider key (from the [providers.<name>] block in config.toml) ---
provider="${MODEL%%/*}"
case "$provider" in
    anthropic) key_env=ANTHROPIC_API_KEY ;;
    openai)    key_env=OPENAI_API_KEY ;;
    gemini)    key_env=GEMINI_API_KEY ;;
    deepseek)  key_env=DEEPSEEK_API_KEY ;;
    kimi-code) key_env=KIMI_CODE_API_KEY ;;
    *) echo "run.sh: unknown provider '$provider' in MODEL=$MODEL" >&2; exit 2 ;;
esac
_toml_key() { # $1 = section header (e.g. [web_search]); reads api_key under it.
    # Compare against a de-quoted copy of each line so both `[providers.kimi-code]`
    # and the quoted `[providers."kimi-code"]` form (valid TOML, used in
    # config.example.toml) match the unquoted section we build.
    awk -v sect="$1" '
        { hdr=$0; gsub(/"/, "", hdr) }
        hdr==sect {f=1; next} /^\[/{f=0}
        f && /^api_key/ {gsub(/^api_key *= *"/,""); gsub(/".*/,""); print; exit}
    ' "$CONFIG"
}
export "$key_env"="$(_toml_key "[providers.$provider]")"
if [ -z "$(eval "echo \$$key_env")" ]; then
    echo "run.sh: no api_key for [providers.$provider] in $CONFIG" >&2; exit 2
fi

# ---------- optional web_search key (forwarded by the adapter into the sandbox)
ws_provider="$(awk '/^\[web_search\]/{f=1;next} /^\[/{f=0} f && /^provider/{gsub(/^provider *= *"/,""); gsub(/".*/,""); print; exit}' "$CONFIG")"
ws_provider="${ws_provider:-brave}"  # ignis defaults web_search to brave when unset
ws_key="$(_toml_key "[web_search]")"
if [ -n "$ws_key" ]; then
    case "$ws_provider" in
        brave)  export BRAVE_API_KEY="$ws_key" ;;
        tavily) export TAVILY_API_KEY="$ws_key" ;;
    esac
fi

# ---------- Daytona key (cloud envs only) -------------------------------------
export DAYTONA_API_KEY="${DAYTONA_API_KEY:-${DAYTONA_KEY_IGNIS_TB2:-}}"

# ---------- env preset: disk vs concurrency -----------------------------------
case "$ENV" in
    daytona) NCONC="${NCONC:-3}";  STORAGE_MB="${STORAGE_MB:-16000}"; storage=(--override-storage-mb "$STORAGE_MB") ;;
    novita)  NCONC="${NCONC:-8}";  STORAGE_MB="${STORAGE_MB:-20000}"; storage=(--override-storage-mb "$STORAGE_MB") ;;
    docker)  NCONC="${NCONC:-4}";  storage=() ;;  # local disk; no sandbox quota override
    *) echo "run.sh: ENV must be daytona|docker|novita (got '$ENV')" >&2; exit 2 ;;
esac

ts="$(date +%Y%m%d-%H%M%S)"
slug="$(echo "$MODEL" | tr '/@' '--')"
JOB_NAME="${JOB_NAME:-ignis-$slug-$ENV}"
OUT="runs/$slug-$ENV-$ts"

# ---------- summary + confirm -------------------------------------------------
{
    echo
    echo "Plan:"
    echo "  benchmark : $DATASET"
    echo "  model     : $MODEL"
    echo "  env       : $ENV  (n=$NCONC, retries=$MAX_RETRIES, timeout x$TIMEOUT_MULT${STORAGE_MB:+, disk=${STORAGE_MB}MB})"
    echo "  web_search: $([ -n "$ws_key" ] && echo "$ws_provider (key forwarded)" || echo "disabled (no key in config)")"
    echo "  output    : $OUT"
    echo "  job-name  : $JOB_NAME"
} >&2

if [ "${DRY_RUN:-0}" = "1" ]; then
    echo "DRY_RUN=1 — not invoking harbor." >&2
    exit 0
fi

if _interactive; then
    read -rp "start? [Y/n] " ok || ok="n"  # treat EOF (Ctrl-D) as abort, not a silent set -e exit
    case "$ok" in
        ""|y|Y|yes|YES) ;;
        *) echo "aborted." >&2; exit 0 ;;
    esac
fi

source .venv/bin/activate
echo "[$(date +%T)] harbor run -> $OUT  (model=$MODEL env=$ENV n=$NCONC retries=$MAX_RETRIES timeout x$TIMEOUT_MULT)"
harbor run \
    -d "$DATASET" \
    -m "$MODEL" \
    --agent-import-path ignis_agent.agent:IgnisAgent \
    -e "$ENV" \
    -n "$NCONC" \
    --max-retries "$MAX_RETRIES" \
    --agent-timeout-multiplier "$TIMEOUT_MULT" \
    "${storage[@]}" \
    --job-name "$JOB_NAME" \
    -o "$OUT" </dev/null 2>&1 | tee "$OUT.log"

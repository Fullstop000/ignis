#!/usr/bin/env bash
# Reproducible TB2 runner for the ignis harbor adapter.
#
# Encodes the defaults we learned the hard way from the first full run:
#   * --max-retries 2            : a transient cloud/control-plane blip retries
#                                  the trial instead of crashing the whole job.
#   * --agent-timeout-multiplier : compute-heavy tasks (builds, ML training)
#                                  need more than the stock budget.
#   * disk + concurrency preset  : torch/cudnn wheels need real disk, or the
#                                  *verifier* dies with "No space left on device"
#                                  and never tests the agent's (correct) work.
#                                  On Daytona, disk x concurrency must fit the
#                                  ~50 GB account quota, so bigger disk => lower -n.
#
# Keys are read from ~/.ignis/config.toml (provider api_key + optional
# [web_search]); DAYTONA_API_KEY is read from the env. Nothing is printed.
#
# Usage:
#   ./run.sh                                  # deepseek/deepseek-v4-flash@max on Daytona
#   MODEL=anthropic/claude-haiku-4-5 ./run.sh
#   ENV=docker ./run.sh                       # local, no cloud quota
#   ENV=novita ./run.sh                       # cheapest cloud, roomy disk
#
# Env overrides: MODEL, ENV (daytona|docker|novita), DATASET, NCONC, STORAGE_MB,
#                TIMEOUT_MULT, MAX_RETRIES, JOB_NAME.
set -eo pipefail
cd "$(dirname "$0")"

MODEL="${MODEL:-deepseek/deepseek-v4-flash@max}"
ENV="${ENV:-daytona}"
DATASET="${DATASET:-terminal-bench/terminal-bench-2}"
TIMEOUT_MULT="${TIMEOUT_MULT:-2.0}"
MAX_RETRIES="${MAX_RETRIES:-2}"
CONFIG="$HOME/.ignis/config.toml"
[ -f "$CONFIG" ] || { echo "run.sh: $CONFIG not found (need provider keys)" >&2; exit 2; }

# --- provider key (from the [providers.<name>] block in config.toml) ----------
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

# --- optional web_search key (forwarded by the adapter into the sandbox) ------
ws_provider="$(awk '/^\[web_search\]/{f=1;next} /^\[/{f=0} f && /^provider/{gsub(/^provider *= *"/,""); gsub(/".*/,""); print; exit}' "$CONFIG")"
ws_provider="${ws_provider:-brave}"  # ignis defaults web_search to brave when unset
ws_key="$(_toml_key "[web_search]")"
if [ -n "$ws_key" ]; then
    case "$ws_provider" in
        brave)  export BRAVE_API_KEY="$ws_key" ;;
        tavily) export TAVILY_API_KEY="$ws_key" ;;
    esac
fi

# --- Daytona key (cloud envs only) --------------------------------------------
export DAYTONA_API_KEY="${DAYTONA_API_KEY:-${DAYTONA_KEY_IGNIS_TB2:-}}"

# --- env preset: disk vs concurrency ------------------------------------------
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

"""
Harbor adapter that runs ignis inside the sandbox to solve a Terminal-Bench task.

Wire-up:
    harbor run -d terminal-bench/terminal-bench-2 \\
        -m anthropic/claude-haiku-4-5 \\
        --agent-import-path ignis_agent.agent:IgnisAgent

The harbor `-m` argument is parsed into provider/model[@effort]. The optional
`@effort` suffix (e.g. `deepseek/deepseek-v4-flash@max`) selects the model's
reasoning level — the generated TOML declares it as a supported level for the
model entry and sets the top-level `reasoning_effort`. The provider segment
selects which env var holds the API key and (for OpenAI-compatible providers
that need a URL) the default endpoint. We write a minimal ~/.ignis/config.toml
inside the sandbox and invoke `ignis -- "<instruction>"` — a non-empty prompt arg
switches ignis off TUI mode into one-shot streaming (`--` lets instructions that
start with `-` through clap's flag parser).
"""

import json
import shlex

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.models.trial.paths import EnvironmentPaths


# provider segment -> (env var with API key, optional api_url to embed in TOML).
# api_url is None when ignis already hardcodes the endpoint internally.
_PROVIDERS: dict[str, tuple[str, str | None]] = {
    "anthropic": ("ANTHROPIC_API_KEY", None),
    "openai": ("OPENAI_API_KEY", "https://api.openai.com/v1"),
    "gemini": ("GEMINI_API_KEY", None),
    "deepseek": ("DEEPSEEK_API_KEY", None),
    # ignis hardcodes https://api.kimi.com/coding/v1 and the KimiCLI User-Agent
    # the Kimi Coding Plan requires; we only need to forward the key.
    "kimi-code": ("KIMI_CODE_API_KEY", None),
}


class IgnisAgent(BaseInstalledAgent):
    """Run ignis (https://github.com/Fullstop000/ignis) as the harbor agent."""

    _OUTPUT_FILENAME = "ignis.txt"

    @staticmethod
    def name() -> str:
        return "ignis"

    def get_version_command(self) -> str | None:
        return "ignis --version"

    def parse_version(self, stdout: str) -> str:
        # `ignis --version` prints e.g. "ignis 0.15.1".
        first = next((ln.strip() for ln in stdout.splitlines() if ln.strip()), "")
        return first.removeprefix("ignis").strip() or first

    async def install(self, environment: BaseEnvironment) -> None:
        # Need curl for install.sh and ca-certificates for the GitHub TLS handshake.
        await self.exec_as_root(
            environment,
            command=(
                "if command -v apt-get >/dev/null 2>&1; then"
                "  apt-get update && apt-get install -y curl ca-certificates;"
                " elif command -v apk >/dev/null 2>&1; then"
                "  apk add --no-cache curl ca-certificates bash;"
                " elif command -v yum >/dev/null 2>&1; then"
                "  yum install -y curl ca-certificates;"
                " fi"
            ),
            env={"DEBIAN_FRONTEND": "noninteractive"},
        )
        # Install into /usr/local/bin so the binary is on PATH for every shell
        # without needing profile sourcing. install.sh respects IGNIS_INSTALL_DIR
        # and IGNIS_VERSION (set the latter for reproducible benchmark runs).
        version_env = f"IGNIS_VERSION={self._version} " if self._version else ""
        await self.exec_as_root(
            environment,
            command=(
                "set -euo pipefail; "
                f"curl -fsSL https://raw.githubusercontent.com/Fullstop000/ignis/master/install.sh "
                f"| {version_env}IGNIS_INSTALL_DIR=/usr/local/bin sh && "
                "ignis --version"
            ),
        )

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        if not self.model_name or "/" not in self.model_name:
            raise ValueError(
                "IgnisAgent: --model must be 'provider/name[@effort]' "
                f"(got {self.model_name!r})"
            )
        provider, model_spec = self.model_name.split("/", 1)
        # Optional `@<effort>` reasoning suffix — e.g. `deepseek-v4-flash@max`.
        if "@" in model_spec:
            model, effort = model_spec.split("@", 1)
        else:
            model, effort = model_spec, None
        if not model or (effort is not None and not effort):
            raise ValueError(
                f"IgnisAgent: malformed --model {self.model_name!r}"
            )
        if effort and ('"' in effort or "\\" in effort):
            raise ValueError(
                f"IgnisAgent: reasoning effort {effort!r} contains characters "
                "that would break the generated TOML"
            )
        if provider not in _PROVIDERS:
            raise ValueError(
                f"IgnisAgent: provider {provider!r} not wired. "
                f"Supported: {sorted(_PROVIDERS)}."
            )

        api_key_env, api_url = _PROVIDERS[provider]
        api_key = self._get_env(api_key_env)
        if not api_key:
            raise ValueError(
                f"IgnisAgent: env var {api_key_env} is required for "
                f"{provider}/* models and is missing"
            )
        # Defensive: a `"` or backslash in the API key would break the TOML
        # string we write below. Real keys are URL-safe, so this is a guard
        # against the env being unexpectedly malformed.
        if '"' in api_key or "\\" in api_key:
            raise ValueError(
                f"IgnisAgent: {api_key_env} contains characters that would "
                "break the generated TOML config"
            )

        # Optional: forward a web-search key so the in-sandbox `web_search` tool
        # actually works (Brave takes precedence over Tavily). Absent from the
        # env → no [web_search] block and the tool stays disabled, as before.
        search_provider = search_key = None
        for env_name, prov in (("BRAVE_API_KEY", "brave"), ("TAVILY_API_KEY", "tavily")):
            val = self._get_env(env_name)
            if val:
                search_provider, search_key = prov, val
                break
        if search_key and ('"' in search_key or "\\" in search_key):
            raise ValueError(
                "IgnisAgent: web-search key contains characters that would "
                "break the generated TOML config"
            )

        config_lines = [f'model = "{provider}/{model}"']
        if effort:
            # Top-level reasoning_effort is only honored when the model entry
            # below declares the same level as supported (see ignis config.rs).
            config_lines.append(f'reasoning_effort = "{effort}"')
        config_lines.append(f"[providers.{provider}]")
        config_lines.append(f'api_key = "{api_key}"')
        if api_url:
            config_lines.append(f'api_url = "{api_url}"')
        if effort:
            config_lines.append(
                f'models = [{{ name = "{model}", reasoning = ["{effort}"] }}]'
            )
        else:
            config_lines.append(f'models = ["{model}"]')
        if search_key:
            config_lines.append("[web_search]")
            config_lines.append(f'provider = "{search_provider}"')
            config_lines.append(f'api_key = "{search_key}"')
        config_toml = "\n".join(config_lines) + "\n"

        # Single-quoted heredoc terminator → no variable expansion inside.
        await self.exec_as_agent(
            environment,
            command=(
                'mkdir -p "$HOME/.ignis" && '
                'cat > "$HOME/.ignis/config.toml" <<\'IGNIS_TOML_EOF\'\n'
                f"{config_toml}"
                "IGNIS_TOML_EOF"
            ),
        )

        log_path = (EnvironmentPaths.agent_dir / self._OUTPUT_FILENAME).as_posix()
        sessions_dst = (EnvironmentPaths.agent_dir / "ignis-projects").as_posix()
        try:
            await self.exec_as_agent(
                environment,
                command=(
                    # `--` ends ignis's flag parsing: a task instruction that
                    # begins with `-` (clap otherwise rejects it as an unknown
                    # flag and the run dies before any tool call) is passed as
                    # the positional prompt.
                    f"ignis -- {shlex.quote(instruction)} "
                    f"2>&1 | stdbuf -oL tee {shlex.quote(log_path)}"
                ),
            )
        finally:
            # ignis persists per-session token usage to
            # $HOME/.ignis/projects/<cwd-hash>/<session>.usage.json. Copy it
            # into the harbor-synced agent dir so populate_context_post_run
            # can read it once the sandbox is torn down. Best-effort — never
            # let trajectory bookkeeping mask the real run outcome.
            try:
                await self.exec_as_agent(
                    environment,
                    command=(
                        f"mkdir -p {shlex.quote(sessions_dst)} && "
                        'if [ -d "$HOME/.ignis/projects" ]; then '
                        f'  cp -R "$HOME/.ignis/projects/." {shlex.quote(sessions_dst)}/; '
                        "fi"
                    ),
                )
            except Exception:
                pass

    def populate_context_post_run(self, context: AgentContext) -> None:
        """Aggregate every synced ignis usage.json into the trial's token counts.

        ignis writes one `<session>.usage.json` per session in v0.4+ — schema
        is `{"input_tokens", "output_tokens", "cache_read_tokens",
        "cache_write_tokens"}`. We sum them up so harbor's per-trial report
        shows real numbers instead of `null`. Cost is left None: ignis doesn't
        compute it and we don't ship a pricing table.
        """
        synced = self.logs_dir / "ignis-projects"
        if not synced.exists():
            return

        in_tot = out_tot = cache_tot = 0
        for usage_file in synced.rglob("*.usage.json"):
            try:
                with open(usage_file) as handle:
                    data = json.load(handle)
            except (OSError, json.JSONDecodeError):
                continue
            in_tot += int(data.get("input_tokens", 0) or 0)
            out_tot += int(data.get("output_tokens", 0) or 0)
            cache_tot += int(data.get("cache_read_tokens", 0) or 0)

        if in_tot or out_tot:
            context.n_input_tokens = in_tot
            context.n_output_tokens = out_tot
            context.n_cache_tokens = cache_tot

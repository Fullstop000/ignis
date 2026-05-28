"""
Harbor adapter that runs ignis inside the sandbox to solve a Terminal-Bench task.

Wire-up:
    harbor run -d terminal-bench/terminal-bench-2 \\
        -m anthropic/claude-haiku-4-5 \\
        --agent-import-path ignis_agent.agent:IgnisAgent

The harbor `-m` argument is parsed into provider/model. The provider segment
selects which env var holds the API key and (for OpenAI-compatible providers
that need a URL) the default endpoint. We write a minimal ~/.ignis/config.toml
inside the sandbox and invoke `ignis "<instruction>"` — a non-empty prompt arg
switches ignis off TUI mode into one-shot streaming.
"""

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
                "IgnisAgent: --model must be 'provider/name' "
                f"(got {self.model_name!r})"
            )
        provider, model = self.model_name.split("/", 1)
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

        config_lines = [
            f'model = "{provider}/{model}"',
            f"[providers.{provider}]",
            f'api_key = "{api_key}"',
        ]
        if api_url:
            config_lines.append(f'api_url = "{api_url}"')
        config_lines.append(f'models = ["{model}"]')
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
        await self.exec_as_agent(
            environment,
            command=(
                f"ignis {shlex.quote(instruction)} "
                f"2>&1 | stdbuf -oL tee {shlex.quote(log_path)}"
            ),
        )

# Web Search Tutorial

Ignis ships a built-in `web_search` tool that the agent can call to look things
up on the live web. It is **backend-switchable**: you choose which search API
to use, and supply that API's key. This guide takes you from zero to a working
web search.

## Prerequisites

- Ignis built and runnable (`cargo build`, plus a working `~/.ignis/config.toml`
  — see the provider setup in the repo's `config.toml`).
- An API key for one supported search backend (see below). The tool requires a
  key — there is no keyless mode.

## Supported backends

| `provider` | Service        | Auth                  | Free tier (approx)        |
|------------|----------------|-----------------------|---------------------------|
| `brave`    | Brave Search   | `X-Subscription-Token`| ~2,000 queries/month      |
| `tavily`   | Tavily (LLM)   | key in request body   | ~1,000 credits/month      |

`brave` is the default if you don't set `provider`.

## Step 1 — Get an API key

- **Brave:** sign up at <https://brave.com/search/api/>, choose the free
  "Data for Search" plan, and copy the subscription token.
- **Tavily:** sign up at <https://tavily.com/>, copy the API key from the
  dashboard.

## Step 2 — Configure

Add a `[web_search]` section to `~/.ignis/config.toml`:

```toml
[web_search]
provider = "brave"   # supported: brave, tavily
api_key  = "YOUR-BRAVE-KEY"
```

That's it — the tool is registered automatically alongside the other native
tools every time Ignis starts.

## Step 3 — Run it

One-shot mode:

```bash
cargo run -- "Use the web_search tool to find the official Rust programming \
language website. Then give me the single top result title and URL."
```

You'll see the agent call the tool and stream back results:

```
>>> [Tool: web_search] args: {"query":"official Rust programming language website"}
<<< [Tool OK: 1. Rust Programming Language - https://rust-lang.org/
   A language empowering everyone to build reliable and efficient software.
2. GitHub - rust-lang/rust ...
... ]
**Title:** Rust Programming Language
**URL:** https://rust-lang.org/
```

Each result is formatted as `N. <title> - <url>` followed by an indented
snippet. The tool returns the top 5 results.

In the **TUI** (`cargo run`), just type a prompt that needs current
information; the agent decides when to call `web_search`, and the call appears
as a color-coded tool block.

## Switching backends

Change one line in `~/.ignis/config.toml` and restart Ignis:

```toml
[web_search]
provider = "tavily"
api_key  = "YOUR-TAVILY-KEY"
```

No code changes, no rebuild.

## Troubleshooting

The tool fails **loudly** — it never silently returns empty. Common errors:

| Message | Cause / fix |
|---------|-------------|
| `web_search provider 'brave' requires an API key (set web_search.api_key in config.toml)` | No `api_key` configured. Add it under `[web_search]`. |
| `Unknown web_search provider 'X' (supported: brave, tavily)` | Typo in `provider`. Use `brave` or `tavily`. |
| `Brave API error 401/422: ...` | Bad/expired key, or quota exhausted. |
| `No results found.` | The query genuinely returned nothing. |

## Adding a new backend (for developers)

The backends live in `ignis/src/tools/web_search.rs`. To add one:

1. Add a variant to the `Backend` enum and map its name in `Backend::from_name`.
2. Write an `async fn search_<name>(&self, query, key)` that calls the API and
   returns `Vec<SearchResult>` (reuse `extract_result` if the JSON has
   `title`/`url` plus a snippet field).
3. Route the new variant in `WebSearchTool::call`.

All results are normalized to `SearchResult { title, url, snippet }`, so the
formatting and the rest of the agent are unaffected.

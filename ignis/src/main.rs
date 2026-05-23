use ignis::{
    storage::FileStorage,
    Agent, AgentEvent,
};
use std::sync::Arc;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct ProviderConfig {
    api_key: Option<String>,
    api_url: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Config {
    active_provider: String,
    session_id: Option<String>,
    providers: HashMap<String, ProviderConfig>,
}

fn load_config() -> Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let paths = vec![
        PathBuf::from("config.yaml"),
        PathBuf::from("/home/zht/ignis/config.yaml"),
    ];

    let mut last_err = None;
    for path in paths {
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    match serde_yaml::from_str::<Config>(&content) {
                        Ok(config) => return Ok(config),
                        Err(e) => last_err = Some(format!("Failed to parse {}: {}", path.display(), e).into()),
                    }
                }
                Err(e) => last_err = Some(format!("Failed to read {}: {}", path.display(), e).into()),
            }
        }
    }

    Err(last_err.unwrap_or_else(|| "config.yaml not found in current directory or /home/zht/ignis/config.yaml".into()))
}

fn build_provider(config: &Config) -> Result<Box<dyn ignis::provider::LlmProvider>, Box<dyn std::error::Error + Send + Sync>> {
    let provider_name = &config.active_provider;
    let prov_cfg = config.providers.get(provider_name)
        .ok_or_else(|| format!("Configuration for active provider '{}' not found", provider_name))?;

    match provider_name.as_str() {
        "openai" => {
            let api_key = prov_cfg.api_key.clone()
                .ok_or("openai provider requires api_key")?;
            let api_url = prov_cfg.api_url.clone()
                .ok_or("openai provider requires api_url")?;
            let model = prov_cfg.model.clone()
                .ok_or("openai provider requires model")?;
            Ok(Box::new(ignis::provider::OpenAiProvider::new(api_key, api_url, model)))
        }
        "deepseek" => {
            let api_key = prov_cfg.api_key.clone()
                .ok_or("deepseek provider requires api_key")?;
            let api_url = prov_cfg.api_url.clone()
                .unwrap_or_else(|| "https://api.deepseek.com/v1".to_string());
            let model = prov_cfg.model.clone()
                .ok_or("deepseek provider requires model")?;
            Ok(Box::new(ignis::provider::DeepSeekProvider::with_url(api_key, api_url, model)))
        }
        "kimi" => {
            let api_key = prov_cfg.api_key.clone()
                .ok_or("kimi provider requires api_key")?;
            let api_url = prov_cfg.api_url.clone()
                .unwrap_or_else(|| "https://api.kimi.com/coding/v1".to_string());
            let model = prov_cfg.model.clone()
                .ok_or("kimi provider requires model")?;
            // Kimi behaves like OpenAiProvider but uses Kimi api_url
            Ok(Box::new(ignis::provider::OpenAiProvider::new(api_key, api_url, model)))
        }
        "anthropic" => {
            let api_key = prov_cfg.api_key.clone()
                .ok_or("anthropic provider requires api_key")?;
            let model = prov_cfg.model.clone()
                .ok_or("anthropic provider requires model")?;
            Ok(Box::new(ignis::provider::AnthropicProvider::new(api_key, model)))
        }
        "gemini" => {
            let api_key = prov_cfg.api_key.clone()
                .ok_or("gemini provider requires api_key")?;
            let model = prov_cfg.model.clone()
                .ok_or("gemini provider requires model")?;
            Ok(Box::new(ignis::provider::GeminiProvider::new(api_key, model)))
        }
        "ollama" => {
            let api_url = prov_cfg.api_url.clone()
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            let model = prov_cfg.model.clone()
                .ok_or("ollama provider requires model")?;
            Ok(Box::new(ignis::provider::OllamaProvider::new(api_url, model)))
        }
        other => Err(format!("Unknown provider type: {}", other).into()),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("=== Starting Ignis ===");

    // 1. Load config
    let config = load_config()?;
    println!("Active Provider: {}", config.active_provider);

    // 2. Build provider
    let provider = build_provider(&config)?;

    // 3. Initialize storage
    let home = dirs::home_dir().ok_or_else(|| "Could not locate home directory")?;
    let storage_dir = home.join(".config/ignis/sessions");
    println!("Storage Directory: {}", storage_dir.display());
    let storage = FileStorage::new(storage_dir);

    // 4. Create Agent
    let session_id = config.session_id.clone().unwrap_or_else(|| "default".to_string());
    let system_prompt = "You are a helpful assistant with powerful native tools and plugins. Use them as needed to accomplish your task.".to_string();
    let mut agent = Agent::new(session_id, system_prompt, provider, Box::new(storage));

    // 5. Register Native Tools
    let cwd = std::env::current_dir()?;
    println!("Current Working Directory: {}", cwd.display());
    ignis::tools::register_native_tools(&mut agent, &cwd);

    // 6. Register Plugins
    let ext_dirs = ignis::plugin::default_extension_dirs();
    for d in &ext_dirs {
        if !d.exists() {
            let _ = std::fs::create_dir_all(d);
        }
    }
    let plugins = ignis::plugin::load_extensions(&ext_dirs);
    println!("Loaded {} extensions.", plugins.len());
    for plugin in plugins {
        agent.register_tool(Arc::new(plugin));
    }

    // 7. Parse Prompt
    let args: Vec<String> = std::env::args().skip(1).collect();
    let prompt_text = if args.is_empty() {
        "List the contents of the current directory and read the Cargo.toml file if it exists."
    } else {
        &args.join(" ")
    };
    println!("Prompt: {}", prompt_text);

    // 8. Channel for streaming events
    let (tx, mut rx) = tokio::sync::mpsc::channel(100);

    // 9. Run prompt in a separate thread
    let prompt_text_owned = prompt_text.to_string();
    let prompt_task = tokio::spawn(async move {
        if let Err(e) = agent.prompt(&prompt_text_owned, tx).await {
            eprintln!("Agent execution error: {:?}", e);
        }
    });

    // 10. Consume events
    use std::io::Write;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::AgentStart => {
                println!("[Agent started]");
            }
            AgentEvent::TurnStart => {
                println!("[Turn started]");
            }
            AgentEvent::MessageStart { message } => {
                print!("\n[{}] starting message...", message.role);
                std::io::stdout().flush()?;
            }
            AgentEvent::MessageUpdate { delta } => {
                print!("{}", delta);
                std::io::stdout().flush()?;
            }
            AgentEvent::MessageEnd { .. } => {
                println!("\n[Message ended]");
            }
            AgentEvent::ToolExecutionStart { tool_name, tool_call_id, arguments } => {
                println!(
                    "\n>>> [Tool executing: {} (ID: {}) with args: {}]",
                    tool_name, tool_call_id, arguments
                );
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, result } => {
                if result.is_error {
                    println!("<<< [Tool execution completed (ID: {}): ERROR: {}]", tool_call_id, result.content);
                } else {
                    println!("<<< [Tool execution completed (ID: {}): OK: {}]", tool_call_id, result.content);
                }
            }
            AgentEvent::TurnEnd => {
                println!("[Turn ended]");
            }
            AgentEvent::AgentEnd => {
                println!("[Agent ended]");
            }
        }
    }

    prompt_task.await?;
    println!("=== Ignis Completed ===");
    Ok(())
}

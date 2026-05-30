use ignis::llm::{providers, Auth, ModelCatalog, ModelOption, Protocol, ProviderConfig};

#[test]
fn llm_module_exposes_public_model_and_protocol_api() {
    let catalog = ModelCatalog::default();
    assert_eq!(catalog.context_for("unknown-model"), None);

    let option = ModelOption {
        provider: "openai".to_string(),
        model: "o3".to_string(),
        effort_levels: vec!["low".to_string()],
        context: Some(200_000),
    };
    assert_eq!(option.provider, "openai");

    let provider_cfg = ProviderConfig {
        api_key: Some("test-key".to_string()),
        api_url: None,
        protocol: Some(Protocol::OpenAi),
        user_agent: None,
        models: Vec::new(),
    };
    assert_eq!(provider_cfg.protocol, Some(Protocol::OpenAi));
    assert_eq!(providers::lookup("openai").map(|p| p.id), Some("openai"));
    assert_eq!(
        providers::lookup("openai").unwrap().endpoints[0].auth,
        Auth::Bearer
    );
}

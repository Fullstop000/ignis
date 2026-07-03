use ignis::AgentTool;

#[ignis::tool(name = "concat_items", description = "Concatenate a list of strings")]
async fn concat_items(items: Vec<String>) -> Result<String, String> {
    Ok(items.join(", "))
}

#[ignis::tool(name = "maybe_concat", description = "Concatenate optional strings")]
async fn maybe_concat(items: Option<Vec<String>>) -> Result<String, String> {
    Ok(items.unwrap_or_default().join(" | "))
}

#[tokio::test]
async fn vec_string_param_emits_array_schema() {
    let tool = ConcatItemsTool;
    let schema = tool.parameters();

    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["items"]["type"], "array");
    assert_eq!(schema["properties"]["items"]["items"]["type"], "string");
    assert_eq!(schema["required"], serde_json::json!(["items"]));
}

#[tokio::test]
async fn vec_string_param_round_trips() {
    let tool = ConcatItemsTool;
    let result = tool
        .call(serde_json::json!({ "items": ["a", "b", "c"] }))
        .await;

    assert!(!result.is_error);
    assert_eq!(result.content, "a, b, c");
}

#[tokio::test]
async fn optional_vec_param_is_not_required() {
    let tool = MaybeConcatTool;
    let schema = tool.parameters();

    assert_eq!(schema["properties"]["items"]["type"], "array");
    assert_eq!(schema["properties"]["items"]["items"]["type"], "string");
    assert!(schema["required"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn optional_vec_param_omits_field() {
    let tool = MaybeConcatTool;
    let result = tool.call(serde_json::json!({})).await;

    assert!(!result.is_error);
    assert_eq!(result.content, "");
}

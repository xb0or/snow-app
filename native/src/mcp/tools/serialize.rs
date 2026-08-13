use super::*;

use serde_json::{json, Value};

pub fn tools_as_openai_chat_json(tools: &[McpTool]) -> Value {
    let functions: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let sanitized_schema = sanitize_tool_input_schema(&tool.input_schema);
            json!({
                "type": "function",
                "function": {
                    "name": tool.full_name(),
                    "description": tool.description,
                    "parameters": sanitized_schema,
                }
            })
        })
        .collect();

    Value::Array(functions)
}

/// Tool APIs require the root input schema to describe an object. Some
/// compatible gateways reject root `oneOf`/`anyOf`/`allOf` combinators when a
/// branch does not explicitly declare an object, even if the root has
/// `type: "object"`. Keep nested constraints intact, but remove root
/// combinators and always emit an object schema. Runtime tool validation still
/// enforces cross-field requirements that cannot be represented at the root.
fn sanitize_tool_input_schema(schema: &Value) -> Value {
    let mut result = schema.as_object().cloned().unwrap_or_default();

    result.remove("oneOf");
    result.remove("anyOf");
    result.remove("allOf");
    result.insert("type".to_string(), Value::String("object".to_string()));

    Value::Object(result)
}

pub fn tools_as_anthropic_json(tools: &[McpTool]) -> Value {
    let tools_json: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let sanitized_schema = sanitize_tool_input_schema(&tool.input_schema);
            json!({
                "name": tool.full_name(),
                "description": tool.description,
                "input_schema": sanitized_schema,
            })
        })
        .collect();

    Value::Array(tools_json)
}

pub fn tools_as_openai_responses_json(tools: &[McpTool]) -> Value {
    let tools_json: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let sanitized_schema = sanitize_tool_input_schema(&tool.input_schema);
            json!({
                "type": "function",
                "name": tool.full_name(),
                "description": tool.description,
                "parameters": sanitized_schema,
            })
        })
        .collect();

    Value::Array(tools_json)
}

pub fn tools_as_gemini_json(tools: &[McpTool]) -> Value {
    let function_declarations: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let sanitized_schema = sanitize_tool_input_schema(&tool.input_schema);
            json!({
                "name": tool.full_name(),
                "description": tool.description,
                "parameters": sanitized_schema,
            })
        })
        .collect();

    json!({
        "functionDeclarations": function_declarations
    })
}

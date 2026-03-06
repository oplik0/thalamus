//! Tool and function definitions for LLM requests
//!
//! Provides unified representations for function-calling tools,
//! server-side tools, and tool choice configuration.

use serde::{Deserialize, Serialize};

/// A tool that can be provided to an LLM
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolDefinition {
    /// A function tool the model can call
    Function { function: FunctionDefinition },
    /// A server-side tool (e.g., Anthropic web_search, code_execution)
    ServerTool(ServerTool),
}

/// Definition of a callable function
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the function parameters
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// A server-side tool provided by the LLM provider
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

/// Controls how the model selects tools
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    /// Model decides whether to use tools
    Auto,
    /// Model will not use tools
    None,
    /// Model must use at least one tool
    Required,
    /// Model must use the specified function
    Function { name: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_tool_round_trip() {
        let tool = ToolDefinition::Function {
            function: FunctionDefinition {
                name: "get_weather".to_string(),
                description: Some("Get current weather".to_string()),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "city": {"type": "string"}
                    },
                    "required": ["city"]
                })),
                strict: Some(true),
            },
        };
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains(r#""type":"function""#));
        let round_tripped: ToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(tool, round_tripped);
    }

    #[test]
    fn server_tool_round_trip() {
        let tool = ToolDefinition::ServerTool(ServerTool {
            name: "web_search".to_string(),
            config: Some(serde_json::json!({"max_results": 5})),
        });
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains(r#""type":"server_tool""#));
        let round_tripped: ToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(tool, round_tripped);
    }

    #[test]
    fn tool_choice_variants() {
        let auto = ToolChoice::Auto;
        let json = serde_json::to_string(&auto).unwrap();
        assert!(json.contains(r#""type":"auto""#));

        let none = ToolChoice::None;
        let json = serde_json::to_string(&none).unwrap();
        assert!(json.contains(r#""type":"none""#));

        let required = ToolChoice::Required;
        let json = serde_json::to_string(&required).unwrap();
        assert!(json.contains(r#""type":"required""#));

        let func = ToolChoice::Function {
            name: "my_func".to_string(),
        };
        let json = serde_json::to_string(&func).unwrap();
        assert!(json.contains(r#""type":"function""#));
        assert!(json.contains(r#""name":"my_func""#));
    }

    #[test]
    fn function_definition_optional_fields_skipped() {
        let func = FunctionDefinition {
            name: "simple".to_string(),
            description: None,
            parameters: None,
            strict: None,
        };
        let json = serde_json::to_string(&func).unwrap();
        assert!(!json.contains("description"));
        assert!(!json.contains("parameters"));
        assert!(!json.contains("strict"));
    }

    #[test]
    fn tool_choice_round_trip() {
        let choices = vec![
            ToolChoice::Auto,
            ToolChoice::None,
            ToolChoice::Required,
            ToolChoice::Function {
                name: "test".to_string(),
            },
        ];
        for choice in choices {
            let json = serde_json::to_string(&choice).unwrap();
            let round_tripped: ToolChoice = serde_json::from_str(&json).unwrap();
            assert_eq!(choice, round_tripped);
        }
    }
}

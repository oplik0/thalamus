//! Test fixtures and builders for E2E testing
//!
//! Provides builder-pattern utilities for creating test data including:
//! - Users with specific roles and scopes
//! - API keys with various permissions
//! - LLM requests in OpenAI and Anthropic formats
//! - Response assertions

use std::collections::HashMap;

use serde_json::json;
use sqlx::PgPool;

/// Builder for creating test users
///
/// # Example
/// ```rust
/// let user = TestUserBuilder::new()
///     .username("testuser")
///     .email("test@example.com")
///     .with_scope("llm:chat")
///     .with_scope("llm:embeddings")
///     .create(&pool)
///     .await;
/// ```
#[derive(Debug)]
pub struct TestUserBuilder {
    username: String,
    email: String,
    password: Option<String>,
    scopes: Vec<String>,
    is_admin: bool,
}

impl TestUserBuilder {
    /// Create a new user builder with default values
    pub fn new() -> Self {
        let uuid = uuid::Uuid::new_v4();
        Self {
            username: format!("testuser_{}", uuid.to_string()[..8].to_string()),
            email: format!("test_{}@example.com", uuid.to_string()[..8].to_string()),
            password: Some("testpassword123".to_string()),
            scopes: Vec::new(),
            is_admin: false,
        }
    }

    /// Set the username
    pub fn username(mut self, username: impl Into<String>) -> Self {
        self.username = username.into();
        self
    }

    /// Set the email
    pub fn email(mut self, email: impl Into<String>) -> Self {
        self.email = email.into();
        self
    }

    /// Set the password
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Add a scope to this user
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scopes.push(scope.into());
        self
    }

    /// Add multiple scopes
    pub fn with_scopes(mut self, scopes: Vec<impl Into<String>>) -> Self {
        self.scopes.extend(scopes.into_iter().map(|s| s.into()));
        self
    }

    /// Set as admin user
    pub fn as_admin(mut self) -> Self {
        self.is_admin = true;
        self.scopes.push("admin:*".to_string());
        self
    }

    /// Create the user in the database
    ///
    /// Returns the user ID
    pub async fn create(self, pool: &PgPool) -> TestUser {
        // Insert user (using actual schema - no password_hash, uses opaque_registration)
        let user_id = sqlx::query_scalar!(
            r#"
            INSERT INTO users (username, email, is_active, created_at, updated_at)
            VALUES ($1, $2, $3, NOW(), NOW())
            RETURNING id
            "#,
            self.username,
            self.email,
            true,
        )
        .fetch_one(pool)
        .await
        .expect("Failed to create test user");

        TestUser {
            id: user_id,
            username: self.username,
            email: self.email,
            scopes: self.scopes,
            is_admin: self.is_admin,
        }
    }
}

impl Default for TestUserBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A test user with known properties
#[derive(Debug, Clone)]
pub struct TestUser {
    pub id: uuid::Uuid,
    pub username: String,
    pub email: String,
    pub scopes: Vec<String>,
    pub is_admin: bool,
}

impl TestUser {
    /// Check if user has a specific scope
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| {
            // Handle wildcard patterns
            if s.ends_with(":*") {
                let prefix = &s[..s.len() - 2];
                scope.starts_with(prefix)
            } else {
                s == scope
            }
        })
    }

    /// Get authorization header for this user (if using API key)
    pub fn auth_header(&self, api_key: &TestApiKey) -> String {
        format!("Bearer {}", api_key.key)
    }
}

/// Builder for creating API keys
///
/// # Example
/// ```rust
/// let api_key = TestApiKeyBuilder::new()
///     .for_user(&user)
///     .with_scope("llm:*")
///     .name("Test Key")
///     .create(&pool)
///     .await;
/// ```
#[derive(Debug)]
pub struct TestApiKeyBuilder {
    user_id: Option<uuid::Uuid>,
    team_id: Option<uuid::Uuid>,
    name: String,
    scopes: Vec<String>,
    expires_in_days: Option<i32>,
}

impl TestApiKeyBuilder {
    /// Create a new API key builder
    pub fn new() -> Self {
        Self {
            user_id: None,
            team_id: None,
            name: "Test API Key".to_string(),
            scopes: vec!["llm:*".to_string()],
            expires_in_days: None,
        }
    }

    /// Associate with a user
    pub fn for_user(mut self, user: &TestUser) -> Self {
        self.user_id = Some(user.id);
        self
    }

    /// Set the user ID directly
    pub fn user_id(mut self, user_id: uuid::Uuid) -> Self {
        self.user_id = Some(user_id);
        self
    }

    /// Set the team ID
    pub fn team_id(mut self, team_id: uuid::Uuid) -> Self {
        self.team_id = Some(team_id);
        self
    }

    /// Set the key name
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Add a scope
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scopes.push(scope.into());
        self
    }

    /// Set all scopes
    pub fn with_scopes(mut self, scopes: Vec<impl Into<String>>) -> Self {
        self.scopes = scopes.into_iter().map(|s| s.into()).collect();
        self
    }

    /// Set expiration
    pub fn expires_in_days(mut self, days: i32) -> Self {
        self.expires_in_days = Some(days);
        self
    }

    /// Create a no-expiration key
    pub fn never_expires(mut self) -> Self {
        self.expires_in_days = None;
        self
    }

    /// Create the API key in the database
    pub async fn create(self, pool: &PgPool) -> TestApiKey {
        // Get or create a team for the API key
        let team_id = match self.team_id {
            Some(id) => id,
            None => {
                // Get the user's default team or create one
                let user_id = self.user_id.expect("user_id must be set");
                let team_id = sqlx::query_scalar!(
                    r#"SELECT team_id FROM team_memberships WHERE user_id = $1 LIMIT 1"#,
                    user_id
                )
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();

                match team_id {
                    Some(id) => id,
                    None => {
                        // Create a default team for the user
                        let new_team_id = uuid::Uuid::new_v4();
                        let team_name =
                            format!("team_{}", new_team_id.to_string()[..8].to_string());

                        sqlx::query!(
                            r#"INSERT INTO teams (id, name) VALUES ($1, $2)"#,
                            new_team_id,
                            team_name
                        )
                        .execute(pool)
                        .await
                        .expect("Failed to create default team");

                        sqlx::query!(
                            r#"INSERT INTO team_memberships (user_id, team_id, role) VALUES ($1, $2, $3)"#,
                            user_id,
                            new_team_id,
                            "admin"
                        )
                        .execute(pool)
                        .await
                        .expect("Failed to create team membership");

                        new_team_id
                    }
                }
            }
        };

        let key_value = format!("thalamus_test_{}", uuid::Uuid::new_v4());
        let key_hash = sha256_hash(&key_value);
        let key_prefix = &key_value[..8.min(key_value.len())];
        let key_id = format!(
            "thal_{}",
            uuid::Uuid::new_v4().to_string()[..12].to_string()
        );

        let expires_at = self
            .expires_in_days
            .map(|days| chrono::Utc::now() + chrono::Duration::days(days as i64));

        let api_key_id = sqlx::query_scalar!(
            r#"
            INSERT INTO api_keys (key_id, key_hash, key_prefix, user_id, team_id, name, scopes, expires_at, created_at, revoked_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW(), NULL)
            RETURNING id
            "#,
            key_id,
            key_hash,
            key_prefix,
            self.user_id,
            team_id,
            self.name,
            &self.scopes,
            expires_at,
        )
        .fetch_one(pool)
        .await
        .expect("Failed to create API key");

        TestApiKey {
            id: api_key_id,
            key: key_value,
            name: self.name,
            scopes: self.scopes,
            user_id: self.user_id,
            team_id: Some(team_id),
        }
    }
}

impl Default for TestApiKeyBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A test API key
#[derive(Debug, Clone)]
pub struct TestApiKey {
    pub id: uuid::Uuid,
    /// The actual key value (plaintext, only available in tests)
    pub key: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub user_id: Option<uuid::Uuid>,
    pub team_id: Option<uuid::Uuid>,
}

impl TestApiKey {
    /// Get the Authorization header value
    pub fn auth_header(&self) -> String {
        format!("Bearer {}", self.key)
    }

    /// Check if this key has a specific scope
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| {
            if s.ends_with(":*") {
                let prefix = &s[..s.len() - 2];
                scope.starts_with(prefix)
            } else {
                s == scope
            }
        })
    }
}

/// Builder for constructing LLM requests
///
/// Supports both OpenAI and Anthropic formats
#[derive(Debug)]
pub struct LlmRequestBuilder {
    format: RequestFormat,
    model: String,
    messages: Vec<serde_json::Value>,
    stream: bool,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    tools: Option<Vec<serde_json::Value>>,
    tool_choice: Option<serde_json::Value>,
    top_p: Option<f32>,
    presence_penalty: Option<f32>,
    frequency_penalty: Option<f32>,
    user: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum RequestFormat {
    OpenAi,
    Anthropic,
}

impl LlmRequestBuilder {
    /// Create a new OpenAI format request builder
    pub fn openai() -> Self {
        Self {
            format: RequestFormat::OpenAi,
            model: "gpt-oss:120b".to_string(),
            messages: Vec::new(),
            stream: false,
            temperature: None,
            max_tokens: None,
            tools: None,
            tool_choice: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            user: None,
        }
    }

    /// Create a new Anthropic format request builder
    pub fn anthropic() -> Self {
        Self {
            format: RequestFormat::Anthropic,
            model: "claude-3-opus".to_string(),
            messages: Vec::new(),
            stream: false,
            temperature: None,
            max_tokens: Some(1024),
            tools: None,
            tool_choice: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            user: None,
        }
    }

    /// Set the model
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Add a system message
    pub fn system_message(mut self, content: impl Into<String>) -> Self {
        match self.format {
            RequestFormat::OpenAi => {
                self.messages.push(json!({
                    "role": "system",
                    "content": content.into(),
                }));
            }
            RequestFormat::Anthropic => {
                // Anthropic uses "system" as a top-level parameter, not a message
                // For now, we'll treat it as a user message for simplicity in tests
                self.messages.push(json!({
                    "role": "user",
                    "content": format!("System: {}", content.into()),
                }));
            }
        }
        self
    }

    /// Add a user message
    pub fn user_message(mut self, content: impl Into<String>) -> Self {
        self.messages.push(json!({
            "role": "user",
            "content": content.into(),
        }));
        self
    }

    /// Add an assistant message (for multi-turn conversations)
    pub fn assistant_message(mut self, content: impl Into<String>) -> Self {
        self.messages.push(json!({
            "role": "assistant",
            "content": content.into(),
        }));
        self
    }

    /// Enable streaming
    pub fn with_streaming(mut self) -> Self {
        self.stream = true;
        self
    }

    /// Disable streaming (default)
    pub fn without_streaming(mut self) -> Self {
        self.stream = false;
        self
    }

    /// Set temperature
    pub fn temperature(mut self, temp: f32) -> Self {
        self.temperature = Some(temp);
        self
    }

    /// Set max tokens
    pub fn max_tokens(mut self, tokens: u32) -> Self {
        self.max_tokens = Some(tokens);
        self
    }

    /// Add a tool definition
    pub fn with_tool(
        mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        let tool = json!({
            "type": "function",
            "function": {
                "name": name.into(),
                "description": description.into(),
                "parameters": parameters,
            }
        });
        self.tools.get_or_insert_with(Vec::new).push(tool);
        self
    }

    /// Set tool choice (e.g., "auto", "none", or specific tool)
    pub fn tool_choice(mut self, choice: impl Into<String>) -> Self {
        self.tool_choice = Some(json!(choice.into()));
        self
    }

    /// Set top_p
    pub fn top_p(mut self, top_p: f32) -> Self {
        self.top_p = Some(top_p);
        self
    }

    /// Build the request body as JSON
    pub fn build(self) -> serde_json::Value {
        match self.format {
            RequestFormat::OpenAi => self.build_openai(),
            RequestFormat::Anthropic => self.build_anthropic(),
        }
    }

    fn build_openai(self) -> serde_json::Value {
        let mut body = json!({
            "model": self.model,
            "messages": self.messages,
            "stream": self.stream,
        });

        if let Some(temp) = self.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(max_tokens) = self.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        if let Some(top_p) = self.top_p {
            body["top_p"] = json!(top_p);
        }
        if let Some(presence) = self.presence_penalty {
            body["presence_penalty"] = json!(presence);
        }
        if let Some(freq) = self.frequency_penalty {
            body["frequency_penalty"] = json!(freq);
        }
        if let Some(tools) = self.tools {
            body["tools"] = json!(tools);
        }
        if let Some(tool_choice) = self.tool_choice {
            body["tool_choice"] = tool_choice;
        }
        if let Some(user) = self.user {
            body["user"] = json!(user);
        }

        body
    }

    fn build_anthropic(self) -> serde_json::Value {
        let mut body = json!({
            "model": self.model,
            "messages": self.messages,
            "max_tokens": self.max_tokens.unwrap_or(1024),
            "stream": self.stream,
        });

        if let Some(temp) = self.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(top_p) = self.top_p {
            body["top_p"] = json!(top_p);
        }
        if let Some(tools) = self.tools {
            body["tools"] = json!(tools);
        }
        if let Some(tool_choice) = self.tool_choice {
            body["tool_choice"] = tool_choice;
        }

        body
    }

    /// Build and serialize to string
    pub fn build_string(self) -> String {
        self.build().to_string()
    }
}

/// Builder for embedding requests
#[derive(Debug)]
pub struct EmbeddingsRequestBuilder {
    model: String,
    input: serde_json::Value,
    user: Option<String>,
}

impl EmbeddingsRequestBuilder {
    /// Create a new embeddings request builder
    pub fn new() -> Self {
        Self {
            model: "text-embedding-3-small".to_string(),
            input: json!("text to embed"),
            user: None,
        }
    }

    /// Set the model
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Set single text input
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.input = json!(text.into());
        self
    }

    /// Set multiple text inputs
    pub fn texts(mut self, texts: Vec<impl Into<String>>) -> Self {
        self.input = json!(texts.into_iter().map(|t| t.into()).collect::<Vec<String>>());
        self
    }

    /// Set user identifier
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Build the request body
    pub fn build(self) -> serde_json::Value {
        let mut body = json!({
            "model": self.model,
            "input": self.input,
        });

        if let Some(user) = self.user {
            body["user"] = json!(user);
        }

        body
    }

    /// Build and serialize to string
    pub fn build_string(self) -> String {
        self.build().to_string()
    }
}

impl Default for EmbeddingsRequestBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Response assertions for LLM responses
pub struct ResponseAsserter {
    response: axum::response::Response,
}

impl ResponseAsserter {
    /// Create a new asserter from an Axum response
    pub fn new(response: axum::response::Response) -> Self {
        Self { response }
    }

    /// Assert the response has the expected status code
    pub fn has_status(&self, expected: u16) -> &Self {
        let status = self.response.status().as_u16();
        assert_eq!(
            status, expected,
            "Expected status {}, got {}",
            expected, status
        );
        self
    }

    /// Assert the response is successful (2xx)
    pub fn is_success(&self) -> &Self {
        assert!(
            self.response.status().is_success(),
            "Expected success status, got {}",
            self.response.status()
        );
        self
    }

    /// Assert content-type header contains expected value
    pub fn has_content_type(&self, expected: &str) -> &Self {
        let content_type = self
            .response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.contains(expected),
            "Expected content-type containing '{}', got '{}'",
            expected,
            content_type
        );
        self
    }

    /// Get the response for further assertions
    pub fn into_response(self) -> axum::response::Response {
        self.response
    }
}

/// Helpers for parsing responses
pub mod response_parsers {
    use serde_json::Value;

    /// Parse a chat completion response
    pub fn parse_chat_completion(body: &Value) -> Option<String> {
        body.get("choices")?
            .get(0)?
            .get("message")?
            .get("content")?
            .as_str()
            .map(|s| s.to_string())
    }

    /// Parse a streaming chunk
    pub fn parse_stream_chunk(chunk: &str) -> Option<Value> {
        // Remove "data: " prefix and parse
        let json_str = chunk.strip_prefix("data: ")?;
        if json_str == "[DONE]" {
            return Some(Value::String("[DONE]".to_string()));
        }
        serde_json::from_str(json_str).ok()
    }

    /// Extract content from a stream chunk
    pub fn extract_chunk_content(chunk: &Value) -> Option<String> {
        chunk
            .get("choices")?
            .get(0)?
            .get("delta")?
            .get("content")?
            .as_str()
            .map(|s| s.to_string())
    }

    /// Parse embedding response
    pub fn parse_embeddings(body: &Value) -> Option<Vec<Vec<f32>>> {
        let data = body.get("data")?.as_array()?;
        Some(
            data.iter()
                .filter_map(|item| {
                    item.get("embedding")?
                        .as_array()?
                        .iter()
                        .filter_map(|v| v.as_f64().map(|f| f as f32))
                        .collect::<Vec<f32>>()
                        .into()
                })
                .collect(),
        )
    }
}

// Helper functions
fn sha256_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

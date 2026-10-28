//! Executable OpenAI-compatible functional probes. This crate never creates Jobs.
mod protocol;
use protocol::protocol_checks;
use reqwest::blocking::{Client, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::Read,
    time::{Duration, Instant},
};
use thalamus_runs::{RunKind, RunRequest};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Base URL ending in /v1 (without a trailing slash).
    pub base_url: String,
    pub model: String,
    pub forbidden_model: String,
    pub suite: Suite,
    /// Required for a candidate gate; must be injected by a trusted gateway, not a client.
    pub backend_header: Option<String>,
    pub expected_backend: Option<String>,
    pub old_backend: Option<String>,
    /// An operator-supplied prompt. Its actual token count is checked using response usage.
    pub context_prompt: String,
    pub context_tokens: u32,
    pub tool_parser: Option<String>,
    pub reasoning: bool,
    pub responses_continuation: bool,
    /// Additional protocols are probed only when explicitly enabled by the recipe.
    #[serde(default)]
    pub responses: bool,
    #[serde(default)]
    pub anthropic: bool,
    pub timeout_seconds: u64,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Suite {
    Candidate,
    Public,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Passed,
    Failed,
    Unsupported,
}
#[derive(Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    pub semantics: &'static str,
}
#[derive(Debug, Serialize)]
pub struct Summary {
    pub run_id: String,
    pub request: RunRequest,
    pub suite: Suite,
    pub runner_version: &'static str,
    pub tool_parser: Option<String>,
    pub answering_backends: Vec<String>,
    pub checks: Vec<Check>,
    pub passed: bool,
}

struct Runner<'a> {
    config: &'a Config,
    key: &'a str,
    client: Client,
    backends: BTreeSet<String>,
    requests: u32,
    max_requests: u32,
    tool_calls: u32,
    max_tool_calls: u32,
    deadline: Instant,
}
impl<'a> Runner<'a> {
    fn post(
        &mut self,
        path: &str,
        key: &str,
        body: Value,
        identify: bool,
    ) -> Result<(u16, String), String> {
        if self.requests >= self.max_requests || Instant::now() >= self.deadline {
            return Err("workload request or duration budget exhausted".into());
        }
        self.requests += 1;
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        let mut request = self
            .client
            .post(format!("{}/{}", self.config.base_url, path))
            .timeout(remaining.min(Duration::from_secs(self.config.timeout_seconds)));
        request = if path == "messages" {
            request
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
        } else {
            request.bearer_auth(key)
        };
        let response = request.json(&body).send().map_err(|e| {
            format!(
                "transport error: {}",
                if e.is_timeout() {
                    "timeout"
                } else {
                    "request failed"
                }
            )
        })?;
        self.read_response(response, identify)
    }
    fn read_response(
        &mut self,
        response: Response,
        identify: bool,
    ) -> Result<(u16, String), String> {
        let status = response.status().as_u16();
        if identify && (200..300).contains(&status) {
            let identity = self
                .config
                .backend_header
                .as_ref()
                .and_then(|h| response.headers().get(h))
                .and_then(|v| v.to_str().ok())
                .filter(|v| !v.trim().is_empty());
            if let Some(id) = identity {
                self.backends.insert(id.to_string());
            }
            if matches!(self.config.suite, Suite::Candidate) {
                let id = identity.ok_or("missing answering backend identity")?;
                if Some(id) != self.config.expected_backend.as_deref()
                    || Some(id) == self.config.old_backend.as_deref()
                {
                    return Err("answering backend is not the candidate".into());
                }
            }
        }
        let mut bytes = Vec::new();
        response
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "response body read failed")?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err("response exceeds size limit".into());
        }
        Ok((
            status,
            String::from_utf8(bytes).map_err(|_| "response is not UTF-8")?,
        ))
    }
    fn json(&mut self, path: &str, body: Value) -> Result<Value, String> {
        let (status, text) = self.post(path, self.key, body, true)?;
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }
        serde_json::from_str(&text).map_err(|_| "invalid JSON response".into())
    }
    fn chat(&mut self, messages: Value) -> Result<Value, String> {
        self.json(
            "chat/completions",
            json!({"model":self.config.model,"messages":messages,"max_tokens":64}),
        )
    }
    fn reserve_tool(&mut self) -> Result<(), String> {
        if self.tool_calls >= self.max_tool_calls {
            return Err("tool call budget exhausted".into());
        }
        self.tool_calls += 1;
        Ok(())
    }
}
fn check(name: &str, semantics: &'static str, result: Result<String, String>) -> Check {
    match result {
        Ok(detail) => Check {
            name: name.into(),
            status: Status::Passed,
            detail,
            semantics,
        },
        Err(detail) => Check {
            name: name.into(),
            status: Status::Failed,
            detail,
            semantics,
        },
    }
}
fn unsupported(name: &str, semantics: &'static str, detail: &str) -> Check {
    Check {
        name: name.into(),
        status: Status::Unsupported,
        detail: detail.into(),
        semantics,
    }
}
fn message(v: &Value) -> Result<&Value, String> {
    v.pointer("/choices/0/message")
        .filter(|m| m.is_object())
        .ok_or("missing assistant message".into())
}
fn text(v: &Value) -> bool {
    v.as_str().is_some_and(|s| !s.trim().is_empty())
}
fn body(model: &str) -> Value {
    json!({"model":model,"messages":[{"role":"user","content":"Reply with a short greeting."}],"max_tokens":16})
}

/// Runs the fixed `functional` workload. Credentials are never included in the summary.
pub fn run(
    config: &Config,
    request: RunRequest,
    run_id: String,
    key: &str,
) -> Result<Summary, String> {
    request.validate().map_err(|e| e.to_string())?;
    if !matches!(request.kind, RunKind::Validation) || request.workload.name != "functional" {
        return Err("only the functional validation workload is supported".into());
    }
    if request.workload.concurrency != 1 || request.workload.warmup_seconds != 0 {
        return Err("functional workload requires concurrency 1 and zero warmup".into());
    }
    if run_id.trim().is_empty()
        || key.is_empty()
        || config.model.is_empty()
        || config.forbidden_model.is_empty()
        || config.model == config.forbidden_model
        || config.context_prompt.is_empty()
        || config.context_prompt.len() > 2_000_000
        || config.context_tokens == 0
        || config.context_tokens > request.workload.model_budget_tokens
        || !(1..=300).contains(&config.timeout_seconds)
    {
        return Err("invalid runner configuration or context budget".into());
    }
    if config
        .tool_parser
        .as_ref()
        .is_some_and(|s| s.trim().is_empty())
    {
        return Err("empty tool parser".into());
    }
    if matches!(config.suite, Suite::Candidate)
        && (config
            .backend_header
            .as_ref()
            .is_none_or(|s| s.trim().is_empty())
            || config
                .expected_backend
                .as_ref()
                .is_none_or(|s| s.trim().is_empty())
            || config
                .old_backend
                .as_ref()
                .is_none_or(|s| s.trim().is_empty())
            || config.expected_backend == config.old_backend)
    {
        return Err(
            "candidate requires distinct old and expected backends and an identity header".into(),
        );
    }
    let url = reqwest::Url::parse(&config.base_url).map_err(|_| "invalid base URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || config.base_url.ends_with('/')
    {
        return Err("invalid base URL".into());
    }
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "cannot create HTTP client")?;
    let deadline = Instant::now() + Duration::from_secs(request.workload.duration_seconds.into());
    let mut r = Runner {
        config,
        key,
        client,
        backends: BTreeSet::new(),
        requests: 0,
        max_requests: request.workload.max_requests,
        tool_calls: 0,
        max_tool_calls: request.workload.tool_budget_calls,
        deadline,
    };
    let mut checks = Vec::new();
    checks.push(check(
        "authentication",
        "user_visible",
        (|| {
            let (s, _) = r.post(
                "chat/completions",
                "thalamus-invalid-validation-key",
                body(&config.model),
                false,
            )?;
            if s == 401 || s == 403 {
                Ok(format!("invalid credential rejected: HTTP {s}"))
            } else {
                Err(format!("invalid credential was not rejected: HTTP {s}"))
            }
        })(),
    ));
    checks.push(check(
        "authorization",
        "user_visible",
        (|| {
            let (s, _) = r.post(
                "chat/completions",
                key,
                body(&config.forbidden_model),
                false,
            )?;
            if s == 403 {
                Ok("forbidden model rejected: HTTP 403".into())
            } else {
                Err(format!("expected HTTP 403 for forbidden model, got {s}"))
            }
        })(),
    ));
    checks.push(check(
        "chat",
        "user_visible",
        (|| {
            let v = r.chat(json!([{"role":"user","content":"Reply with a short greeting."}]))?;
            if text(&message(&v)?["content"]) {
                Ok("authenticated chat returned content".into())
            } else {
                Err("chat returned no content".into())
            }
        })(),
    ));
    checks.push(check(
        "streaming",
        "client",
        (|| {
            let mut b = body(&config.model);
            b["stream"] = json!(true);
            let (s, data) = r.post("chat/completions", key, b, true)?;
            if !(200..300).contains(&s) {
                return Err(format!("HTTP {s}"));
            }
            let mut content = false;
            let mut done = false;
            for line in data.lines() {
                if let Some(d) = line.strip_prefix("data:") {
                    let d = d.trim();
                    if d == "[DONE]" {
                        done = true;
                        continue;
                    }
                    let v: Value = serde_json::from_str(d).map_err(|_| "invalid SSE JSON")?;
                    content |= text(
                        &v.pointer("/choices/0/delta/content")
                            .cloned()
                            .unwrap_or(Value::Null),
                    );
                }
            }
            if content && done {
                Ok("SSE content and terminal marker received".into())
            } else {
                Err("stream missing content or terminal marker".into())
            }
        })(),
    ));
    if let Some(parser) = &config.tool_parser {
        checks.push(check("tool_loop", "client", (|| {
            r.reserve_tool()?;
            let v = r.json("chat/completions", json!({"model":config.model,"messages":[{"role":"user","content":"Call the lookup tool for key alpha, then report its result."}],"tools":[{"type":"function","function":{"name":"lookup","description":"Look up a key","parameters":{"type":"object","properties":{"key":{"type":"string"}},"required":["key"]}}}],"tool_choice":"required","max_tokens":128}))?;
            let m = message(&v)?.clone();
            let calls = m["tool_calls"].as_array().ok_or("missing tool calls")?;
            if calls.len() != 1 || calls[0]["function"]["name"] != "lookup" { return Err("expected exactly one lookup call".into()); }
            let id = calls[0]["id"].as_str().filter(|s| !s.is_empty()).ok_or("missing tool call id")?.to_owned();
            let args: Value = serde_json::from_str(calls[0]["function"]["arguments"].as_str().ok_or("missing tool arguments")?).map_err(|_| "invalid tool arguments")?;
            if args["key"] != "alpha" { return Err("unexpected tool arguments".into()); }
            let reply = r.chat(json!([{"role":"user","content":"Call the lookup tool for key alpha, then report its result."},m,{"role":"tool","tool_call_id":id,"content":"alpha is blue"}]))?;
            if !message(&reply)?["content"].as_str().is_some_and(|s| s.to_ascii_lowercase().contains("blue")) { return Err("tool continuation did not report the tool result".into()); }
            Ok(format!("tool call and continuation succeeded; configured parser: {parser}"))
        })()));
    } else {
        checks.push(unsupported(
            "tool_loop",
            "client",
            "no tool parser configured",
        ));
    }
    if config.reasoning {
        checks.push(check(
            "reasoning",
            "engine",
            (|| {
                let v = r.chat(
                    json!([{"role":"user","content":"Think step by step: what is 17 plus 25?"}]),
                )?;
                let m = message(&v)?;
                if !(text(&m["reasoning_content"]) || text(&m["reasoning"])) {
                    return Err("reasoning output absent".into());
                }
                if !m["content"].as_str().is_some_and(|s| {
                    s.split(|c: char| !c.is_ascii_digit())
                        .any(|part| part == "42")
                }) {
                    return Err("reasoning answer incorrect or absent".into());
                }
                Ok("reasoning output present and answer correct".into())
            })(),
        ));
    } else {
        checks.push(unsupported("reasoning", "engine", "reasoning not enabled"));
    }
    if config.responses_continuation {
        checks.push(check("responses_continuation", "client", (|| { let first = r.json("responses", json!({"model":config.model,"input":"Remember the word cobalt.","store":true,"max_output_tokens":64}))?; let id = first["id"].as_str().filter(|s| !s.is_empty()).ok_or("missing response id")?; let second = r.json("responses", json!({"model":config.model,"previous_response_id":id,"input":"What word did I ask you to remember? Reply with the word.","max_output_tokens":64}))?; let output = second["output"].as_array().ok_or("missing response output")?; if output.iter().flat_map(|o| o["content"].as_array().into_iter().flatten()).any(|c| c["text"].as_str().is_some_and(|s| s.to_ascii_lowercase().contains("cobalt"))) { Ok("stateful continuation recalled prior input".into()) } else { Err("continuation did not recall prior input".into()) } })()));
    } else {
        checks.push(unsupported(
            "responses_continuation",
            "client",
            "Responses continuation not enabled",
        ));
    }
    checks.push(check(
        "context_limit",
        "engine",
        (|| {
            let v = r.chat(json!([{"role":"user","content":config.context_prompt}]))?;
            let tokens = v
                .pointer("/usage/prompt_tokens")
                .and_then(Value::as_u64)
                .ok_or("missing prompt token usage; cannot establish accepted context")?;
            if tokens < u64::from(config.context_tokens)
                || tokens > u64::from(request.workload.model_budget_tokens)
            {
                return Err(format!(
                    "prompt token count {tokens} outside configured target and budget"
                ));
            }
            if !text(&message(&v)?["content"]) {
                return Err("context request returned no content".into());
            }
            Ok(format!(
                "accepted {tokens} prompt tokens; quality under load not measured"
            ))
        })(),
    ));
    protocol_checks(&mut r, &mut checks, request.workload.model_budget_tokens);
    let passed = checks.iter().all(|c| !matches!(c.status, Status::Failed));
    Ok(Summary {
        run_id,
        request,
        suite: config.suite,
        runner_version: env!("CARGO_PKG_VERSION"),
        tool_parser: config.tool_parser.clone(),
        answering_backends: r.backends.into_iter().collect(),
        checks,
        passed,
    })
}

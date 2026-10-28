use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};
use thalamus_validation::{Config, Status, Suite, run};

fn request() -> thalamus_runs::RunRequest {
    let request: thalamus_runs::RunRequest = serde_json::from_value(json!({"project_id":"00000000-0000-4000-8000-000000000001","kind":"validation","service":"svc","deployment":"new","runner":"validator","workload":{"name":"functional","seed":1,"concurrency":1,"duration_seconds":30,"warmup_seconds":0,"max_requests":16,"model_budget_tokens":1000,"tool_budget_calls":1},"evidence":{"desired_configuration":"manifest","observed_runtime":"runtime","test_definition":"suite","conditions":{"hardware":"gpu","nodes":["node"],"replicas":1,"cache_state":"cold","competing_traffic":"none","observed_at":"2026-01-01T00:00:00Z"}}})).unwrap();
    request.validate().unwrap();
    request
}
fn fixture(
    responses: Vec<(u16, &'static str, &'static str)>,
) -> (String, thread::JoinHandle<Vec<(String, Value)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let mut received = Vec::new();
        for (status, identity, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buf = [0; 4096];
            let (end, length) = loop {
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
                if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..pos]);
                    let length = headers
                        .lines()
                        .find_map(|l| {
                            l.split_once(':')
                                .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                                .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    break (pos + 4, length);
                }
            };
            while bytes.len() < end + length {
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
            }
            let header = String::from_utf8_lossy(&bytes[..end]).to_string();
            received.push((
                header,
                serde_json::from_slice(&bytes[end..end + length]).unwrap(),
            ));
            write!(stream, "HTTP/1.1 {status} fixture\r\nContent-Length: {}\r\nX-Backend: {identity}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        received
    });
    (url, handle)
}
fn config(url: String) -> Config {
    Config {
        base_url: url,
        model: "allowed".into(),
        forbidden_model: "denied".into(),
        suite: Suite::Candidate,
        backend_header: Some("x-backend".into()),
        expected_backend: Some("new".into()),
        old_backend: Some("old".into()),
        context_prompt: "a long prompt".into(),
        context_tokens: 10,
        tool_parser: None,
        reasoning: false,
        responses_continuation: false,
        responses: false,
        anthropic: false,
        timeout_seconds: 5,
    }
}
const CHAT: &str = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}],"usage":{"prompt_tokens":10}}"#;
const STREAM: &str =
    "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\ndata: [DONE]\n\n";
#[test]
fn functional_checks_reach_route_and_record_backend() {
    let (url, server) = fixture(vec![
        (401, "", "{}"),
        (403, "", "{}"),
        (200, "new", CHAT),
        (200, "new", STREAM),
        (200, "new", CHAT),
    ]);
    let summary = run(&config(url), request(), "run-1".into(), "secret").unwrap();
    assert!(summary.passed);
    assert_eq!(summary.answering_backends, vec!["new"]);
    assert!(matches!(
        summary
            .checks
            .iter()
            .find(|c| c.name == "tool_loop")
            .unwrap()
            .status,
        Status::Unsupported
    ));
    let calls = server.join().unwrap();
    assert!(
        calls[0]
            .0
            .to_ascii_lowercase()
            .contains("authorization: bearer thalamus-invalid-validation-key")
    );
    assert_eq!(calls[1].1["model"], "denied");
    assert_eq!(calls[3].1["stream"], true);
    assert!(!serde_json::to_string(&summary).unwrap().contains("secret"));
}
#[test]
fn candidate_never_passes_on_old_or_missing_identity() {
    let (url, server) = fixture(vec![
        (401, "", "{}"),
        (403, "", "{}"),
        (200, "old", CHAT),
        (200, "", STREAM),
        (200, "old", CHAT),
    ]);
    let summary = run(&config(url), request(), "run-2".into(), "secret").unwrap();
    assert!(!summary.passed);
    assert!(summary.answering_backends.contains(&"old".to_string()));
    assert!(
        summary
            .checks
            .iter()
            .any(|c| c.detail.contains("missing answering backend"))
    );
    server.join().unwrap();
}
#[test]
fn enabled_tool_reasoning_and_responses_are_exercised() {
    let tool = r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"call1","type":"function","function":{"name":"lookup","arguments":"{\"key\":\"alpha\"}"}}]}}]}"#;
    let reasoning =
        r#"{"choices":[{"message":{"content":"42","reasoning_content":"17 plus 25"}}]}"#;
    let first = r#"{"id":"resp1","output":[]}"#;
    let second =
        r#"{"id":"resp2","output":[{"content":[{"type":"output_text","text":"cobalt"}]}]}"#;
    let (url, server) = fixture(vec![
        (401, "", "{}"),
        (403, "", "{}"),
        (200, "new", CHAT),
        (200, "new", STREAM),
        (200, "new", tool),
        (
            200,
            "new",
            r#"{"choices":[{"message":{"content":"blue"}}]}"#,
        ),
        (200, "new", reasoning),
        (200, "new", first),
        (200, "new", second),
        (200, "new", CHAT),
    ]);
    let mut c = config(url);
    c.tool_parser = Some("hermes".into());
    c.reasoning = true;
    c.responses_continuation = true;
    let summary = run(&c, request(), "run-3".into(), "secret").unwrap();
    assert!(summary.passed);
    let calls = server.join().unwrap();
    assert_eq!(calls[5].1["messages"][2]["tool_call_id"], "call1");
    assert_eq!(calls[8].1["previous_response_id"], "resp1");
}
#[test]
fn accepted_context_requires_usage_evidence() {
    let missing = r#"{"choices":[{"message":{"content":"ok"}}]}"#;
    let (url, server) = fixture(vec![
        (401, "", "{}"),
        (403, "", "{}"),
        (200, "new", CHAT),
        (200, "new", STREAM),
        (200, "new", missing),
    ]);
    let summary = run(&config(url), request(), "run-4".into(), "secret").unwrap();
    assert!(!summary.passed);
    assert!(
        summary
            .checks
            .iter()
            .any(|c| c.detail.contains("missing prompt token usage"))
    );
    server.join().unwrap();
}

#[test]
fn responses_tool_stream_and_context_are_observed() {
    let chat_tool = r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"c","type":"function","function":{"name":"lookup","arguments":"{\"key\":\"alpha\"}"}}]}}]}"#;
    let response = r#"{"id":"r1","output":[{"type":"message","content":[{"type":"output_text","text":"hello"}]}],"usage":{"input_tokens":10}}"#;
    let stream = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\ndata: {\"type\":\"response.completed\"}\n\n";
    let tool = r#"{"id":"r2","output":[{"type":"function_call","name":"lookup","call_id":"call1","arguments":"{\"key\":\"alpha\"}"}]}"#;
    let result =
        r#"{"output":[{"type":"message","content":[{"type":"output_text","text":"blue"}]}]}"#;
    let (url, server) = fixture(vec![
        (401, "", "{}"),
        (403, "", "{}"),
        (200, "new", CHAT),
        (200, "new", STREAM),
        (200, "new", chat_tool),
        (
            200,
            "new",
            r#"{"choices":[{"message":{"content":"blue"}}]}"#,
        ),
        (200, "new", CHAT),
        (401, "", "{}"),
        (403, "", "{}"),
        (200, "new", response),
        (200, "new", stream),
        (200, "new", tool),
        (200, "new", result),
        (200, "new", response),
    ]);
    let mut c = config(url);
    c.tool_parser = Some("parser".into());
    c.responses = true;
    let mut req = request();
    req.workload.tool_budget_calls = 2;
    let summary = run(&c, req, "run-responses".into(), "secret").unwrap();
    assert!(summary.passed);
    for name in [
        "responses",
        "responses_streaming",
        "responses_tool_loop",
        "responses_context_limit",
    ] {
        assert!(matches!(
            summary
                .checks
                .iter()
                .find(|c| c.name == name)
                .unwrap()
                .status,
            Status::Passed
        ));
    }
    let calls = server.join().unwrap();
    assert_eq!(calls[11].1["max_output_tokens"], 128);
    assert_eq!(calls[12].1["previous_response_id"], "r2");
    assert_eq!(calls[12].1["input"][0]["call_id"], "call1");
}

#[test]
fn anthropic_tool_stream_and_context_use_messages_protocol() {
    let chat_tool = r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"c","type":"function","function":{"name":"lookup","arguments":"{\"key\":\"alpha\"}"}}]}}]}"#;
    let response = r#"{"content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":10}}"#;
    let stream = "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hello\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n";
    let tool =
        r#"{"content":[{"type":"tool_use","id":"t1","name":"lookup","input":{"key":"alpha"}}]}"#;
    let result = r#"{"content":[{"type":"text","text":"blue"}]}"#;
    let (url, server) = fixture(vec![
        (401, "", "{}"),
        (403, "", "{}"),
        (200, "new", CHAT),
        (200, "new", STREAM),
        (200, "new", chat_tool),
        (
            200,
            "new",
            r#"{"choices":[{"message":{"content":"blue"}}]}"#,
        ),
        (200, "new", CHAT),
        (401, "", "{}"),
        (403, "", "{}"),
        (200, "new", response),
        (200, "new", stream),
        (200, "new", tool),
        (200, "new", result),
        (200, "new", response),
    ]);
    let mut c = config(url);
    c.tool_parser = Some("parser".into());
    c.anthropic = true;
    let mut req = request();
    req.workload.tool_budget_calls = 2;
    let summary = run(&c, req, "run-anthropic".into(), "secret").unwrap();
    assert!(summary.passed);
    for name in [
        "anthropic",
        "anthropic_streaming",
        "anthropic_tool_loop",
        "anthropic_context_limit",
    ] {
        assert!(matches!(
            summary
                .checks
                .iter()
                .find(|c| c.name == name)
                .unwrap()
                .status,
            Status::Passed
        ));
    }
    let calls = server.join().unwrap();
    assert!(calls[9].0.starts_with("POST /v1/messages "));
    assert!(
        calls[9]
            .0
            .to_ascii_lowercase()
            .contains("x-api-key: secret")
    );
    assert_eq!(calls[11].1["max_tokens"], 128);
    assert_eq!(
        calls[12].1["messages"][2]["content"][0]["tool_use_id"],
        "t1"
    );
}

#[test]
fn responses_context_on_old_backend_fails_and_tools_remain_unsupported() {
    let response = r#"{"output":[{"type":"message","content":[{"type":"output_text","text":"hello"}]}],"usage":{"input_tokens":10}}"#;
    let stream = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\ndata: {\"type\":\"response.completed\"}\n\n";
    let (url, server) = fixture(vec![
        (401, "", "{}"),
        (403, "", "{}"),
        (200, "new", CHAT),
        (200, "new", STREAM),
        (200, "new", CHAT),
        (401, "", "{}"),
        (403, "", "{}"),
        (200, "new", response),
        (200, "new", stream),
        (200, "old", response),
    ]);
    let mut c = config(url);
    c.responses = true;
    let summary = run(&c, request(), "run-old".into(), "secret").unwrap();
    assert!(!summary.passed);
    assert!(matches!(
        summary
            .checks
            .iter()
            .find(|c| c.name == "responses_context_limit")
            .unwrap()
            .status,
        Status::Failed
    ));
    assert!(matches!(
        summary
            .checks
            .iter()
            .find(|c| c.name == "responses_tool_loop")
            .unwrap()
            .status,
        Status::Unsupported
    ));
    server.join().unwrap();
}

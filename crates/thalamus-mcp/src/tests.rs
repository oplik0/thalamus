use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use std::sync::Mutex;
use tower::ServiceExt;

#[derive(Default)]
struct Backend {
    calls: Mutex<Vec<String>>,
}
#[async_trait]
impl ControlPlane for Backend {
    async fn authenticate(&self, token: &str, audience: &str) -> Result<Identity, ()> {
        if audience != "https://control.example/mcp" {
            return Err(());
        }
        match token {
            "alice" | "bob" => Ok(Identity {
                issuer: "https://id.example".into(),
                subject: token.into(),
                debug_session: None,
            }),
            _ => Err(()),
        }
    }
    async fn execute(&self, identity: &Identity, action: Action) -> Result<Value, String> {
        self.calls.lock().unwrap().push(identity.subject.clone());
        match action {
            Action::ServiceContext(s) => Ok(json!({"service":s.service,"actor":identity.subject})),
            _ => Err("denied".into()),
        }
    }
}
fn config() -> Config {
    Config::new("https://control.example/mcp", "https://id.example").unwrap()
}
fn request(token: Option<&str>, body: &str) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(token) = token {
        b = b.header("authorization", format!("Bearer {token}"));
    }
    b.body(Body::from(body.to_owned())).unwrap()
}
#[test]
fn config_rejects_unsafe_urls() {
    assert!(Config::new("http://control.example/mcp", "https://id.example").is_err());
    assert!(Config::new("https://control.example/other", "https://id.example").is_err());
    assert!(Config::new("https://control.example/mcp", "https://user@id.example").is_err());
}
#[tokio::test]
async fn discovery_and_authentication_are_per_request() {
    let backend = Arc::new(Backend::default());
    let app = router(config(), backend.clone());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/.well-known/oauth-protected-resource/mcp")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let data = to_bytes(response.into_body(), 8192).await.unwrap();
    let metadata: Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(metadata["resource"], "https://control.example/mcp");
    let response = app.clone().oneshot(request(None, "{}")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers()[header::WWW_AUTHENTICATE]
        .to_str()
        .unwrap()
        .contains("/.well-known/oauth-protected-resource/mcp"));
    assert_eq!(
        app.clone()
            .oneshot(request(Some("bad"), "{}"))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_service_context","arguments":{"service":"model"}}}"#;
    for actor in ["alice", "bob"] {
        let response = app
            .clone()
            .oneshot(request(Some(actor), call))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        assert!(std::str::from_utf8(&bytes).unwrap().contains(actor));
    }
    assert_eq!(*backend.calls.lock().unwrap(), ["alice", "bob"]);
}
#[tokio::test]
async fn schemas_and_bounds_prevent_unchecked_operations() {
    let backend = Arc::new(Backend::default());
    let server = Server::new(backend.clone());
    let tools = server.tool_router.list_all();
    assert_eq!(tools.len(), 9);
    for name in [
        "get_service_context",
        "compare_revisions",
        "query_metrics",
        "get_logs",
        "check_compatibility",
        "run_validation",
        "run_benchmark",
        "get_run",
        "propose_change",
    ] {
        assert!(tools.iter().any(|t| t.name == name));
    }
    let (mut parts, _) = Request::new(Body::empty()).into_parts();
    parts.extensions.insert(Identity {
        issuer: "https://id.example".into(),
        subject: "alice".into(),
        debug_session: None,
    });
    assert!(server
        .invoke(
            parts,
            Action::GetLogs(Logs {
                service: "s".into(),
                deployment: "d".into(),
                since_seconds: 1,
                limit: 1001
            })
        )
        .await
        .is_err());
    assert!(backend.calls.lock().unwrap().is_empty());
    assert!(serde_json::from_value::<Service>(json!({"service":"s","actor":"admin"})).is_err());
}

#[derive(Default)]
struct SessionBackend {
    calls: Mutex<Vec<Option<String>>>,
}
#[async_trait]
impl ControlPlane for SessionBackend {
    async fn authenticate(&self, token: &str, audience: &str) -> Result<Identity, ()> {
        if audience != "https://control.example/mcp" {
            return Err(());
        }
        let debug_session = match token {
            "oauth" => None,
            "debug-credential" => Some("verified-session".into()),
            "bad-binding" => Some(String::new()),
            _ => return Err(()),
        };
        Ok(Identity {
            issuer: "https://id.example".into(),
            subject: "alice".into(),
            debug_session,
        })
    }
    async fn execute(&self, identity: &Identity, _: Action) -> Result<Value, String> {
        self.calls
            .lock()
            .unwrap()
            .push(identity.debug_session.clone());
        Ok(json!({"ok": true}))
    }
}

#[tokio::test]
async fn debug_binding_comes_only_from_the_authenticated_credential() {
    let backend = Arc::new(SessionBackend::default());
    let app = router(config(), backend.clone());
    let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_service_context","arguments":{"service":"model"}}}"#;
    for token in ["oauth", "debug-credential"] {
        let mut req = request(Some(token), call);
        req.headers_mut()
            .insert("x-debug-session", "attacker-session".parse().unwrap());
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains("\"ok\""), "{body}");
        assert!(!body.contains("\"error\""), "{body}");
    }
    assert_eq!(
        *backend.calls.lock().unwrap(),
        [None, Some("verified-session".into())]
    );

    let forged = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_service_context","arguments":{"service":"model","debug_session":"attacker-session"}}}"#;
    let response = app
        .clone()
        .oneshot(request(Some("oauth"), forged))
        .await
        .unwrap();
    let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
    assert!(std::str::from_utf8(&bytes).unwrap().contains("error"));
    assert_eq!(backend.calls.lock().unwrap().len(), 2);
    assert_eq!(
        app.oneshot(request(Some("bad-binding"), call))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

use std::{
    env, fs,
    io::{self, Read},
    path::PathBuf,
};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use reqwest::{
    header::{HeaderMap, HeaderValue, AUTHORIZATION, COOKIE},
    Method, Url,
};
use serde_json::Value;

/// Thalamus control-plane CLI. All successful responses are printed as JSON.
///
/// Set THALAMUS_TOKEN (bearer token) or THALAMUS_COOKIE (session cookie header)
/// in the environment. Credentials are never accepted as command-line arguments.
#[derive(Parser)]
#[command(name = "thalamus", version, about, long_about)]
struct Cli {
    /// API origin or base URL. HTTPS is required except on loopback.
    #[arg(long, env = "THALAMUS_API_URL")]
    api_url: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List published services.
    Services,
    /// Get a service and its operational context.
    Context { service: String },
    /// Compare two revisions of a published service.
    Compare {
        service: String,
        from: String,
        to: String,
    },
    /// Query the service's preset metrics.
    Metrics { service: String },
    /// Get compatibility information for a service.
    Compatibility { service: String },
    /// Manage validation and benchmark runs.
    Run {
        #[command(subcommand)]
        command: RunCommand,
    },
    /// Manage GitOps change proposals.
    Change {
        #[command(subcommand)]
        command: ChangeCommand,
    },
    /// Manage projects.
    Project {
        #[command(subcommand)]
        command: ResourceCommand,
    },
    /// Manage API keys. Creation returns the secret only once.
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Generate or check a client configuration.
    ClientConfig {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Subcommand)]
enum RunCommand {
    List,
    Get {
        id: String,
    },
    /// Submit a validation run from a JSON or YAML request object.
    Validation {
        #[arg(
            long,
            value_name = "FILE",
            help = "JSON/YAML request object; - reads stdin"
        )]
        file: PathBuf,
    },
    /// Submit a benchmark run from a JSON or YAML request object.
    Benchmark {
        #[arg(
            long,
            value_name = "FILE",
            help = "JSON/YAML request object; - reads stdin"
        )]
        file: PathBuf,
    },
}
#[derive(Subcommand)]
enum ChangeCommand {
    List,
    /// Propose a structured patch, supplied as a JSON or YAML request object.
    Propose {
        #[arg(
            long,
            value_name = "FILE",
            help = "Request object containing service, base_commit and patch; - reads stdin"
        )]
        file: PathBuf,
    },
    Validate {
        id: String,
    },
    OpenPr {
        id: String,
    },
}
#[derive(Subcommand)]
enum ResourceCommand {
    List,
    Create {
        #[arg(
            long,
            value_name = "FILE",
            help = "JSON/YAML request object; - reads stdin"
        )]
        file: PathBuf,
    },
}
#[derive(Subcommand)]
enum KeyCommand {
    List,
    Create {
        #[arg(
            long,
            value_name = "FILE",
            help = "JSON/YAML request object; - reads stdin"
        )]
        file: PathBuf,
    },
    Revoke {
        id: String,
    },
}
#[derive(Subcommand)]
enum ConfigCommand {
    Generate {
        #[arg(
            long,
            value_name = "FILE",
            help = "JSON/YAML request object; - reads stdin"
        )]
        file: PathBuf,
    },
    Check {
        #[arg(
            long,
            value_name = "FILE",
            help = "JSON/YAML request object, including key; - reads stdin"
        )]
        file: PathBuf,
    },
}

struct Request {
    method: Method,
    segments: Vec<String>,
    query: Vec<(String, String)>,
    body: Option<Value>,
}

fn read_object(path: &PathBuf) -> Result<Value> {
    let text = if path.as_os_str() == "-" {
        let mut text = String::new();
        io::stdin()
            .read_to_string(&mut text)
            .context("reading request from stdin")?;
        text
    } else {
        fs::read_to_string(path)
            .with_context(|| format!("reading request file {}", path.display()))?
    };
    // JSON is a YAML subset; deserializing to JSON values also rejects non-JSON YAML values.
    let value: Value = serde_yaml::from_str(&text).context("parsing JSON/YAML request")?;
    if !value.is_object() {
        bail!("request must be a JSON/YAML object");
    }
    Ok(value)
}

fn request(method: Method, segments: &[&str], body: Option<Value>) -> Request {
    Request {
        method,
        segments: segments.iter().map(|s| (*s).to_owned()).collect(),
        query: Vec::new(),
        body,
    }
}

fn run_body(file: &PathBuf, kind: &str) -> Result<Value> {
    let mut body = read_object(file)?;
    let object = body.as_object_mut().expect("read_object checks object");
    if let Some(existing) = object.get("kind") {
        if existing.as_str() != Some(kind) {
            bail!("request kind must be {kind}");
        }
    }
    object.insert("kind".into(), Value::String(kind.into()));
    Ok(body)
}

fn make_request(command: Command) -> Result<Request> {
    let get = |segments: &[&str]| request(Method::GET, segments, None);
    let post_file = |segments: &[&str], file: &PathBuf| -> Result<Request> {
        Ok(request(Method::POST, segments, Some(read_object(file)?)))
    };
    Ok(match command {
        Command::Services => get(&["services"]),
        Command::Context { service } => get(&["services", &service]),
        Command::Metrics { service } => get(&["services", &service, "metrics"]),
        Command::Compatibility { service } => get(&["services", &service, "compatibility"]),
        Command::Compare { service, from, to } => {
            let mut r = get(&["services", &service, "compare"]);
            r.query = vec![("from".into(), from), ("to".into(), to)];
            r
        }
        Command::Run { command } => match command {
            RunCommand::List => get(&["runs"]),
            RunCommand::Get { id } => get(&["runs", &id]),
            RunCommand::Validation { file } => request(
                Method::POST,
                &["runs"],
                Some(run_body(&file, "validation")?),
            ),
            RunCommand::Benchmark { file } => {
                request(Method::POST, &["runs"], Some(run_body(&file, "benchmark")?))
            }
        },
        Command::Change { command } => match command {
            ChangeCommand::List => get(&["changes"]),
            ChangeCommand::Propose { file } => post_file(&["changes"], &file)?,
            ChangeCommand::Validate { id } => {
                request(Method::POST, &["changes", &id, "validate"], None)
            }
            ChangeCommand::OpenPr { id } => {
                request(Method::POST, &["changes", &id, "open-pr"], None)
            }
        },
        Command::Project { command } => match command {
            ResourceCommand::List => get(&["projects"]),
            ResourceCommand::Create { file } => post_file(&["projects"], &file)?,
        },
        Command::Key { command } => match command {
            KeyCommand::List => get(&["keys"]),
            KeyCommand::Create { file } => post_file(&["keys"], &file)?,
            KeyCommand::Revoke { id } => request(Method::DELETE, &["keys", &id], None),
        },
        Command::ClientConfig { command } => match command {
            ConfigCommand::Generate { file } => post_file(&["client-config"], &file)?,
            ConfigCommand::Check { file } => post_file(&["client-config", "check"], &file)?,
        },
    })
}

fn base_url(input: &str) -> Result<Url> {
    let url = Url::parse(input).context("invalid API URL")?;
    if url.cannot_be_a_base()
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("API URL must be an HTTP(S) base URL without credentials, query or fragment");
    }
    let loopback = match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        bail!("API URL must use HTTPS (HTTP is allowed only on loopback)");
    }
    Ok(url)
}

fn target_url(mut url: Url, req: &Request) -> Result<Url> {
    // The URL library silently drops dot segments, which would send a request
    // to a different endpoint than the one selected by the caller.
    if req
        .segments
        .iter()
        .any(|segment| segment.is_empty() || matches!(segment.as_str(), "." | ".."))
    {
        bail!("API path segments must not be empty, '.' or '..'");
    }
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| anyhow!("API URL cannot be used as a base"))?;
        path.pop_if_empty()
            .extend(["api", "v1"])
            .extend(req.segments.iter().map(String::as_str));
    }
    if !req.query.is_empty() {
        url.query_pairs_mut().extend_pairs(&req.query);
    }
    Ok(url)
}

fn client(token: Option<&str>, cookie: Option<&str>) -> Result<reqwest::Client> {
    if token.is_some() && cookie.is_some() {
        bail!("set only one of THALAMUS_TOKEN and THALAMUS_COOKIE");
    }
    let mut headers = HeaderMap::new();
    if let Some(token) = token {
        if token.is_empty() {
            bail!("THALAMUS_TOKEN is empty");
        }
        let mut value =
            HeaderValue::from_str(&format!("Bearer {token}")).context("invalid token header")?;
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);
    }
    if let Some(cookie) = cookie {
        if cookie.is_empty() {
            bail!("THALAMUS_COOKIE is empty");
        }
        let mut value = HeaderValue::from_str(cookie).context("invalid cookie header")?;
        value.set_sensitive(true);
        headers.insert(COOKIE, value);
    }
    Ok(reqwest::Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

async fn execute(client: &reqwest::Client, base: Url, req: Request) -> Result<Value> {
    let url = target_url(base, &req)?;
    let mut builder = client.request(req.method, url);
    if let Some(body) = req.body {
        builder = builder.json(&body);
    }
    let response = builder.send().await.context("sending API request")?;
    let status = response.status();
    let bytes = response.bytes().await.context("reading API response")?;
    if !status.is_success() {
        let message = String::from_utf8_lossy(&bytes);
        bail!("API returned {status}: {message}");
    }
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).context("API returned invalid JSON")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();
    let base = base_url(&args.api_url)?;
    let client = client(
        env::var("THALAMUS_TOKEN").ok().as_deref(),
        env::var("THALAMUS_COOKIE").ok().as_deref(),
    )?;
    let result = execute(&client, base, make_request(args.command)?).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn url_encodes_segments_and_query_and_preserves_prefix() {
        let req = make_request(Command::Compare {
            service: "a/b".into(),
            from: "a&b".into(),
            to: "z".into(),
        })
        .unwrap();
        assert_eq!(
            target_url(base_url("https://example.com/prefix/").unwrap(), &req)
                .unwrap()
                .as_str(),
            "https://example.com/prefix/api/v1/services/a%2Fb/compare?from=a%26b&to=z"
        );
    }

    #[test]
    fn rejects_path_segments_that_cannot_identify_the_requested_resource() {
        for service in ["", ".", ".."] {
            let req = make_request(Command::Metrics {
                service: service.into(),
            })
            .unwrap();
            assert!(target_url(base_url("https://example.com/prefix").unwrap(), &req).is_err());
        }
    }

    #[test]
    fn credentials_and_urls_are_restricted() {
        assert!(base_url("http://example.com").is_err());
        assert!(base_url("https://user:pass@example.com").is_err());
        assert!(base_url("http://127.0.0.1:8080").is_ok());
        assert!(client(Some("token"), Some("session=x")).is_err());
        assert!(client(Some("bad\nheader"), None).is_err());
    }

    #[test]
    fn maps_run_lookup_and_key_revoke() {
        let run = make_request(Command::Run {
            command: RunCommand::Get {
                id: "run-42".into(),
            },
        })
        .unwrap();
        assert_eq!(run.method, Method::GET);
        assert_eq!(run.segments, ["runs", "run-42"]);
        let key = make_request(Command::Key {
            command: KeyCommand::Revoke { id: "key-1".into() },
        })
        .unwrap();
        assert_eq!(key.method, Method::DELETE);
        assert_eq!(key.segments, ["keys", "key-1"]);
    }
    #[tokio::test]
    async fn sends_auth_and_returns_server_json() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 4096];
            let n = socket.read(&mut bytes).await.unwrap();
            let request = String::from_utf8_lossy(&bytes[..n]).to_ascii_lowercase();
            assert!(request.starts_with(
                "get /api/v1/services HTTP/1.1"
                    .to_ascii_lowercase()
                    .as_str()
            ));
            assert!(request.contains("authorization: bearer secret"));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"services\":[]}").await.unwrap();
        });
        let result = execute(
            &client(Some("secret"), None).unwrap(),
            base_url(&format!("http://{addr}")).unwrap(),
            make_request(Command::Services).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(result, serde_json::json!({"services": []}));
        server.await.unwrap();
    }
}

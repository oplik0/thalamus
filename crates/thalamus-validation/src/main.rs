use reqwest::{Url, blocking::Client, redirect::Policy};
use std::{env, process::ExitCode};
use thalamus_validation::{Config, run};

fn main() -> ExitCode {
    match execute() {
        Ok(passed) => {
            if passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(error) => {
            eprintln!("validation runner: {error}");
            ExitCode::from(2)
        }
    }
}
fn execute() -> Result<bool, String> {
    let config: Config = serde_json::from_str(
        &env::var("THALAMUS_VALIDATION_CONFIG")
            .map_err(|_| "missing THALAMUS_VALIDATION_CONFIG")?,
    )
    .map_err(|e| format!("invalid validation config: {e}"))?;
    let request = serde_json::from_str(
        &env::var("THALAMUS_RUN_REQUEST").map_err(|_| "missing THALAMUS_RUN_REQUEST")?,
    )
    .map_err(|e| format!("invalid run request: {e}"))?;
    let id = env::var("THALAMUS_RUN_ID").map_err(|_| "missing THALAMUS_RUN_ID")?;
    let parsed = uuid::Uuid::parse_str(&id).map_err(|_| "invalid THALAMUS_RUN_ID")?;
    if parsed.to_string() != id {
        return Err("THALAMUS_RUN_ID must be a canonical UUID".into());
    }
    let key = env::var("THALAMUS_API_KEY").map_err(|_| "missing THALAMUS_API_KEY")?;
    let summary = run(&config, request, id, &key)?;
    let output = serde_json::to_vec(&summary).map_err(|e| e.to_string())?;
    let client = Client::builder()
        .redirect(Policy::none())
        .https_only(true)
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|_| "cannot initialize result uploader")?;
    let upload_url = read_upload_url(
        std::path::Path::new("/var/run/thalamus/result-upload/url"),
        &summary.run_id,
    )?;
    let response = client
        .put(upload_url)
        .header(reqwest::header::IF_NONE_MATCH, "*")
        .body(output.clone())
        .send()
        .map_err(|_| "result upload failed")?;
    if !response.status().is_success() {
        return Err("result upload failed".into());
    }
    println!(
        "{}",
        String::from_utf8(output).map_err(|_| "invalid summary encoding")?
    );
    Ok(summary.passed)
}

fn read_upload_url(path: &std::path::Path, id: &str) -> Result<Url, String> {
    let value = std::fs::read_to_string(path).map_err(|_| "cannot read result upload URL")?;
    validate_upload_url(&value, id)
}

fn validate_upload_url(value: &str, id: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| "invalid result upload URL")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !url
            .path()
            .ends_with(&format!("/runs/{id}/result/output.json"))
        || url.query().is_none()
    {
        return Err("invalid result upload URL".into());
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::{read_upload_url, validate_upload_url};

    #[test]
    fn upload_url_is_https_and_bound_to_exact_run_object() {
        let id = "00000000-0000-0000-0000-000000000001";
        let valid = format!(
            "https://objects.example/bucket/runs/{id}/result/output.json?X-Amz-Signature=secret"
        );
        assert!(validate_upload_url(&valid, id).is_ok());
        for invalid in [
            valid.replace("https://", "http://"),
            valid.replace("objects.example", "user:pass@objects.example"),
            valid.replace(id, "00000000-0000-0000-0000-000000000002"),
            valid.replace("output.json?", "other.json?"),
            format!("{valid}#fragment"),
            valid.split('?').next().unwrap().to_owned(),
        ] {
            let error = validate_upload_url(&invalid, id).unwrap_err();
            assert_eq!(error, "invalid result upload URL");
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn projected_upload_url_is_read_again_after_rotation() {
        let id = "00000000-0000-0000-0000-000000000001";
        let path = std::env::temp_dir().join(format!("thalamus-upload-{}", std::process::id()));
        let url = format!("https://objects.example/runs/{id}/result/output.json?signature=");
        std::fs::write(&path, format!("{url}first")).unwrap();
        assert_eq!(
            read_upload_url(&path, id).unwrap().query(),
            Some("signature=first")
        );
        std::fs::write(&path, format!("{url}second")).unwrap();
        assert_eq!(
            read_upload_url(&path, id).unwrap().query(),
            Some("signature=second")
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            read_upload_url(&path, id).unwrap_err(),
            "cannot read result upload URL"
        );
    }
}

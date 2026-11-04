use reqwest::{Url, blocking::Client, redirect::Policy};
use std::{env, io::Read, path::Path, process::ExitCode};
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
    let ca_path = env::var_os("THALAMUS_S3_CA_FILE");
    let client = result_uploader(ca_path.as_deref().map(Path::new))?;
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

fn result_uploader(ca_path: Option<&Path>) -> Result<Client, String> {
    const MAX_CA_BYTES: u64 = 1024 * 1024;
    let mut builder = Client::builder()
        .redirect(Policy::none())
        .https_only(true)
        .timeout(std::time::Duration::from_secs(60));
    if let Some(path) = ca_path {
        if path.as_os_str().is_empty() {
            return Err("invalid S3 CA file".into());
        }
        match std::fs::File::open(path) {
            Ok(file) => {
                let mut pem = Vec::new();
                file.take(MAX_CA_BYTES + 1)
                    .read_to_end(&mut pem)
                    .map_err(|_| "cannot read S3 CA file")?;
                if pem.is_empty() || pem.len() > MAX_CA_BYTES as usize {
                    return Err("invalid S3 CA bundle".into());
                }
                let certificates = reqwest::Certificate::from_pem_bundle(&pem)
                    .map_err(|_| "invalid S3 CA bundle")?;
                if certificates.is_empty() {
                    return Err("invalid S3 CA bundle".into());
                }
                for certificate in certificates {
                    builder = builder.add_root_certificate(certificate);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("cannot read S3 CA file".into()),
        }
    }
    builder
        .build()
        .map_err(|_| "cannot initialize result uploader".into())
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
    use super::{read_upload_url, result_uploader, validate_upload_url};

    const TEST_CA: &str = "-----BEGIN CERTIFICATE-----\n\
MIICGzCCAaGgAwIBAgIQQdKd0XLq7qeAwSxs6S+HUjAKBggqhkjOPQQDAzBPMQsw\n\
CQYDVQQGEwJVUzEpMCcGA1UEChMgSW50ZXJuZXQgU2VjdXJpdHkgUmVzZWFyY2gg\n\
R3JvdXAxFTATBgNVBAMTDElTUkcgUm9vdCBYMjAeFw0yMDA5MDQwMDAwMDBaFw00\n\
MDA5MTcxNjAwMDBaME8xCzAJBgNVBAYTAlVTMSkwJwYDVQQKEyBJbnRlcm5ldCBT\n\
ZWN1cml0eSBSZXNlYXJjaCBHcm91cDEVMBMGA1UEAxMMSVNSRyBSb290IFgyMHYw\n\
EAYHKoZIzj0CAQYFK4EEACIDYgAEzZvVn4CDCuwJSvMWSj5cz3es3mcFDR0HttwW\n\
+1qLFNvicWDEukWVEYmO6gbf9yoWHKS5xcUy4APgHoIYOIvXRdgKam7mAHf7AlF9\n\
ItgKbppbd9/w+kHsOdx1ymgHDB/qo0IwQDAOBgNVHQ8BAf8EBAMCAQYwDwYDVR0T\n\
AQH/BAUwAwEB/zAdBgNVHQ4EFgQUfEKWrt5LSDv6kviejM9ti6lyN5UwCgYIKoZI\n\
zj0EAwMDaAAwZQIwe3lORlCEwkSHRhtFcP9Ymd70/aTSVaYgLXTWNLxBo1BfASdW\n\
tL4ndQavEi51mI38AjEAi/V3bNTIZargCyzuFJ0nN6T5U6VR5CmD1/iQMVtCnwr1\n\
/q4AaOeMSQ+2b1tbFfLn\n\
-----END CERTIFICATE-----\n";

    #[test]
    fn uploader_handles_optional_and_invalid_ca_files() {
        let path = std::env::temp_dir().join(format!("thalamus-upload-ca-{}", std::process::id()));
        assert!(result_uploader(None).is_ok());
        assert!(result_uploader(Some(&path)).is_ok());
        std::fs::write(&path, "invalid secret certificate").unwrap();
        assert_eq!(
            result_uploader(Some(&path)).err().unwrap(),
            "invalid S3 CA bundle"
        );
        std::fs::write(&path, vec![b'x'; 1024 * 1024 + 1]).unwrap();
        assert_eq!(
            result_uploader(Some(&path)).err().unwrap(),
            "invalid S3 CA bundle"
        );
        std::fs::write(&path, format!("{TEST_CA}{TEST_CA}")).unwrap();
        assert!(result_uploader(Some(&path)).is_ok());
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            result_uploader(Some(std::env::temp_dir().as_path()))
                .err()
                .unwrap(),
            "cannot read S3 CA file"
        );
    }

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

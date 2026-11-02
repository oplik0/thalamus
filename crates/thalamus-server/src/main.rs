use std::sync::Arc;
use thalamus_server::{AppState, Config};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--openapi")) {
        println!(
            "{}",
            serde_json::to_string_pretty(&thalamus_server::openapi_document())?
        );
        return Ok(());
    }
    let config =
        Config::from_env().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let db = sqlx::postgres::PgPoolOptions::new()
        .max_connections(20)
        .connect(&config.database_url)
        .await?;
    thalamus_server::migrate(&db).await?;
    let identity = thalamus_identity::IdentityService::discover(
        db.clone(),
        thalamus_identity::Config {
            issuer: config.oidc_issuer.clone(),
            client_id: config.oidc_client_id.clone(),
            client_secret: Some(config.oidc_client_secret.clone()),
            redirect_uri: format!("{}/auth/callback", config.public_origin),
            ca_pem: config.oidc_ca_pem.clone(),
            session_signing_key: config.cookie_signing_key.clone(),
            session_lifetime: chrono::Duration::hours(12),
            groups: config.groups.clone(),
        },
    )
    .await?;
    let artifacts = thalamus_runs::S3Config::from_env()?
        .map(|config| config.artifacts())
        .transpose()?;
    let kube = if config.runtime.is_some() || config.debug.is_some() {
        Some(thalamus_k8s::default_client().await?)
    } else {
        None
    };
    let runtime_observer = config
        .runtime
        .as_ref()
        .map(|runtime| {
            thalamus_runtime::LiveObserver::new(
                kube.as_ref()
                    .expect("runtime requires Kubernetes client")
                    .clone(),
                runtime.clone(),
            )
            .map(Arc::new)
        })
        .transpose()?;
    let capability_evidence = match (&config.runtime, &runtime_observer) {
        (Some(runtime), Some(observer)) => {
            let max_age_seconds = std::env::var("THALAMUS_EVIDENCE_MAX_AGE_SECONDS")
                .unwrap_or_else(|_| "86400".into())
                .parse()?;
            Some(thalamus_server::runtime_observer::provider(
                db.clone(),
                artifacts
                    .clone()
                    .ok_or("runtime evidence requires S3 configuration")?,
                observer.as_ref().clone(),
                runtime.clone(),
                max_age_seconds,
            )?)
        }
        _ => None,
    };
    let policy_source = config.policy_source.clone();
    let debug_proxy = match &config.debug {
        Some(debug) => Some(thalamus_server::debug_host::new_debug_proxy(
            db.clone(),
            policy_source
                .clone()
                .ok_or("debug requires an access policy")?,
            kube.as_ref()
                .expect("debug requires Kubernetes client")
                .clone(),
            debug.namespace.clone(),
            debug.service_cidrs.clone(),
        )?),
        None => None,
    };
    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    let state = Arc::new(AppState {
        config,
        db: db.clone(),
        identity,
        capability_evidence,
        policy_source,
        runtime_observer,
        benchmark_artifacts: artifacts,
        debug_kube: kube,
        debug_proxy: debug_proxy.clone(),
    });
    let debug_supervisor = debug_proxy
        .map(|proxy| thalamus_server::debug_host::spawn_debug_supervisor(proxy, db.clone()));
    let rollout_refresh = thalamus_server::rollout_ci::enabled(&state).then(|| {
        let rollout_state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if let Err(error) =
                    thalamus_server::rollout_ci::reconcile(rollout_state.clone()).await
                {
                    eprintln!("rollout check refresh failed: {error}");
                }
            }
        })
    });
    let result = axum::serve(listener, thalamus_server::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    if let Some(refresh) = rollout_refresh {
        refresh.abort();
    }
    if let Some(supervisor) = debug_supervisor {
        supervisor.abort();
    }
    result?;
    Ok(())
}

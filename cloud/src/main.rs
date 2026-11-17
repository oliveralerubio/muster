use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use muster_cloud::auth::Oidc;
use muster_cloud::config::Config;
use muster_cloud::state::AppState;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MUSTER_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .json()
        .init();

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("muster-cloud: configuration error: {err}");
            return ExitCode::from(78);
        }
    };
    tracing::info!(config = ?config, "starting");

    let db = match sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(5))
        .connect(config.database_url())
        .await
    {
        Ok(pool) => pool,
        Err(err) => {
            eprintln!("muster-cloud: database connection error: {err}");
            return ExitCode::from(78);
        }
    };

    if let Err(err) = sqlx::migrate!("./migrations").run(&db).await {
        eprintln!("muster-cloud: migration error: {err}");
        return ExitCode::from(78);
    }
    // Lifecycle authority before serving: never created here (explicit
    // `muster-cloud-ops init`); missing, stale, divergent or fenced refuses.
    match muster_cloud::control::check_for_service(&config.control, &db).await {
        Ok(status) => tracing::info!(
            sequence = status.sequence,
            replayed = status.db.as_ref().map_or(0, |d| d.replayed),
            "lifecycle authority verified"
        ),
        Err(err) => {
            eprintln!("muster-cloud: {err}");
            return ExitCode::from(78);
        }
    }

    let oidc = match Oidc::discover(&config).await {
        Ok(oidc) => Arc::new(oidc),
        Err(err) => {
            eprintln!("muster-cloud: OIDC discovery error: {err}");
            return ExitCode::from(78);
        }
    };

    let listener = match tokio::net::TcpListener::bind(config.bind).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("muster-cloud: cannot bind {}: {err}", config.bind);
            return ExitCode::FAILURE;
        }
    };
    let addr = listener.local_addr().expect("bound address");
    if let Err(err) = std::fs::create_dir_all(&config.repo_root) {
        eprintln!(
            "muster-cloud: cannot create MUSTER_REPO_ROOT {}: {err}",
            config.repo_root.display()
        );
        return ExitCode::from(78);
    }
    let repos = match muster_cloud::repo_store::RepoStore::open(config.repo_root.clone()) {
        Ok(repos) => Arc::new(repos),
        Err(err) => {
            eprintln!(
                "muster-cloud: cannot prepare scratch in MUSTER_REPO_ROOT {}: {err}",
                config.repo_root.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let state = AppState {
        config: Arc::new(config),
        db,
        oidc,
        repos,
        role_gate: Arc::new(muster_cloud::control::RoleGate::default()),
    };
    // Before serving: tombstoned workspaces with residue (a database or root
    // restored from before their purge, or a purge cut short) are erased
    // now. Whatever cannot be erased yet keeps /readyz at 503 and is retried
    // by the worker below; access to it is already refused.
    match muster_cloud::lifecycle_purge::reconcile_before_serving(&state.purge_ctx()).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            workspaces = n,
            "purge: restored residue erased before serving"
        ),
        Err(err) => tracing::warn!(error = %err, "purge: residue left for the worker; not ready"),
    }
    // Purges deletion requests whose grace has expired, retrying with
    // backoff (including purges left unfinished by a previous process).
    tokio::spawn(muster_cloud::lifecycle_purge::worker(
        state.purge_ctx(),
        if state.config.env.is_production() {
            Duration::from_secs(60)
        } else {
            Duration::from_secs(1)
        },
    ));
    if state.config.paddle.is_some() {
        // Retries billing reconciliations left pending, including by a
        // previous process.
        tokio::spawn(muster_cloud::billing_reconcile::worker(
            state.clone(),
            Duration::from_secs(1),
        ));
    }
    // Retries pending immediate cancellations of deletion requests,
    // including those left by a previous process.
    tokio::spawn(muster_cloud::lifecycle_provider::worker(
        state.clone(),
        Duration::from_secs(1),
    ));
    println!("muster-cloud listening on http://{addr}");
    if let Err(err) = axum::serve(listener, muster_cloud::router(state)).await {
        eprintln!("muster-cloud: server error: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

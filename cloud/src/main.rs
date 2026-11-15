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
    let repos = Arc::new(muster_cloud::repo_store::RepoStore::new(
        config.repo_root.clone(),
    ));
    let state = AppState {
        config: Arc::new(config),
        db,
        oidc,
        repos,
    };
    println!("muster-cloud listening on http://{addr}");
    if let Err(err) = axum::serve(listener, muster_cloud::router(state)).await {
        eprintln!("muster-cloud: server error: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

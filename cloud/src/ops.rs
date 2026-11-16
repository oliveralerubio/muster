//! `muster-cloud-ops`: operator commands for the lifecycle authority
//! (`muster_cloud::control`). Shell scripts call this instead of reading or
//! editing control files themselves.
//!
//! ```text
//! muster-cloud-ops init                               pristine targets only
//! muster-cloud-ops status [--database-url URL]        reconcile (+ replay)
//! muster-cloud-ops register --database-url URL        new candidate incarnation
//! muster-cloud-ops activate --incarnation ID --database-url URL
//! muster-cloud-ops restore-check [--database-url URL] read-only restore gate
//!                     (URL from MUSTER_DATABASE_URL when the flag is absent)
//! ```
//!
//! Environment: `MUSTER_CONTROL_DIR`, `MUSTER_CONTROL_WITNESS`,
//! `MUSTER_REPO_ROOT` (disjointness and `init`), `MUSTER_DATABASE_URL`
//! (`init`), `MUSTER_ENV`. `MUSTER_DEV_CONTROL_CRASH=after_journal|
//! after_witness` injects a crash in `register`/`activate`, development
//! only (refused in production).
//!
//! Exit: 0 ok (JSON status on stdout), 64 usage, 65 authority refused,
//! 66 not pristine, 74 I/O, 78 configuration.

use std::process::ExitCode;

use muster_cloud::control::{Authority, ControlError, CrashPoint};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn config_error(message: String) -> ExitCode {
    eprintln!("muster-cloud-ops: configuration error: {message}");
    ExitCode::from(78)
}

fn usage(message: &str) -> ExitCode {
    eprintln!("muster-cloud-ops: {message}");
    ExitCode::from(64)
}

/// How a command may touch its database.
#[derive(Clone, Copy, PartialEq)]
enum DbAccess {
    /// Creates or binds: schema migrations are applied first.
    Migrate,
    /// Reads and projects (replay) an already bound database; no migrations.
    Existing,
    /// Restore gate: every session transaction is read-only, no migrations.
    ReadOnly,
}

async fn connect(url: &str, access: DbAccess) -> Result<sqlx::PgPool, ControlError> {
    use std::str::FromStr;
    let mut options = sqlx::postgres::PgConnectOptions::from_str(url)
        .map_err(|e| ControlError::Usage(format!("invalid database URL: {e}")))?;
    if access == DbAccess::ReadOnly {
        options = options.options([("default_transaction_read_only", "on")]);
    }
    let db = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect_with(options)
        .await
        .map_err(|e| ControlError::Io(format!("database connection: {e}")))?;
    if access == DbAccess::Migrate {
        sqlx::migrate!("./migrations")
            .run(&db)
            .await
            .map_err(|e| ControlError::Io(format!("database migration: {e}")))?;
    }
    Ok(db)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().cloned() else {
        return usage("expected a command: init|status|register|activate|restore-check");
    };
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };

    let production = !matches!(env("MUSTER_ENV").as_deref(), Some("development"));
    let crash = match env("MUSTER_DEV_CONTROL_CRASH") {
        None => None,
        Some(_) if production => {
            return config_error(
                "MUSTER_DEV_CONTROL_CRASH is invalid: a development-only setting".into(),
            )
        }
        Some(v) => match CrashPoint::parse(&v) {
            Some(p) => Some(p),
            None => {
                return config_error(format!(
                    "MUSTER_DEV_CONTROL_CRASH is invalid: unknown point {v:?}"
                ))
            }
        },
    };
    let Some(repo_root) = env("MUSTER_REPO_ROOT").map(std::path::PathBuf::from) else {
        return config_error("MUSTER_REPO_ROOT is required".into());
    };
    let paths = match muster_cloud::config::control_paths(&env, &repo_root) {
        Ok(p) => p,
        Err(e) => return config_error(e.to_string()),
    };

    let result: Result<serde_json::Value, ControlError> = async {
        let db_url = flag("--database-url");
        let status = match command.as_str() {
            "init" => {
                let url = env("MUSTER_DATABASE_URL")
                    .ok_or_else(|| ControlError::Usage("MUSTER_DATABASE_URL is required".into()))?;
                Authority::init(
                    &paths,
                    &connect(&url, DbAccess::Migrate).await?,
                    &repo_root,
                    crash,
                )
                .await?
            }
            "status" => {
                let authority = Authority::open(&paths)?;
                let db = match db_url {
                    Some(url) => Some(
                        authority
                            .bind_db(&connect(&url, DbAccess::Existing).await?, false)
                            .await?,
                    ),
                    None => None,
                };
                muster_cloud::control::Status {
                    db,
                    ..authority.status()
                }
            }
            "restore-check" => {
                // From the environment too, so scripts never put a database
                // password in argv.
                let url = db_url
                    .or_else(|| env("MUSTER_DATABASE_URL"))
                    .ok_or_else(|| {
                        ControlError::Usage(
                            "--database-url (or MUSTER_DATABASE_URL) is required".into(),
                        )
                    })?;
                let authority = Authority::open(&paths)?;
                let db = authority
                    .bind_db(&connect(&url, DbAccess::ReadOnly).await?, true)
                    .await?;
                muster_cloud::control::Status {
                    db: Some(db),
                    ..authority.status()
                }
            }
            "register" => {
                let url = db_url
                    .ok_or_else(|| ControlError::Usage("--database-url is required".into()))?;
                let db = connect(&url, DbAccess::Migrate).await?;
                let mut authority = Authority::open(&paths)?;
                let db_status = authority.register(&db, crash).await?;
                muster_cloud::control::Status {
                    db: Some(db_status),
                    ..authority.status()
                }
            }
            "activate" => {
                let url = db_url
                    .ok_or_else(|| ControlError::Usage("--database-url is required".into()))?;
                let id = flag("--incarnation")
                    .and_then(|s| uuid::Uuid::parse_str(&s).ok())
                    .ok_or_else(|| {
                        ControlError::Usage("--incarnation <uuid> is required".into())
                    })?;
                let db = connect(&url, DbAccess::Existing).await?;
                let mut authority = Authority::open(&paths)?;
                let db_status = authority.activate(id, &db, crash).await?;
                muster_cloud::control::Status {
                    db: Some(db_status),
                    ..authority.status()
                }
            }
            other => return Err(ControlError::Usage(format!("unknown command {other:?}"))),
        };
        serde_json::to_value(status).map_err(|e| ControlError::Io(e.to_string()))
    }
    .await;

    match result {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("muster-cloud-ops: {e}");
            ExitCode::from(e.exit_code())
        }
    }
}

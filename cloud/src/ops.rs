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
//! muster-cloud-ops purge-due [--database-url URL]     one purge pass (due deletions)
//! muster-cloud-ops lifecycle-report --workspace ID [--database-url URL]
//!                     read-only residue report: counts and presence, never data
//! muster-cloud-ops backup run [--plaintext-synthetic]
//!                     one snapshot-bound, age-encrypted, catalogued snapshot
//! muster-cloud-ops backup verify --snapshot DIR   structure + every hash
//! muster-cloud-ops retention run                  remove expired snapshots
//! ```
//!
//! Backups (`cloud/src/backup.rs`): `MUSTER_BACKUP_ROOT` (managed root),
//! `MUSTER_BACKUP_AGE_RECIPIENT` (required unless `--plaintext-synthetic`,
//! itself development only), `MUSTER_BACKUP_RETENTION_DAYS` (default 30);
//! development only: `MUSTER_DEV_BACKUP_RETENTION_SECS`,
//! `MUSTER_DEV_BACKUP_CRASH=after_snapshot|after_db|after_git|before_publish`,
//! `MUSTER_DEV_BACKUP_PAUSE_FILE`. Backup exits: 65 refused, 75 busy.
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

    if command == "backup" || command == "retention" {
        return backup_command(&args, &paths, repo_root, production).await;
    }
    if command == "purge-due" || command == "lifecycle-report" {
        return lifecycle_command(&command, flag, &paths, repo_root, production).await;
    }

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

/// `purge-due` and `lifecycle-report` (database URL from `--database-url`
/// or `MUSTER_DATABASE_URL`).
async fn lifecycle_command(
    command: &str,
    flag: impl Fn(&str) -> Option<String>,
    paths: &muster_cloud::control::ControlPaths,
    repo_root: std::path::PathBuf,
    production: bool,
) -> ExitCode {
    use muster_cloud::lifecycle_purge as purge;
    let Some(url) = flag("--database-url").or_else(|| env("MUSTER_DATABASE_URL")) else {
        return usage("--database-url (or MUSTER_DATABASE_URL) is required");
    };
    let fail = |e: String| {
        eprintln!("muster-cloud-ops: {e}");
        ExitCode::from(74)
    };
    let json = if command == "purge-due" {
        let db = match connect(&url, DbAccess::Existing).await {
            Ok(db) => db,
            Err(e) => return fail(e.to_string()),
        };
        // Reapply the journal first (a database restored from before a
        // request, or a crash after journaling): the purge reads the
        // projection.
        let bound = match Authority::open(paths) {
            Ok(authority) => authority.bind_db(&db, false).await.map(|_| ()),
            Err(e) => Err(e),
        };
        if let Err(e) = bound {
            eprintln!("muster-cloud-ops: {e}");
            return ExitCode::from(e.exit_code());
        }
        let ctx = purge::PurgeCtx {
            db,
            control: paths.clone(),
            repo_root,
            crash: None,
            drain_wait: std::time::Duration::from_secs(if production { 30 } else { 2 }),
        };
        serde_json::json!({ "workspaces": purge::run_due(&ctx).await })
    } else {
        let Some(ws) = flag("--workspace").and_then(|s| uuid::Uuid::parse_str(&s).ok()) else {
            return usage("--workspace <uuid> is required");
        };
        let db = match connect(&url, DbAccess::ReadOnly).await {
            Ok(db) => db,
            Err(e) => return fail(e.to_string()),
        };
        // Read-only: the journal is read, never projected, by the report.
        let journal = match Authority::open(paths) {
            Ok(authority) => purge::JournalState::of(&authority, ws),
            Err(e) => {
                eprintln!("muster-cloud-ops: {e}");
                return ExitCode::from(e.exit_code());
            }
        };
        match purge::report(&db, &repo_root, ws, journal).await {
            Ok(v) => v,
            Err(e) => return fail(e),
        }
    };
    println!("{json}");
    ExitCode::SUCCESS
}

/// `backup run|verify` and `retention run`.
async fn backup_command(
    args: &[String],
    paths: &muster_cloud::control::ControlPaths,
    repo_root: std::path::PathBuf,
    production: bool,
) -> ExitCode {
    use muster_cloud::backup::{self, BackupError, Encryption, Settings};
    let sub = format!(
        "{} {}",
        args[0],
        args.get(1).map(String::as_str).unwrap_or("")
    );
    let has = |name: &str| args.iter().any(|a| a == name);
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let fail = |e: BackupError| {
        eprintln!("muster-cloud-ops: {e}");
        ExitCode::from(e.exit_code())
    };
    if sub == "backup verify" {
        let Some(dir) = flag("--snapshot") else {
            return usage("--snapshot <dir> is required");
        };
        return match backup::verify(std::path::Path::new(&dir)) {
            Ok(v) => {
                println!("{v}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        };
    }
    if sub != "backup run" && sub != "retention run" {
        return usage("expected: backup run|verify, retention run");
    }
    let dev = |name: &str| -> Result<Option<String>, ExitCode> {
        match env(name) {
            Some(_) if production => Err(config_error(format!(
                "{name} is invalid: a development-only setting"
            ))),
            v => Ok(v),
        }
    };
    let crash = match dev("MUSTER_DEV_BACKUP_CRASH") {
        Err(code) => return code,
        Ok(None) => None,
        Ok(Some(v)) => match backup::BackupCrash::parse(&v) {
            Some(p) => Some(p),
            None => {
                return config_error(format!(
                    "MUSTER_DEV_BACKUP_CRASH is invalid: unknown point {v:?}"
                ))
            }
        },
    };
    let pause_file = match dev("MUSTER_DEV_BACKUP_PAUSE_FILE") {
        Err(code) => return code,
        Ok(v) => v.map(std::path::PathBuf::from),
    };
    let pause_at = match dev("MUSTER_DEV_BACKUP_PAUSE_AT") {
        Err(code) => return code,
        Ok(None) => backup::PausePoint::AfterSnapshot,
        Ok(Some(v)) => match backup::PausePoint::parse(&v) {
            Some(p) => p,
            None => {
                return config_error(format!(
                    "MUSTER_DEV_BACKUP_PAUSE_AT is invalid: unknown point {v:?}"
                ))
            }
        },
    };
    let dev_retention = match dev("MUSTER_DEV_BACKUP_RETENTION_SECS") {
        Err(code) => return code,
        Ok(None) => None,
        Ok(Some(v)) => match v.parse::<i64>() {
            Ok(n) if n > 0 => Some(time::Duration::seconds(n)),
            _ => {
                return config_error(
                    "MUSTER_DEV_BACKUP_RETENTION_SECS is invalid: a positive integer".into(),
                )
            }
        },
    };
    let retention = match (dev_retention, env("MUSTER_BACKUP_RETENTION_DAYS")) {
        (Some(d), _) => d,
        (None, None) => time::Duration::days(30),
        (None, Some(v)) => match v.parse::<i64>() {
            Ok(n) if (1..=3650).contains(&n) => time::Duration::days(n),
            _ => {
                return config_error(
                    "MUSTER_BACKUP_RETENTION_DAYS is invalid: an integer from 1 to 3650".into(),
                )
            }
        },
    };
    // Encryption first: a refusal must never depend on anything else.
    let enc = if sub == "backup run" {
        if has("--plaintext-synthetic") {
            if production {
                return fail(BackupError::Refused(
                    "plaintext backups are for synthetic data in development only".into(),
                ));
            }
            eprintln!("muster-cloud-ops: WARNING: unencrypted backup (synthetic data only)");
            Some(Encryption::PlaintextSynthetic)
        } else {
            match env("MUSTER_BACKUP_AGE_RECIPIENT") {
                Some(recipient) => Some(Encryption::Age { recipient }),
                None => {
                    return fail(BackupError::Refused(
                        "MUSTER_BACKUP_AGE_RECIPIENT is required: backups are encrypted".into(),
                    ))
                }
            }
        }
    } else {
        None
    };
    let root = match muster_cloud::config::backup_root(&env, &repo_root, paths) {
        Ok(r) => r,
        Err(e) => return config_error(e.to_string()),
    };
    let Some(url) = flag("--database-url").or_else(|| env("MUSTER_DATABASE_URL")) else {
        return usage("--database-url (or MUSTER_DATABASE_URL) is required");
    };
    let db = match connect(&url, DbAccess::Existing).await {
        Ok(db) => db,
        Err(e) => {
            eprintln!("muster-cloud-ops: {e}");
            return ExitCode::from(e.exit_code());
        }
    };
    let settings = Settings {
        root,
        repo_root,
        database_url: url,
        retention,
        overdue_grace: dev_retention.unwrap_or(time::Duration::hours(24)),
        crash,
        pause_file,
        pause_at,
        capture_wait: std::time::Duration::from_secs(if production { 600 } else { 30 }),
    };
    // Both read the current requests: reapply the journal first (a crash
    // between journal and projection, or a restored database), which also
    // checks the authority and the database's incarnation.
    let bound = match Authority::open(paths) {
        Ok(authority) => authority.bind_db(&db, false).await.map(|_| ()),
        Err(e) => Err(e),
    };
    if let Err(e) = bound {
        eprintln!("muster-cloud-ops: {e}");
        return ExitCode::from(e.exit_code());
    }
    let result = match enc {
        Some(enc) => {
            // No backup may hold a workspace past its cap: purge what is
            // overdue first; the capture refuses (65) only if one remains.
            let ctx = muster_cloud::lifecycle_purge::PurgeCtx {
                db: db.clone(),
                control: paths.clone(),
                repo_root: settings.repo_root.clone(),
                crash: None,
                drain_wait: std::time::Duration::from_secs(if production { 30 } else { 2 }),
            };
            for o in muster_cloud::lifecycle_purge::run_due_past_cap(&ctx).await {
                if let Some(why) = &o.last_error {
                    eprintln!(
                        "muster-cloud-ops: purge of {} not complete: {why}",
                        o.workspace_id
                    );
                }
            }
            backup::run_backup(&settings, &enc, &db).await
        }
        None => backup::run_retention(&settings, &db).await,
    };
    match result {
        Ok(v) => {
            println!("{v}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

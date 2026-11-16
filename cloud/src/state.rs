use std::sync::Arc;

use sqlx::PgPool;

use crate::auth::Oidc;
use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: PgPool,
    pub oidc: Arc<Oidc>,
    pub repos: Arc<crate::repo_store::RepoStore>,
}

impl AppState {
    /// What the purge worker and the Git access guard need.
    pub fn purge_ctx(&self) -> crate::lifecycle_purge::PurgeCtx {
        crate::lifecycle_purge::PurgeCtx {
            db: self.db.clone(),
            control: self.config.control.clone(),
            repo_root: self.config.repo_root.clone(),
            crash: self.config.dev_purge_crash,
            drain_wait: if self.config.env.is_production() {
                std::time::Duration::from_secs(30)
            } else {
                std::time::Duration::from_secs(2)
            },
        }
    }

    /// Shared hold on `ws`'s Git storage; 404 once the workspace is
    /// tombstoned. Take it before any database row lock.
    pub async fn repo_access(
        &self,
        ws: uuid::Uuid,
    ) -> Result<crate::lifecycle_purge::RepoAccess, crate::error::AppError> {
        crate::lifecycle_purge::repo_access(&self.db, &self.config.repo_root, ws).await
    }
}

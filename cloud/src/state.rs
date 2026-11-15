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

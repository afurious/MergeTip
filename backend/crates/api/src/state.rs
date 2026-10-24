use std::{fmt, sync::Arc};

use mergetip_core::Clock;
use mergetip_crypto::Keyring;
use mergetip_store::PgPool;

use crate::ApiConfig;

/// Dependencies shared by every handler, behind one `Arc` so cloning per request is cheap.
#[derive(Clone)]
pub struct AppState(Arc<Inner>);

struct Inner {
    pool: PgPool,
    config: Arc<ApiConfig>,
    clock: Arc<dyn Clock>,
}

impl AppState {
    pub fn new(pool: PgPool, config: Arc<ApiConfig>, clock: Arc<dyn Clock>) -> Self {
        Self(Arc::new(Inner {
            pool,
            config,
            clock,
        }))
    }

    pub fn pool(&self) -> &PgPool {
        &self.0.pool
    }

    pub fn config(&self) -> &ApiConfig {
        &self.0.config
    }

    pub fn keyring(&self) -> &Keyring {
        &self.0.config.keyring
    }

    pub fn clock(&self) -> &dyn Clock {
        self.0.clock.as_ref()
    }
}

impl fmt::Debug for AppState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppState")
            .field("config", &self.0.config)
            .finish_non_exhaustive()
    }
}

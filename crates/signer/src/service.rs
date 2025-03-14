use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json,
};
use axum_extra::TypedHeader;
use bimap::BiHashMap;
use cb_common::{
    commit::{
        constants::{
            GENERATE_PROXY_KEY_PATH, GET_PUBKEYS_PATH, RELOAD_PATH, REQUEST_SIGNATURE_PATH,
            STATUS_PATH,
        },
        request::{
            EncryptionScheme, GenerateProxyRequest, GetPubkeysResponse, SignConsensusRequest,
            SignProxyRequest, SignRequest,
        },
    },
    config::StartSignerConfig,
    constants::{COMMIT_BOOST_COMMIT, COMMIT_BOOST_VERSION},
    types::{Chain, Jwt, ModuleId},
};
use cb_metrics::provider::MetricsProvider;
use eyre::Context;
use headers::{authorization::Bearer, Authorization};
use tokio::{net::TcpListener, sync::RwLock};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{
    error::SignerModuleError,
    manager::SigningManager,
    metrics::{uri_to_tag, SIGNER_METRICS_REGISTRY, SIGNER_STATUS},
};

/// Implements the Signer API and provides a service for signing requests
pub struct SigningService;

#[derive(Clone)]
struct SigningState {
    /// Manager handling different signing methods
    manager: Arc<RwLock<SigningManager>>,
    /// Map of JWTs to module ids. This also acts as registry of all modules
    /// running
    jwts: Arc<BiHashMap<ModuleId, Jwt>>,
}

impl SigningService {
    pub async fn run(config: StartSignerConfig) -> eyre::Result<()> {
        if config.jwts.is_empty() {
            warn!("Signing service was started but no module is registered. Exiting");
            return Ok(());
        }

        let manager = start_manager(&config)
            .map_err(|err| eyre::eyre!("failed to start signing manager {err}"))?;

        let module_ids: Vec<String> = config.jwts.left_values().cloned().map(Into::into).collect();

        let loaded_consensus = manager.consensus_pubkeys().len();
        let proxies = manager.proxies();
        let loaded_proxies = proxies.bls_signers.len() + proxies.ecdsa_signers.len();

        info!(version = COMMIT_BOOST_VERSION, commit = COMMIT_BOOST_COMMIT, modules =? module_ids, port =? config.server_port, loaded_consensus, loaded_proxies, "Starting signing service");

        let state = SigningState { manager: RwLock::new(manager).into(), jwts: config.jwts.into() };
        SigningService::init_metrics(config.chain)?;

        let app = axum::Router::new()
            .route(REQUEST_SIGNATURE_PATH, post(handle_request_signature))
            .route(GET_PUBKEYS_PATH, get(handle_get_pubkeys))
            .route(GENERATE_PROXY_KEY_PATH, post(handle_generate_proxy))
            .route_layer(middleware::from_fn_with_state(state.clone(), jwt_auth))
            .route(RELOAD_PATH, post(handle_reload))
            .with_state(state.clone())
            .route_layer(middleware::from_fn(log_request));
        let status_router = axum::Router::new().route(STATUS_PATH, get(handle_status));

        let address = SocketAddr::from(([0, 0, 0, 0], config.server_port));
        let listener = TcpListener::bind(address).await?;

        axum::serve(listener, axum::Router::new().merge(app).merge(status_router))
            .await
            .wrap_err("signer server exited")
    }

    fn init_metrics(network: Chain) -> eyre::Result<()> {
        MetricsProvider::load_and_run(network, SIGNER_METRICS_REGISTRY.clone())
    }
}

/// Authentication middleware layer
async fn jwt_auth(
    State(state): State<SigningState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    mut req: Request,
    next: Next,
) -> Result<Response, SignerModuleError> {
    let jwt: Jwt = auth.token().to_string().into();

    let module_id = state.jwts.get_by_right(&jwt).ok_or_else(|| {
        error!("Unauthorized request. Was the module started correctly?");
        SignerModuleError::Unauthorized
    })?;

    req.extensions_mut().insert(module_id.clone());

    Ok(next.run(req).await)
}

/// Requests logging middleware layer
async fn log_request(req: Request, next: Next) -> Result<Response, SignerModuleError> {
    let url = &req.uri().clone();
    let response = next.run(req).await;
    SIGNER_STATUS.with_label_values(&[response.status().as_str(), uri_to_tag(url)]).inc();
    Ok(response)
}

/// Status endpoint for the Signer API
async fn handle_status() -> Result<impl IntoResponse, SignerModuleError> {
    Ok((StatusCode::OK, "OK"))
}

/// Implements get_pubkeys from the Signer API
async fn handle_get_pubkeys(
    Extension(module_id): Extension<ModuleId>,
    State(state): State<SigningState>,
) -> Result<impl IntoResponse, SignerModuleError> {
    let req_id = Uuid::new_v4();

    debug!(event = "get_pubkeys", ?req_id, "New request");

    let signing_manager = state.manager.read().await;
    let map = signing_manager
        .get_consensus_proxy_maps(&module_id)
        .map_err(|err| SignerModuleError::Internal(err.to_string()))?;

    let res = GetPubkeysResponse { keys: map };

    Ok((StatusCode::OK, Json(res)).into_response())
}

/// Implements request_signature from the Signer API
async fn handle_request_signature(
    Extension(module_id): Extension<ModuleId>,
    State(state): State<SigningState>,
    Json(request): Json<SignRequest>,
) -> Result<impl IntoResponse, SignerModuleError> {
    let req_id = Uuid::new_v4();

    debug!(event = "request_signature", ?module_id, ?req_id, "New request");

    let signing_manager = state.manager.read().await;

    let signature_response = match request {
        SignRequest::Consensus(SignConsensusRequest { pubkey, object_root }) => signing_manager
            .sign_consensus(&pubkey, &object_root)
            .await
            .map(|sig| Json(sig).into_response()),
        SignRequest::ProxyBls(SignProxyRequest { pubkey: bls_pk, object_root }) => {
            if !signing_manager.has_proxy_bls_for_module(&bls_pk, &module_id) {
                return Err(SignerModuleError::UnknownProxySigner(bls_pk.to_vec()));
            }

            signing_manager
                .sign_proxy_bls(&bls_pk, &object_root)
                .await
                .map(|sig| Json(sig).into_response())
        }
        SignRequest::ProxyEcdsa(SignProxyRequest { pubkey: ecdsa_pk, object_root }) => {
            if !signing_manager.has_proxy_ecdsa_for_module(&ecdsa_pk, &module_id) {
                return Err(SignerModuleError::UnknownProxySigner(ecdsa_pk.to_vec()));
            }

            signing_manager
                .sign_proxy_ecdsa(&ecdsa_pk, &object_root)
                .await
                .map(|sig| Json(sig).into_response())
        }
    }?;

    Ok(signature_response)
}

async fn handle_generate_proxy(
    Extension(module_id): Extension<ModuleId>,
    State(state): State<SigningState>,
    Json(request): Json<GenerateProxyRequest>,
) -> Result<impl IntoResponse, SignerModuleError> {
    let req_id = Uuid::new_v4();

    debug!(event = "generate_proxy", module_id=?module_id, ?req_id, "New request");

    let mut signing_manager = state.manager.write().await;

    let response = match request.scheme {
        EncryptionScheme::Bls => {
            let proxy_delegation =
                signing_manager.create_proxy_bls(module_id, request.consensus_pubkey).await?;
            Json(proxy_delegation).into_response()
        }
        EncryptionScheme::Ecdsa => {
            let proxy_delegation =
                signing_manager.create_proxy_ecdsa(module_id, request.consensus_pubkey).await?;
            Json(proxy_delegation).into_response()
        }
    };

    Ok(response)
}

async fn handle_reload(
    State(state): State<SigningState>,
) -> Result<impl IntoResponse, SignerModuleError> {
    let req_id = Uuid::new_v4();

    debug!(event = "reload", ?req_id, "New request");

    let config = match StartSignerConfig::load_from_env() {
        Ok(config) => config,
        Err(err) => {
            error!(event = "reload", ?req_id, error = ?err, "Failed to reload config");
            return Err(SignerModuleError::Internal("failed to reload config".to_string()));
        }
    };

    let new_manager = match start_manager(&config) {
        Ok(manager) => manager,
        Err(err) => {
            error!(event = "reload", ?req_id, error = ?err, "Failed to reload manager");
            return Err(SignerModuleError::Internal("failed to reload config".to_string()));
        }
    };

    *state.manager.write().await = new_manager;

    Ok((StatusCode::OK, "OK"))
}

fn start_manager(config: &StartSignerConfig) -> eyre::Result<SigningManager> {
    let proxy_store = if let Some(store) = config.store.clone() {
        Some(store.init_from_env()?)
    } else {
        warn!("Proxy store not configured. Proxies keys and delegations will not be persisted");
        None
    };

    let mut manager = SigningManager::new(config.chain, proxy_store)?;

    for signer in config.loader.clone().load_keys()? {
        manager.add_consensus_signer(signer);
    }

    Ok(manager)
}

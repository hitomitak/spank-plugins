//
// (C) Copyright IBM 2025
//
// This code is licensed under the Apache License, Version 2.0. You may
// obtain a copy of this license in the LICENSE.txt file in the root directory
// of this source tree or at http://www.apache.org/licenses/LICENSE-2.0.
//
// Any modifications or derivative works of this code must retain this
// copyright notice, and modified files need to carry a notice indicating
// that they have been altered from the originals.
#![allow(dead_code)]


use axum::{
    body::Body,
    extract::{Request, State},
    http::uri::Uri,
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use http::header::{HeaderValue, AUTHORIZATION, HOST};
use hyper::StatusCode;
use hyper_util::{client::legacy::connect::HttpConnector, rt::TokioExecutor};

use std::sync::Arc;
use tokio::sync::Mutex;

use hyper_tls::HttpsConnector;
use url::Url;

use crate::proxy::auth::TokenManager;
use crate::proxy::models::proxy_runtime_config::ProxyRuntimeConfig;
type Client = hyper_util::client::legacy::Client<HttpsConnector<HttpConnector>, Body>;
use tracing::info;

#[derive(Clone)]
pub(crate) struct ProxyState {
    pub(crate) client: Client,
    pub(crate) token_manager: Arc<Mutex<TokenManager>>,
    pub(crate) proxy_pass: String,
    pub(crate) service_crn: String,
    pub(crate) host: String,
}

pub async fn start_reverse_proxy(cfg: ProxyRuntimeConfig) -> Result<(), Box<dyn std::error::Error>> {
    info!("Proxy: start reverse Proxy");

    let token_url = format!("{}/identity/token", cfg.iam_endpoint);
    info!("Proxy : token url {}", token_url);

    let dest_url = Url::parse(&cfg.proxy_pass)?;
    let port: u16 = if dest_url.scheme() == "https" {
    dest_url.port_or_known_default().unwrap_or(443)
    } else {
        dest_url.port_or_known_default().unwrap_or(80)
    };
    let host_port = format!("{}:{}", dest_url.host_str().unwrap_or(""), port);
    info!("Proxy : dest url {}", dest_url);

    let state = ProxyState {
        client: hyper_util::client::legacy::Client::<(), ()>::builder(TokioExecutor::new())
            .build(HttpsConnector::new()),
        token_manager: Arc::new(Mutex::new(TokenManager::new(
            token_url,
            &cfg.iam_apikey,
        ))),
        proxy_pass: cfg.proxy_pass.clone(),
        service_crn: cfg.service_crn.clone(),
        host: host_port,
    };

    let router = if cfg.paths.is_empty() {
        Router::new().route("/*path", any(handler))
    } else {
        let mut r = Router::new();
        for p in cfg.paths.iter() {
            r = r.route(p.as_str(), any(handler));
        }
        r
    }.with_state(state);

    let bind = format!("{}:{}", cfg.bind_host, cfg.bind_port);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, router).await?;
    Ok(())
}

async fn handler(
    State(state): State<ProxyState>,
    mut req: Request<Body>,
) -> Result<Response, StatusCode> {
    let path = req.uri().path();
    let path_query = req
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or(path);

    let uri = format!("{}{}", state.proxy_pass, path_query);
    let mut token_manager = state.token_manager.lock().await;

    let token = match token_manager.get_token().await {
        Ok(val) => val,
        Err(err) => {
            info!("Proxy : Failed to get access token: {:#?}", err);
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    *req.uri_mut() = Uri::try_from(uri).unwrap();
    req.headers_mut().insert(
        "Service-CRN",
        HeaderValue::from_str(&state.service_crn).unwrap(),
    );
    req.headers_mut()
        .insert(HOST, HeaderValue::from_str(&state.host).unwrap());
    req.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
    );

    match state.client.request(req).await {
        Ok(v) => Ok(v.into_response()),
        Err(err) => {
            info!("Proxy : Request failed: {:#?}", err);
            Err(StatusCode::BAD_REQUEST)
        }
    }
}

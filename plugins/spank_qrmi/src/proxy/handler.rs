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
use std::fs::File;

use std::sync::Arc;
use tokio::sync::Mutex;

use hyper_tls::HttpsConnector;
use url::Url;

use crate::proxy::auth::TokenManager;
use crate::proxy::models::proxy_config::{Config, ProxyConfig};

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

pub async fn start_reverse_proxy(config_path: String, proxy_name: String) -> Result<(), Box<dyn std::error::Error>> {
    info!("input start reverse Proxy");
    info!("proxy: file open");
    let file = File::open(config_path).expect("file should open read only");

    info!("proxy: file open 2");
    let config_file =
        serde_json::from_reader::<File, ProxyConfig>(file).expect("file should be proper JSON");

    info!("proxy: file open 3");
    let mut config: Option<Config> = None;
    for proxy_item in config_file.proxies {
        if proxy_item.name == proxy_name {
            config = Some(proxy_item);
            break;
        }
    }

    if config.is_none() {
        info!("proxy : Config for proxy name: {} not found", proxy_name);
        return Ok(());
    }

    let proxy_config = config.unwrap();

    let token_url = format!("{}/identity/token", &proxy_config.iam_endpoint);

    info!("proxy : token url {}", token_url);
    let dest_url = Url::parse(&proxy_config.proxy_pass).unwrap();
    let port: u16 = if dest_url.scheme() == "https" {
        dest_url.port_or_known_default().unwrap_or(443)
    } else {
        dest_url.port_or_known_default().unwrap_or(80)
    };
    let host_port = format!("{}:{}", dest_url.host_str().unwrap_or(""), port);

    info!("proxy: call ProxyState");
    let state = ProxyState {
        client: hyper_util::client::legacy::Client::<(), ()>::builder(TokioExecutor::new())
            .build(HttpsConnector::new()),
        token_manager: Arc::new(Mutex::new(TokenManager::new(
            token_url,
            &proxy_config.iam_apikey,
        ))),
        proxy_pass: proxy_config.proxy_pass,
        service_crn: proxy_config.service_crn,
        host: host_port,
    };

    info!("proxy: create Router");
    let mut router = Router::new();
    for path in proxy_config.paths {
        info!("proxy : Adding path: {}", path);
        router = router.route(&path, any(handler));
    }
    let routes = router.with_state(state);

    info!("proxy: create bind");
    let listener = tokio::net::TcpListener::bind(format!(
        "{}:{}",
        proxy_config.bind_host, proxy_config.bind_port
    ))
    .await
    .unwrap();
    info!("proxy : listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, routes).await.unwrap();
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
            info!("proxy : Failed to get access token: {:#?}", err);
            return Err(StatusCode::BAD_REQUEST);
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
            info!("proxy : Request failed: {:#?}", err);
            Err(StatusCode::BAD_REQUEST)
        }
    }
    //Ok(state
    //    .client
    //    .request(req)
    //    .await
    //    .map_err(|_| StatusCode::BAD_REQUEST)?
    //    .into_response())
}

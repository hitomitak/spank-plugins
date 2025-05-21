// This code is part of Qiskit.
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

use serde::Deserialize;

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Config {
    pub(crate) name: String,
    pub(crate) auth_type: String,
    pub(crate) bind_host: String,
    pub(crate) bind_port: u16,
    pub(crate) iam_endpoint: String,
    pub(crate) iam_apikey: String,
    pub(crate) service_crn: String,
    pub(crate) paths: Vec<String>,
    pub(crate) proxy_pass: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ProxyConfig {
    pub(crate) proxies: Vec<Config>,
}

// This code is part of Qiskit.
//
// (C) Copyright IBM 2025
//
// This program and the accompanying materials are made available under the
// terms of the GNU General Public License version 3, as published by the
// Free Software Foundation.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <[https://www.gnu.org/licenses/gpl-3.0.txt]
//
use eyre::{eyre, WrapErr};
use slurm_spank::{Context, Plugin, SpankHandle, SpankOption, SLURM_VERSION_NUMBER, SPANK_PLUGIN};
use tracing::{debug, error, info};

use std::error::Error;
use std::process;

use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::io::BufReader;
use std::thread;

use once_cell::sync::OnceCell;
use tokio::runtime::Runtime;

mod models;
use self::models::{QRMIResource, QRMIResources, ResourceType};

use crate::proxy::handler::start_reverse_proxy;
use crate::proxy::models::proxy_runtime_config::ProxyRuntimeConfig;
use qrmi::ibm::{IBMDirectAccess, IBMQiskitRuntimeService};
use qrmi::pasqal::PasqalCloud;
use qrmi::QuantumResource;

pub mod proxy;
const SLURM_BATCH_SCRIPT: u32 = 0xfffffffb;

const DENY_KEYS: &[&str] = &["QRMI_IBM_DA_IAM_APIKEY", "QRMI_IBM_DA_SERVICE_CRN"];

// spank_qrmi plugin
//
// All spank plugins must define this macro for the Slurm plugin loader.
SPANK_PLUGIN!(b"spank_qrmi", SLURM_VERSION_NUMBER, SpankQrmi);

/// Resource metadata
struct Resource {
    /// QPU name
    name: String,
    /// Resource type
    r#type: ResourceType,
    /// acquisition token which is obtained by QRMI.acquire()
    token: String,
}

#[derive(Default)]
struct SpankQrmi {
    /// A list of available QPU resources
    resources: Vec<Resource>,
    runtime: OnceCell<Runtime>,
}
impl SpankQrmi {
    fn get_runtime(&self) -> &Runtime {
        self.runtime.get_or_init(|| {
            Runtime::new().expect("Failed to create runtime")
        })
    }
}

/// Log entering function
macro_rules! enter {
    () => {
        debug!("PID = {}, UID = {}", process::id(), unsafe {
            libc::getuid()
        });
    };
}

/// Dump Spank context
macro_rules! dump_context {
    ($spank:expr) => {
        if let Ok(result) = $spank.job_id() {
            debug!("S_JOB_ID = {}", result);
        } else {
            debug!("S_JOB_ID =");
        }
        if let Ok(result) = $spank.job_stepid() {
            debug!("S_JOB_STEPID = {:x}", result);
        } else {
            debug!("S_JOB_STEPID =");
        }
        debug!("S_JOB_ARGV = {:#?}", $spank.job_argv().unwrap_or(vec!()));
        debug!(
            "S_PLUGIN_ARGV = {:#?}",
            $spank.plugin_argv().unwrap_or(vec!())
        );
    };
}

unsafe impl Plugin for SpankQrmi {
    /// slurm_spank_init
    ///
    /// Called just after plugins are loaded.
    ///
    /// In remote context, this is just after job step is initialized. This
    /// function is called before any plugin option processing.
    ///
    /// This plugin registers '--qpu=names' option to allow users to specify
    /// quantum resources to be used in the job.
    fn init(&mut self, spank: &mut SpankHandle) -> Result<(), Box<dyn Error>> {
        enter!();
        if spank.context()? == Context::Remote {
            dump_context!(spank);
        }
        // Register the --qpu=names option
        match spank.context()? {
            Context::Local | Context::Remote | Context::Allocator => {
                spank
                    .register_option(
                        SpankOption::new("qpu")
                            .takes_value("names")
                            .usage("Comma separated list of QPU resources to use."),
                    )
                    .wrap_err("Failed to register --qpu=names option")?;
            }
            _ => {}
        }
        Ok(())
    }

    /// slurm_spank_init_post_opt
    ///
    /// Called at the same point as slurm_spank_init, but after all user options
    /// to the plugin have been processed.
    ///
    /// The reason that the init and init_post_opt callbacks are separated is so
    /// that plugins can process system-wide options specified in plugstack.conf
    /// in the init callback, then process user options, and finally take some
    /// action in slurm_spank_init_post_opt if necessary. In the case of a
    /// heterogeneous job, slurm_spank_init is invoked once per job component.
    ///
    /// This plugin invokes QRMI.acquire() to obtain access to Quantum resource, and
    /// store the returned acquisition tokens to memory.
    fn init_post_opt(&mut self, spank: &mut SpankHandle) -> Result<(), Box<dyn Error>> {
        // Check if the option was set
        enter!();
        if spank.context()? == Context::Remote {
            dump_context!(spank);
        } else {
            // skip if context != remote
            return Ok(());
        }

        if let Ok(step_id) = spank.job_stepid() {
            // skip if this is slurm task steps
            if step_id != SLURM_BATCH_SCRIPT {
                return Ok(());
            }
        }

        let qpu_option = spank
            .get_option_value("qpu")
            .wrap_err("Failed to read --qpu=names option")?
            .map(|s| s.to_string());

        let binding = match qpu_option {
            Some(v) => v,
            None => {
                // do nothing if not qpu job
                return Ok(());
            }
        };

        // initializes job environment variables in case an error is returned within this function.
        spank.setenv("SLURM_JOB_QPU_RESOURCES", "", true)?;
        spank.setenv("SLURM_JOB_QPU_TYPES", "", true)?;

        // converts comma separated string to string array
        let qpu_names: Vec<String> = binding
            .split(',')
            .map(|l| l.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        info!("qpu names = {:#?}", qpu_names);

        // tries to open qrmi_config.json
        let plugin_argv = spank.plugin_argv().unwrap_or_default();
        if plugin_argv.len() != 1 {
            return Ok(());
        }
        let f = match File::open(plugin_argv[0]) {
            Ok(v) => v,
            Err(err) => {
                return Err(eyre!(
                    "Failed to open {}. reason = {}",
                    plugin_argv[0],
                    err.to_string()
                )
                .into());
            }
        };

        // reads qrmi_config.json and parse it. 
        let mut buf_reader = BufReader::new(f);
        let mut config_json_str = String::new();
        buf_reader.read_to_string(&mut config_json_str)?;
        // returns Err if fails to parse a file - invalid JSON, invalid resource type etc.
        let config = serde_json::from_str::<QRMIResources>(&config_json_str)?;

        let mut config_map: HashMap<String, QRMIResource> = HashMap::new();
        for qrmi in config.resources {
            config_map.insert(qrmi.name.clone(), qrmi);
        }

        // list of QPU names & types that have successfully called QRMI.acquire().
        let mut avail_names: String = Default::default();
        let mut avail_types: String = Default::default();

        for qpu_name in &qpu_names {
            if let Some(qrmi) = config_map.get(qpu_name.as_str()) {
                info!(
                    "qpu = {}, type = {:#?} env = {:#?}",
                    qpu_name, qrmi.r#type, qrmi.environment
                );

                let proxy_cfg_opt = build_proxy_runtime_config(qrmi);

                if let Some(cfg) = proxy_cfg_opt.clone() {
                    let cfg_clone = cfg.clone();
                    let qpu_name_clone = qpu_name.clone();
                    info!("call proxy server [{}]", qpu_name_clone);
                    thread::spawn(move || {
                        let rt = tokio::runtime::Builder::new_multi_thread()
                            .enable_all()
                            .build()
                            .unwrap();
                            rt.block_on(async move {
                                if let Err(e) = start_reverse_proxy(cfg_clone).await {
                                    eprintln!("proxy[{}] error: {:?}", qpu_name_clone, e);
                                }
                            });
                        });
                }
                // If user specifies access details in environment variables,
                // these are available as job environment variables. Reads through them and
                // set user-specified {qpu_name}_QRMI_xxx env vars to this slurm daemon process
                // for subsequent QRMI.acquire/release call.
                if let Ok(result) = spank.job_env() {
                    for env in result {
                        if let Some((key, value)) = env.split_once("=") {
                            if key.starts_with(&format!("{qpu_name}_QRMI_")) {
                                debug!("set user-specified envvar: {} = {}", key, value);
                                env::set_var(key, value);
                            }
                        }
                    }
                }

                let restrict = proxy_cfg_opt.is_some();

                // Next, set environment variables specified in config file.
                for (key, value) in &qrmi.environment {
                    // set to job's envronment - overrides == false
                    if restrict && DENY_KEYS.iter().any(|k| key == *k) { 
                        continue; 
                    }
                    if spank.setenv(format!("{qpu_name}_{key}"), value, false).is_ok() {
                        // set to the current process for subsequent QRMI.acquire() call
                        env::set_var(format!("{qpu_name}_{key}"), value);
                    }
                }

                let mut instance: Box<dyn QuantumResource> = match qrmi.r#type {
                    ResourceType::IBMDirectAccess => Box::new(IBMDirectAccess::new(qpu_name)),
                    ResourceType::QiskitRuntimeService => {
                        Box::new(IBMQiskitRuntimeService::new(qpu_name))
                    }
                    ResourceType::PasqalCloud => Box::new(PasqalCloud::new(qpu_name)),
                };

                let result = self.get_runtime().block_on(async {
                    instance.acquire().await
                });
                let token: Option<String> = match result {
                    Ok(v) => Some(v),
                    Err(err) => {
                        error!(
                            "Failed to acquire quantum resource: {}/{:#?}, reason: {}",
                            qpu_name,
                            qrmi.r#type,
                            err.to_string()
                        );
                        None
                    }
                };
                if let Some(acquisition_token) = token {
                    debug!("acquisition token = {}", acquisition_token);
                    match qrmi.r#type {
                        // TODO: Use unified environment variable name
                        ResourceType::IBMDirectAccess => {
                            spank.setenv(
                                format!("{qpu_name}_QRMI_IBM_DA_SESSION_ID"),
                                &acquisition_token,
                                true,
                            )?;
                        }
                        ResourceType::QiskitRuntimeService => {
                            spank.setenv(
                                format!("{qpu_name}_QRMI_IBM_QRS_SESSION_ID"),
                                &acquisition_token,
                                true,
                            )?;
                        }
                        _ => {}
                    }

                    self.resources.push(Resource {
                        name: qpu_name.to_string(),
                        r#type: qrmi.r#type.clone(),
                        token: acquisition_token,
                    });

                    // re-creates comma separated values
                    if !avail_names.is_empty() {
                        avail_names += ",";
                        avail_types += ",";
                    }
                    avail_names += qpu_name;
                    avail_types += qrmi.r#type.as_str();
                }
            }
        }
        spank.setenv("SLURM_JOB_QPU_RESOURCES", avail_names, true)?;
        spank.setenv("SLURM_JOB_QPU_TYPES", avail_types, true)?;
        Ok(())
    }

    /// slurm_spank_exit
    ///
    /// Called once just before slurmstepd exits in remote context. In local
    /// context, called before srun exits.
    ///
    /// This plugin invokes QRMI.release() to release Quantum resource.
    fn exit(&mut self, spank: &mut SpankHandle) -> Result<(), Box<dyn Error>> {
        enter!();
        if spank.context()? == Context::Remote {
            dump_context!(spank);

            for res in self.resources.iter() {
                debug!("releasing {}, {:#?}, {}", res.name, res.r#type, res.token);
                let mut instance: Box<dyn QuantumResource> = match res.r#type {
                    ResourceType::IBMDirectAccess => Box::new(IBMDirectAccess::new(&res.name)),
                    ResourceType::QiskitRuntimeService => {
                        Box::new(IBMQiskitRuntimeService::new(&res.name))
                    }
                    ResourceType::PasqalCloud => Box::new(PasqalCloud::new(&res.name)),
                };

                let result = self.get_runtime().block_on(async {
                    instance.release(&res.token).await
                });
                match result {
                    Ok(()) => (),
                    Err(err) => {
                        error!(
                            "Failed to release quantum resource: {}/{}. reason = {}",
                            res.name,
                            res.r#type.as_str(),
                            err.to_string()
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Called for each task just before execve (2).
    ///
    /// If you are restricting memory with cgroups, memory allocated here will be
    /// in the job's cgroup. (remote context only)
    fn task_init(&mut self, spank: &mut SpankHandle) -> Result<(), Box<dyn Error>> {
        enter!();
        dump_context!(spank);
        if let Ok(result) = spank.job_env() {
            // dump job environment variables for development
            info!("{:#?}", result);
        }
        Ok(())
    }
}

fn build_proxy_runtime_config(qrmi: &QRMIResource) -> Option<ProxyRuntimeConfig> {
    let env = &qrmi.environment;

    let enabled = env
        .get("QRMI_REVERSE_PROXY_ENABLE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(true);
    if !enabled {
        return None;
    }

    let upstream    = env.get("QRMI_REVERSE_PROXY_PASS")?;
    let iam_ep      = env.get("QRMI_IBM_DA_IAM_ENDPOINT")?;
    let iam_key     = env.get("QRMI_IBM_DA_IAM_APIKEY")?;
    let service_crn = env.get("QRMI_IBM_DA_SERVICE_CRN")?;

    let bind_host = env
        .get("QRMI_REVERSE_PROXY_BIND_HOST")
        .cloned()
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let bind_port: u16 = env
        .get("QRMI_REVERSE_PROXY_BIND_PORT")
        .and_then(|s| s.trim().parse::<u16>().ok())?;

    let paths: Vec<String> = env
        .get("QRMI_REVERSE_PROXY_PATHS")
        .map(|csv| {
            csv.split(',')
               .map(|s| s.trim().to_string())
               .filter(|s| !s.is_empty())
               .collect()
        })
        .unwrap_or_else(|| vec!["/*path".to_string()]);

    Some(ProxyRuntimeConfig {
        bind_host,
        bind_port,
        proxy_pass: upstream.clone(),
        iam_endpoint: iam_ep.clone(),
        iam_apikey: iam_key.clone(),
        service_crn: service_crn.clone(),
        paths,
    })
}

#![allow(missing_docs)]

extern crate futures;
extern crate simwood_rs;
#[macro_use]
extern crate swagger;
extern crate tokio_core;
extern crate uuid;
#[macro_use]
extern crate log;
#[macro_use]
extern crate serde_derive;
extern crate hyper;

use swagger::{AuthData, ContextBuilder, EmptyContext, Push, XSpanIdString};

use futures::{future, Future};
use simwood_rs::models;
use simwood_rs::{
    Api, DeleteAllocatedNumberResponse, GetAvailableNumbersResponse, PutAllocatedNumberResponse,
    PutNumberConfigResponse,
};
use tokio_core::reactor;

use kube::{
    api::{Informer, Object, PostParams, RawApi, WatchEvent},
    client::APIClient,
    config,
};

// Own custom resource spec
#[derive(Deserialize, Serialize, Clone)]
pub struct Simwood {
    account: String,
    username: String,
    password: String,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct Provider {
    simwood: Option<Simwood>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct Inbound {
    numbers: u32,
    service: String,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct PstnConnectionSpec {
    provider: Provider,
    inbound: Option<Inbound>,
    outbound: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone)]
pub enum Status {
    Unsynchronized,
    Synchronized,
    Failed,
    UnknownProvider,
}
impl Default for Status {
    fn default() -> Self {
        Status::Unsynchronized
    }
}

#[derive(Default, Deserialize, Serialize, Clone)]
pub struct PstnConnectionStatus {
    status: Status,
    numbers: Vec<String>,
}

// The kubernetes generic object with our spec and no status
type PstnConnection = Object<PstnConnectionSpec, PstnConnectionStatus>;

type SimwoodContext = make_context_ty!(
    ContextBuilder,
    EmptyContext,
    Option<AuthData>,
    XSpanIdString
);
fn make_simwood_context(simwood: &Simwood) -> SimwoodContext {
    make_context!(
        ContextBuilder,
        EmptyContext,
        Some(AuthData::basic(&simwood.username, &simwood.password)) as Option<AuthData>,
        XSpanIdString(self::uuid::Uuid::new_v4().to_string())
    )
}

fn main() -> Result<(), failure::Error> {
    // Instantiate Kubernetes client
    env_logger::init();

    let config = config::load_kube_config().expect("failed to load kubeconfig");
    let kube_client = APIClient::new(config);
    let namespace = std::env::var("NAMESPACE").unwrap_or("default".into());

    // Requires `kubectl apply -f kubernetes/crd.yaml` run first
    let resource = RawApi::customResource("pstn-connections")
        .group("alpha.matt-williams.github.io")
        .within(&namespace);

    let informer: Informer<PstnConnection> = Informer::raw(kube_client, resource.clone()).init()?;

    // Instantiate Simwood client
    let mut core = reactor::Core::new().unwrap();
    let base_url = "https://api.simwood.com/";

    let simwood_client = simwood_rs::Client::try_new_https(core.handle(), &base_url, "ca.pem")
        .expect("Failed to create HTTPS client");

    loop {
        informer.poll()?;
        while let Some(event) = informer.pop() {
            match event {
                WatchEvent::Added(pstn) => {
                    let name = pstn.metadata.name.clone();
                    let status = core
                        .run(handle_pstn_connection_add(pstn, &simwood_client))
                        .unwrap();
                    match resource.replace_status(
                        &name,
                        &PostParams { dry_run: false },
                        serde_json::to_vec(&status).unwrap(),
                    ) {
                        Ok(_) => info!("Updated status for PSTN connection {}", name),
                        Err(_) => warn!("Failed to update status for PSTN connection {}", name),
                    }
                }
                WatchEvent::Modified(pstn) => warn!(
                    "PSTN connection {} modified - unsupported!",
                    pstn.metadata.name
                ),
                WatchEvent::Deleted(pstn) => core
                    .run(handle_pstn_connection_delete(pstn, &simwood_client))
                    .unwrap(),
                WatchEvent::Error(error) => warn!("Watch error occurred: {}", error.to_string()),
            }
        }
    }
}

fn handle_pstn_connection_add(
    pstn: PstnConnection,
    simwood_client: &simwood_rs::Client<hyper::client::FutureResponse>,
) -> Box<dyn Future<Item = PstnConnectionStatus, Error = ()> + 'static> {
    match pstn.spec.provider {
        Provider {
            simwood: Some(ref simwood),
        } => {
            info!(
                "PSTN connection {} added for Simwood account {}\n",
                pstn.metadata.name, simwood.account
            );
            let context = make_simwood_context(simwood);
            Box::new(simwood_client.get_available_numbers(
                simwood.account.clone(),
                "standard".to_string(),
                1,
                None,
                &context
            ).then({
                let simwood = simwood.clone();
                let simwood_client = simwood_client.clone();
                move |result| match result {
                Ok(GetAvailableNumbersResponse::Success(success_result)) => {
                    match success_result[0] {
                        models::AvailableNumber {
                            country_code: Some(ref cc),
                            number: Some(ref num),
                            ..
                        } => {
                            let number: String = cc.to_string() + num;
                            info!("Found available number {}", number);

                            // Put allocated number
                            Box::new(simwood_client.put_allocated_number(
                                simwood.account.clone(),
                                number.clone(),
                                &context
                            ).then(move |result| match result {
                                Ok(PutAllocatedNumberResponse::Success) => {
                                    info!(
                                        "Claimed available number {}",
                                        number
                                    );

                                    // Configure route
                                    let endpoint = "%number@ec2-35-173-185-145.compute-1.amazonaws.com:5080".to_string();
                                    let config = models::NumberConfig {
                                        options: Some(
                                            models::NumberConfigOptions {
                                                enabled: Some(true),
                                                ..models::NumberConfigOptions::new()
                                            },
                                        ),
                                        routing: Some(
                                            [(
                                                "default".to_string(),
                                                vec![vec![models::RoutingEntry {
                                                    endpoint: Some(
                                                        endpoint.clone(),
                                                    ),
                                                    ..models::RoutingEntry::new()
                                                }]],
                                            )]
                                            .iter()
                                            .cloned()
                                            .collect(),
                                        ),
                                    };
                                    Box::new(simwood_client.put_number_config(
                                            simwood.account.clone(),
                                            number.to_string(),
                                            Some(config),
                                            &context
                                        ).then(move |result| {match result {
                                            Ok(PutNumberConfigResponse::Success(_)) => {
                                                info!("Set routing configuration for number {} to {}", number, endpoint);
                                                future::ok(PstnConnectionStatus{status: Status::Synchronized, numbers: vec![number]})
                                            },
                                            Ok(PutNumberConfigResponse::NumberNotAllocated) => {
                                                warn!("Claimed number {} no longer allocated when setting routing configuration", number);
                                                future::ok(PstnConnectionStatus{status: Status::Failed, numbers: vec![number.clone()]})
                                            },
                                            Err(error) => {
                                                warn!("Failed to set routing configuration for number {}: {}", number, error.0);
                                                future::ok(PstnConnectionStatus{status: Status::Failed, numbers: vec![number.clone()]})
                                            }
                                        }
                                        })) as Box<dyn Future<Item=PstnConnectionStatus, Error=()> + 'static>
                                }
                                Ok(
                                    PutAllocatedNumberResponse::NumberNotAvailable,
                                ) => {
                                    // TODO: Handle this race condition
                                    warn!("Available number {} not available after all", number);
                                    Box::new(future::ok(PstnConnectionStatus{status: Status::Failed, numbers: vec![]})) as Box<dyn Future<Item=PstnConnectionStatus, Error=()> + 'static>
                                }
                                Err(error) => {
                                    warn!("Failed to claim available number: {}", error.0);
                                    Box::new(future::ok(PstnConnectionStatus{status: Status::Failed, numbers: vec![]})) as Box<dyn Future<Item=PstnConnectionStatus, Error=()> + 'static>
                                },
                            })) as Box<dyn Future<Item=PstnConnectionStatus, Error=()> + 'static>
                        },
                        _ => { warn!(
                            "Failed to parse available number - got {:?}",
                            success_result[0]);
                            Box::new(future::ok(PstnConnectionStatus{status: Status::Failed, numbers: vec![]})) as Box<dyn Future<Item=PstnConnectionStatus, Error=()> + 'static>
                        }
                    }
                },
                Err(error) => {
                    warn!("Failed to retrieve available number: {}", error.0);
                    Box::new(future::ok(PstnConnectionStatus{status: Status::Failed, numbers: vec![]})) as Box<dyn Future<Item=PstnConnectionStatus, Error=()> + 'static>
                }
            }})) as Box<dyn Future<Item=PstnConnectionStatus, Error=()> + 'static>
        }
        _ => {
            warn!(
                "PSTN connection {} added with unknown provider",
                pstn.metadata.name
            );
            Box::new(future::ok(PstnConnectionStatus {
                status: Status::UnknownProvider,
                numbers: vec![],
            })) as Box<dyn Future<Item = PstnConnectionStatus, Error = ()> + 'static>
        }
    }
}

fn handle_pstn_connection_delete(
    pstn: PstnConnection,
    simwood_client: &simwood_rs::Client<hyper::client::FutureResponse>,
) -> Box<dyn Future<Item = (), Error = ()> + 'static> {
    match pstn.spec.provider {
        Provider {
            simwood: Some(ref simwood),
        } => {
            info!(
                "PSTN connection {} deleted for Simwood account {}\n",
                pstn.metadata.name, simwood.account
            );
            let context = make_simwood_context(simwood);

            // Delete allocated number
            let delete_futures: Vec<_> = pstn
                .status
                .map(|s| s.numbers)
                .unwrap_or_else(|| Vec::new())
                .iter()
                .cloned()
                .map({
                    let simwood = simwood.clone();
                    let simwood_client = simwood_client.clone();
                    let context = context.clone();
                    move |number| {
                        simwood_client
                            .delete_allocated_number(
                                simwood.account.clone(),
                                number.clone(),
                                &context,
                            )
                            .then(move |result| {
                                match result {
                                    Ok(DeleteAllocatedNumberResponse::Success) => {
                                        info!("Deleted number {}", number)
                                    }
                                    Ok(DeleteAllocatedNumberResponse::NumberNotAllocated) => {
                                        info!("Failed to delete number {} - not allocated", number)
                                    }
                                    Err(error) => {
                                        warn!("Failed to delete number {}: {}", number, error.0)
                                    }
                                }
                                future::ok(())
                            })
                    }
                })
                .collect();
            Box::new(
                future::join_all(delete_futures).then(move |_: Result<Vec<()>, ()>| future::ok(())),
            ) as Box<dyn Future<Item = (), Error = ()> + 'static>
        }
        _ => {
            warn!(
                "PSTN connection {} deleted with unknown provider",
                pstn.metadata.name
            );
            Box::new(future::ok(())) as Box<dyn Future<Item = (), Error = ()> + 'static>
        }
    }
}

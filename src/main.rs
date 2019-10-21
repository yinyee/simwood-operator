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
use std::sync::{Arc, Mutex};
use tokio_core::reactor;

use kube::{
    api::{Informer, Object, RawApi, Void, WatchEvent},
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
// The kubernetes generic object with our spec and no status
type PstnConnection = Object<PstnConnectionSpec, Void>;

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
    let mut directory_number = Arc::new(Mutex::new("".to_string()));

    // Instantiate Kubernetes client
    env_logger::init();

    let config = config::load_kube_config().expect("failed to load kubeconfig");
    let kube_client = APIClient::new(config);
    let namespace = std::env::var("NAMESPACE").unwrap_or("default".into());

    // Requires `kubectl apply -f kubernetes/crd.yaml` run first
    let resource = RawApi::customResource("pstn-connections")
        .group("alpha.matt-williams.github.io")
        .within(&namespace);

    let informer: Informer<PstnConnection> = Informer::raw(kube_client, resource).init()?;

    // Instantiate Simwood client
    let mut core = reactor::Core::new().unwrap();
    let base_url = "https://api.simwood.com/";

    let simwood_client = simwood_rs::Client::try_new_https(core.handle(), &base_url, "ca.pem")
        .expect("Failed to create HTTPS client");

    loop {
        informer.poll()?;
        while let Some(event) = informer.pop() {
            core.run(match event {
                WatchEvent::Added(pstn) => {
                    handle_pstn_connection_add(pstn, &simwood_client, &mut directory_number)
                }
                WatchEvent::Modified(pstn) => {
                    warn!(
                        "PSTN connection {} modified - unsupported!",
                        pstn.metadata.name
                    );
                    Box::new(future::ok(()))
                }
                WatchEvent::Deleted(pstn) => {
                    handle_pstn_connection_delete(pstn, &simwood_client, &directory_number)
                }
                WatchEvent::Error(error) => {
                    warn!("Watch error occurred: {}", error.to_string());
                    Box::new(future::ok(()))
                }
            })
            .unwrap(); // TODO: Get rid of unwrap - in futures 0.3, this will naturally go away
        }
    }
}

fn handle_pstn_connection_add(
    pstn: PstnConnection,
    simwood_client: &simwood_rs::Client<hyper::client::FutureResponse>,
    directory_number: &Arc<Mutex<String>>,
) -> Box<dyn Future<Item = (), Error = ()> + 'static> {
    match pstn.spec.provider {
        Provider {
            simwood: Some(ref simwood),
        } => {
            info!(
                "Pstn connection {} added for Simwood account {}\n",
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
                let directory_number = directory_number.clone();
                move |result| match result {
                Ok(GetAvailableNumbersResponse::Success(success_result)) => {
                    match success_result[0] {
                        models::AvailableNumber {
                            country_code: Some(ref cc),
                            number: Some(ref num),
                            ..
                        } => {
                            let directory_number: String = {
                                let mut directory_number = directory_number.lock().unwrap();
                                *directory_number = cc.to_string() + num;
                                directory_number.clone()
                            };
                            info!("Found available number {}", directory_number);

                            // Put allocated number
                            Box::new(simwood_client.put_allocated_number(
                                simwood.account.clone(),
                                directory_number.to_string(),
                                &context
                            ).then(move |result| match result {
                                Ok(PutAllocatedNumberResponse::Success) => {
                                    info!(
                                        "Claimed available number {}",
                                        directory_number
                                    );

                                    // TODO: Store directory number in Kubernetes status

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
                                            directory_number.to_string(),
                                            Some(config),
                                            &context
                                        ).then(move |result| {match result {
                                        Ok(PutNumberConfigResponse::Success(_)) =>
                                            info!("Set routing configuration for number {} to {}", directory_number, endpoint),
                                        Ok(PutNumberConfigResponse::NumberNotAllocated) =>
                                            warn!("Claimed number {} no longer allocated when setting routing configuration", directory_number),
                                        Err(error) =>
                                            warn!("Failed to set routing configuration for number {}: {}", directory_number, error.0),
                                        };
                                        future::ok(())
                                        })) as Box<dyn Future<Item=(), Error=()> + 'static>
                                }
                                Ok(
                                    PutAllocatedNumberResponse::NumberNotAvailable,
                                ) => {
                                    // TODO: Handle this race condition
                                    warn!("Available number {} not available after all", directory_number);
                                    Box::new(future::ok(())) as Box<dyn Future<Item=(), Error=()> + 'static>
                                }
                                Err(error) => {
                                    warn!(
                                    "Failed to claim available number: {}",
                                    error.0
                                );
                                    Box::new(future::ok(())) as Box<dyn Future<Item=(), Error=()> + 'static>
                                },
                            })) as Box<dyn Future<Item=(), Error=()> + 'static>
                        },
                        _ => { warn!(
                            "Failed to parse available number - got {:?}",
                            success_result[0]);
                            Box::new(future::ok(())) as Box<dyn Future<Item=(), Error=()> + 'static>
                        }
                    }
                },
                Err(error) => {
                    warn!("Failed to retrieve available number: {}", error.0);
                    Box::new(future::ok(())) as Box<dyn Future<Item=(), Error=()> + 'static>
                }
            }})) as Box<dyn Future<Item=(), Error=()> + 'static>
        }
        _ => {
            warn!(
                "PSTN connection {} added with unknown provider",
                pstn.metadata.name
            );
            Box::new(future::ok(())) as Box<dyn Future<Item = (), Error = ()> + 'static>
        }
    }
}

fn handle_pstn_connection_delete(
    pstn: PstnConnection,
    simwood_client: &simwood_rs::Client<hyper::client::FutureResponse>,
    directory_number: &Arc<Mutex<String>>,
) -> Box<dyn Future<Item = (), Error = ()> + 'static> {
    match pstn.spec.provider {
        Provider {
            simwood: Some(ref simwood),
        } => {
            info!(
                "Pstn connection {} deleted for Simwood account {}\n",
                pstn.metadata.name, simwood.account
            );
            let context = make_simwood_context(simwood);

            let directory_number = directory_number.lock().unwrap().clone();

            // Delete allocated number
            Box::new(
                simwood_client
                    .delete_allocated_number(
                        simwood.account.clone(),
                        directory_number.clone(),
                        &context,
                    )
                    .then(move |result| {
                        match result {
                            Ok(DeleteAllocatedNumberResponse::Success) => {
                                info!("Deleted number {}", directory_number)
                            }
                            Ok(DeleteAllocatedNumberResponse::NumberNotAllocated) => info!(
                                "Failed to delete number {} - not allocated",
                                directory_number
                            ),
                            Err(error) => {
                                warn!("Failed to delete number {}: {}", directory_number, error.0)
                            }
                        }
                        future::ok(())
                    }),
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

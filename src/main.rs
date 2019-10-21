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

use swagger::{AuthData, ContextBuilder, EmptyContext, Push, XSpanIdString};

use simwood_rs::models;
use simwood_rs::{
    ApiNoContext, ContextWrapperExt, DeleteAllocatedNumberResponse, GetAvailableNumbersResponse,
    PutAllocatedNumberResponse, PutNumberConfigResponse,
};
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

fn make_simwood_context(
    simwood: &Simwood,
) -> make_context_ty!(
    ContextBuilder,
    EmptyContext,
    Option<AuthData>,
    XSpanIdString
) {
    make_context!(
        ContextBuilder,
        EmptyContext,
        Some(AuthData::basic(&simwood.username, &simwood.password)) as Option<AuthData>,
        XSpanIdString(self::uuid::Uuid::new_v4().to_string())
    )
}

fn main() -> Result<(), failure::Error> {
    let mut directory_number = "".to_string();

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
            match event {
                WatchEvent::Added(pstn) => {
                    match pstn.spec.provider {
                        Provider {
                            simwood: Some(ref simwood),
                        } => {
                            info!(
                                "Pstn connection {} added for Simwood account {}\n",
                                pstn.metadata.name, simwood.account
                            );

                            let simwood_client =
                                simwood_client.with_context(make_simwood_context(simwood));
                            match core.run(simwood_client.get_available_numbers(
                                simwood.account.clone(),
                                "standard".to_string(),
                                1,
                                None,
                            )) {
                                Ok(GetAvailableNumbersResponse::Success(success_result)) => {
                                    match success_result[0] {
                                        models::AvailableNumber {
                                            country_code: Some(ref cc),
                                            number: Some(ref num),
                                            ..
                                        } => {
                                            directory_number = cc.to_string() + num;
                                            info!("Found available number {}", directory_number);

                                            // Put allocated number
                                            match core.run(simwood_client.put_allocated_number(
                                                simwood.account.clone(),
                                                directory_number.to_string(),
                                            )) {
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
                                                    match
                                                        core.run(simwood_client.put_number_config(
                                                            simwood.account.clone(),
                                                            directory_number.to_string(),
                                                            Some(config),
                                                        )) {
                                                        Ok(PutNumberConfigResponse::Success(_)) =>
                                                        info!("Set routing configuration for number {} to {}", directory_number, endpoint),
                                                        Ok(PutNumberConfigResponse::NumberNotAllocated) =>
                                                        warn!("Claimed number {} no longer allocated when setting routing configuration", directory_number),
                                                        Err(error) =>
                                                        warn!("Failed to set routing configuration for number {}: {}", directory_number, error.0),
                                                        }
                                                }
                                                Ok(
                                                    PutAllocatedNumberResponse::NumberNotAvailable,
                                                ) => {
                                                    // TODO: Handle this race condition
                                                    warn!("Available number {} not available after all", directory_number)
                                                }
                                                Err(error) => warn!(
                                                    "Failed to claim available number: {}",
                                                    error.0
                                                ),
                                            }
                                        }
                                        _ => warn!(
                                            "Failed to parse available number - got {:?}",
                                            success_result[0]
                                        ),
                                    };
                                }
                                Err(error) => {
                                    warn!("Failed to retrieve available number: {}", error.0)
                                }
                            }
                        }
                        _ => warn!(
                            "PSTN connection {} added with unknown provider",
                            pstn.metadata.name
                        ),
                    }
                }
                WatchEvent::Modified(pstn) => {
                    warn!(
                        "PSTN connection {} modified - unsupported!",
                        pstn.metadata.name
                    );
                }
                WatchEvent::Deleted(pstn) => {
                    match pstn.spec.provider {
                        Provider {
                            simwood: Some(ref simwood),
                        } => {
                            info!(
                                "Pstn connection {} deleted for Simwood account {}\n",
                                pstn.metadata.name, simwood.account
                            );
                            let simwood_client =
                                simwood_client.with_context(make_simwood_context(simwood));

                            // Delete allocated number
                            match core.run(simwood_client.delete_allocated_number(
                                simwood.account.clone(),
                                directory_number.clone(),
                            )) {
                                Ok(DeleteAllocatedNumberResponse::Success) => {
                                    info!("Deleted number {}", directory_number)
                                }
                                Ok(DeleteAllocatedNumberResponse::NumberNotAllocated) => info!(
                                    "Failed to delete number {} - not allocated",
                                    directory_number
                                ),
                                Err(error) => warn!(
                                    "Failed to delete number {}: {}",
                                    directory_number, error.0
                                ),
                            }
                        }
                        _ => {
                            warn!(
                                "PSTN connection {} deleted with unknown provider",
                                pstn.metadata.name
                            );
                        }
                    }
                }
                WatchEvent::Error(error) => warn!("Watch error occurred: {}", error.to_string()),
            }
        }
    }
}

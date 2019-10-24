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
extern crate k8s_openapi;

use swagger::{AuthData, ContextBuilder, EmptyContext, Push, XSpanIdString};

use futures::{future, Future};
use simwood_rs::models;
use simwood_rs::{
    Api as SimwoodApi, DeleteAllocatedNumberResponse, GetAvailableNumbersResponse,
    PutAllocatedNumberResponse, PutNumberConfigResponse,
};
use tokio_core::reactor;

use k8s_openapi::api::core::v1::{ServicePort, ServiceSpec};
use kube::{
    api::{Api, Informer, ListParams, Object, ObjectList, PostParams, RawApi, WatchEvent},
    client::APIClient,
    config,
};

// Own custom resources spec
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
    Synchronized,
    Failed,
    UnknownProvider,
}

#[derive(Default, Deserialize, Serialize, Clone)]
pub struct PstnConnectionStatus {
    status: Option<Status>,
    numbers: Option<Vec<String>>,
}

// The kubernetes generic object with our spec and status
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

    // Load kubeconfig and create a client
    let config = config::load_kube_config().expect("failed to load kubeconfig");
    let kube_client = APIClient::new(config);

    // Get the resources we need to manage
    let services = Api::v1Service(kube_client.clone());
    let nodes = Api::v1Node(kube_client.clone());
    let pstn_connections = RawApi::customResource("pstn-connections")
        .group("alpha.matt-williams.github.io");

    // Instantiate Simwood client
    let mut core = reactor::Core::new().unwrap();
    let base_url = "https://api.simwood.com/";
    let simwood_client = simwood_rs::Client::try_new_https(core.handle(), &base_url, "ca.pem")
        .expect("Failed to create HTTPS client");

    // Create an informer and watch for changes to PSTN connections
    let informer: Informer<PstnConnection> =
        Informer::raw(kube_client, pstn_connections.clone()).init()?;
    loop {
        informer.poll()?;
        while let Some(event) = informer.pop() {
            match event {
                WatchEvent::Added(pstn) => {
                    info!("PSTN connection {} added", pstn.metadata.name);
                    let name = pstn.metadata.name.clone();

                    let service_sip_uri = pstn.spec.inbound.as_ref().and_then(|inbound| {
                        let mut service_sip_uri = None;
                        match services.get(&inbound.service) {
                            Ok(Object { ref spec, .. }) => match spec {
                                ServiceSpec {
                                    type_: Some(ref type_),
                                    external_name: Some(external_name),
                                    ports: Some(ports),
                                    ..
                                } if type_ == "ExternalName" => {
                                    let sip_uri = build_sip_uri(external_name, ports.first());
                                    info!("Service {} is of type ExternalName - constructed SIP URI {}", &inbound.service, sip_uri);
                                    service_sip_uri = Some(sip_uri);
                                }
                                ServiceSpec {
                                    type_: Some(ref type_),
                                    ports: Some(ports),
                                    ..
                                } if type_ == "NodePort" => {
                                    match nodes.list(&ListParams::default()) {
                                        Ok(ObjectList { items, .. }) => match items
                                            .first()
                                            .and_then(|n| n.status.as_ref())
                                            .and_then(|s| s.addresses.as_ref())
                                            .and_then(|a| {
                                                a.iter()
                                                    .filter(|a| {
                                                        a.type_ == "Hostname"
                                                            || a.type_ == "ExternalIP"
                                                    })
                                                    .nth(0)
                                            }) {
                                            Some(ref address) => {
                                                let sip_uri = build_sip_uri(&address.address, ports.first());
                                                info!("Service {} is of type NodePort - constructed SIP URI {}", &inbound.service, sip_uri);
                                                service_sip_uri = Some(sip_uri);
                                            }
                                            None => warn!("Service {} is of type NodePort but no hostname or external IP found - can't construct routable SIP URI", &inbound.service),
                                        },
                                        Err(_) => warn!("Failed to query node resources"),
                                    }
                                }
                                ServiceSpec {
                                    type_: Some(ref type_),
                                    load_balancer_ip: Some(load_balancer_ip),
                                    ports: Some(ports),
                                    ..
                                } if type_ == "LoadBalancer" => {
                                    let sip_uri = build_sip_uri(load_balancer_ip, ports.first());
                                    info!("Service {} is of type LoadBalancer - constructed SIP URI {}", &inbound.service, sip_uri);
                                    service_sip_uri = Some(sip_uri);
                                }
                                ServiceSpec {
                                    type_: Some(ref type_),
                                    ..
                                } if type_ == "ClusterIP" => warn!("Service {} is of type ClusterIP - not externally routable", &inbound.service),
                                ServiceSpec {
                                    type_: Some(type_), ..
                                } => warn!("Service {} is of type {} - unsupported", &inbound.service, type_),
                                ServiceSpec { type_: None, .. } => warn!("Service {} has no type specified - unsupported", &inbound.service),
                            },
                            Err(_) => warn!("Failed to query service {}", &inbound.service),
                        }
                        service_sip_uri
                    });

                    let status = core
                        .run(handle_pstn_connection_add(
                            pstn,
                            service_sip_uri,
                            &simwood_client,
                        ))
                        .unwrap();
                    match pstn_connections.replace_status(
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
                WatchEvent::Deleted(pstn) => {
                    info!("PSTN connection {} deleted", pstn.metadata.name);
                    core.run(handle_pstn_connection_delete(pstn, &simwood_client))
                        .unwrap();
                }
                WatchEvent::Error(error) => warn!("Watch error occurred: {}", error.to_string()),
            }
        }
    }
}

fn build_sip_uri(host: &str, port: Option<&ServicePort>) -> String {
    let mut sip_uri = format!("sip:{}", host);
    if let Some(port) = port {
        if let ServicePort {
            node_port: Some(port),
            ..
        } = port
        {
            sip_uri = format!("{}:{}", sip_uri, port);
        }
        if let ServicePort {
            protocol: Some(protocol),
            ..
        } = port
        {
            sip_uri = format!("{};transport={}", sip_uri, protocol);
        }
    }
    sip_uri
}

fn handle_pstn_connection_add(
    pstn: PstnConnection,
    service_sip_uri: Option<String>,
    simwood_client: &simwood_rs::Client<hyper::client::FutureResponse>,
) -> Box<dyn Future<Item = PstnConnectionStatus, Error = ()> + 'static> {
    match pstn.spec.provider {
        Provider {
            simwood: Some(ref simwood),
        } => {
            info!(
                "Using Simwood account {} with username {}",
                simwood.account, simwood.username
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
                                                    endpoint: service_sip_uri.clone(),
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
                                                info!("Set routing configuration for number {} to {}", number, service_sip_uri.unwrap_or("<none>".to_string()));
                                                future::ok(PstnConnectionStatus{status: Some(Status::Synchronized), numbers: Some(vec![number])})
                                            },
                                            Ok(PutNumberConfigResponse::NumberNotAllocated) => {
                                                warn!("Claimed number {} no longer allocated when setting routing configuration", number);
                                                future::ok(PstnConnectionStatus{status: Some(Status::Failed), numbers: Some(vec![number.clone()])})
                                            },
                                            Err(error) => {
                                                warn!("Failed to set routing configuration for number {}: {}", number, error.0);
                                                future::ok(PstnConnectionStatus{status: Some(Status::Failed), numbers: Some(vec![number.clone()])})
                                            }
                                        }
                                        })) as Box<dyn Future<Item=PstnConnectionStatus, Error=()> + 'static>
                                }
                                Ok(
                                    PutAllocatedNumberResponse::NumberNotAvailable,
                                ) => {
                                    // TODO: Handle this race condition
                                    warn!("Available number {} not available after all", number);
                                    Box::new(future::ok(PstnConnectionStatus{status: Some(Status::Failed), ..PstnConnectionStatus::default()})) as Box<dyn Future<Item=PstnConnectionStatus, Error=()> + 'static>
                                }
                                Err(error) => {
                                    warn!("Failed to claim available number: {}", error.0);
                                    Box::new(future::ok(PstnConnectionStatus{status: Some(Status::Failed), ..PstnConnectionStatus::default()})) as Box<dyn Future<Item=PstnConnectionStatus, Error=()> + 'static>
                                },
                            })) as Box<dyn Future<Item=PstnConnectionStatus, Error=()> + 'static>
                        },
                        _ => { warn!(
                            "Failed to parse available number - got {:?}",
                            success_result[0]);
                            Box::new(future::ok(PstnConnectionStatus{status: Some(Status::Failed), ..PstnConnectionStatus::default()})) as Box<dyn Future<Item=PstnConnectionStatus, Error=()> + 'static>
                        }
                    }
                },
                Err(error) => {
                    warn!("Failed to retrieve available number: {}", error.0);
                    Box::new(future::ok(PstnConnectionStatus{status: Some(Status::Failed), ..PstnConnectionStatus::default()})) as Box<dyn Future<Item=PstnConnectionStatus, Error=()> + 'static>
                }
            }})) as Box<dyn Future<Item=PstnConnectionStatus, Error=()> + 'static>
        }
        _ => {
            warn!(
                "PSTN connection {} added with unknown provider",
                pstn.metadata.name
            );
            Box::new(future::ok(PstnConnectionStatus {
                status: Some(Status::UnknownProvider),
                ..PstnConnectionStatus::default()
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
                "Using Simwood account {} with username {}",
                simwood.account, simwood.username
            );
            let context = make_simwood_context(simwood);

            // Delete allocated number
            let delete_futures: Vec<_> = pstn
                .status
                .and_then(|s| s.numbers)
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

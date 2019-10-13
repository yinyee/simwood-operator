#![allow(missing_docs, unused_variables, trivial_casts)]

extern crate simwood_rs;
#[allow(unused_extern_crates)]
extern crate futures;
#[allow(unused_extern_crates)]
#[macro_use]
extern crate swagger;
#[allow(unused_extern_crates)]
extern crate tokio_core;
extern crate uuid;
#[macro_use] extern crate log;
#[macro_use] extern crate serde_derive;

use swagger::{ContextBuilder, EmptyContext, XSpanIdString, Has, Push, AuthData};

#[allow(unused_imports)]
use futures::{Future, future, Stream, stream};
use tokio_core::reactor;
#[allow(unused_imports)]
use simwood_rs::{ApiNoContext, ContextWrapperExt,
                      ApiError,
                      GetAccountTypeResponse,
                      DeleteAllocatedNumberResponse,
                      GetAllocatedNumberResponse,
                      GetAllocatedNumbersResponse,
                      GetAvailableNumbersResponse,
                      GetNumberRangesResponse,
                      PutAllocatedNumberResponse,
                      DeleteOutboundAclIpResponse,
                      DeleteOutboundTrunkResponse,
                      GetOutboundAclIpsResponse,
                      GetOutboundTrunkResponse,
                      GetOutboundTrunksResponse,
                      PutOutboundAclIpResponse,
                      PutOutboundTrunkResponse,
                      GetMyIpResponse,
                      GetTimeResponse
                     };
use simwood_rs::models;

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
    simwood: Simwood,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct Inbound {
    numbers: u32,
    service: String,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct PstnConnectionSpec {
    provider: Provider,
    inbound: Inbound,
    outbound: bool,
}
// The kubernetes generic object with our spec and no status
type PstnConnection = Object<PstnConnectionSpec, Void>;

// Global constants
const AC_NUM: &str = "931210";
const API_USER: &str = "ca54d1be2a5a219c10fd3b64940a6aa3d1476ae5";
const API_PASSWORD: &str = "cc1c5dcc77a4c660e1c2c7f9e5ada652ac85f09e";

fn main() -> Result<(), failure::Error> {

    let mut directory_number = "".to_string();

    // Instantiate Kubernetes client
    env_logger::init();
    
    let config = config::load_kube_config().expect("failed to load kubeconfig");
    let kube_client = APIClient::new(config);
    let namespace = std::env::var("NAMESPACE").unwrap_or("default".into());

    // Requires `kubectl apply -f examples/pstn-connection.yaml` run first
    let resource = RawApi::customResource("pstn-connections")
        .group("alpha.matt-williams.github.io")
        .within(&namespace);

    let informer : Informer<PstnConnection> = Informer::raw(kube_client, resource).init()?;

    // Instantiate Simwood client
    let mut core = reactor::Core::new().unwrap();
    let base_url = "https://api.simwood.com/";

    let simwood_client = simwood_rs::Client::try_new_https(core.handle(), &base_url, "ca.pem")
            .expect("Failed to create HTTPS client");
    let context: make_context_ty!(ContextBuilder, EmptyContext, Option<AuthData>, XSpanIdString) =
        make_context!(ContextBuilder, EmptyContext, Some(AuthData::basic(API_USER, API_PASSWORD)) as Option<AuthData>, XSpanIdString(self::uuid::Uuid::new_v4().to_string()));
    let simwood_client = simwood_client.with_context(context);

    loop {

        informer.poll()?;
        while let Some(event) = informer.pop() {
            match event {
                WatchEvent::Added(pstn) => {

                    println!("ADDED PSTN CONNECTION {} FOR ACCOUNT {}\n", pstn.metadata.name, pstn.spec.provider.simwood.account);
                    
                    // Get available numbers
                    let result = core.run(simwood_client.get_available_numbers(AC_NUM.to_string(), "standard".to_string(), 1, None));
                    println!("GET AVAILABLE NUMBER(S): {:?} (X-Span-ID: {:?})\n", result, (simwood_client.context() as &Has<XSpanIdString>).get().clone());

                    match result {
                        // Get available numbers: SUCCESS
                        Ok(GetAvailableNumbersResponse::Success(success_result)) => {
                            // println!("Country code: {:?} Number: {:?}", success_result[0].country_code, success_result[0].number);
                            directory_number = match success_result[0] {
                                models::AvailableNumber{country_code: Some(ref cc), number: Some(ref num), ..} => cc.to_string() + num,
                                _ => "".to_string(),
                            };
                            println!("***Directory number: {}***\n", directory_number);

                            // Put allocated number
                            let result = core.run(simwood_client.put_allocated_number(AC_NUM.to_string(), directory_number.to_string()));
                            println!("PUT ALLOCATED NUMBER: {}: {:?} (X-Span-ID: {:?})\n", directory_number, result, (simwood_client.context() as &Has<XSpanIdString>).get().clone());

                            // Configure route
                            let config = models::NumberConfig {
                            options: Some(models::NumberConfigOptions {
                                enabled: Some(true),
                                ..models::NumberConfigOptions::new()
                            }),
                            routing: Some([
                                ( "default".to_string(), vec![vec![
                                    models::RoutingEntry {
                                        endpoint: Some("%number@ec2-35-173-185-145.compute-1.amazonaws.com:5080".to_string()),
                                        ..models::RoutingEntry::new()
                                    }
                                ]])
                                ].iter().cloned().collect()),
                            };
                            let result = core.run(simwood_client.put_number_config(AC_NUM.to_string(), directory_number.to_string(), Some(config)));
                            println!("CONFIGURE ROUTE: {}: {:?} (X-Span-ID: {:?})\n", directory_number, result, (simwood_client.context() as &Has<XSpanIdString>).get().clone());

                        },
                        // Get available numbers: FAILURE
                        _ => {
                            println!("Error retrieving available numbers\n");
                        },
                    }
                },
                WatchEvent::Deleted(pstn) => {

                    println!("DELETED PSTN CONNECTION {} FOR ACCOUNT {}\n", pstn.metadata.name, pstn.spec.provider.simwood.account);
                    println!("***Directory number: {}***\n", directory_number);
                    
                    // Delete allocated number
                    let result = core.run(simwood_client.delete_allocated_number(AC_NUM.to_string(), directory_number.to_string()));
                    println!("DELETE ALLOCATED NUMBER: {}: {:?} (X-Span-ID: {:?})\n", directory_number.to_string(), result, (simwood_client.context() as &Has<XSpanIdString>).get().clone());
                },
                event => {
                    println!("Another event occurred: {:?}", event);
                }
            }
        }
    }
}

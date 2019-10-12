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

use kube::{
    api::{Object, RawApi, Reflector, Void},
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

fn main() -> Result<(), failure::Error> {

    std::env::set_var("RUST_LOG", "info,kube=trace");
    env_logger::init();
    
    let config = config::load_kube_config().expect("failed to load kubeconfig");
    let client = APIClient::new(config);
    let namespace = std::env::var("NAMESPACE").unwrap_or("default".into());

    // This example requires `kubectl apply -f examples/foo.yaml` run first
    let resource = RawApi::customResource("pstn-connections")
        .group("alpha.matt-williams.github.io")
        .within(&namespace);

    let rf : Reflector<PstnConnection> = Reflector::raw(client, resource).init()?;

    loop {
        // Update internal state by calling watch (blocks):
        rf.poll()?;

        // Read updated internal state (instant):
        rf.read()?.into_iter().for_each(|crd| {
            info!("pstn-connection {}: {}", crd.metadata.name, crd.spec.provider.simwood.account);
        });
    }

    let mut core = reactor::Core::new().unwrap();
    let is_https = true;
    let base_url = "https://api.simwood.com/";

    let client = simwood_rs::Client::try_new_https(core.handle(), &base_url, "ca.pem")
            .expect("Failed to create HTTPS client");

    let context: make_context_ty!(ContextBuilder, EmptyContext, Option<AuthData>, XSpanIdString) =
        make_context!(ContextBuilder, EmptyContext, Some(AuthData::basic("ca54d1be2a5a219c10fd3b64940a6aa3d1476ae5", "cc1c5dcc77a4c660e1c2c7f9e5ada652ac85f09e")) as Option<AuthData>, XSpanIdString(self::uuid::Uuid::new_v4().to_string()));

    let client = client.with_context(context);

            let result = core.run(client.get_account_type("931210".to_string()));
            println!("{:?} (X-Span-ID: {:?})", result, (client.context() as &Has<XSpanIdString>).get().clone());

}

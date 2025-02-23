use crate::config;
use crate::network_protocol::testonly as data;
use crate::network_protocol::{PeerAddr, PeerMessage, RoutedMessageBody};
use crate::peer_manager;
use crate::peer_manager::peer_manager_actor::Event as PME;
use crate::peer_manager::testonly::start as start_pm;
use crate::peer_manager::testonly::Event;
use crate::tcp;
use crate::testonly::{make_rng, Rng};
use crate::time;
use crate::types::{NetworkRequests, NetworkResponses, PeerManagerMessageRequest};
use near_o11y::testonly::init_test_logger;
use near_o11y::WithSpanContextExt;
use near_primitives::block_header::{Approval, ApprovalInner, ApprovalMessage};
use near_primitives::validator_signer::ValidatorSigner;
use near_store::db::TestDB;
use rand::Rng as _;
use std::collections::HashSet;
use std::sync::Arc;

/// Constructs a random TIER1 message.
fn make_block_approval(rng: &mut Rng, signer: &dyn ValidatorSigner) -> Approval {
    let inner = ApprovalInner::Endorsement(data::make_hash(rng));
    let target_height = rng.gen_range(0..100000);
    Approval {
        signature: signer.sign_approval(&inner, target_height),
        account_id: signer.validator_id().clone(),
        target_height,
        inner,
    }
}

async fn establish_connections(clock: &time::Clock, pms: &[&peer_manager::testonly::ActorHandler]) {
    // Make TIER1 validators connect to proxies.
    let mut data = HashSet::new();
    for pm in pms {
        data.extend(pm.tier1_advertise_proxies(clock).await);
    }
    tracing::info!(target:"test", "tier1_advertise_proxies() DONE");

    // Wait for accounts data to propagate.
    for pm in pms {
        tracing::info!(target:"test", "{}: wait_for_accounts_data()",pm.cfg.node_id());
        pm.wait_for_accounts_data(&data).await;
        tracing::info!(target:"test", "{}: wait_for_accounts_data() DONE",pm.cfg.node_id());
        pm.tier1_connect(clock).await;
        tracing::info!(target:"test", "{}: tier1_connect() DONE",pm.cfg.node_id());
    }
}

async fn send_tier1_message(
    rng: &mut Rng,
    from: &peer_manager::testonly::ActorHandler,
    to: &peer_manager::testonly::ActorHandler,
) {
    let from_signer = from.cfg.validator.as_ref().unwrap().signer.clone();
    let to_signer = to.cfg.validator.as_ref().unwrap().signer.clone();
    let target = to_signer.validator_id().clone();
    let want = make_block_approval(rng, from_signer.as_ref());
    let req = NetworkRequests::Approval {
        approval_message: ApprovalMessage { approval: want.clone(), target },
    };
    let mut events = to.events.from_now();
    let resp = from
        .actix
        .addr
        .send(PeerManagerMessageRequest::NetworkRequests(req).with_span_context())
        .await
        .unwrap();
    assert_eq!(NetworkResponses::NoResponse, resp.as_network_response());
    let got = events
        .recv_until(|ev| match ev {
            Event::PeerManager(PME::MessageProcessed(tcp::Tier::T1, PeerMessage::Routed(got))) => {
                Some(got)
            }
            _ => None,
        })
        .await;
    assert_eq!(from.cfg.node_id(), got.author);
    assert_eq!(RoutedMessageBody::BlockApproval(want), got.body);
}

/// Send a message over each connection.
async fn test_clique(rng: &mut Rng, pms: &[&peer_manager::testonly::ActorHandler]) {
    for from in pms {
        for to in pms {
            if from.cfg.node_id() == to.cfg.node_id() {
                continue;
            }
            send_tier1_message(rng, from, to).await;
        }
    }
}

// In case a node is its own proxy, it should advertise its address as soon as
// it becomes a TIER1 node.
#[tokio::test]
async fn first_proxy_advertisement() {
    init_test_logger();
    let mut rng = make_rng(921853233);
    let rng = &mut rng;
    let mut clock = time::FakeClock::default();
    let chain = Arc::new(data::Chain::make(&mut clock, rng, 10));
    let pm = peer_manager::testonly::start(
        clock.clock(),
        near_store::db::TestDB::new(),
        chain.make_config(rng),
        chain.clone(),
    )
    .await;
    let chain_info = peer_manager::testonly::make_chain_info(&chain, &[&pm]);
    tracing::info!(target:"test", "set_chain_info()");
    // TODO(gprusak): The default config constructed via chain.make_config(),
    // currently returns a validator config with its own server addr in the list of TIER1 proxies.
    // You might want to set it explicitly within this test to not rely on defaults.
    pm.set_chain_info(chain_info).await;
    let got = pm.tier1_advertise_proxies(&clock.clock()).await;
    tracing::info!(target:"test", "awaiting for Tier1AdvertiseProxies");
    assert_eq!(
        got[0].proxies,
        vec![PeerAddr { peer_id: pm.cfg.node_id(), addr: pm.cfg.node_addr.unwrap() }]
    );
}

#[tokio::test]
async fn direct_connections() {
    init_test_logger();
    let mut rng = make_rng(921853233);
    let rng = &mut rng;
    let mut clock = time::FakeClock::default();
    let chain = Arc::new(data::Chain::make(&mut clock, rng, 10));

    let mut pms = vec![];
    for _ in 0..5 {
        pms.push(
            peer_manager::testonly::start(
                clock.clock(),
                near_store::db::TestDB::new(),
                chain.make_config(rng),
                chain.clone(),
            )
            .await,
        );
    }
    let pms: Vec<_> = pms.iter().collect();

    tracing::info!(target:"test", "Connect peers serially.");
    for i in 1..pms.len() {
        pms[i - 1].connect_to(&pms[i].peer_info(), tcp::Tier::T2).await;
    }

    tracing::info!(target:"test", "Set chain info.");
    let chain_info = peer_manager::testonly::make_chain_info(&chain, &pms[..]);
    for pm in &pms {
        pm.set_chain_info(chain_info.clone()).await;
    }
    tracing::info!(target:"test", "Establish connections.");
    establish_connections(&clock.clock(), &pms[..]).await;
    tracing::info!(target:"test", "Test clique.");
    test_clique(rng, &pms[..]).await;
}

/// Test which spawns N validators, each with 1 proxy.
/// All the nodes are connected in TIER2 star topology.
/// Then all validators connect to the proxy of each other validator.
#[tokio::test]
async fn proxy_connections() {
    init_test_logger();
    let mut rng = make_rng(921853233);
    let rng = &mut rng;
    let mut clock = time::FakeClock::default();
    let chain = Arc::new(data::Chain::make(&mut clock, rng, 10));

    const N: usize = 5;

    let mut proxies = vec![];
    for _ in 0..N {
        proxies.push(
            peer_manager::testonly::start(
                clock.clock(),
                near_store::db::TestDB::new(),
                chain.make_config(rng),
                chain.clone(),
            )
            .await,
        );
    }
    let proxies: Vec<_> = proxies.iter().collect();

    let mut validators = vec![];
    for i in 0..N {
        let mut cfg = chain.make_config(rng);
        cfg.validator.as_mut().unwrap().proxies =
            config::ValidatorProxies::Static(vec![PeerAddr {
                peer_id: proxies[i].cfg.node_id(),
                addr: proxies[i].cfg.node_addr.unwrap(),
            }]);
        validators.push(
            peer_manager::testonly::start(
                clock.clock(),
                near_store::db::TestDB::new(),
                cfg,
                chain.clone(),
            )
            .await,
        );
    }
    let validators: Vec<_> = validators.iter().collect();

    // Connect validators and proxies in a star topology. Any connected graph would do.
    let hub = peer_manager::testonly::start(
        clock.clock(),
        near_store::db::TestDB::new(),
        chain.make_config(rng),
        chain.clone(),
    )
    .await;
    for pm in &validators {
        pm.connect_to(&hub.peer_info(), tcp::Tier::T2).await;
    }
    for pm in &proxies {
        pm.connect_to(&hub.peer_info(), tcp::Tier::T2).await;
    }

    let mut all = vec![];
    all.extend(validators.clone());
    all.extend(proxies.clone());
    all.push(&hub);

    let chain_info = peer_manager::testonly::make_chain_info(&chain, &validators[..]);
    for pm in &all {
        pm.set_chain_info(chain_info.clone()).await;
    }
    establish_connections(&clock.clock(), &all[..]).await;
    test_clique(rng, &validators[..]).await;
}

#[tokio::test]
async fn account_keys_change() {
    init_test_logger();
    let mut rng = make_rng(921853233);
    let rng = &mut rng;
    let mut clock = time::FakeClock::default();
    let chain = Arc::new(data::Chain::make(&mut clock, rng, 10));

    let v0 = start_pm(clock.clock(), TestDB::new(), chain.make_config(rng), chain.clone()).await;
    let v1 = start_pm(clock.clock(), TestDB::new(), chain.make_config(rng), chain.clone()).await;
    let v2 = start_pm(clock.clock(), TestDB::new(), chain.make_config(rng), chain.clone()).await;
    let hub = start_pm(clock.clock(), TestDB::new(), chain.make_config(rng), chain.clone()).await;
    hub.connect_to(&v0.peer_info(), tcp::Tier::T2).await;
    hub.connect_to(&v1.peer_info(), tcp::Tier::T2).await;
    hub.connect_to(&v2.peer_info(), tcp::Tier::T2).await;

    // TIER1 nodes in 1st epoch are {v0,v1}.
    let chain_info = peer_manager::testonly::make_chain_info(&chain, &[&v0, &v1]);
    for pm in [&v0, &v1, &v2, &hub] {
        pm.set_chain_info(chain_info.clone()).await;
    }
    establish_connections(&clock.clock(), &[&v0, &v1, &v2, &hub]).await;
    test_clique(rng, &[&v0, &v1]).await;

    // TIER1 nodes in 2nd epoch are {v0,v2}.
    let chain_info = peer_manager::testonly::make_chain_info(&chain, &[&v0, &v2]);
    for pm in [&v0, &v1, &v2, &hub] {
        pm.set_chain_info(chain_info.clone()).await;
    }
    establish_connections(&clock.clock(), &[&v0, &v1, &v2, &hub]).await;
    test_clique(rng, &[&v0, &v2]).await;

    drop(v0);
    drop(v1);
    drop(v2);
    drop(hub);
}

// Let's say that a validator has 2 proxies configured. At first proxy0 is available and proxy1 is not,
// then proxy1 is available and proxy0 is not. In both situations validator should be reachable,
// as long as it manages to advertise the currently available proxy and the TIER1 nodes connect to
// that proxy.
#[tokio::test]
async fn proxy_change() {
    init_test_logger();
    let mut rng = make_rng(921853233);
    let rng = &mut rng;
    let mut clock = time::FakeClock::default();
    let chain = Arc::new(data::Chain::make(&mut clock, rng, 10));

    // v0 has proxies {p0,p1}
    // v1 has no proxies.
    let p0cfg = chain.make_config(rng);
    let p1cfg = chain.make_config(rng);
    let mut v0cfg = chain.make_config(rng);
    v0cfg.validator.as_mut().unwrap().proxies = config::ValidatorProxies::Static(vec![
        PeerAddr { peer_id: p0cfg.node_id(), addr: p0cfg.node_addr.unwrap() },
        PeerAddr { peer_id: p1cfg.node_id(), addr: p1cfg.node_addr.unwrap() },
    ]);
    let mut v1cfg = chain.make_config(rng);
    v1cfg.validator.as_mut().unwrap().proxies = config::ValidatorProxies::Static(vec![]);

    tracing::info!(target:"test", "Start all nodes.");
    let p0 = start_pm(clock.clock(), TestDB::new(), p0cfg.clone(), chain.clone()).await;
    let p1 = start_pm(clock.clock(), TestDB::new(), p1cfg.clone(), chain.clone()).await;
    let v0 = start_pm(clock.clock(), TestDB::new(), v0cfg.clone(), chain.clone()).await;
    let v1 = start_pm(clock.clock(), TestDB::new(), v1cfg.clone(), chain.clone()).await;
    let hub = start_pm(clock.clock(), TestDB::new(), chain.make_config(rng), chain.clone()).await;
    hub.connect_to(&p0.peer_info(), tcp::Tier::T2).await;
    hub.connect_to(&p1.peer_info(), tcp::Tier::T2).await;
    hub.connect_to(&v0.peer_info(), tcp::Tier::T2).await;
    hub.connect_to(&v1.peer_info(), tcp::Tier::T2).await;
    tracing::info!(target:"dupa","p0 = {}",p0cfg.node_id());
    tracing::info!(target:"dupa","hub = {}",hub.cfg.node_id());

    tracing::info!(target:"test", "p0 goes down");
    drop(p0);
    tracing::info!(target:"test", "remaining nodes learn that [v0,v1] are TIER1 nodes");
    let chain_info = peer_manager::testonly::make_chain_info(&chain, &[&v0, &v1]);
    for pm in [&v0, &v1, &p1, &hub] {
        pm.set_chain_info(chain_info.clone()).await;
    }
    tracing::info!(target:"test", "TIER1 connections get established: v0 -> p1 <- v1.");
    establish_connections(&clock.clock(), &[&v0, &v1, &p1, &hub]).await;
    tracing::info!(target:"test", "Send message v1 -> v0 over TIER1.");
    send_tier1_message(rng, &v1, &v0).await;

    // Advance time, so that the new AccountsData has newer timestamp.
    clock.advance(time::Duration::hours(1));

    tracing::info!(target:"test", "p1 goes down.");
    drop(p1);
    tracing::info!(target:"test", "p0 goes up and learns that [v0,v1] are TIER1 nodes.");
    let p0 = start_pm(clock.clock(), TestDB::new(), p0cfg.clone(), chain.clone()).await;
    p0.set_chain_info(chain_info).await;
    hub.connect_to(&p0.peer_info(), tcp::Tier::T2).await;
    tracing::info!(target:"test", "TIER1 connections get established: v0 -> p0 <- v1.");
    establish_connections(&clock.clock(), &[&v0, &v1, &p0, &hub]).await;
    tracing::info!(target:"test", "Send message v1 -> v0 over TIER1.");
    send_tier1_message(rng, &v1, &v0).await;

    drop(hub);
    drop(v0);
    drop(v1);
    drop(p0);
}

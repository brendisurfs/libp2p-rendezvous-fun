use std::error::Error;
use std::time::Duration;

use async_std::process;
use libp2p::futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::rendezvous::client::Behaviour as RendezvousBehaviour;
use libp2p::rendezvous::client::Event as RendezvousEvent;
use libp2p::rendezvous::Namespace;
use libp2p::swarm::{keep_alive, SwarmEvent};
use libp2p::{identify, identity};
use libp2p::{ping, Swarm};
use libp2p::{rendezvous, NetworkBehaviour};
use libp2p::{Multiaddr, PeerId};
use tokio::sync::mpsc;
use void::Void;

#[derive(Debug)]
pub enum Signal {
    Running,
    Stop,
}

#[derive(Debug)]
enum MyEvent {
    Rendezvous(rendezvous::client::Event),
    Identify(identify::Event),
    Ping(ping::Event),
}

impl From<RendezvousEvent> for MyEvent {
    fn from(event: RendezvousEvent) -> Self {
        MyEvent::Rendezvous(event)
    }
}

impl From<identify::Event> for MyEvent {
    fn from(event: identify::Event) -> Self {
        MyEvent::Identify(event)
    }
}

impl From<libp2p::ping::Event> for MyEvent {
    fn from(event: libp2p::ping::Event) -> Self {
        MyEvent::Ping(event)
    }
}

impl From<Void> for MyEvent {
    fn from(event: Void) -> Self {
        void::unreachable(event)
    }
}

#[derive(NetworkBehaviour)]
#[behaviour(event_process = false)]
#[behaviour(out_event = "MyEvent")]
struct MyBehaviour {
    ping: libp2p::ping::Behaviour,
    identify: identify::Behaviour,
    rendezvous: RendezvousBehaviour,
    keep_alive: keep_alive::Behaviour,
}

const NAMESPACE: &str = "rendezvous";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("Hello, world!");
    let rendezvous_namespace = rendezvous::Namespace::new(NAMESPACE.to_string())?;

    let rendezvous_point_addr = "/ip4/127.0.0.1/tcp/62649".parse::<Multiaddr>()?;
    let rendezvous_point =
        "12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN".parse::<PeerId>()?;

    let kp_id = identity::Keypair::generate_ed25519();

    let mut swarm = {
        // create transport.
        let transport = libp2p::development_transport(kp_id.clone()).await?;
        // create behaviour
        let behaviour = MyBehaviour {
            identify: identify::Behaviour::new(identify::Config::new(
                "rendezvous-example/1.0.0".to_string(),
                kp_id.public(),
            )),
            rendezvous: rendezvous::client::Behaviour::new(kp_id.clone()),
            ping: ping::Behaviour::new(ping::Config::new().with_interval(Duration::from_secs(1))),
            keep_alive: keep_alive::Behaviour,
        };
        // create peer id
        let peer_id = PeerId::from(kp_id.public());

        Swarm::new(transport, behaviour, peer_id)
    };
    println!("local peer id: {:?}", swarm.local_peer_id());

    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse().unwrap())?;
    swarm.dial(rendezvous_point_addr)?;

    let (tx, mut rx) = mpsc::channel::<Signal>(32);

    let mut cookie = None;

    // nice ctrlc handler
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.expect("ctrl c error");

        std::process::exit(1);
    });

    loop {
        match swarm.select_next_some().await {
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                if peer_id == rendezvous_point {
                    println!(
                        "connected to rendezvous point, discovering nodes in '{}' namespace ... ",
                        NAMESPACE,
                    );

                    swarm.behaviour_mut().rendezvous.discover(
                        Some(Namespace::new(NAMESPACE.to_string()).unwrap()),
                        None,
                        None,
                        rendezvous_point,
                    );
                }
            }

            SwarmEvent::NewListenAddr {
                listener_id,
                address,
            } => {
                println!("listener id: {:?} address: {:?}", listener_id, address);
            }

            //
            SwarmEvent::ConnectionClosed {
                peer_id,
                cause: Some(error),
                ..
            } => {
                println!("connection closed to {:?}. Reason: {:?}", peer_id, error);
                swarm.behaviour_mut().rendezvous.unregister(
                    Namespace::new(NAMESPACE.to_string()).unwrap(),
                    rendezvous_point,
                );
            }

            // Handle custom events.
            SwarmEvent::Behaviour(my_event) => {
                match my_event {
                    MyEvent::Identify(identify::Event::Sent { peer_id }) => {
                        println!("sent peer id :{:?}", peer_id);
                    }

                    MyEvent::Identify(identify::Event::Received { peer_id, .. }) => {
                        println!("received: {:?}", peer_id);
                        swarm.behaviour_mut().rendezvous.register(
                            rendezvous::Namespace::from_static(NAMESPACE),
                            rendezvous_point,
                            None,
                        );
                    }
                    MyEvent::Rendezvous(rendezvous::client::Event::Discovered {
                        registrations,
                        cookie: new_cookie,
                        ..
                    }) => {
                        cookie.replace(new_cookie);

                        for reg in registrations {
                            for addr in reg.record.addresses() {
                                let peer = reg.record.peer_id();
                                println!("discovered peer {:#?} at {:#?}", peer, addr);

                                let p2p_suffix = Protocol::P2p(*peer.as_ref());
                                let addr_with_p2p = if !addr
                                    .ends_with(&Multiaddr::empty().with(p2p_suffix.clone()))
                                {
                                    addr.clone().with(p2p_suffix)
                                } else {
                                    addr.clone()
                                };

                                swarm.dial(addr_with_p2p).unwrap()
                            }
                        }
                    }
                    // on event register
                    MyEvent::Rendezvous(rendezvous::client::Event::Registered {
                        rendezvous_node,
                        ttl,
                        namespace,
                    }) => {
                        println!(
                            "registered for namespace: {:?} \
                             at rendezvous {} for the next {} seconds",
                            namespace, rendezvous_node, ttl
                        );
                    }
                    // register failed.
                    MyEvent::Rendezvous(rendezvous::client::Event::RegisterFailed(error)) => {
                        println!("failed to register: {:?}", error);
                    }

                    MyEvent::Ping(ping::Event {
                        peer,
                        result: Ok(ping::Success::Ping { rtt }),
                    }) if peer != rendezvous_point => {
                        println!("ping to {} is {}ms", peer, rtt.as_millis());
                    }

                    MyEvent::Ping(ping::Event { peer, result }) => match result {
                        Ok(success) => (),
                        Err(why) => println!("error: {why:?}"),
                    },
                    other_event => println!("other: {:?}", other_event),
                }
            }
            other => {
                println!("unkown: {:?}", other);
                if cookie.is_some() {
                    swarm.behaviour_mut().rendezvous.discover(
                        Some(Namespace::new(NAMESPACE.to_string()).unwrap()),
                        cookie.clone(),
                        None,
                        rendezvous_point,
                    )
                }
            }
        }
    }
}

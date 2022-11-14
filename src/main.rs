use libp2p::core::peer_record;
use libp2p::futures::channel::mpsc;
use libp2p::futures::SinkExt;
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
use std::error::Error;
use std::time::Duration;
use tokio::signal::ctrl_c;
use void::Void;

// Signals we will eventually use to signal start and stop,
// for better unregistering from the network.
#[derive(Debug, PartialEq, PartialOrd)]
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
            ping: ping::Behaviour::new(ping::Config::new().with_interval(Duration::from_secs(10))),
            keep_alive: keep_alive::Behaviour,
        };
        // create peer id
        let peer_id = PeerId::from(kp_id.public());

        Swarm::new(transport, behaviour, peer_id)
    };
    println!("local peer id: {:?}", swarm.local_peer_id());

    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse().unwrap())?;
    swarm.dial(rendezvous_point_addr)?;

    let mut cookie = None;

    let (mut tx, mut rx) = mpsc::channel::<Signal>(32);

    let mut swarm_tx = tx.clone();
    let mut signal_tx = tx.clone();
    // tokio::spawn(async move {
    //     ctrl_c().await.expect("ctrl c error");
    //
    //     signal_tx
    //         .send(Signal::Stop)
    //         .await
    //         .expect("could not send signal");
    // });

    loop {
        // First, we need to check if a ctrl c has been placed so we can sucessfully de-register.
        // match rx.select_next_some().await {
        //     Signal::Stop => {
        //         println!("\nexiting ... ");
        //         tokio::time::sleep(Duration::from_secs(1)).await;
        //         process::exit(1);
        //     }
        // If the signal is not stop, keep going.
        match swarm.select_next_some().await {
            // match any incoming connection.
            SwarmEvent::IncomingConnection {
                local_addr,
                send_back_addr,
            } => {
                println!("incoming: {:?} {:?}", local_addr, send_back_addr);
            }

            // Handle connection established.
            // need to start discovering on connection.
            SwarmEvent::ConnectionEstablished {
                peer_id,
                num_established,
                ..
            } => {
                if peer_id == rendezvous_point {
                    println!(
                        "connected to rendezvous point, discovering nodes in '{}' namespace ... ",
                        NAMESPACE,
                    );

                    println!("number established: {:?}", num_established);

                    swarm.behaviour_mut().rendezvous.discover(
                        Some(Namespace::new(NAMESPACE.to_string()).unwrap()),
                        None,
                        None,
                        rendezvous_point,
                    );
                }
            }

            // If we have a new listener, handle.
            SwarmEvent::NewListenAddr {
                listener_id,
                address,
            } => {
                println!("listener id: {:?} address: {:?}", listener_id, address);
            }

            // Handle connection closed.
            // TODO: unregister sometimes doesnt work right. But probably because of my
            // implementation.
            SwarmEvent::ConnectionClosed {
                peer_id,
                cause: Some(error),
                ..
            } => {
                println!("connection closed to {:?}. Reason: {:#?}", peer_id, error);
            }

            // Handle MyEvents.
            SwarmEvent::Behaviour(my_event) => {
                match my_event {
                    // handle id events.
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
                    // handle rendezvous
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

                                swarm
                                    .dial(addr_with_p2p)
                                    .expect("always able to dial addr with p2p")
                            }
                        }
                    }
                    // handle any expired peers.
                    MyEvent::Rendezvous(rendezvous::client::Event::Expired { peer }) => {
                        println!("peer has expired: {peer:?}");
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
                    }) => {
                        if peer != rendezvous_point {
                            let peers = swarm
                                .connected_peers()
                                .filter(|p| *p != &rendezvous_point)
                                .collect::<Vec<_>>();
                            println!("peers: {:#?}", peers.len());
                            println!("ping to {} is {}ms", peer, rtt.as_millis());
                        } else {
                            ()
                            // println!("pinged rendezvous point: {:?}", peer);
                        }
                    }

                    MyEvent::Ping(ping::Event { peer, result }) => match result {
                        Ok(_) => (),
                        Err(why) => {
                            // match the different reasons why we got a ping error.
                            match why {
                                ping::Failure::Timeout => {
                                    println!("ping timeout to {:?}", peer);
                                    swarm.disconnect_peer_id(peer).expect("disconnect peer");
                                    println!("disconnected peer due to ping error: {:?}", peer);
                                }
                                _ => (),
                            }
                            // swarm
                            //     .behaviour_mut()
                            //     .rendezvous
                            //     .unregister(rendezvous_namespace.clone(), rendezvous_point);

                            println!("error: {why:?}");
                        }
                    },
                    other_event => println!("other: {:?}", other_event),
                }
            }
            SwarmEvent::OutgoingConnectionError { peer_id, error } => {
                let err_desc = error.source();
                println!("error outgoing connection: {:#?}", err_desc);
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

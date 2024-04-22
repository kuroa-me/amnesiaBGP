use crate::fsm::*;
use connection::BgpConn;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::net::{TcpListener, TcpStream};
use tokio::select;
use tokio::sync::{broadcast, mpsc};

mod config;
mod connection;
mod error;
mod fsm;
mod message;
mod session;
mod table;

#[tokio::main]
async fn main() {
    //TODO: Add a listener for admin commands

    let conf = config::Bgp::new();
    // let peers = vec![SocketAddr::new(
    //     IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
    //     179,
    // )];
    let mut sessions: HashMap<IpAddr, mpsc::Sender<Event>> = HashMap::new();

    for (addr, neigh) in conf.neighbors.clone().into_iter() {
        let (admin_tx, admin_rx) = mpsc::channel::<Event>(32);
        // let (conn_tx, conn_rx) = oneshot::channel::<TcpStream>();
        sessions.insert(addr.clone(), admin_tx);
        // let admin_rx = admin_tx.subscribe();
        tokio::spawn(async move {
            session::run(
                State::new(SocketAddr::new(
                    addr.clone(),
                    neigh.config.neighbor_port.clone(),
                )),
                admin_rx,
                neigh,
            )
        });
    }

    let listener = TcpListener::bind("127.0.0.1:179").await.unwrap();
    let (open_tx, mut open_rx) = mpsc::channel::<(SocketAddr, message::OpenMsg)>(32);
    tokio::spawn(async move {
        loop {
            let (sock, open) = open_rx.recv().await.unwrap();
            let session = sessions.get(&sock.ip()).unwrap();
            //Fixme: This is a hack, we should send the message to the bgp channel
            // instead of the admin channel.
            session.send(Event::BGPOpen { msg: open }).await.unwrap();
        }
    });
    loop {
        let (socket, _) = listener.accept().await.unwrap();
        let open_tx = open_tx.clone();
        tokio::spawn(async move {
            process_passive(socket, open_tx).await;
        });
    }
}

// TODO: Redundant code
async fn process_passive(socket: TcpStream, bgp_tx: mpsc::Sender<(SocketAddr, message::OpenMsg)>) {
    let mut conn = BgpConn::new(socket);
    loop {
        match conn.read_message().await {
            Ok(msg) => {
                println!("Received message: {:?}", msg);
                match msg {
                    message::Message::Open(open) => {
                        let sock = conn.peer_sock().unwrap();
                        bgp_tx.send((sock, open)).await.unwrap();
                    }
                    _ => {
                        println!("Not an Open message");
                        break;
                    }
                }
            }
            Err(e) => {
                println!("Error: {:?}", e);
                break;
            }
        }
    }
}

use crate::fsm::*;
use connection::BgpConn;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::net::{TcpListener, TcpStream};
use tokio::select;
use tokio::sync::{broadcast, mpsc, oneshot};

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
    let mut sessions: HashMap<(IpAddr, u32), mpsc::Sender<Event>> = HashMap::new();

    for (addr, neigh) in conf.neighbors.clone().into_iter() {
        let (admin_tx, admin_rx) = mpsc::channel::<Event>(32);
        // let (conn_tx, conn_rx) = oneshot::channel::<TcpStream>();
        sessions.insert((addr.clone(), neigh.config.peer_as), admin_tx);
        // let admin_rx = admin_tx.subscribe();
        tokio::spawn(async move {
            session::run(
                State::new(SocketAddr::new(addr, neigh.config.neighbor_port)),
                admin_rx,
                neigh,
            )
        });
    }

    //TODO: This listener probably will have to cover multiple instances of the BGP router
    let listener = TcpListener::bind("127.0.0.1:179").await.unwrap();
    let (open_tx, mut open_rx) = mpsc::channel::<(BgpConn, message::OpenMsg)>(32);
    tokio::spawn(async move {
        loop {
            let (conn, open) = open_rx.recv().await.unwrap();
            let session = sessions
                .get(&(conn.peer_sock().unwrap().ip(), open.as_number))
                .unwrap();

            //? This is a hack to make the state machine behave. In order for
            //? our implementation to handle multiple remote peers and dynamic
            //? AS numbers, we have to read the AS number from the Open message.
            //? Thus, skipping a few steps in the FSM.
            session
                .send(Event::TcpConnectionConfirmed { bgp_conn: conn })
                .await
                .unwrap();
            session
                .send(Event::BGPOpen {
                    msg: open,
                    remote_syn: true,
                })
                .await
                .unwrap();
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

async fn process_passive(socket: TcpStream, bgp_tx: mpsc::Sender<(BgpConn, message::OpenMsg)>) {
    let mut conn = BgpConn::new(socket);
    let recv_msg: message::OpenMsg;
    match conn.read_message().await {
        //TODO: Timeout
        Ok(msg) => {
            println!("Received message: {:?}", msg);
            match msg {
                message::Message::Open(open) => {
                    // let sock = conn.peer_sock().unwrap();
                    // bgp_tx.send((sock, open)).await.unwrap();
                    recv_msg = open;
                }
                _ => {
                    println!("Not an Open message");
                    return;
                }
            }
        }
        Err(e) => {
            println!("Error: {:?}", e);
            return;
        }
    }
    bgp_tx.send((conn, recv_msg)).await.unwrap();
}

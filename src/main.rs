use crate::fsm::{Active, Connect, Established, Event, Fsm, Idle, OpenConfirm, OpenSent, State};
use connection::BgpConn;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::net::{TcpListener, TcpStream};
use tokio::select;
use tokio::sync::{broadcast, mpsc};

mod connection;
mod error;
mod fsm;
mod message;
mod session;
mod table;

// struct Session {
//     fsm: State,
//     conn: BgpConn,
// }

// struct Chans {
//     admin_rx: broadcast::Receiver<Event>,
//     tcp_tx: mpsc::Sender<Event>,
//     tcp_rx: mpsc::Receiver<Event>,
// }

// struct control_chan {
//     admin_tx: broadcast::Sender<Event>,
//     conn_tx: oneshot::Sender<TcpStream>,
// }

// struct session_chan {
//     admin_rx: broadcast::Receiver<Event>,
//     conn_rx: oneshot::Receiver<TcpStream>,
// }

#[tokio::main]
async fn main() {
    //TODO: Add a listener for admin commands

    let peers = vec![SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        179,
    )];
    let mut sessions = HashMap::new();

    for peer in peers.into_iter() {
        let (admin_tx, admin_rx) = mpsc::channel::<Event>(32);
        // let (conn_tx, conn_rx) = oneshot::channel::<TcpStream>();
        sessions.insert(peer.clone(), admin_tx);
        // let admin_rx = admin_tx.subscribe();
        tokio::spawn(async move {
            session::run(State::new(peer), admin_rx).await;
        });
    }

    // let listener = TcpListener::bind("127.0.0.1:179").await.unwrap();
    // loop {
    //     let (socket, _) = listener.accept().await.unwrap();
    //     tokio::spawn(async move {
    //         process(socket).await;
    //     });
    // }
}

// TODO: Redundant code
async fn process(socket: TcpStream, bgp_tx: mpsc::Sender<Event>) {
    let mut conn = BgpConn::new(socket);
    loop {
        match conn.read_message().await {
            Ok(msg) => {
                println!("Received message: {:?}", msg);
            }
            Err(e) => {
                println!("Error: {:?}", e);
                break;
            }
        }
    }
}

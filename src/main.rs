use connection::BgpConn;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

mod connection;
mod error;
mod message;

#[tokio::main]
async fn main() {
    let listener = TcpListener::bind("127.0.0.1:179").await.unwrap();

    loop {
        let (socket, _) = listener.accept().await.unwrap();
        tokio::spawn(async move {
            process(socket);
        });
    }
}

async fn process(socket: TcpStream) {
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

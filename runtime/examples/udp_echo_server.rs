//! One-datagram local UDP echo server used by the RamLang C ABI integration test.

use std::env;
use tokio::net::UdpSocket;

const MAX_DATAGRAM_SIZE: usize = 65_507;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:39872".to_owned());
    let socket = UdpSocket::bind(&address).await?;
    println!("READY");

    let mut payload = [0_u8; MAX_DATAGRAM_SIZE];
    let (length, peer) = socket.recv_from(&mut payload).await?;
    socket.send_to(&payload[..length], peer).await?;
    Ok(())
}

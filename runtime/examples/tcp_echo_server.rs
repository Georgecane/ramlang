//! One-frame local TCP echo server used by the C ABI integration test.

use std::{env, io};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const MAX_FRAME_SIZE: usize = 1024 * 1024;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:39871".to_owned());
    let listener = TcpListener::bind(&address).await?;
    println!("READY");

    let (mut stream, _) = listener.accept().await?;
    let mut header = [0_u8; 4];
    stream.read_exact(&mut header).await?;
    let length = u32::from_be_bytes(header) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "test frame is too large").into());
    }

    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).await?;
    stream.write_all(&header).await?;
    stream.write_all(&payload).await?;
    stream.shutdown().await?;
    Ok(())
}

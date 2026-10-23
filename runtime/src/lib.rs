//! Async runtime building blocks for RamLang.
//!
//! This crate is a Rust runtime foundation. The current RamLang compiler does not
//! yet lower native async fn / await syntax; see docs/ASYNC_DESIGN.md.
//! Network frames are length-prefixed and bounded to avoid untrusted allocations.

use std::{io, net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

/// A TCP connection with a per-operation timeout.
pub struct TcpConnection {
    stream: TcpStream,
    operation_timeout: Duration,
}

impl TcpConnection {
    /// Connect without blocking the executor thread.
    pub async fn connect(address: &str, operation_timeout: Duration) -> io::Result<Self> {
        let stream = timeout(operation_timeout, TcpStream::connect(address))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TCP connect timed out"))??;
        stream.set_nodelay(true)?;
        Ok(Self { stream, operation_timeout })
    }

    /// Read one big-endian u32 length-prefixed frame, enforcing a maximum size.
    pub async fn read_frame(&mut self, max_frame_size: usize) -> io::Result<Vec<u8>> {
        timeout(self.operation_timeout, async {
            let mut header = [0_u8; 4];
            self.stream.read_exact(&mut header).await?;
            let length = u32::from_be_bytes(header) as usize;
            if length > max_frame_size {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "frame exceeds configured maximum"));
            }
            let mut payload = vec![0_u8; length];
            self.stream.read_exact(&mut payload).await?;
            Ok(payload)
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "frame read timed out"))?
    }

    /// Write one length-prefixed frame.
    pub async fn write_frame(&mut self, payload: &[u8]) -> io::Result<()> {
        let length = u32::try_from(payload.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
        timeout(self.operation_timeout, async {
            self.stream.write_all(&length.to_be_bytes()).await?;
            self.stream.write_all(payload).await?;
            self.stream.flush().await
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "frame write timed out"))?
    }
}

/// Bind a TCP listener asynchronously.
pub async fn listen(address: &str) -> io::Result<TcpListener> {
    TcpListener::bind(address).await
}

/// Accept the next connection and return its peer address.
pub async fn accept(listener: &TcpListener) -> io::Result<(TcpStream, SocketAddr)> {
    listener.accept().await
}

/// Run a future on a managed multi-thread Tokio runtime.
/// Do not call from inside an existing Tokio runtime.
pub fn run<F>(future: F) -> io::Result<F::Output>
where
    F: std::future::Future,
{
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    Ok(runtime.block_on(future))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn frame_round_trip_over_localhost() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = [0_u8; 4];
            stream.read_exact(&mut header).await.unwrap();
            let length = u32::from_be_bytes(header) as usize;
            let mut payload = vec![0; length];
            stream.read_exact(&mut payload).await.unwrap();
            stream.write_all(&header).await.unwrap();
            stream.write_all(&payload).await.unwrap();
        });
        let mut client = TcpConnection::connect(&address.to_string(), Duration::from_secs(2)).await.unwrap();
        client.write_frame(b"RamLang async").await.unwrap();
        assert_eq!(client.read_frame(1024).await.unwrap(), b"RamLang async");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_oversized_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(&2048_u32.to_be_bytes()).await.unwrap();
        });
        let mut client = TcpConnection::connect(&address.to_string(), Duration::from_secs(2)).await.unwrap();
        let error = client.read_frame(1024).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        server.await.unwrap();
    }
}

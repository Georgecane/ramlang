//! Async runtime building blocks for RamLang.
//!
//! This crate is a Rust runtime foundation. The current RamLang compiler does not
//! yet lower native async fn / await syntax; see docs/ASYNC_DESIGN.md.
//! Network frames are length-prefixed and bounded to avoid untrusted allocations.

pub mod ffi;

use std::{io, net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

use tokio::task::{JoinError, JoinSet};

/// A structured-concurrency scope for homogeneous async tasks.
///
/// Dropping a task group aborts all child tasks that have not completed. join_all
/// collects results in completion order; if one task panics or is cancelled, it returns
/// the first JoinError and dropping the remaining scope aborts unfinished children.
pub struct TaskGroup<T: Send + 'static> {
    tasks: JoinSet<T>,
}

impl<T: Send + 'static> Default for TaskGroup<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + 'static> TaskGroup<T> {
    /// Create an empty child-task scope.
    pub fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
        }
    }

    /// Spawn a child task owned by this scope.
    pub fn spawn<F>(&mut self, future: F)
    where
        F: std::future::Future<Output = T> + Send + 'static,
    {
        self.tasks.spawn(future);
    }

    /// Return the number of tasks whose results have not been collected.
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Return whether the scope has no outstanding task results.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Abort all child tasks. Join them with join_next or shutdown to observe exit.
    pub fn abort_all(&mut self) {
        self.tasks.abort_all();
    }

    /// Wait for and collect every child result in completion order.
    ///
    /// Returns the first task failure. Any still-running tasks are aborted when the
    /// scope is dropped after this early return.
    pub async fn join_all(mut self) -> Result<Vec<T>, JoinError> {
        let mut results = Vec::with_capacity(self.tasks.len());
        while let Some(result) = self.tasks.join_next().await {
            results.push(result?);
        }
        Ok(results)
    }

    /// Abort all children and wait for them to exit.
    pub async fn shutdown(&mut self) {
        self.tasks.shutdown().await;
    }
}

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
        Ok(Self {
            stream,
            operation_timeout,
        })
    }

    /// Read one big-endian u32 length-prefixed frame, enforcing a maximum size.
    pub async fn read_frame(&mut self, max_frame_size: usize) -> io::Result<Vec<u8>> {
        timeout(self.operation_timeout, async {
            let mut header = [0_u8; 4];
            self.stream.read_exact(&mut header).await?;
            let length = u32::from_be_bytes(header) as usize;
            if length > max_frame_size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame exceeds configured maximum",
                ));
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

/// A UDP endpoint with bounded datagram reads and per-operation timeouts.
pub struct UdpEndpoint {
    socket: tokio::net::UdpSocket,
    operation_timeout: Duration,
}

impl UdpEndpoint {
    /// Bind a local UDP socket without blocking the executor thread.
    pub async fn bind(address: &str, operation_timeout: Duration) -> io::Result<Self> {
        let socket = timeout(operation_timeout, tokio::net::UdpSocket::bind(address))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "UDP bind timed out"))??;
        Ok(Self {
            socket,
            operation_timeout,
        })
    }

    /// Return the local socket address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Send one UDP datagram, rejecting payloads above the maximum supported datagram size.
    pub async fn send_to(&self, payload: &[u8], address: &str) -> io::Result<usize> {
        const MAX_UDP_PAYLOAD: usize = 65_507;
        if payload.len() > MAX_UDP_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "UDP payload exceeds maximum supported datagram size",
            ));
        }

        timeout(
            self.operation_timeout,
            self.socket.send_to(payload, address),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "UDP send timed out"))?
    }

    /// Receive one datagram, rejecting it if it exceeds the configured maximum.
    ///
    /// One extra byte is read so oversized datagrams can be detected instead of silently
    /// accepting a truncated payload. The maximum buffer is bounded to the UDP payload limit.
    pub async fn recv_from(&self, max_datagram_size: usize) -> io::Result<(Vec<u8>, SocketAddr)> {
        const MAX_UDP_PAYLOAD: usize = 65_507;
        let buffer_size = max_datagram_size.saturating_add(1).min(MAX_UDP_PAYLOAD + 1);
        let mut buffer = vec![0_u8; buffer_size];

        let (length, peer) = timeout(self.operation_timeout, self.socket.recv_from(&mut buffer))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "UDP receive timed out"))??;

        if length > max_datagram_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "UDP datagram exceeds configured maximum",
            ));
        }

        buffer.truncate(length);
        Ok((buffer, peer))
    }
}

/// Run a future on a managed multi-thread Tokio runtime.
/// Do not call from inside an existing Tokio runtime.
pub fn run<F>(future: F) -> io::Result<F::Output>
where
    F: std::future::Future,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    Ok(runtime.block_on(future))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn task_group_joins_child_tasks() {
        let mut tasks = TaskGroup::new();
        tasks.spawn(async { 20_u32 });
        tasks.spawn(async { 22_u32 });

        let mut results = tasks.join_all().await.unwrap();
        results.sort_unstable();
        assert_eq!(results, [20, 22]);
    }

    #[tokio::test]
    async fn task_group_aborts_children_on_shutdown() {
        let mut tasks = TaskGroup::new();
        tasks.spawn(std::future::pending::<()>());
        assert_eq!(tasks.len(), 1);
        tasks.shutdown().await;
        assert!(tasks.is_empty());
    }

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
        let mut client = TcpConnection::connect(&address.to_string(), Duration::from_secs(2))
            .await
            .unwrap();
        client.write_frame(b"RamLang async").await.unwrap();
        assert_eq!(client.read_frame(1024).await.unwrap(), b"RamLang async");
        server.await.unwrap();
    }


    #[tokio::test]
    async fn concurrent_tcp_clients_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut handlers = JoinSet::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                handlers.spawn(async move {
                    let mut header = [0_u8; 4];
                    stream.read_exact(&mut header).await.unwrap();
                    let length = u32::from_be_bytes(header) as usize;
                    let mut payload = vec![0_u8; length];
                    stream.read_exact(&mut payload).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    stream.write_all(&header).await.unwrap();
                    stream.write_all(&payload).await.unwrap();
                });
            }
            while let Some(result) = handlers.join_next().await {
                result.unwrap();
            }
        });

        let mut clients = TaskGroup::new();
        for payload in [b"first".to_vec(), b"second".to_vec()] {
            let address = address.to_string();
            clients.spawn(async move {
                let mut client =
                    TcpConnection::connect(&address, Duration::from_secs(2)).await?;
                client.write_frame(&payload).await?;
                client.read_frame(1024).await
            });
        }

        let results = clients.join_all().await.unwrap();
        let mut payloads = results.into_iter().collect::<io::Result<Vec<_>>>().unwrap();
        payloads.sort();
        assert_eq!(payloads, [b"first".to_vec(), b"second".to_vec()]);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn udp_datagram_round_trip_over_localhost() {
        let receiver = UdpEndpoint::bind("127.0.0.1:0", Duration::from_secs(2))
            .await
            .unwrap();
        let sender = UdpEndpoint::bind("127.0.0.1:0", Duration::from_secs(2))
            .await
            .unwrap();
        let receiver_address = receiver.local_addr().unwrap();

        sender
            .send_to(b"RamLang UDP", &receiver_address.to_string())
            .await
            .unwrap();
        let (payload, peer) = receiver.recv_from(1024).await.unwrap();

        assert_eq!(payload, b"RamLang UDP");
        assert_eq!(peer, sender.local_addr().unwrap());
    }

    #[tokio::test]
    async fn rejects_oversized_udp_datagram() {
        let receiver = UdpEndpoint::bind("127.0.0.1:0", Duration::from_secs(2))
            .await
            .unwrap();
        let sender = UdpEndpoint::bind("127.0.0.1:0", Duration::from_secs(2))
            .await
            .unwrap();

        sender
            .send_to(b"too large", &receiver.local_addr().unwrap().to_string())
            .await
            .unwrap();
        let error = receiver.recv_from(4).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_oversized_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(&2048_u32.to_be_bytes()).await.unwrap();
        });
        let mut client = TcpConnection::connect(&address.to_string(), Duration::from_secs(2))
            .await
            .unwrap();
        let error = client.read_frame(1024).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        server.await.unwrap();
    }
}

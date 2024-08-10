use async_trait::async_trait;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::io::{self, AsyncRead, AsyncReadExt, ReadBuf};

use crate::connection::{AsyncMavConnection, get_socket_addr};
use crate::{write_versioned_msg, MavHeader, MavlinkVersion, Message, async_read_versioned_msg};

use std::collections::VecDeque;
use std::net::{SocketAddr, ToSocketAddrs};
use std::ops::DerefMut;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use crate::async_peek_reader::AsyncPeekReader;

pub async fn select_protocol<M: Message + Sync + Send>(
    address: &str,
) -> io::Result<Box<dyn AsyncMavConnection<M> + Sync + Send>> {
    let connection = if let Some(address) = address.strip_prefix("udpin:") {
        udpin(address).await
    } else if let Some(address) = address.strip_prefix("udpout:") {
        udpout(address).await
    } else if let Some(address) = address.strip_prefix("udpbcast:") {
        udpbcast(address).await
    } else {
        Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "Protocol unsupported",
        ))
    };

    Ok(Box::new(connection?))
}

pub async fn udpbcast<T: ToSocketAddrs>(address: T) -> io::Result<AsyncUdpConnection> {
    let addr = get_socket_addr(address)?;
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.set_broadcast(true)?;
    AsyncUdpConnection::new(socket, false, Some(addr)).await
}

pub async fn udpout<T: ToSocketAddrs>(address: T) -> io::Result<AsyncUdpConnection> {
    let addr = get_socket_addr(address)?;
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    AsyncUdpConnection::new(socket, false, Some(addr)).await
}

pub async fn udpin<T: ToSocketAddrs>(address: T) -> io::Result<AsyncUdpConnection> {
    let addr = get_socket_addr(address)?;
    let socket = UdpSocket::bind(addr).await?;
    AsyncUdpConnection::new(socket, true, None).await
}

struct UdpRead {
    socket: Arc<UdpSocket>,
    buffer: VecDeque<u8>,
    last_recv_address: Option<SocketAddr>,
}

const MTU_SIZE: usize = 1500;
impl AsyncRead for UdpRead {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.buffer.is_empty() {
            let n = std::cmp::min(buf.remaining(), self.buffer.len());
            buf.put_slice(&self.buffer.drain(..n).collect::<Vec<_>>());
            Poll::Ready(Ok(()))
        } else {
            let mut read_buffer = [0u8; MTU_SIZE];
            let mut read_buffer = ReadBuf::new(&mut read_buffer);
            match self.socket.poll_recv_from(cx, &mut read_buffer) {
                Poll::Ready(Ok(address)) => {
                    let n_buffer = read_buffer.filled().len();
                    let n = std::cmp::min(buf.remaining(), n_buffer);
                    buf.put_slice(&read_buffer.filled()[..n]);
                    self.buffer.extend(&read_buffer.filled()[n..n_buffer]);
                    self.last_recv_address = Some(address);
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Pending => Poll::Pending,
            }
        }
    }
}

struct UdpWrite {
    socket: Arc<UdpSocket>,
    dest: Option<SocketAddr>,
    sequence: u8,
}

pub struct AsyncUdpConnection {
    reader: Mutex<AsyncPeekReader<UdpRead>>,
    writer: Mutex<UdpWrite>,
    protocol_version: MavlinkVersion,
    server: bool,
}

impl AsyncUdpConnection {
    async fn new(socket: UdpSocket, server: bool, dest: Option<SocketAddr>) -> io::Result<Self> {
        let socket = Arc::new(socket);
        Ok(Self {
            server,
            reader: Mutex::new(AsyncPeekReader::new(UdpRead {
                socket: socket.clone(),
                buffer: VecDeque::new(),
                last_recv_address: None,
            })),
            writer: Mutex::new(UdpWrite {
                socket,
                dest,
                sequence: 0,
            }),
            protocol_version: MavlinkVersion::V2,
        })
    }
}

#[async_trait]
impl<M: Message + Sync + Send> AsyncMavConnection<M> for AsyncUdpConnection {
    async fn recv(&self) -> Result<(MavHeader, M), crate::error::MessageReadError> {
        let mut reader = self.reader.lock().await;

        loop {
            let result = async_read_versioned_msg(reader.deref_mut(), self.protocol_version).await;
            if self.server {
                if let addr @ Some(_) = reader.reader_ref().last_recv_address {
                    self.writer.lock().await.dest = addr;
                }
            }
            if let ok @ Ok(..) = result {
                return ok;
            }
        }
    }

    async fn send(&self, header: &MavHeader, data: &M) -> Result<usize, crate::error::MessageWriteError> {
        let mut guard = self.writer.lock().await;
        let state = &mut *guard;

        let header = MavHeader {
            sequence: state.sequence,
            system_id: header.system_id,
            component_id: header.component_id,
        };

        state.sequence = state.sequence.wrapping_add(1);

        let len = if let Some(addr) = state.dest {
            let mut buf = Vec::new();
            write_versioned_msg(&mut buf, self.protocol_version, header, data)?;
            state.socket.send_to(&buf, addr).await?
        } else {
            0
        };

        Ok(len)
    }

    fn set_protocol_version(&mut self, version: MavlinkVersion) {
        self.protocol_version = version;
    }

    fn get_protocol_version(&self) -> MavlinkVersion {
        self.protocol_version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_datagram_buffering() {
        let receiver_socket = UdpSocket::bind("127.0.0.1:5000").await.unwrap();
        let receiver_socket = Arc::new(receiver_socket);

        let mut udp_reader = UdpRead {
            socket: receiver_socket,
            buffer: VecDeque::new(),
            last_recv_address: None,
        };
        let sender_socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        sender_socket.connect("127.0.0.1:5000").await.unwrap();

        let datagram: Vec<u8> = (0..50).collect::<Vec<_>>();

        let mut n_sent = sender_socket.send(&datagram).await.unwrap();
        assert_eq!(n_sent, datagram.len());
        n_sent = sender_socket.send(&datagram).await.unwrap();
        assert_eq!(n_sent, datagram.len());

        let mut buf = [0u8; 30];

        let mut n_read = udp_reader.read(&mut buf).await.unwrap();
        assert_eq!(n_read, 30);
        assert_eq!(&buf[0..n_read], (0..30).collect::<Vec<_>>().as_slice());

        n_read = udp_reader.read(&mut buf).await.unwrap();
        assert_eq!(n_read, 20);
        assert_eq!(&buf[0..n_read], (30..50).collect::<Vec<_>>().as_slice());

        n_read = udp_reader.read(&mut buf).await.unwrap();
        assert_eq!(n_read, 30);
        assert_eq!(&buf[0..n_read], (0..30).collect::<Vec<_>>().as_slice());

        n_read = udp_reader.read(&mut buf).await.unwrap();
        assert_eq!(n_read, 20);
        assert_eq!(&buf[0..n_read], (30..50).collect::<Vec<_>>().as_slice());
    }
}

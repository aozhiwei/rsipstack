use super::{
    connection::{SipAddr, TransportSender},
    SipConnection,
};
use crate::{
    transport::{
        connection::{KEEPALIVE_REQUEST, KEEPALIVE_RESPONSE},
        TransportEvent,
    },
    Result,
};
use std::{net::SocketAddr, sync::Arc};
use stun_rs::{
    attributes::stun::XorMappedAddress, methods::BINDING, MessageClass, MessageDecoderBuilder,
    MessageEncoderBuilder, StunMessageBuilder,
};
use tokio::net::{lookup_host, UdpSocket};
use tracing::{debug, error, info, instrument, trace};
struct UdpInner {
    pub(self) conn: UdpSocket,
    pub(self) addr: SipAddr,
}

#[derive(Clone)]
pub struct UdpConnection {
    external: Option<SipAddr>,
    inner: Arc<UdpInner>,
}

impl UdpConnection {
    pub async fn create_connection(
        local: SocketAddr,
        external: Option<SocketAddr>,
    ) -> Result<Self> {
        let conn = UdpSocket::bind(local).await?;

        let addr = SipAddr {
            r#type: Some(rsip::transport::Transport::Udp),
            addr: conn.local_addr()?,
        };

        let t = UdpConnection {
            external: external.map(|addr| SipAddr {
                r#type: Some(rsip::transport::Transport::Udp),
                addr,
            }),
            inner: Arc::new(UdpInner { addr, conn }),
        };
        info!("created UDP connection: {} external: {:?}", t, external);
        Ok(t)
    }

    pub async fn external_by_stun(&mut self, stun_server: String) -> Result<SocketAddr> {
        info!("getting external IP by STUN server: {}", stun_server);
        let msg = StunMessageBuilder::new(BINDING, MessageClass::Request).build();

        let encoder = MessageEncoderBuilder::default().build();
        let mut buffer: [u8; 150] = [0x00; 150];
        encoder.encode(&mut buffer, &msg).map_err(|e| {
            crate::Error::TransportLayerError(e.to_string(), self.get_addr().to_owned())
        })?;

        let mut addrs = lookup_host(stun_server).await?;
        let target = addrs.next().ok_or_else(|| {
            crate::Error::TransportLayerError(
                "STUN server address not found".to_string(),
                self.get_addr().to_owned(),
            )
        })?;

        self.send_raw(
            &buffer,
            SipAddr {
                addr: target,
                r#type: None,
            },
        )
        .await?;

        let buf = &mut [0u8; 2048];
        self.recv_raw(buf).await?;

        let decoder = MessageDecoderBuilder::default().build();
        let (resp, _) = decoder.decode(buf).map_err(|e| {
            crate::Error::TransportLayerError(e.to_string(), self.get_addr().to_owned())
        })?;

        let xor_addr = resp
            .get::<XorMappedAddress>()
            .ok_or(crate::Error::TransportLayerError(
                "XorMappedAddress attribute not found".to_string(),
                self.get_addr().to_owned(),
            ))?
            .as_xor_mapped_address()
            .map_err(|e| {
                crate::Error::TransportLayerError(e.to_string(), self.get_addr().to_owned())
            })?;
        let socket = xor_addr.socket_address();
        info!("external IP: {}", socket);
        self.external = Some(SipAddr {
            r#type: Some(rsip::transport::Transport::Udp),
            addr: socket.clone(),
        });
        Ok(socket.clone())
    }

    pub async fn serve_loop(&self, sender: TransportSender) -> Result<()> {
        let mut buf = vec![0u8; 2048];
        loop {
            let (len, addr) = match self.inner.conn.recv_from(&mut buf).await {
                Ok((len, addr)) => (len, addr),
                Err(e) => {
                    error!("error receiving UDP packet: {}", e);
                    continue;
                }
            };

            match &buf[..len] {
                KEEPALIVE_REQUEST => {
                    self.inner.conn.send_to(KEEPALIVE_RESPONSE, addr).await.ok();
                    continue;
                }
                KEEPALIVE_RESPONSE => continue,
                _ => {
                    if buf.iter().all(|&b| b.is_ascii_whitespace()) {
                        continue;
                    }
                }
            }

            let undecoded = match std::str::from_utf8(&buf[..len]) {
                Ok(s) => s,
                Err(e) => {
                    info!(
                        "decoding text from: {} error: {} buf: {:?}",
                        addr,
                        e,
                        &buf[..len]
                    );
                    continue;
                }
            };

            let msg = match rsip::SipMessage::try_from(undecoded) {
                Ok(msg) => msg,
                Err(e) => {
                    info!(
                        "error parsing SIP message from: {} error: {} buf: {}",
                        addr, e, undecoded
                    );
                    continue;
                }
            };

            let msg = match SipConnection::update_msg_received(msg, addr) {
                Ok(msg) => msg,
                Err(e) => {
                    info!(
                        "error updating SIP via from: {} error: {:?} buf: {}",
                        addr, e, undecoded
                    );
                    continue;
                }
            };

            debug!(
                "received {} {} -> {} {}",
                len,
                addr,
                self.get_addr(),
                undecoded
            );

            sender.send(TransportEvent::Incoming(
                msg,
                SipConnection::Udp(self.clone()),
                SipAddr {
                    r#type: Some(rsip::transport::Transport::Udp),
                    addr,
                },
            ))?;
        }
    }

    #[instrument(skip(self, msg), fields(addr = %self.get_addr()))]
    pub async fn send(&self, msg: rsip::SipMessage) -> crate::Result<()> {
        let target = SipConnection::get_target_socketaddr(&msg)?;
        let buf = msg.to_string();

        trace!("sending {} -> {} {}", buf.len(), target, buf);

        self.inner
            .conn
            .send_to(buf.as_bytes(), target)
            .await
            .map_err(|e| {
                crate::Error::TransportLayerError(e.to_string(), self.get_addr().to_owned())
            })
            .map(|_| ())
    }

    #[instrument(skip(self, buf), fields(addr = %self.get_addr()))]
    pub async fn send_raw(&self, buf: &[u8], target: SipAddr) -> Result<()> {
        trace!("sending {} -> {}", buf.len(), target);
        self.inner
            .conn
            .send_to(buf, target.addr)
            .await
            .map_err(|e| {
                crate::Error::TransportLayerError(e.to_string(), self.get_addr().to_owned())
            })
            .map(|_| ())
    }

    #[instrument(skip(self, buf), fields(addr = %self.get_addr()))]
    pub async fn recv_raw(&self, buf: &mut [u8]) -> Result<(usize, SipAddr)> {
        let (len, addr) = self.inner.conn.recv_from(buf).await?;
        trace!("received {} -> {}", len, addr);
        Ok((
            len,
            SipAddr {
                r#type: Some(rsip::transport::Transport::Udp),
                addr,
            },
        ))
    }

    pub fn get_addr(&self) -> &SipAddr {
        if let Some(external) = &self.external {
            external
        } else {
            &self.inner.addr
        }
    }
}

impl std::fmt::Display for UdpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.inner.conn.local_addr() {
            Ok(addr) => write!(f, "{}", addr),
            Err(_) => write!(f, "*:*"),
        }
    }
}

impl std::fmt::Debug for UdpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.inner.addr)
    }
}

impl Drop for UdpInner {
    fn drop(&mut self) {
        info!("dropping UDP transport: {}", self.addr);
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        transport::{
            connection::{KEEPALIVE_REQUEST, KEEPALIVE_RESPONSE},
            udp::UdpConnection,
            TransportEvent,
        },
        Result,
    };
    use std::time::Duration;
    use tokio::{select, sync::mpsc::unbounded_channel, time::sleep};
    use tracing::info;

    #[tokio::test]
    async fn test_udp_keepalive() -> Result<()> {
        let peer_bob = UdpConnection::create_connection("127.0.0.1:0".parse()?, None).await?;
        let peer_alice = UdpConnection::create_connection("127.0.0.1:0".parse()?, None).await?;
        let (alice_tx, _) = unbounded_channel();

        let bob_loop = async {
            sleep(Duration::from_millis(20)).await; // wait for serve_loop to start
                                                    // send keep alive
            peer_bob
                .send_raw(KEEPALIVE_REQUEST, peer_alice.get_addr().to_owned())
                .await
                .expect("send_raw");
            // wait for keep alive response
            let buf = &mut [0u8; 2048];
            let (n, _) = peer_bob.recv_raw(buf).await.expect("recv_raw");
            assert_eq!(&buf[..n], KEEPALIVE_RESPONSE);
        };

        select! {
            _ = peer_alice.serve_loop(alice_tx) => {
                assert!(false, "serve_loop exited");
            }
            _ = bob_loop => {}
            _= sleep(Duration::from_millis(200)) => {
                assert!(false, "timeout waiting for keep alive response");
            }
        };
        Ok(())
    }

    #[tokio::test]
    async fn test_udp_recv_sip_message() -> Result<()> {
        let peer_bob = UdpConnection::create_connection("127.0.0.1:0".parse()?, None).await?;
        let peer_alice = UdpConnection::create_connection("127.0.0.1:0".parse()?, None).await?;
        let (alice_tx, _) = unbounded_channel();
        let (bob_tx, mut bob_rx) = unbounded_channel();

        let send_loop = async {
            sleep(Duration::from_millis(20)).await; // wait for serve_loop to start
            let msg_1 = "REGISTER sip:bob@restsend.com SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5061;branch=z9hG4bKnashd92\r\nCSeq: 1 REGISTER\r\n\r\n";
            peer_alice
                .send_raw(msg_1.as_bytes(), peer_bob.get_addr().to_owned())
                .await
                .expect("send_raw");
            sleep(Duration::from_secs(3)).await;
        };

        select! {
            _ = peer_alice.serve_loop(alice_tx) => {
                assert!(false, "alice serve_loop exited");
            }
            _ = peer_bob.serve_loop(bob_tx) => {
                assert!(false, "bob serve_loop exited");
            }
            _ = send_loop => {
                assert!(false, "send_loop exited");
            }
            event = bob_rx.recv() => {
                match event {
                    Some(TransportEvent::Incoming(msg, connection, from)) => {
                        assert!(msg.is_request());
                        assert_eq!(from, peer_alice.get_addr().to_owned());
                        assert_eq!(connection.get_addr(), peer_bob.get_addr());
                    }
                    _ => {
                        assert!(false, "unexpected event");
                    }
                }
            }
            _= sleep(Duration::from_millis(500)) => {
                assert!(false, "timeout waiting");
            }
        };
        Ok(())
    }

    #[tokio::test]
    async fn test_external_with_stun() -> Result<()> {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_file(true)
            .with_line_number(true)
            .with_timer(tracing_subscriber::fmt::time::LocalTime::rfc_3339())
            .try_init()
            .ok();

        let addrs = tokio::net::lookup_host("restsend.com:3478").await?;
        for addr in addrs {
            info!("stun server: {}", addr);
        }
        let mut peer_bob = UdpConnection::create_connection("127.0.0.1:0".parse()?, None).await?;
        let addr = peer_bob
            .external_by_stun("restsend.com:3478".to_string())
            .await
            .expect("external_by_stun");
        info!("external IP: {}", addr);
        Ok(())
    }
}

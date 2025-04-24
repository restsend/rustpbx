use anyhow::Result;
use get_if_addrs::get_if_addrs;
use rsipstack::transport::{udp::UdpConnection, SipAddr};
use std::{
    io::BufReader,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tracing::info;
use webrtc::stun::{
    agent::TransactionId,
    message::{Getter, Message, BINDING_REQUEST},
    xoraddr::XorMappedAddress,
};

pub fn get_first_non_loopback_interface() -> Result<IpAddr> {
    get_if_addrs()?
        .iter()
        .find(|i| !i.is_loopback())
        .map(|i| match i.addr {
            get_if_addrs::IfAddr::V4(ref addr) => Ok(std::net::IpAddr::V4(addr.ip)),
            _ => Err(anyhow::anyhow!("No IPv4 address found")),
        })
        .unwrap_or(Err(anyhow::anyhow!("No interface found")))
}

pub async fn external_by_stun(
    conn: &mut UdpConnection,
    stun_server: &str,
    expires: Duration,
) -> Result<SocketAddr> {
    let mut msg = Message::new();
    msg.build(&[Box::<TransactionId>::default(), Box::new(BINDING_REQUEST)])?;
    let mut addrs = tokio::net::lookup_host(stun_server).await?;
    let target = addrs
        .next()
        .ok_or_else(|| anyhow::anyhow!("STUN server address not found"))?;

    info!("getting external IP by STUN server: {}", stun_server);

    let buffer = msg.raw;
    match conn
        .send_raw(
            &buffer,
            &SipAddr {
                addr: target.into(),
                r#type: None,
            },
        )
        .await
    {
        Ok(_) => (),
        Err(e) => {
            info!("failed to send stun request: {}", e);
            return Err(anyhow::anyhow!(e));
        }
    }
    let mut buf = [0u8; 2048];
    let (len, _) = match tokio::time::timeout(expires, conn.recv_raw(&mut buf)).await {
        Ok(r) => r.map_err(|e| anyhow::anyhow!(e))?,
        Err(e) => {
            info!("stun timeout {}", stun_server);
            return Err(anyhow::anyhow!(e));
        }
    };
    let mut msg = Message::new();
    let mut reader = BufReader::new(&buf[..len]);
    msg.read_from(&mut reader)?;

    let mut xor_addr = XorMappedAddress::default();
    xor_addr.get_from(&msg)?;
    let external = SocketAddr::new(xor_addr.ip, xor_addr.port);

    info!("external address: {} -> {}", external, conn.get_addr());
    conn.external = Some(SipAddr {
        r#type: Some(rsip::transport::Transport::Udp),
        addr: external.to_owned().into(),
    });
    Ok(external)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsipstack::transport::udp::UdpConnection;
    use std::net::{Ipv4Addr, SocketAddr};

    #[tokio::test]
    async fn test_external_by_stun() -> Result<()> {
        tracing_subscriber::fmt::try_init().ok();
        let mut conn = UdpConnection::create_connection("0.0.0.0:0".parse()?, None)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;

        let external = external_by_stun(&mut conn, "restsend.com:3478", Duration::from_secs(5))
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        assert_ne!(
            external,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0)
        );
        info!("conn address: {}", conn.get_addr());
        Ok(())
    }
}

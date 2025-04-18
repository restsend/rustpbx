use anyhow::Result;
use get_if_addrs::get_if_addrs;
use rsipstack::transport::{udp::UdpConnection, SipAddr};
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use stun_rs::{
    attributes::stun::XorMappedAddress, methods::BINDING, MessageClass, MessageDecoderBuilder,
    MessageEncoderBuilder, StunMessageBuilder,
};
use tracing::info;

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
    info!("getting external IP by STUN server: {}", stun_server);
    let msg = StunMessageBuilder::new(BINDING, MessageClass::Request).build();

    let encoder = MessageEncoderBuilder::default().build();
    let mut buffer: [u8; 150] = [0x00; 150];
    encoder
        .encode(&mut buffer, &msg)
        .map_err(|e| anyhow::anyhow!(e))?;

    let mut addrs = tokio::net::lookup_host(stun_server).await?;
    let target = addrs
        .next()
        .ok_or_else(|| anyhow::anyhow!("STUN server address not found"))?;

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

    let decoder = MessageDecoderBuilder::default().build();
    let (resp, _) = decoder
        .decode(&buf[..len])
        .map_err(|e| anyhow::anyhow!(e))?;

    let xor_addr = resp
        .get::<XorMappedAddress>()
        .ok_or(anyhow::anyhow!("XorMappedAddress attribute not found"))?
        .as_xor_mapped_address()
        .map_err(|e| anyhow::anyhow!(e))?;

    let external: &SocketAddr = xor_addr.socket_address();
    info!(
        "useragent: external address: {} -> {}",
        external,
        conn.get_addr()
    );
    conn.external = Some(SipAddr {
        r#type: Some(rsip::transport::Transport::Udp),
        addr: external.to_owned().into(),
    });
    Ok(external.clone())
}

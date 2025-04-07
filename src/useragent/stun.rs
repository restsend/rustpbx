use anyhow::Result;
use get_if_addrs::get_if_addrs;
use std::net::IpAddr;

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

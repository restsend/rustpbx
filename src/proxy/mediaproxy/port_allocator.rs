use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// Statistics for port allocation
#[derive(Clone, Debug)]
pub struct PortAllocatorStats {
    pub total_ports: u16,
    pub allocated_ports: usize,
    pub available_ports: usize,
}

/// Port allocator for managing RTP/RTCP port assignments
pub struct PortAllocator {
    start_port: u16,
    end_port: u16,
    allocated_ports: Arc<RwLock<HashSet<u16>>>,
}

impl PortAllocator {
    /// Create a new port allocator with the specified range
    pub fn new(start_port: u16, end_port: u16) -> Self {
        if start_port >= end_port {
            panic!("Invalid port range: start_port must be less than end_port");
        }

        if start_port % 2 != 0 {
            panic!("start_port must be even for RTP/RTCP port pairing");
        }

        Self {
            start_port,
            end_port,
            allocated_ports: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Allocate an even port number for RTP (RTCP will use port+1)
    pub async fn allocate_port(&self) -> Result<u16> {
        let mut allocated = self.allocated_ports.write().await;

        // Try to find an available even port
        for port in (self.start_port..self.end_port).step_by(2) {
            if !allocated.contains(&port) && !allocated.contains(&(port + 1)) {
                // Check if we have space for both RTP and RTCP ports
                if port + 1 <= self.end_port {
                    allocated.insert(port);
                    allocated.insert(port + 1);
                    debug!("Allocated port pair: RTP={}, RTCP={}", port, port + 1);
                    return Ok(port);
                }
            }
        }

        Err(anyhow!(
            "No available ports in range {}-{}",
            self.start_port,
            self.end_port
        ))
    }

    /// Allocate a specific port if available
    pub async fn allocate_specific_port(&self, port: u16) -> Result<()> {
        if port < self.start_port || port > self.end_port {
            return Err(anyhow!(
                "Port {} is outside allowed range {}-{}",
                port,
                self.start_port,
                self.end_port
            ));
        }

        if port % 2 != 0 {
            return Err(anyhow!("Port {} must be even for RTP/RTCP pairing", port));
        }

        if port + 1 > self.end_port {
            return Err(anyhow!(
                "Cannot allocate RTCP port {} (out of range)",
                port + 1
            ));
        }

        let mut allocated = self.allocated_ports.write().await;

        if allocated.contains(&port) || allocated.contains(&(port + 1)) {
            return Err(anyhow!("Port pair {}/{} already allocated", port, port + 1));
        }

        allocated.insert(port);
        allocated.insert(port + 1);
        debug!(
            "Allocated specific port pair: RTP={}, RTCP={}",
            port,
            port + 1
        );
        Ok(())
    }

    /// Deallocate a port pair (RTP and RTCP)
    pub async fn deallocate_port(&self, rtp_port: u16) -> Result<()> {
        if rtp_port % 2 != 0 {
            warn!("Attempting to deallocate odd port number: {}", rtp_port);
            return Err(anyhow!("RTP port must be even"));
        }

        let mut allocated = self.allocated_ports.write().await;
        let rtcp_port = rtp_port + 1;

        let rtp_removed = allocated.remove(&rtp_port);
        let rtcp_removed = allocated.remove(&rtcp_port);

        if rtp_removed || rtcp_removed {
            debug!(
                "Deallocated port pair: RTP={}, RTCP={}",
                rtp_port, rtcp_port
            );
            Ok(())
        } else {
            Err(anyhow!(
                "Port pair {}/{} was not allocated",
                rtp_port,
                rtcp_port
            ))
        }
    }

    /// Check if a port is allocated
    pub async fn is_port_allocated(&self, port: u16) -> bool {
        self.allocated_ports.read().await.contains(&port)
    }

    /// Get statistics about port allocation
    pub async fn get_stats(&self) -> PortAllocatorStats {
        let allocated = self.allocated_ports.read().await;
        let total_ports = self.end_port - self.start_port + 1;
        let allocated_count = allocated.len();

        PortAllocatorStats {
            total_ports,
            allocated_ports: allocated_count,
            available_ports: total_ports as usize - allocated_count,
        }
    }

    /// Get list of allocated ports
    pub async fn get_allocated_ports(&self) -> Vec<u16> {
        let allocated = self.allocated_ports.read().await;
        let mut ports: Vec<u16> = allocated.iter().cloned().collect();
        ports.sort();
        ports
    }

    /// Clear all allocated ports (useful for testing or cleanup)
    pub async fn clear_all(&self) {
        self.allocated_ports.write().await.clear();
        debug!("Cleared all allocated ports");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_port_allocator_basic() {
        let allocator = PortAllocator::new(20000, 20010);

        // Test basic allocation
        let port1 = allocator.allocate_port().await.unwrap();
        assert_eq!(port1, 20000);
        assert!(allocator.is_port_allocated(20000).await);
        assert!(allocator.is_port_allocated(20001).await);

        // Test second allocation
        let port2 = allocator.allocate_port().await.unwrap();
        assert_eq!(port2, 20002);

        // Test stats
        let stats = allocator.get_stats().await;
        assert_eq!(stats.allocated_ports, 4); // 2 port pairs
        assert_eq!(stats.total_ports, 11);
    }

    #[tokio::test]
    async fn test_port_allocator_specific() {
        let allocator = PortAllocator::new(20000, 20010);

        // Allocate specific port
        allocator.allocate_specific_port(20004).await.unwrap();
        assert!(allocator.is_port_allocated(20004).await);
        assert!(allocator.is_port_allocated(20005).await);

        // Try to allocate the same port again (should fail)
        assert!(allocator.allocate_specific_port(20004).await.is_err());

        // Try to allocate overlapping port (should fail)
        assert!(allocator.allocate_specific_port(20005).await.is_err());
    }

    #[tokio::test]
    async fn test_port_allocator_deallocation() {
        let allocator = PortAllocator::new(20000, 20010);

        let port = allocator.allocate_port().await.unwrap();
        assert!(allocator.is_port_allocated(port).await);

        allocator.deallocate_port(port).await.unwrap();
        assert!(!allocator.is_port_allocated(port).await);
        assert!(!allocator.is_port_allocated(port + 1).await);

        // Should be able to allocate the same port again
        let port2 = allocator.allocate_port().await.unwrap();
        assert_eq!(port, port2);
    }

    #[tokio::test]
    async fn test_port_allocator_exhaustion() {
        let allocator = PortAllocator::new(20000, 20002);

        // Should be able to allocate one port pair (20000, 20001)
        let port1 = allocator.allocate_port().await.unwrap();
        assert_eq!(port1, 20000);

        // Next allocation should fail as 20002 would need 20003 which is out of range
        assert!(allocator.allocate_port().await.is_err());
    }

    #[tokio::test]
    async fn test_invalid_port_range() {
        // Test panic on invalid range
        std::panic::catch_unwind(|| {
            PortAllocator::new(20001, 20000);
        })
        .unwrap_err();

        // Test panic on odd start port
        std::panic::catch_unwind(|| {
            PortAllocator::new(20001, 20010);
        })
        .unwrap_err();
    }
}

# MediaProxy Implementation

This document describes the MediaProxy functionality implemented in `rustpbx`.

## Overview

MediaProxy provides transparent RTP media forwarding for SIP calls, allowing the PBX to handle scenarios where direct media flow between endpoints is not possible due to NAT, firewalls, or network topology constraints.

## Configuration

### MediaProxyMode

The MediaProxy supports three operating modes:

- **None**: No media proxying (default)
- **NatOnly**: Only proxy media when private IP addresses are detected in SDP
- **All**: Proxy all media traffic regardless of IP addresses

### Configuration Example

```toml
[proxy.media_proxy]
mode = "nat_only"           # Options: "none", "nat_only", "all"
rtp_start_port = 20000      # Start of RTP port range
rtp_end_port = 30000        # End of RTP port range
external_ip = "192.168.1.1" # External IP address for proxy
# force_proxy = ["192.168.1.100"]  # Optional: Always proxy these IPs
```

### Module Order

**Important**: MediaProxy must be loaded BEFORE the call module to properly intercept SDP:

```toml
[proxy]
modules = ["ban", "auth", "registrar", "cdr", "mediaproxy", "call"]
```

## Architecture

### Components

#### 1. MediaSession
Tracks call sessions with:
- Call ID and tags
- SDP offer/answer content  
- RTP addresses for both parties
- Allocated proxy ports
- Proxy task handles

#### 2. RtpPortManager
Manages RTP port allocation:
- Allocates ports in pairs (for RTP + RTCP)
- Tracks port assignments per session
- Handles port deallocation on call teardown

#### 3. RtpProxy  
Handles UDP packet forwarding:
- Bidirectional RTP packet relay
- Asynchronous packet forwarding
- Cancellation token support for cleanup

#### 4. MediaProxyModule
Main proxy logic:
- SDP interception and modification
- Session lifecycle management
- Decision logic based on configuration

## Operation Flow

### Call Setup

1. **INVITE Processing**:
   - Extracts SDP from INVITE request
   - Checks if proxying is needed based on mode and IP analysis
   - Allocates RTP port for caller
   - Modifies SDP to use proxy IP/port
   - Updates Content-Length header

2. **200 OK Processing**:
   - Extracts SDP from 200 OK response
   - Checks proxying decision against both SDPs
   - Allocates RTP port for callee  
   - Starts RTP forwarding tasks
   - Note: Response SDP modification needs special handling

3. **Media Forwarding**:
   - Creates RtpProxy instances for each direction
   - Spawns async tasks for packet forwarding
   - Handles bidirectional media flow transparently

### Call Teardown

1. **BYE Processing**:
   - Stops RTP forwarding tasks
   - Deallocates reserved ports
   - Cleans up session state

## Key Features

### Intelligent Proxying
- **NAT Detection**: Automatically detects private IP addresses (RFC 1918, RFC 4193)
- **SDP Analysis**: Parses SDP content to extract media endpoints  
- **Configuration-Driven**: Behavior controlled by configuration mode

### Port Management
- **Port Pairs**: Allocates consecutive port pairs (RTP + RTCP)
- **Range Management**: Configurable port range allocation
- **Automatic Cleanup**: Deallocates ports on session end

### Network Utilities
- **Private IP Detection**: `is_private_ip()` for RFC compliance
- **SDP Parsing**: Extracts RTP addresses from SDP content
- **Address Validation**: Comprehensive IP address analysis

## Testing

The implementation includes comprehensive unit tests:

```bash
cargo test mediaproxy
```

Tests cover:
- Proxy mode decision logic
- Port management operations  
- SDP parsing and modification
- Network utility functions

## Limitations and Notes

1. **Response Modification**: Current implementation logs intended SDP modifications for responses but cannot modify immutable responses directly
2. **Module Order**: MediaProxy must be positioned before call module in configuration
3. **External IP**: Requires manual configuration of external IP address
4. **Port Range**: Ensure sufficient port range for expected concurrent calls

## Future Enhancements

1. **Dynamic External IP**: Automatic external IP detection via STUN
2. **Response Modification**: Proper response interception and modification
3. **Media Recording**: Integration with call recording functionality
4. **Codec Transcoding**: Support for codec translation between endpoints
5. **Load Balancing**: Multiple proxy instances for high availability

## Configuration Best Practices

1. Set appropriate port range based on expected concurrent calls
2. Configure external IP to match your network topology
3. Use "nat_only" mode for most deployments to minimize unnecessary proxying
4. Monitor port usage to avoid exhaustion
5. Ensure firewall rules allow RTP traffic on configured port range

## Error Handling

The MediaProxy includes comprehensive error handling:
- Graceful fallback on port allocation failures
- Proper cleanup on task failures
- Detailed logging for troubleshooting
- Session state consistency maintenance 
# RustPBX Configuration Guide

RustPBX configuration is split into several logical sections. The application loads its main configuration from `rustpbx.toml` by default, or from a path specified by the `--conf` argument. The format is **TOML**.

## Navigation

1. [Overview & Concepts](config/00-overview.md)  
   *File structure, reload behavior, generated configs.*

2. [Platform & Networking](config/01-platform.md)  
   *HTTP, Logging, Database, RTP, NAT, ICE.*

3. [Proxy Core](config/02-proxy-core.md)  
   *Binding ports, Transport (UDP/TCP/TLS/WS), Concurrency, Modules.*

4. [Authentication & Users](config/03-auth-users.md)  
   *User Backends (Memory, DB, HTTP), Locators, Realms.*

5. [Routing](config/04-routing.md)  
   *Static Routes, Regex Matching, Rewrites, HTTP Dynamic Router.*

6. [Trunks & Queues](config/05-trunks-queues.md)  
   *SIP Gateways, Load Balancing, Queue Strategies, Agent Management.*

7. [Media, Recording & CDR](config/06-media-recording.md)  
   *Media Proxy, Recording Policies, Storage Backends (Local/S3).*

8. [Addons, Console & Admin](config/07-addons-admin-storage.md)  
   *Web Console, AMI, Archiving, Wholesale & Custom Addons.*

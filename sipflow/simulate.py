import socket
import struct
import time

def send_packet(msg_type, payload, callid=None):
    # Header format:
    # Magic Header (u16) | Version (u8) | MsgType (u8) | IP Family (u8) | Src IP (4) | Src Port (u16) | Dst IP (4) | Dst Port (u16) | Timestamp (u64) | Payload Len (u32)
    magic = 0x5346
    version = 1
    ip_family = 4
    src_ip = socket.inet_aton("127.0.0.1")
    src_port = 5060
    dst_ip = socket.inet_aton("127.0.0.1")
    dst_port = 5060
    timestamp = int(time.time() * 1000000)
    
    if msg_type == 0: # SIP
        if callid:
            payload = f"INVITE sip:bob@example.com SIP/2.0\r\nCall-ID: {callid}\r\n\r\n".encode() + payload
    
    payload_len = len(payload)
    
    header = struct.pack(">HBBB4sH4sHQI", magic, version, msg_type, ip_family, src_ip, src_port, dst_ip, dst_port, timestamp, payload_len)
    
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.sendto(header + payload, ("127.0.0.1", 3000))

def send_sip_with_sdp(sock, callid):
    magic = 0x5346
    version = 1
    ip_family = 4
    src_ip = socket.inet_aton("127.0.0.1")
    src_port = 5060
    dst_ip = socket.inet_aton("127.0.0.1")
    dst_port = 5060
    timestamp = int(time.time() * 1000000)
    
    sdp = (
        "v=0\r\n"
        "o=- 123 456 IN IP4 127.0.0.1\r\n"
        "s=-\r\n"
        "c=IN IP4 127.0.0.1\r\n"
        "t=0 0\r\n"
        "m=audio 10000 RTP/AVP 0\r\n"
    )
    payload = f"INVITE sip:bob@example.com SIP/2.0\r\nCall-ID: {callid}\r\nContent-Type: application/sdp\r\nContent-Length: {len(sdp)}\r\n\r\n{sdp}".encode()
    
    header = struct.pack(">HBBB4sH4sHQI", magic, version, 0, ip_family, src_ip, src_port, dst_ip, dst_port, timestamp, len(payload))
    sock.sendto(header + payload, ("127.0.0.1", 3000))

    # 200 OK
    payload_ok = f"SIP/2.0 200 OK\r\nCall-ID: {callid}\r\nContent-Type: application/sdp\r\nContent-Length: {len(sdp)}\r\n\r\n{sdp}".encode()
    sock.sendto(header + payload_ok, ("127.0.0.1", 3000))

def send_rtp(sock, count):
    magic = 0x5346
    version = 1
    ip_family = 4
    src_ip = socket.inet_aton("127.0.0.1")
    src_port = 10000
    dst_ip = socket.inet_aton("127.0.0.1")
    dst_port = 10000
    
    for i in range(count):
        timestamp = int(time.time() * 1000000)
        # RTP Header (12 bytes) + 80 samples of G.711
        rtp_payload = struct.pack(">BBHII", 0x80, 0x00, i, i*160, 0x12345678) + b"\x55" * 80
        header = struct.pack(">HBBB4sH4sHQI", magic, version, 1, ip_family, src_ip, src_port, dst_ip, dst_port, timestamp, len(rtp_payload))
        sock.sendto(header + rtp_payload, ("127.0.0.1", 3000))
        time.sleep(0.02)

if __name__ == "__main__":
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    cid = "record-test-999"
    print(f"Sending SIP with SDP for {cid}...")
    send_sip_with_sdp(sock, cid)
    print("Sending RTP...")
    send_rtp(sock, 50) # 1 second of audio
    print("Done. Please wait for flush and check /media?callid=" + cid)

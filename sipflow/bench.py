import socket
import struct
import time
import argparse
import concurrent.futures

def send_packet(sock, msg_type, payload, callid=None):
    magic = 0x5346
    version = 1
    ip_family = 4
    src_ip = socket.inet_aton("127.0.0.1")
    src_port = 5060
    dst_ip = socket.inet_aton("127.0.0.1")
    dst_port = 5060
    timestamp = int(time.time() * 1000000)
    
    if msg_type == 0 and callid:
        payload = f"INVITE sip:bob@example.com SIP/2.0\r\nCall-ID: {callid}\r\n\r\n".encode() + payload
    
    payload_len = len(payload)
    header = struct.pack(">HBBB4sH4sHQI", magic, version, msg_type, ip_family, src_ip, src_port, dst_ip, dst_port, timestamp, payload_len)
    sock.sendto(header + payload, ("127.0.0.1", 3000))

def worker(count):
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    for i in range(count):
        send_packet(sock, 1, b"RTP Data Payload " + str(i).encode())
        if i % 100 == 0:
            send_packet(sock, 0, b"SIP Info", callid=f"call-{i}")

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--packets", type=int, default=1000)
    parser.add_argument("--workers", type=int, default=1)
    args = parser.parse_args()

    start = time.time()
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as executor:
        futures = [executor.submit(worker, args.packets // args.workers) for _ in range(args.workers)]
        concurrent.futures.wait(futures)
    
    end = time.time()
    pps = args.packets / (end - start)
    print(f"Sent {args.packets} packets in {end - start:.2f}s ({pps:.2f} packets/s)")

if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""A learning Ethernet switch over UDP on loopback, for bench/vm.sh.

Every VM with a LAN NIC sends its frames as UDP datagrams to this hub
(qemu `-netdev dgram`, one frame per datagram) from its own local port.
The hub learns which port each source MAC lives behind, forwards unicast
frames to that port and floods broadcast, multicast and unknown ones to
every other port it has heard from. macOS does not loop multicast back on
lo0, so qemu's `socket,mcast=` cannot be used for this.

    bench/lanhub.py <port>
"""
import socket
import sys


def main():
    port = int(sys.argv[1])
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 << 20)
    sock.bind(("127.0.0.1", port))
    macs = {}
    ports = set()
    while True:
        frame, src = sock.recvfrom(65536)
        if len(frame) < 14:
            continue
        ports.add(src)
        macs[frame[6:12]] = src
        dst = frame[0:6]
        target = None if dst[0] & 1 else macs.get(dst)
        for p in [target] if target else ports:
            if p != src:
                try:
                    sock.sendto(frame, p)
                except OSError:
                    pass


if __name__ == "__main__":
    main()

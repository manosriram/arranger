#!/usr/bin/env python3
"""Local-only SSH tunnel stand-in for demo.sh; use a real private Unix socket."""
import os
import select
import socket
import sys
import threading


def forward(client, remote):
    with client, socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
        server.connect(remote)
        peers = {client: server, server: client}
        while True:
            ready, _, _ = select.select(list(peers), [], [])
            for source in ready:
                data = source.recv(65536)
                if not data:
                    return
                peers[source].sendall(data)


def main():
    local, remote = sys.argv[1].split(":", 1)
    os.umask(0o077)
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
        listener.bind(local)
        listener.listen()
        while True:
            client, _ = listener.accept()
            threading.Thread(target=forward, args=(client, remote), daemon=True).start()


if __name__ == "__main__":
    main()

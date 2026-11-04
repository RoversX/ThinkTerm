#!/usr/bin/env python3
"""One synthetic SSH exchange, for testing the partial-success wire flag.

Requires Paramiko in an isolated virtual environment. No real accounts,
configuration files or keys are loaded. stdout carries only a port and count.
"""

import socket
import sys
import threading
import time

import paramiko


class Server(paramiko.ServerInterface):
    def __init__(self, scenario):
        self.scenario = scenario
        self.first_step = False
        self.attempts = 0
        self.authenticated = False

    def get_allowed_auths(self, username):
        if self.scenario == "key-password":
            return "password" if self.first_step else "publickey,password"
        return "keyboard-interactive" if self.first_step else "password"

    def check_auth_publickey(self, username, key):
        assert username == "example"
        self.attempts += 1
        self.first_step = True
        return paramiko.AUTH_PARTIALLY_SUCCESSFUL

    def check_auth_password(self, username, password):
        assert username == "example" and password == "example-password"
        self.attempts += 1
        if self.scenario == "key-password":
            self.authenticated = self.first_step
            return paramiko.AUTH_SUCCESSFUL if self.first_step else paramiko.AUTH_FAILED
        self.first_step = True
        return paramiko.AUTH_PARTIALLY_SUCCESSFUL

    def check_auth_interactive(self, username, submethods):
        assert username == "example" and self.first_step
        return paramiko.InteractiveQuery("Example authentication", "", ("Password: ", False))

    def check_auth_interactive_response(self, responses):
        assert responses == ["example-password"]
        self.attempts += 1
        self.authenticated = True
        return paramiko.AUTH_SUCCESSFUL


def main():
    assert sys.argv[1:] in (["key-password"], ["password-interactive"])
    server = Server(sys.argv[1])
    # Throwaway host identity exists only in this process.
    host_key = paramiko.RSAKey.generate(2048)
    with socket.socket() as listener:
        listener.settimeout(10)
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        print(listener.getsockname()[1], flush=True)
        peer, _ = listener.accept()
        with peer, paramiko.Transport(peer) as transport:
            transport.add_server_key(host_key)
            # The synchronous waiter checks disconnect before its completion
            # event. A fast successful client can otherwise look like a KEX
            # failure after it has already finished authentication and exited.
            transport.start_server(event=threading.Event(), server=server)
            deadline = time.monotonic() + 10
            while transport.is_active() and time.monotonic() < deadline:
                time.sleep(0.01)
            assert server.authenticated, "synthetic SSH authentication did not complete"
    print(server.attempts, flush=True)


if __name__ == "__main__":
    main()

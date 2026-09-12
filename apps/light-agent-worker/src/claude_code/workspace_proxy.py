# Fixed stdio-to-Unix bridge. Identity, store and task never come from MCP input.
import socket
import sys

with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
    client.connect('/session/tools.sock')
    reader = client.makefile('rb')
    while True:
        line = sys.stdin.buffer.readline(1048577)
        if not line:
            break
        if len(line) > 1048576 or not line.endswith(b'\n'):
            raise SystemExit(2)
        client.sendall(line)
        # Notifications have no response; the host emits a private blank ack.
        response = reader.readline(1048577)
        if not response or len(response) > 1048576 or not response.endswith(b'\n'):
            raise SystemExit(2)
        if response != b'\n':
            sys.stdout.buffer.write(response)
            sys.stdout.buffer.flush()

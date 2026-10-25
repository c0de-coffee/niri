#!/usr/bin/env python3
"""Minimal greetd IPC client: log a user in through a greeter socket, like a greeter would.

usage: greetd-login.py SOCKET USER PASSWORD
"""
import json, socket, struct, sys

path, user, password = sys.argv[1:4]
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(path)

def call(request):
    body = json.dumps(request).encode()
    sock.sendall(struct.pack("=I", len(body)) + body)
    (length,) = struct.unpack("=I", sock.recv(4, socket.MSG_WAITALL))
    response = json.loads(sock.recv(length, socket.MSG_WAITALL))
    shown = dict(response)
    print(">", request.get("type"), "->", shown)
    return response

response = call({"type": "create_session", "username": user})
while response["type"] == "auth_message":
    kind = response["auth_message_type"]
    answer = password if kind == "secret" else (user if kind == "visible" else None)
    response = call({"type": "post_auth_message_response", "response": answer})

if response["type"] != "success":
    sys.exit(f"login failed: {response}")
call({"type": "start_session", "cmd": ["niri-session"], "env": []})

#!/usr/bin/python3
"""Manual focused-field test; never submits the omnibox."""
import json
import os
import socket
import time


def main():
    path = os.path.join(os.environ["XDG_RUNTIME_DIR"], "daapstt", "ime.sock")
    print("Focus an editable field now; starting in five seconds.", flush=True)
    time.sleep(5)
    with socket.socket(socket.AF_UNIX) as sock:
        sock.connect(path)
        reader = sock.makefile("rb")
        session = None
        for sequence, (operation, text) in enumerate((
            ("begin", None), ("partial", "bonj"),
            ("partial", "bonjour fran"), ("partial", "bonjour français 👋"),
            ("partial", "bonjour français 👋 café"),
            ("commit", "bonjour français 👋 café"),
        ), 1):
            request = {"v": 1, "id": sequence, "op": operation}
            if session is not None:
                request["session"] = session
            if text is not None:
                request["text"] = text
            sock.sendall((json.dumps(request, ensure_ascii=False) + "\n").encode())
            while True:
                reply = json.loads(reader.readline())
                if reply.get("event"):
                    raise RuntimeError(f"IME invalidated: {reply['reason']}")
                if reply.get("id") == sequence:
                    break
            if not reply.get("ok"):
                raise RuntimeError(f"IME rejected {operation}: {reply['error']}")
            if operation == "begin":
                session = reply["session"]
            print(f"{operation}: acknowledged", flush=True)
            time.sleep(0.5)
    print("Expected visible text: bonjour français 👋 café", flush=True)


if __name__ == "__main__":
    main()

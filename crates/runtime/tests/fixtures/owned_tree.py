"""Bounded inert ACP tree; release stops both descendants and leader."""
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time

home = Path(sys.argv[1])
child = subprocess.Popen([
    sys.executable, "-c",
    "import sys,time,pathlib; p=pathlib.Path(sys.argv[1]); end=time.monotonic()+15\n"
    "while time.monotonic()<end and not p.exists(): time.sleep(.01)",
    str(home / "release"),
], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
(home / "child.pid").write_text(str(child.pid))

def finish():
    end = time.monotonic() + 15
    while time.monotonic() < end and not (home / "release").exists():
        time.sleep(.01)
    child.wait(timeout=2)
    os._exit(0)

threading.Thread(target=finish, daemon=True).start()
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    method = msg.get("method")
    result = {"protocolVersion": 1, "agentCapabilities": {}} if method == "initialize" else {"sessionId": "owned-session"}
    print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)

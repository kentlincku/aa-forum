import json
import os
import sys
import threading
import time

mode = sys.argv[1]
sid = f"auditor-{os.getpid()}"
lock = threading.Lock()

def send(v):
    with lock:
        print(json.dumps(v), flush=True)

def heartbeat():
    while True:
        send({'jsonrpc': '2.0', 'method': 'session/update', 'params': {'sessionId': sid, 'update': {'sessionUpdate': 'agent_message_chunk', 'content': {'type': 'text', 'text': '.'}}}})
        time.sleep(.04)

for line in sys.stdin:
    v = json.loads(line)
    method = v.get('method')
    i = v.get('id')
    if method == 'initialize':
        send({'jsonrpc': '2.0', 'id': i, 'result': {'protocolVersion': 1, 'agentCapabilities': {'loadSession': True}, 'authMethods': []}})
    elif method in ('session/new', 'session/load'):
        sid = v.get('params', {}).get('sessionId', sid)
        send({'jsonrpc': '2.0', 'id': i, 'result': {'sessionId': sid}})
    elif method == 'session/prompt':
        if mode == 'blocked':
            send({'jsonrpc': '2.0', 'id': 900, 'method': 'session/request_permission', 'params': {'sessionId': sid, 'toolCall': {'toolCallId': 'pending', 'title': 'fixture pending'}, 'options': [{'optionId': 'allow', 'name': 'Allow', 'kind': 'allow_once'}]}})
        else:
            threading.Thread(target=heartbeat, daemon=True).start()
    # Intentionally ignore session/cancel and permission responses; never finish the prompt.

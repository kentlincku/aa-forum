import json, os, subprocess, sys
from pathlib import Path
home = Path(sys.argv[1])
child = subprocess.Popen([sys.executable, '-c', 'import pathlib,time,sys; p=pathlib.Path(sys.argv[1]); end=time.monotonic()+20\nwhile time.monotonic()<end and not p.exists(): time.sleep(.02)', str(home/'release')], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
(home/'child.pid').write_text(str(child.pid))
(home/'leader.pid').write_text(str(os.getpid()))
for line in sys.stdin:
    m = json.loads(line)
    method = m.get('method')
    if 'id' not in m: continue
    if method == 'initialize': r = {'protocolVersion': 1, 'agentCapabilities': {}}
    elif method == 'session/new': r = {'sessionId':'review-session'}
    elif method == 'session/prompt': r = {'stopReason':'end_turn'}
    else: r = {}
    print(json.dumps({'jsonrpc':'2.0','id':m['id'],'result':r}), flush=True)

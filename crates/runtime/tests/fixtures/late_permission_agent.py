import json,sys,threading,time
from pathlib import Path
trigger=Path(sys.argv[1]);journal=Path(sys.argv[2]);lock=threading.Lock();sid='idle-permission-test';sent=False
def send(v):
    with lock:print(json.dumps(v),flush=True)
def late_permission():
    while not trigger.exists():time.sleep(.01)
    send({'jsonrpc':'2.0','id':900,'method':'session/request_permission','params':{'sessionId':sid,'toolCall':{'toolCallId':'background-edit','title':'background fixture edit'},'options':[{'optionId':'allow','name':'Allow','kind':'allow_once'}]}})
for line in sys.stdin:
    v=json.loads(line);method=v.get('method');i=v.get('id')
    if method=='initialize':send({'jsonrpc':'2.0','id':i,'result':{'protocolVersion':1,'agentCapabilities':{'loadSession':True},'authMethods':[]}})
    elif method in ('session/new','session/load'):send({'jsonrpc':'2.0','id':i,'result':{'sessionId':sid}})
    elif method=='session/prompt':
        with journal.open('a') as f:f.write('prompt\n')
        send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':sid,'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'CONTROL_OK'}}}})
        send({'jsonrpc':'2.0','id':i,'result':{'stopReason':'end_turn'}})
        if not sent:sent=True;threading.Thread(target=late_permission,daemon=True).start()
    elif i==900:
        trigger.with_suffix('.answered').write_text(json.dumps(v))

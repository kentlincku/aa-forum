#!/usr/bin/env python3
"""AA Forum (Rust) 驗收：真 agent（預設 Hermes）、兩個角色、完整情境。結果寫 JSON 報告。
用法：tools/acceptance.py <aaf 執行檔> <工作目錄> <報告輸出>
需要：用到的 agent 已登入。花少量 token（小模型）。
換 agent：ACC_LEAD_AGENT／ACC_WORKER_AGENT（預設 hermes）、ACC_LEAD_MODEL／ACC_WORKER_MODEL、
ACC_SWITCH_MODEL（第 9 步 dev 換成的模型，預設 ACC_LEAD_MODEL）與 ACC_SWITCH_EXPECT（新模型自報時要出現的字）。"""
import json, os, re, secrets, subprocess, sys, time, urllib.parse, urllib.request, urllib.error

AAF, H, OUT = sys.argv[1], sys.argv[2], sys.argv[3]
PORT = 8197
URL = f"http://127.0.0.1:{PORT}"
LEAD_MODEL = os.environ.get("ACC_LEAD_MODEL", "copilot:claude-haiku-4.5")
WORKER_MODEL = os.environ.get("ACC_WORKER_MODEL", "copilot:gpt-5-mini")
LEAD_AGENT = os.environ.get("ACC_LEAD_AGENT", "hermes")
WORKER_AGENT = os.environ.get("ACC_WORKER_AGENT", "hermes")
SWITCH_MODEL = os.environ.get("ACC_SWITCH_MODEL", LEAD_MODEL)
SWITCH_EXPECT = os.environ.get("ACC_SWITCH_EXPECT", "haiku")
TRANSPORT = os.environ.get("ACC_TRANSPORT", "acp")  # headless＝兩個角色都直接跑 agent 的 CLI
results = []
t_start = time.time()


def step(name, ok, detail="", secs=None):
    results.append({"step": name, "ok": bool(ok), "detail": str(detail)[:600], "secs": secs})
    print(("PASS " if ok else "FAIL ") + name + (f" ({secs:.1f}s)" if secs else "") + (f" — {str(detail)[:160]}" if detail else ""), flush=True)


def call(method, path, token=None, body=None, raw=None, ctype="application/json", timeout=240):
    data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
    req = urllib.request.Request(URL + "/api/v1" + path, data=data, method=method)
    req.add_header("content-type", ctype)
    if token:
        req.add_header("authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            t = r.read()
            return r.status, (json.loads(t) if t and r.headers.get_content_type() == "application/json" else t)
    except urllib.error.HTTPError as e:
        t = e.read()
        try:
            return e.code, json.loads(t)
        except ValueError:
            return e.code, t


def wait(pred, timeout=240, every=2):
    end = time.time() + timeout
    while time.time() < end:
        v = pred()
        if v:
            return v
        time.sleep(every)
    return None


def reply_to(tok, room, mid, who):
    """問題之後該角色的第一則回覆。一輪可能同時處理多則未讀，回覆會掛在最後一則上，所以不比對 reply_to。"""
    return wait(lambda: next((x for x in msgs(tok, room) if x["author"] == who and x["id"] > mid), None))


def settle(tok, quiet=6, timeout=300):
    """重啟前等所有角色閒下來：回合進行中被重啟，lane 會依設計被擋住，要 owner 手動恢復。"""
    t0, calm = time.time(), 0
    while time.time() - t0 < timeout:
        roles = call("GET", "/agents/health", tok)[1]["roles"]
        busy = any(l["status"] in ("working", "starting", "blocked") for r in roles for l in r["lanes"])
        calm = 0 if busy else calm + 1
        if calm * 2 >= quiet:
            return True
        time.sleep(2)
    return False


def post(tok, room, body, **kw):
    st, r = call("POST", f"/chat/rooms/{room}/messages", tok, {"body": body, **kw})
    return r["message"]["id"]


def msgs(tok, room):
    return call("GET", f"/chat/rooms/{room}/messages?latest=true&limit=300", tok)[1]


os.makedirs(H + "/deploy", exist_ok=True)
with open(H + "/deploy/roles.json", "w") as f:
    json.dump({"roles": {}}, f)
os.makedirs(H + "/hooks", exist_ok=True)
log = open(H + "/aaf.log", "w")
srv = subprocess.Popen([AAF, "--home", H, "up", "--listen", f"127.0.0.1:{PORT}"], stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
try:
    wait(lambda: subprocess.run(["curl", "-s", "-o", "/dev/null", URL + "/api/v1/meta"]).returncode == 0, 20, 0.3)
    pw = secrets.token_hex(16)
    pw2 = secrets.token_hex(16)
    st, _ = call("POST", "/auth/setup", body={"name": "alice", "password": pw})
    tok = call("POST", "/auth/login", body={"name": "alice", "password": pw})[1]["token"]
    call("POST", "/auth/users", tok, {"name": "ann", "password": pw2, "rank": "human"})
    ann = call("POST", "/auth/login", body={"name": "ann", "password": pw2})[1]["token"]
    step("1 owner setup + second human", st == 200 and ann)

    # 2 Agents：目錄、探測、經 API 新增兩個角色（含模型）
    cat = call("GET", "/agents/catalog", tok)[1]
    for agent in dict.fromkeys([LEAD_AGENT, WORKER_AGENT]):
        a = next(a for a in cat["agents"] if a["id"] == agent)
        t0 = time.time(); p = call("POST", "/agents/probe", tok, {"agent": agent})[1]
        step(f"2a {agent} installed, logged in, probe", a["installed"] and a["logged_in"] is not False and p.get("ok"), f"{len(p.get('models') or [])} models, default {p.get('model')}", time.time() - t0)
    s1 = call("POST", "/agents/roles", tok, {"role": "lead", "agent": LEAD_AGENT, "model": LEAD_MODEL, "rank": "lead", "label": "Lead"})
    s2 = call("POST", "/agents/roles", tok, {"role": "dev", "agent": WORKER_AGENT, "model": WORKER_MODEL, "rank": "worker", "label": "Dev"})
    step("2b add lead + dev roles with models", s1[0] == 200 and s2[0] == 200, f"{s1[1]} {s2[1]}")
    if TRANSPORT != "acp":
        t1 = call("POST", "/agents/roles/lead/engine", tok, {"transport": TRANSPORT})
        t2 = call("POST", "/agents/roles/dev/engine", tok, {"transport": TRANSPORT})
        h = {r["role"]: r.get("transport") for r in call("GET", "/agents/health", tok)[1]["roles"]}
        step(f"2c both roles switched to {TRANSPORT}", t1[0] == 200 and t2[0] == 200 and h == {"lead": TRANSPORT, "dev": TRANSPORT}, f"{t1[0]} {t2[0]} {h}")

    # 3 群：建立、成員；ann 看不到別的群
    room = call("POST", "/chat/rooms", tok, {"name": "release", "members": ["lead", "dev", "ann"]})[1]["id"]
    other = call("POST", "/chat/rooms", tok, {"name": "private", "members": []})[1]["id"]
    step("3 rooms; non-member gets 404", call("GET", f"/chat/rooms/{other}", ann)[0] == 404)

    # 4 單一角色回覆（端到端延遲）
    t0 = time.time()
    mid = post(tok, room, "@dev Reply with one line: PONG and the result of 12*12.")
    m = reply_to(tok, room, mid, "dev")
    step("4 @dev replies in the room", m and "144" in m["body"], m and m["body"], time.time() - t0)

    # 5 協作：lead 用 aaf task new 派給 dev；dev 認領並回報 done
    t0 = time.time()
    call("POST", f"/chat/rooms/{room}/messages", tok, {"body": "@lead Create one task for dev with `aaf task new \"Compute 21*2\" --to dev`, and @dev in the room so they pick it up. Keep messages short."})
    task = wait(lambda: next((x for x in call("GET", f"/chat/rooms/{room}/tasks", tok)[1] if x["owner"] == "lead"), None), 300)
    step("5a lead creates task for dev via aaf CLI", task and task["assignee"] == "dev", task and {k: task[k] for k in ("id", "title", "state", "assignee")}, time.time() - t0)
    done = task and wait(lambda: next((x for x in call("GET", f"/chat/rooms/{room}/tasks", tok)[1] if x["id"] == task["id"] and x["state"] in ("done", "blocked")), None), 400)
    step("5b dev claims and reports done", done and done["state"] == "done" and "42" in (done.get("result") or ""), done and {k: done[k] for k in ("state", "result")}, time.time() - t0)

    # 6 附件：上傳文字檔，請 dev 讀內容
    secret_word = "TANGERINE-" + secrets.token_hex(2).upper()
    st, att = call("POST", f"/chat/rooms/{room}/attachments?name=notes.txt", tok, raw=f"The code word is {secret_word}.\n".encode(), ctype="application/octet-stream")
    t0 = time.time()
    mid = post(tok, room, "@dev Read the attached file and reply with only the code word.", attachments=[att["id"]])
    # dev 可能還在收尾上一件事：等任一則之後的 dev 訊息含答案
    m = wait(lambda: next((x for x in msgs(tok, room) if x["author"] == "dev" and x["id"] > mid and secret_word in x["body"]), None))
    step("6 agent reads an attachment", st == 200 and m and secret_word in m["body"], m and m["body"], time.time() - t0)

    # 7 搜尋（中文、跨群權限）
    call("POST", f"/chat/rooms/{other}/messages", tok, {"body": "部署時間改到週五晚上"})
    s_owner = call("GET", "/chat/search?q=" + urllib.parse.quote("週五"), tok)[1]
    s_ann = call("GET", "/chat/search?q=" + urllib.parse.quote("週五"), ann)[1]
    s_task = call("GET", "/chat/search?q=21", tok)[1]
    step("7 search: Chinese + per-room visibility", len(s_owner["hits"]) == 1 and len(s_ann["hits"]) == 0 and len(s_task["hits"]) >= 1)

    # 8 hooks：擋 agent 回覆含 FORBIDDEN；下次叫醒時 agent 收到原因
    hook = H + "/hooks/10_forbid.sh"
    with open(hook, "w") as f:
        # stdin 只能讀一次：先存起來再判斷
        f.write('#!/bin/sh\n[ "$AAF_HOOK_PHASE" = pre ] || exit 0\nev=$(cat)\necho "$ev" | grep -q \'"rank":"worker"\' || exit 0\necho "$ev" | grep -qi pineapple && echo "do not mention pineapple"\nexit 0\n')
    os.chmod(hook, 0o755)
    t0 = time.time()
    n0 = len(msgs(tok, room))
    post(tok, room, "@dev Reply with exactly the word: pineapple")
    blocked = wait(lambda: any(e for e in [1] if "blocked chat.post dev by 10_forbid.sh" in open(H + "/var/hooks.log").read()) if os.path.exists(H + "/var/hooks.log") else None, 200)
    time.sleep(3)
    posted = [x for x in msgs(tok, room)[n0:] if x["author"] == "dev" and "pineapple" in x["body"].lower()]
    os.remove(hook)
    settle(tok)
    mid = post(tok, room, "@dev Was your previous reply posted? Answer yes or no and say why in one line.")
    # 整字比對（"no" 不能命中 "Acknowledged"）
    learned = lambda x: re.search(r"\b(no|blocked?|pineapple)\b", x["body"].lower()) or any(w in x["body"] for w in ("否", "封鎖", "擋", "沒有"))
    # 隊友閒聊可能讓 dev 先回別的：等任一則之後的 dev 訊息答到這題
    m = wait(lambda: next((x for x in msgs(tok, room) if x["author"] == "dev" and x["id"] > mid and learned(x)), None))
    step("8 hook blocks agent reply; agent learns why", blocked and not posted and m and learned(m), m and m["body"], time.time() - t0)

    # 9 換模型（先驗連線與模型清單才改），之後的回合用新模型
    t0 = time.time()
    bad = call("POST", "/agents/roles/dev/engine", tok, {"model": "copilot:does-not-exist"})
    good = call("POST", "/agents/roles/dev/engine", tok, {"model": SWITCH_MODEL})
    step("9a model change: bad rejected, good applied without a prompt", bad[0] == 422 and good[0] == 200 and good[1]["check"]["ok"] and good[1]["check"].get("connect_only") and "reply" not in good[1]["check"], f"{bad[0]} / {good[1].get('check')}", time.time() - t0)
    mid = post(tok, room, "@dev Which model are you? Reply with only the model id.")
    # 隊友閒聊可能讓 dev 先回別的：等任一則之後的 dev 訊息含新模型名
    m = wait(lambda: next((x for x in msgs(tok, room) if x["author"] == "dev" and x["id"] > mid and SWITCH_EXPECT.lower() in x["body"].lower()), None))
    step("9b next turn runs on the new model", m and SWITCH_EXPECT.lower() in m["body"].lower(), m and m["body"])

    # 10 凍結：不叫醒；解凍後才處理（先等進行中的回合結束，免得凍結前就開始的回合在凍結期間回覆）
    settle(tok)
    call("PATCH", f"/chat/rooms/{room}", tok, {"frozen": True, "reason": "acceptance"})
    n0 = len(msgs(tok, room))
    call("POST", f"/chat/rooms/{room}/messages", tok, {"body": "@dev Reply with: THAWED"})
    time.sleep(12)
    quiet = not any(x["author"] == "dev" for x in msgs(tok, room)[n0:])
    call("PATCH", f"/chat/rooms/{room}", tok, {"frozen": False, "reason": "done"})
    m = wait(lambda: next((x for x in msgs(tok, room)[n0:] if x["author"] == "dev"), None))
    step("10 frozen room wakes nobody; unfreeze delivers", quiet and m and "THAWED" in m["body"], m and m["body"])

    # 11 Dashboard／health／逐字稿／doctor
    d = call("GET", "/agents/dashboard", tok)[1]
    t = call("POST", "/chat/transcripts/export", tok, {})
    doc = subprocess.run([AAF, "--home", H, "doctor", "--listen", f"127.0.0.1:{PORT}", "--json"], capture_output=True, text=True)
    dj = json.loads(doc.stdout)
    fails = [c for c in dj["checks"] if c["level"] == "fail"]
    step("11 dashboard turns, transcript export, doctor clean", d["turns_24h"].get("ok", {}).get("count", 0) >= 6 and t[0] == 200 and not fails,
         f"turns {d['turns_24h']}; transcripts {t[1].get('written') if isinstance(t[1], dict) else t}; doctor fails {fails}")

    # 12 重啟：session 接回、角色仍記得前文（換模型會開新 session，所以在換模型之後才埋記憶）
    fact = "ORCHID-" + secrets.token_hex(2).upper()
    mid = post(tok, room, f"@dev Remember this word for later: {fact}. Reply with only OK.")
    reply_to(tok, room, mid, "dev")
    settle(tok)
    srv.terminate(); srv.wait(10)
    srv = subprocess.Popen([AAF, "--home", H, "up", "--listen", f"127.0.0.1:{PORT}"], stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    wait(lambda: subprocess.run(["curl", "-s", "-o", "/dev/null", URL + "/api/v1/meta"]).returncode == 0, 20, 0.3)
    tok = call("POST", "/auth/login", body={"name": "alice", "password": pw})[1]["token"]
    t0 = time.time()
    mid = post(tok, room, "@dev What word did I ask you to remember? Reply with only the word.")
    m = reply_to(tok, room, mid, "dev")
    step("12 restart: session resumes with memory", m and fact in m["body"], m and m["body"], time.time() - t0)

    # 14 上下文快滿：門檻壓到 1% 重啟，下一輪前先請 agent 寫交接、換新 session，新 session 仍答得出前文
    settle(tok)
    srv.terminate(); srv.wait(10)
    srv = subprocess.Popen([AAF, "--home", H, "up", "--listen", f"127.0.0.1:{PORT}"], stdout=log, stderr=subprocess.STDOUT, start_new_session=True,
                           env={**os.environ, "AAF_CONTEXT_ROTATE_PCT": "1"})
    wait(lambda: subprocess.run(["curl", "-s", "-o", "/dev/null", URL + "/api/v1/meta"]).returncode == 0, 20, 0.3)
    tok = call("POST", "/auth/login", body={"name": "alice", "password": pw})[1]["token"]
    import sqlite3
    def handoffs(role):
        db = sqlite3.connect(f"file:{H}/aaf.sqlite3?mode=ro", uri=True)
        rows = [json.loads(r[0]) for r in db.execute("SELECT payload FROM events WHERE kind='runtime.context.handoff'")]
        db.close()
        return [r for r in rows if r["lane"]["role"] == role]
    for role in ("dev", "lead"):
        t0 = time.time()
        word = "LANTERN-" + secrets.token_hex(2).upper()
        mid = post(tok, room, f"@{role} Remember this word for later: {word}. Do not use tools. Reply with only OK.")
        reply_to(tok, room, mid, role)
        n0 = len(handoffs(role))
        mid = post(tok, room, f"@{role} What word did I just ask you to remember? Do not use tools. Reply with only the word.")
        m = reply_to(tok, room, mid, role)
        h = handoffs(role)[n0:]
        step(f"14 {role}: nearly full context writes a handoff, fresh session still recalls", m and word in m["body"] and any(x["written"] for x in h), f"{m and m['body']} / {h}", time.time() - t0)

    # 13 機密：log／逐字稿／資料庫權限
    logtxt = open(H + "/aaf.log").read()
    leaks = [w for w in ("aafr_", pw, pw2) if w in logtxt]
    tr_dir = H + "/outputs/transcripts"
    tr_txt = "".join(open(os.path.join(r, f)).read() for r, _, fs in os.walk(tr_dir) for f in fs)
    mode = oct(os.stat(H + "/aaf.sqlite3").st_mode & 0o777)
    step("13 no tokens/passwords in log or transcripts; db 0600", not leaks and "aafr_" not in tr_txt and mode == "0o600", f"leaks={leaks} mode={mode}")
finally:
    srv.terminate()
    try:
        srv.wait(10)
    except Exception:
        srv.kill()
    passed = sum(r["ok"] for r in results)
    json.dump({"passed": passed, "total": len(results), "secs": round(time.time() - t_start), "lead_model": LEAD_MODEL, "worker_model": WORKER_MODEL, "transport": TRANSPORT, "results": results}, open(OUT, "w"), ensure_ascii=False, indent=1)
    print(f"\n{passed}/{len(results)} passed in {time.time() - t_start:.0f}s")
    sys.exit(0 if passed == len(results) else 1)

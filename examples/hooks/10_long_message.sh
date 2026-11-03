#!/bin/sh
# 範例規則：agent 發文超過 1500 字要附理由（源自先前的 Python 版）。
# 複製到 <instance>/hooks/ 並 chmod +x 即生效，不需重啟。人（rank=human）不受限。
# 事件 JSON 從 stdin 進來；印出文字＝擋下（pre）或提醒（post）。需要 python3。
exec python3 -c '
import json, os, sys
e = json.load(sys.stdin)
phase = os.environ.get("AAF_HOOK_PHASE")
body = e.get("body") or ""
if phase == "pre" and e.get("rank") != "human" and len(body) > 1500:
    tag = "# long-ok:"
    i = body.find(tag)
    if i < 0 or len(body[i + len(tag):].split("\n", 1)[0].strip()) < 6:
        print(f"Message is {len(body)} characters (limit 1500). Put details in a file and post the path, "
              f"or add a line \"{tag} <reason, 6+ chars>\".")
'

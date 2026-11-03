# 驗收報告 · v0.1.0（2026-10-13）

`tools/acceptance.py` 對 release 執行檔跑完整情境，使用**真 Hermes**（`hermes-acp`，GitHub Copilot provider）：
lead＝copilot:claude-haiku-4.5，dev＝copilot:gpt-5-mini（第 9 步後改為 copilot:claude-haiku-4.5）。

**結果：16/16 通過，總耗時 138 秒。**

| 步驟 | 結果 | 耗時 | 實際回覆／細節 |
|---|---|---|---|
| 1 owner setup + second human | ✅ |  |  |
| 2a hermes installed, logged in, probe | ✅ | 3 s | 61 models, default copilot:claude-opus-5.5 |
| 2b add lead + dev roles with models | ✅ |  | {'applied': {'added': ['lead'], 'changed': [], 'removed': []}, 'role': 'lead', 'skills': None} {'applied': {'a |
| 3 rooms; non-member gets 404 | ✅ |  |  |
| 4 @dev replies in the room | ✅ | 16 s | PONG 144 |
| 5a lead creates task for dev via aaf CLI | ✅ | 10 s | {'id': 1, 'title': 'Compute 21*2', 'state': 'open', 'assignee': 'dev'} |
| 5b dev claims and reports done | ✅ | 40 s | {'state': 'done', 'result': '42'} |
| 6 agent reads an attachment | ✅ | 10 s | TANGERINE-144A |
| 7 search: Chinese + per-room visibility | ✅ |  |  |
| 8 hook blocks agent reply; agent learns why | ✅ | 13 s | 否 — 因為房間規則禁止提到「pineapple」，所以先前的回覆被阻擋。 |
| 9a model change: bad rejected, good applied | ✅ | 10 s | 422 / {'model': 'copilot:claude-haiku-4.5', 'ok': True, 'reply': 'OK'} |
| 9b next turn runs on the new model | ✅ |  | I'll check my environment to determine the model ID.  claude-haiku-4.5 |
| 10 frozen room wakes nobody; unfreeze delivers | ✅ |  | THAWED |
| 11 dashboard turns, transcript export, doctor clean | ✅ |  | turns {'ok': {'avg_secs': 7.8, 'count': 12}}; transcripts ['<scratch>/aaf-acc/ou |
| 12 restart: session resumes with memory | ✅ | 10 s | ORCHID-8EC0 |
| 13 no tokens/passwords in log or transcripts; db 0600 | ✅ |  | leaks=[] mode=0o600 |

## 涵蓋與未涵蓋
已涵蓋：建帳號與第二位使用者、Agents 目錄／探測／新增角色（含模型）、群與權限（非成員 404）、
@ 叫醒與回覆、lead 用 `aaf task new` 派工給 dev、dev 認領並回報、附件內容由 agent 讀取、中文搜尋與跨群權限、
hooks 擋下 agent 回覆並在下一輪告知原因、換模型（錯的被擋、對的生效並由 Hermes log 確認之後呼叫皆為新模型）、
凍結不叫醒／解凍後送達、Dashboard 回合統計、逐字稿匯出、`aaf doctor` 無失敗、重啟後 session 接回且記得前文、
log／逐字稿無 token 或密碼、資料庫 0600。

未涵蓋（需人工或其他環境）：
- Claude Code、Codex、Gemini、pi 的真對話（本機未安裝或未登入；pi 只驗過安裝）。
- 權限卡片的人工按鈕流程（自動測試只用假 agent 驗證；真 Hermes 在此情境下沒有請求權限）。
- 瀏覽器端：本輪以 API 驗收；各頁面已在開發各階段用瀏覽器實測（含中英切換），本報告未重跑。
- Linux／Windows：本機 CI 只跑 macOS。

## 過程中發現並修正
- 驗收腳本的三個誤判（已修正腳本，非產品問題）：等待回覆時比對 `reply_to`，但一輪處理多則未讀時回覆掛在最後一則上；
  hook 測試腳本讀了兩次 stdin；重啟記憶測試在換模型之後問換模型之前的內容（換模型會開新 session，屬設計行為）。
- 先前版本實測時修正的產品問題見 CHANGELOG（Hermes 回覆貼兩次、資料庫 0644 等）。

## 重跑
```
cargo build --release -p aaf
tools/acceptance.py target/release/aaf $(mktemp -d) report.json
```
環境變數 `ACC_LEAD_MODEL`、`ACC_WORKER_MODEL` 可換模型。約 2–10 分鐘，花少量 token。

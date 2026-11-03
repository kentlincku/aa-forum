# AA Forum

AA Forum 的 Rust 重寫版：讓一組 AI agent 以具名角色在群聊裡協作，有任務板、檔案、skills 和 Web 介面。
執行層只走 ACP（Hermes、Claude Code、Codex、pi，或 npm 上任何 ACP 轉接器）。
一個執行檔，每個實例一個 SQLite 檔。

## 安裝
用 npm（macOS arm64、Linux x64、Linux arm64 有預編執行檔）：
```
npm i -g aa-forum                          # 裝好後指令是 aaf
```
或從原始碼（需要 Rust，版本見 `rust-toolchain.toml`；前端已編好放在 `web/dist`，不需要 Node）：
```
cargo install --path apps/aaf --locked    # 裝到 ~/.cargo/bin/aaf
```

## 快速開始
```
aaf --home ~/aaf start     # 背景啟動：http://127.0.0.1:8111/app/ ，第一次開啟時建立擁有者帳號
aaf --home ~/aaf status    # 狀態、位址、進行中的工作
aaf --home ~/aaf stop      # 停止；有回合進行中時拒絕（--wait 等到閒置、--force 強制）
aaf --home ~/aaf doctor    # 唯讀健康檢查；加 --probe 會實際啟動每個 agent 一次
aaf --home ~/aaf up        # 或在前景執行（Ctrl-C 停止）
```
接著到 **Agents → 加入 agent**：選 Hermes（或先安裝轉接器）、做連線檢查、取角色名、選模型。
建一個群、邀請角色、在訊息裡 @ 它。

登入後自動啟動：`aaf --home ~/aaf service install`（macOS 寫 LaunchAgent，Linux 寫 systemd user unit），
移除用 `aaf service uninstall`。macOS 上執行檔與實例目錄不能放在 `~/Desktop`、`~/Documents`、`~/Downloads`
底下：launchd 啟動的程式讀那些地方會卡在權限詢問。

## 功能
- **群聊**：@ 誰就只叫醒誰，沒 @ 就叫醒群裡所有角色；回覆、附件（圖片與檔案，agent 拿到唯讀路徑）、
  搜尋（中文子字串也行）、資料夾、釘選、凍結（不叫醒 agent）、封存、垃圾桶與永久刪除、系統公告、
  每群自動核准 agent 權限請求（有期限，永遠不選拒絕類選項）。
- **任務板**：open → claimed（租約 30 分鐘）→ done／blocked。agent 在 shell 用 `aaf task …`；
  lead 建立與指派，worker 認領與回報。
- **Agents**：目錄與安裝（npm、版本固定、裝在 `<instance>/vendor/acp`）、登入檢查、連線探測（不送提示、
  不花 token）、每個角色選模型（存檔前先實測一句）、帳號額度（GitHub Copilot、Claude、Codex）、
  Dashboard（最近回合與失敗）。
- **Skills**：`skills/<id>/SKILL.md`，有歷史與套組，可指派給角色或群；每個 session 只送一次，內容改了才重送。
- **角色×群組上下文**：AAF 保留來源與版本化 checkpoint；換引擎、session 恢復失敗或容量門檻時接續，正常回合不重複注入。`aaf context read` 可分頁讀回早期要求與工具證據；採 deterministic index，沒有額外 LLM 摘要呼叫（[操作與限制](docs/context.md)）。
- **檔案**：擁有者的檔案頁；**逐字稿**：每群每天一份 Markdown。
- **Hooks**：`<instance>/hooks/` 裡的執行檔可以擋下發文或加提醒（[docs/hooks.md](docs/hooks.md)）。
- **English／繁體中文** 介面。

## 安全預設
只聽 127.0.0.1。AA Forum 不提供 agent 檔案系統或終端機能力（不開 ACP fs／terminal）；權限請求交給人處理，
除非該群開了自動核准。角色 token 只在記憶體（資料庫只存雜湊）。資料庫與 `.aaf.env` 維持 0600。

## 開發
```
cargo test --workspace
(cd web && npm ci && npm run build)   # 前端改動後才需要；web/dist 已進 git
cargo run -p aaf -- --home ./dev-home up
tools/ci-local.sh                     # 本機 CI：預設只跑 macOS；另有 linux、linux-amd64（Docker）
tools/acceptance.py target/release/aaf <空目錄> report.json   # 真 Hermes 端到端驗收（花少量 token）
tools/release-mac.sh                  # 打包 macOS（arm64）tar.gz＋SHA256
```

發布：推 `vX.Y.Z` tag（要等於 Cargo.toml 的版本）會觸發 `.github/workflows/release.yml`：建三個平台、冒煙測試、
建 GitHub Release、發布 npm（第一版需要 repo secret `NPM_TOKEN`；套件在 npm 上建立後，之後可以改用 npm trusted publishing）。手動執行同一個 workflow
只建置與測試、不發布。打包與測試腳本：`tools/npm-pack.mjs`、`tools/npm-smoke.sh`。

## 文件
- [角色×群組持久上下文：操作、設定與限制](docs/context.md)
- [驗收報告](docs/acceptance.md)
- [Hooks](docs/hooks.md)、[變更紀錄](CHANGELOG.md)

## 授權
MIT，見 [LICENSE](LICENSE)。

# Hooks（外部規則）

把可執行檔放在 `<instance>/hooks/`（或環境變數 `AAF_HOOKS` 指定的目錄）即生效，不需重啟。
依檔名排序執行；`_`、`.` 開頭、不可執行、symlink 都略過。

每次呼叫：事件 JSON 從 stdin 傳入，`AAF_HOOK_PHASE` 是下列之一。規則印到 stdout 的文字就是結果。

| phase | 何時 | 印出文字的效果 |
|---|---|---|
| `pre` | 人或 agent 發群文、建立任務之前 | 擋下；文字是給發文者的原因（人看到 422，agent 下次叫醒時收到） |
| `post` | 發出之後 | 提醒；下次叫醒「發文者」時附在提示最前面（只附一次、同內容去重） |
| `wake` | 每次叫醒 agent 之前 | 附在這一輪提示最前面 |
| `turn_end` | 每一輪結束後 | 提醒；下次叫醒該 agent 時附上 |

事件欄位：`type`（`chat.post`／`task.post`／`wake`／`turn_end`）、`sender`、`rank`（human／lead／worker）、
`room`、`body`、`reply_to`、`to`（任務指派對象）、`role`、`unread`、`session_new`、`ok`、`error`、`text`。

安全與失敗處理（fail-open）：
- 規則逾時（5 秒）、非 0 結束、無法啟動時一律放行，原因寫進 `var/hooks.log`。
- 規則只拿到 `PATH`、`HOME`、`AAF_HOME`、`AAF_HOOK_PHASE`，不會拿到任何 token。
- 系統公告不跑規則。

範例：`examples/hooks/10_long_message.sh`（agent 發文超過 1500 字要附理由）。

# Codex Tool Output Persistence

本機 Rust CLI 與 Codex 原生 Bash hook。長輸出先存到本機，tool result 只保留固定預覽與讀取命令。
Codex 直接連原本的模型 provider。本工具沒有 HTTP server，不需要 Docker、proxy token 或網路 fallback。
保留 `codex-turn-proxy` 執行檔名稱，讓既有 hook 與卡片讀取命令仍可使用。

## 資料流程

1. Bash hook 先執行既有 RTK／etoon transformer。
2. CLI 執行轉換後的命令，將長輸出存入使用者的本機目錄。
3. tool result 回傳前 2,000 個 Unicode 字元、ID、原文路徑與讀取命令。
4. 模型需要更多證據時，透過 `read` 讀取指定行數。結果追加到正常對話歷史。
5. 新增檔案時清理過期資料；可選的 systemd user timer 處理閒置期間清理。

落盤門檻是 4,096 bytes。預覽已涵蓋全文，或卡片不比原文短時，直接回傳原文。
單份輸出超過 8 MiB，或保存失敗時，回傳原始輸出與 exit code。
CLI 不改寫歷史、不自動展開卡片，也不控制 Codex 的 compaction 或 prompt cache。
這些 fallback 仍受 Codex 自己的工具輸出上限限制。

## 安裝

需要 Linux／WSL、Rust 1.88 以上，以及支援 Bash `PreToolUse` hook 的 Codex。

```bash
cargo build --release --locked
install -d -m 700 ~/.local/lib/codex-turn-proxy
install -m 755 target/release/codex-turn-proxy ~/.local/lib/codex-turn-proxy/codex-turn-proxy
~/.local/lib/codex-turn-proxy/codex-turn-proxy hook-config
```

將 `hook-config` 產生的 handler 合併到目前 Codex 帳號的 `hooks.json`，保留其他 hooks。
若原本已有改寫 Bash 的 handler，改用下列 `pre_hook_argv` 串接，避免兩個 handler 競爭 `updatedInput`。
在 Codex `/hooks` 檢查並信任這個 handler，重新啟動 session 後驗證。
Hook 的 `permissionDecision: "allow"` 讓 Codex 接受 `updatedInput`；工具仍經正常 sandbox。

執行檔旁的 `client.json` 可設定既有 transformer 與保存目錄。
以下路徑需替換成你的絕對路徑。沒有 transformer 時可省略整份檔案。

```json
{
  "pre_hook_argv": ["python3", "/home/USER/.codex/hooks/rtk-etoon.py"],
  "output_dir": "/home/USER/.local/state/codex-turn-proxy/tool-results"
}
```

串接會保留前一個 hook 的 deny／ask 決策。`client.json` 建議使用 0600 權限。
舊的 `proxy_url`、`proxy_token` 欄位不再使用，可刪除。

## 讀取原文

```text
<persisted-output>
ID: <64-character ID>
Bash exit: 1 | 18000 bytes
Full output saved to: /home/USER/.local/state/codex-turn-proxy/tool-results/<ID>.txt
Read when needed: /absolute/path/codex-turn-proxy read <ID> 1 2000
Preview (first 2000 characters at most):
...
</persisted-output>
```

`read ID [OFFSET LIMIT]` 使用從 1 起算的行號，預設最多 2,000 行。
讀取刷新閒置期限；仍有內容時，結果附下一次 offset。
使用卡片中的執行檔絕對路徑，可在前面加 `rtk proxy`。
獨立的 `read`／`expand` 命令豁免再次落盤；複合 shell 命令與直接 `cat` 仍走一般門檻。
`expand` 是 `read` 的相容別名。既有 stdio MCP `expand` 也保留，但一般工作不需要 MCP。
原文過期後，需重新執行原工具。讀回全文會增加正常 input token。

## 清理與容量

| 設定 | 預設 | 意義 |
|---|---:|---|
| `OUTPUT_STORE_DIR` | `$HOME/.local/state/codex-turn-proxy/tool-results` | 原文目錄；優先於 `client.json` |
| `MIN_OUTPUT_BYTES` | 4096 | Bash 落盤門檻 |
| `STORE_IDLE_SECONDS` | 900 | 新增或讀取後的閒置期限，秒 |
| `MAX_STORE_BYTES` | 268435456 | 磁碟原文容量上限，256 MiB |
| `MAX_STORE_ENTRIES` | 8192 | 保存筆數上限 |

手動清理：

```bash
~/.local/lib/codex-turn-proxy/codex-turn-proxy cleanup
```

Linux／WSL 已啟用 systemd user manager 時，可安裝每 30 秒執行一次的清理排程：

```bash
install -d ~/.config/systemd/user
install -m 644 systemd/codex-output-cleanup.service systemd/codex-output-cleanup.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now codex-output-cleanup.timer
systemctl --user start codex-output-cleanup.service
```

排程沿用固定安裝路徑與同一份 `client.json`。若 hook 使用自訂環境變數，須同步到 service 的 `Environment=`。
User manager 停止或主機關機時不清理；下次 timer 執行或新增原文時再清理。
不使用 systemd 時，保存與讀取仍正常，但閒置期間需自行執行 `cleanup`。

目錄須為使用者擁有、權限 0700，檔案為 0600。容量不足時淘汰最久未使用的原文。
清理只移除本工具的原文與中斷寫入的 `.pending-*` 檔案，不修改對話卡片。
ID 由 session 與 tool_use_id 產生。保存採跨程序鎖與不可覆寫的原子發布，讀取拒絕 symlink。
同一本機使用者可以讀取自己的所有 session；這不是多租戶權限系統。

## 從 Docker 版遷移

先備份目前 helper、`client.json` 與 Codex 設定，再安裝本機 CLI／hook 與清理排程。
保留原有 output 目錄與執行檔路徑，讓未過期的卡片仍能讀取。

若曾設定 `model_provider = "turn_proxy"`，切回原本 provider，移除專用的 provider 定義與本機 proxy token。
只有為舊 proxy 停用的 request compression 設定才需還原。重新啟動 Codex 後驗證直連。
若本來就直連，不必修改 provider。

確認沒有 session 依賴舊 proxy 後，移除該專案的容器、專用網路與 image。
舊部署目錄仍有 Compose 檔時，可在該目錄執行 `docker compose down --rmi local`。
保留 bind mount 的原文目錄；不要使用全域 Docker prune。
本版已移除 Dockerfile、Compose 與 HTTP routes。`serve`／`healthcheck` 會回傳遷移提示。

## 驗證

```bash
cargo test --locked
cargo test --locked --test codex -- --ignored --nocapture
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```

Codex 整合測試使用隔離帳號與本機模擬 Responses 上游，不經 proxy，不呼叫付費模型。
測試驗證首次預覽、CLI read 追加原文、舊 input 前綴不變與 rollout。
測試專用 hook 使用 `--dangerously-bypass-hook-trust`，不修改日常帳號的信任設定。
這些測試不代表實際 API 費用或 cache 命中率改善；節省量取決於輸出內容與讀回比例。

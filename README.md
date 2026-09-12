# Codex Tool Output Persistence

Rust CLI＋Codex 原生 Bash hook。長輸出第一次就落盤，tool result 只保留固定 preview 與檔案路徑。Docker 轉送 Responses 請求並清理過期檔案，不注入原文，也不改寫 input。

```text
本地 Bash 產生長輸出
    → CLI 將原文寫入本機檔案
    → tool result 回傳 2,000 字元 preview、ID、檔案路徑及讀取命令
    → 第一次模型請求只有這張卡
後續請求
    → 同一張卡保持不變，不自動展開、不按 K 輪移除內容
模型需要原文
    → 呼叫卡片上的 CLI read 命令
    → 選定內容作為新的 tool result 追加
    → read 豁免再次落盤
檔案到期或容量不足
    → 清理磁碟原文，不改變對話中的卡片
```

這實作 Claude Code 的早期 tool-result persistence 模式，沒有移植 context_hint／microcompact。
Preview 固定為 **2,000 個 Unicode 字元**，對應本次指定的 S2e 大小。Bash 落盤門檻沿用 4,096 bytes。
Preview 已包含全部內容，或完整卡片不比原文短時，直接回傳原文。
這些單位不同；本程式沒有修改 Claude Code 的 binary 或設定。

## 日常使用

既有 hook 仍執行同一路徑的 `codex-turn-proxy hook`。它先串接既有 RTK／etoon，再包裝 Bash。
因此保存的是既有 transformer 處理後、原本會交給模型的完整輸出。
Capture 直接寫本機 `/tmp`，不需要 localhost 連線或 MCP。

卡片範例：

```text
<persisted-output>
ID: <64-character ID>
Bash exit: 1 | 18000 bytes
Full output saved to: /tmp/codex-tool-results-1000/<ID>.txt
Read when needed: '/absolute/path/codex-turn-proxy' read <ID> 1 2000
Preview (first 2000 characters at most):
...
</persisted-output>
```

`read ID [OFFSET LIMIT]` 使用從 1 起算的行號，預設最多 2,000 行。
有後續內容時，結果附上下次 offset。讀取會刷新檔案的閒置期限。
使用卡片所列的完整執行檔路徑，直接呼叫，或在前面加 `rtk proxy`。
Hook 以 shell token 解析識別單獨的 read／expand，並豁免這次輸出。
複合 shell 命令不豁免；例如 `read ...; another-command`。
直接用 `cat` 讀檔也能取得原文，但它仍受一般 Bash 落盤門檻限制。

`expand ID` 是 read 的相容別名。既有 stdio MCP `expand` 也直接回傳最多 2,000 行，沒有自動注入。
一般工作只需要 CLI。讀回內容會留在正常歷史，直到 Codex 自己做上下文管理。
Codex 的工具輸出 token 上限仍然適用；必要時指定較少行數。

## 設定與清理

| 設定 | 預設 | 意義 |
|---|---:|---|
| `OUTPUT_STORE_DIR` | `/tmp/codex-tool-results-<effective UID>` | 本機原文目錄 |
| `MIN_OUTPUT_BYTES` | 4096 | Host Bash 落盤門檻 |
| `STORE_IDLE_SECONDS` | 900 | 保存或 CLI 讀取後 15 分鐘過期 |
| `MAX_STORE_BYTES` | 268435456 | 磁碟原文容量上限，256 MiB |
| `MAX_STORE_ENTRIES` | 8192 | 最多保留的原文檔案數 |
| `UPSTREAM_BASE_URL` | `https://api.openai.com/v1` | Docker 轉送目的地 |
| `PROXY_TOKEN` | 必填 | 至少 32 bytes 的本機轉送驗證 token |

`KEEP_RECENT`、`MAX_INJECTED_BYTES` 已移除。沒有常駐記憶體原文 Store。
CLI 只在處理當次輸出或讀取時持有原文；清理只需暫存檔案 metadata。
單份輸出超過 8 MiB，或寫入失敗時，直接保留原始輸出與 exit code。
這些 fallback 仍受 Codex 自己的截斷限制。

每次新增原文會先清理到期檔案，容量不足時淘汰最久未使用的檔案。
Docker 每 30 秒掃描一次，即使沒有新工具輸出也會清理。
`codex-turn-proxy cleanup` 可手動執行同一套清理。
只用 CLI、沒有啟動 Docker 時，閒置期間不會有背景掃描。
被中斷寫入留下的 `.pending-*` 檔案也由清理移除。
其他檔名不會被此清理刪除。

目錄權限必須為 0700，原文檔案為 0600，且目錄屬於執行者。
ID 由 session 與 tool_use_id 產生，讀取只接受 64 位小寫十六進位 ID。
保存採用跨程序鎖與不可覆寫的原子發布。重複 ID 的不同內容會保存失敗並退回原文。
讀取拒絕 symlink，避免路徑穿越。相同本機使用者可以讀取自己的所有 session。
這不是多租戶權限系統，也不保證安全覆寫磁碟。

## Docker 與安裝

需要 Linux／WSL、Docker，以及 Rust 1.88 以上。Host helper 負責執行本機命令。
Docker 必須以同一 UID／GID 執行，並掛載同一個 output 目錄。

```bash
cargo build --release --locked
cp .env.example .env
# 將 openssl rand -hex 32 的結果填入 .env 的 PROXY_TOKEN。
# 以下範例假設本機 UID/GID 都是 1000。
install -d -m 700 /tmp/codex-tool-results-1000
docker compose up -d --build
curl --fail http://127.0.0.1:8787/healthz
```

在 `.env` 設定 `PROXY_TOKEN` 與 `UPSTREAM_BASE_URL`。
UID／GID 不為 1000 時，設定 `LOCAL_UID`、`LOCAL_GID` 和 `OUTPUT_STORE_DIR`。
Compose 不會自動建立 host 目錄，以免 root 建立錯誤權限。
Host CLI 與 Docker 的清理期限、容量設定必須一致。

Host helper 會讀執行檔旁的 `client.json`：

沒有使用 RTK／etoon 時，可省略 `client.json`，直接使用 capture。
以下設定只用於串接已安裝的 transformer。

```json
{
  "pre_hook_argv": ["python3", "/absolute/path/to/rtk-etoon.py"]
}
```

可選 `output_dir` 覆寫 host 的預設路徑；環境變數優先。
舊版的 `proxy_url`、`proxy_token` 欄位會被忽略，CLI 不再傳送原文給 HTTP 端點。

產生 hook 設定：

```bash
/absolute/path/codex-turn-proxy hook-config
```

把 handler 合併到目前 Codex 帳號的 `hooks.json`，保留其他 hooks。
若既有 hook 也改寫 Bash，透過 `pre_hook_argv` 串接；不要放兩個競爭 `updatedInput` 的 handler。
既有 hook 的 deny／ask 保持有效。新安裝在 `/hooks` 信任 handler。
Codex 0.154 的 hook 協定要求 `permissionDecision: "allow"` 才接受 updatedInput；工具仍經正常 sandbox。
本程式不修改 sandbox、模型、experimental_mode 或 prompt cache 設定。

將 host helper 安裝到固定的絕對路徑，並在 hook 設定使用同一路徑。
Provider 仍可使用既有 `turn_proxy`，位址 `http://127.0.0.1:8787/v1`。
ChatGPT 登入沿用 `requires_openai_auth = true` 與 `name = "OpenAI"`；上游為 `https://chatgpt.com/backend-api/codex`。
HTTP token 留在 provider 的 `x-proxy-token` header。
不支援 WebSocket。模型清單 query 由 Codex 提供，不固定 client_version。

ChatGPT 登入的最小 provider 設定如下。先在 `.env` 設定上面所列的 ChatGPT 上游，並完成 `codex login`。
把這些項目合併到目前 Codex 帳號的 `config.toml`，保留其他既有設定：

```toml
model_provider = "turn_proxy"

[features]
hooks = true
enable_request_compression = false

[model_providers.turn_proxy]
name = "OpenAI"
base_url = "http://127.0.0.1:8787/v1"
wire_api = "responses"
requires_openai_auth = true
supports_websockets = false

[model_providers.turn_proxy.http_headers]
x-proxy-token = "REPLACE_WITH_THE_SAME_PROXY_TOKEN_FROM_ENV"
```

包含 token 的設定檔使用 0600 權限。修改 provider／hook 設定後重新啟動 Codex。
若只使用 CLI 保存與讀取，則不需要更換 provider；自行執行 `cleanup` 以清理過期檔案。

## 請求與相容性

`POST /v1/responses` 與 `/v1/responses/compact` 原樣轉送 body bytes，包括欄位順序與空白。
Proxy 不掃卡片、不注入、不刪除 input，也不依賴完整歷史。
`previous_response_id` 可直接交給上游判斷。舊 `/store`、`/expand` HTTP 端點已移除。
所有轉送端點需要本機 token；只有 `/healthz` 例外。
Proxy 移除本機 token 與 hop-by-hop headers，轉送 Authorization，保留 HTTP status 與 SSE 串流。
不記錄憑證、請求原文或模型回應。

新版 Docker 不保留舊版 RAM Store。舊卡片不會自動轉存，缺失的舊 ID 必須重跑原工具。
新檔案使用 host bind mount，Docker 重啟不會刪除它們；TTL、容量淘汰與系統 `/tmp` 清理仍適用。

此機制避免自己重寫歷史前綴，不保證服務端一定命中 prompt cache。
讀回全文會增加正常 input token，Codex 自己的 compaction 也可能改變前綴。
沒有宣稱實際 API token 費用或 cache 命中率改善百分比。

## 驗證

```bash
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
# 隔離 Codex 帳號、本地假 Responses server，不使用付費模型。
cargo test --locked --test codex -- --ignored --nocapture
```

原生測試驗證首次 preview、CLI read 追加原文、舊 input 前綴不變及保存的 rollout。
測試專用 hook 使用 `--dangerously-bypass-hook-trust`，不改動日常帳號設定。

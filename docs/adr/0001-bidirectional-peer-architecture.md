# ADR 0001 — quickvm 真雙主（無縫雙向共享）架構

- **狀態**：Proposed（待 open questions 拍板後轉 Accepted）
- **日期**：2026-06-15
- **背景來源**：multi-agent 設計探索（survey + 4 架構提案 + judge panel + 綜合）

## Context

目前 quickvm 是單向主從：一個 process 一個角色（`Cmd` enum 只有 `Serve | Connect`，`crates/app/src/main.rs:34-50`）。`connect` 是唯一帶 capture 的角色、跑邊緣切換狀態機（`Target::Local/Remote`，drive 內 local 變數），`serve` 是被動注入 slave。PR#13 給了 Windows capture backend，所以「兩台都能當 controller」，但任一時刻仍是「一台 connect → 一台 serve」，**從未同時雙主**。

目標：真雙向無縫——兩台都接實體鍵鼠，任一台把游標推到「朝對端那側」邊緣都能過界控制對方（像 Deskflow / lan-mouse 的多機共享游標），推回交還。

關鍵硬約束（原始碼級查證）：

1. **無焦點權威**：兩台的 `Target` 各自獨立、非共享、非持久。直接讓兩台並存跑 capture，雙撞邊會同時 `set_grab(true)` 打架。
2. **macOS self-echo**：capture tap 裝在 `CGEventTapLocation::Session`，唯一過濾是 grab 模式 `location≈center` 的 warp 回響；**無 injected-source 過濾**。enigo 在 `CGEventTapLocation::HID`（Session 之下）post，事件向上流經 Session tap → 同機 capture+inject 必自噬。**但**：enigo 0.6.1 對每個注入事件蓋 `EVENT_SOURCE_USER_DATA`（預設 `EVENT_MARKER=100`，`Settings` 可自訂），`core-graphics 0.25` 有對應 field 與 getter → capture 端讀該欄位比對即可 drop，**一行可修**。Windows 靠 OS 的 `LLKHF/LLMHF_INJECTED` flag 免治（與 marker 無關）。
3. **transport 已全雙工**：connect(client) 撥向 serve(server) 的單一 QUIC connection 本身雙工——serve 已用 `BackLink` 反向 `open_uni` 回推剪貼簿。只是反向目前不傳 Input/Motion。
4. **HS_VERSION=3**，握手版本不符即 bail。

## Decision

把 serve/connect 二分升級為對稱複合角色 **`quickvm peer`**：每台一個 process 同時持 capture + `EnigoEmulator` + 統一三態事件迴圈。`serve`/`connect` 保留為相容與除錯入口。

四個核心裁決：

### 1. 角色：peer 三態狀態機
`Target` 從二態 `Local/Remote` 擴成三態：
- **Local**：capture 判自己這側邊緣，鍵鼠不轉發。
- **Controlling**：本機 grab、轉發 Input/Motion 給對端（本機是 active controller）。
- **Controlled**：被對端控制，注入對端事件到本機、short-circuit 自身 `enter_point`、靠 marker filter 擋自噬。

### 2. 焦點仲裁：單調 epoch + node_id tie-break（去中心，無單點 arbiter）
焦點權威 = 全網單調 `epoch:u64` + `node_id` tie-break。過界走**混合策略**：
- 撞邊 → 樂觀本地 `set_grab(true)` + 送 `Claim{epoch=known+1, node, x, y, mods}`；
- **但實際轉發 Input/Motion 要等 `Grant` 才開始**；
- 等 Grant 的短窗內本機 grab 已凍結（不漏游標）；
- 若收到對端**較大 epoch** 的 Claim/Grant，立即 `set_grab(false)` 退回 Local（撤銷越界）。

效果：單邊撞邊（常態）本地先 grab 幾乎無感；雙撞邊（罕見）靠 epoch 序 + tie-break + 「轉發等 Grant」三道閘收斂，**可形式化證明杜絕雙重控制對端**。摒棄中央 arbiter（單點 + 全 RTT 過界稅）與「永遠服從 Enter」（雙撞邊會變雙無主、游標卡死）。

### 3. self-echo（macOS 唯一缺口）
- `EnigoEmulator` 改帶**自訂非 100** marker（避免撞其他 enigo app）；常數放共用 crate 給 capture/emulation 對齊。
- capture Session tap callback 最前面讀 `EVENT_SOURCE_USER_DATA`，命中 marker 即放行（給本機 app）但不 translate/不進 channel。
- 雙層防線：marker drop（主）+ Controlled 態 short-circuit `enter_point`（冗餘）+ 既有 center-warp 過濾（正交）。
- Windows 免改（OS injected flag）。

### 4. transport：單 link 全雙工 + node_id pin
- `BackLink` 擴成對稱 `Link`（反向加 Input/Control/Motion 路徑）。單 link → PSK 只認證一次、pin 一次、**無 connection dedup 競態**。
- 兩端都 `load_or_create_cert`（目前只 serve 端有，connect 端要補）+ 互 pin。
- **known_hosts key 從 addr 改 node_id**（應對 Mac 網段浮動——addr 當 key 不穩）。
- HS_VERSION 3→4，握手加交換 node_id；dial/listen 用 node_id 字典序 deterministic 決定，兩端都 spawn 重連 watchdog。

## 實作階段

- **階段 0（self-echo，可獨立先合）**：emulation 加自訂 marker + capture macOS 過濾。不改架構、低風險、可單機測（serve+connect 同機驗不自噬）。雙主硬前提。
- **階段 1（wire + 焦點型別）**：`Control` 擴 `Claim/Grant/Reject/Release`（帶 epoch）；epoch 收斂 + tie-break 寫成純函數 + 單元測試（雙撞邊 epoch 相同→tie-break、舊 epoch 丟棄、Reject 收斂）。HS_VERSION 3→4。
- **階段 2（transport 對稱化）**：connect 端補持久 cert；握手交換 node_id；PinVerify key 改 node_id（known_hosts 行格式改、純函數與測試配合改）；BackLink 擴對稱 Link；deterministic dial/listen + 雙端 watchdog。
- **階段 3（複合 peer loop，最大重活）**：`Cmd::Peer`；融合 run_serve + drive 成三態統一迴圈；同 process 持 capture handle + EnigoEmulator；back_rx 擴 Input/Control/Motion 分支；back channel capacity 16→256。
- **階段 4（失敗模式 + 看門狗）**：斷線一律回 Local + ungrab + release_all；Controlled 偵測 Disconnected 自動奪回本機 grab；macOS TapDisabled 主動送 Release 通知對端；epoch 跨重啟 max() 對齊先於任何 Claim；換手 mods 雙向對齊。
- **階段 5（高頻反向路徑，不推遲）**：反向 Input 不沿用「每 event open 新 uni stream」，改長命 bidi stream（length-prefixed frame）或鍵盤降 datagram；Motion 走 per-direction seq 命名空間。
- **階段 6（實機驗證）**：Mac↔Win 雙向過界、組合鍵、剪貼簿/檔案雙向、雙撞邊 race、斷線各場景、TapDisabled。grab/游標只能實體測。

## 風險與取捨

- **雙撞邊 < 1 RTT「短暫雙凍結」**：混合策略在罕見雙撞邊時兩端各自先 grab、收較大 epoch 者退回，退回前 < 1 RTT 兩台游標都凍。接受為可接受瞬態（罕見、自癒）以換無單點 + 常態低延遲。
- **epoch 跨重啟**：重啟歸零會被當過期 Reject 卡住。緩解：epoch 用 wall-clock 高位 + counter 低位起始 + 重連握手 max() 對齊先於 Claim。
- **假性分區奪 grab**（regression 風險）：Mac WiFi 抖時 QUIC 判死但實體網路只是抖，被控端奪回 grab 會「打字打一半焦點被搶」。緩解：奪 grab 前加 grace（token 心跳超時，config 可調、預設保守偏長）。
- **複合 loop 重寫量**：run_serve（被動、無 capture）與 drive（狀態機 + grab）語意完全不同，融合 + grab 雙來源（本機撞邊 + 對端接管）是此 codebase 最大結構重寫，排期別低估。
- **marker 碰撞**：`EVENT_SOURCE_USER_DATA` 無命名空間，他人注入器（Karabiner/BTT）若用同值會繞過/誤殺。機率極低；第二道防線（Controlled short-circuit）兜底；long-term 走 emulation v2 virtual-HID。
- **範圍**：本設計天生兩台對稱；>2 台共享需重新設計成中央 server 星狀，**不在此案**。
- **平台繼承**：macOS Secure Input / Windows secure desktop 盲區雙向都繼承，焦點協商在密碼欄/UAC 失效，靠 disconnect/timeout 兜底。
- **部署前提**：king 反向連浮動 Mac 需 Tailscale（目前 king 未裝），或兩台固定同網段。

## Open Questions（進實作前須定）

1. **Controlled 態本機實體輸入吞還是放行**：被控時本機鍵鼠要不要鎖死（grab=true）？建議鎖死（像真 KVM），需實機驗證 macOS CGEventTap `Drop` 與 enigo HID 層 post 注入能否共存（理論可行未實測）。
2. **epoch 起始策略**：wall-clock 高位 + counter（免持久化、需時鐘同步）vs 純握手 max() 對齊（免時鐘、重啟後第一次 Claim 前須先對齊）？取決於 Mac/Win 時鐘同步精度。
3. **node_id 生成**：cert 指紋衍生（綁身分）vs config 顯式邏輯名（mac/king，穩定但要手設）？

## Consequences

- serve/connect 降為相容/除錯入口；peer 成主路徑，config 從單向 `server/side` 改鏡像式（對端 addr + 對端在我哪側 + 對端尺寸 + node_id），兩台 side 互為相反，建議加啟動時 side 一致性自校驗。
- 階段 0 的 self-echo 修是獨立價值（修同機 capture+inject 自噬潛在 bug），即使不續做雙主也值得合入。
- HANDOFF.md / README 角色文件需改寫。

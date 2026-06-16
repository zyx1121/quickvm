import AppKit
import ApplicationServices

/// QuicKVM — quickvm 主控端（`connect`）的選單列外殼。
/// LSUIElement app：無 Dock 圖示，選單列是唯一控制入口（啟動 / 停止 / 開機自啟）。
/// 真正捕捉鍵鼠 + QUIC 轉發的是嵌在 bundle 裡的 Rust binary，本殼只負責 spawn / kill 它。
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private var controller: QuickVMController!
    private var statusBar: StatusBarController!

    func applicationDidFinishLaunching(_: Notification) {
        let c = QuickVMController()
        controller = c
        statusBar = StatusBarController(controller: c)

        // CGEventTap 需「輔助使用」權限，TCC 歸屬於本 app（helper 由本 app spawn）。
        // 已授權 → 直接連線；未授權 → 彈窗把 QuicKVM 加進清單，授權後使用者自行點啟動。
        if AXIsProcessTrusted() {
            c.start()
        } else {
            _ = AXIsProcessTrustedWithOptions(["AXTrustedCheckOptionPrompt": true] as CFDictionary)
        }
    }

    func applicationWillTerminate(_: Notification) {
        controller?.stop() // 結束 app 一併收掉 helper，不留孤兒程序持續吞輸入
    }
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.accessory) // 無 Dock 圖示的背景 app
app.run()

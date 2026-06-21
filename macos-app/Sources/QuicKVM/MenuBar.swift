import AppKit
import ApplicationServices
import ServiceManagement

/// 選單列入口 — LSUIElement app 沒有 Dock 圖示，這是唯一能控制 app 的地方。
/// 圖示實心 = 執行中、空心 = 已停止；選單每次打開重建以刷新狀態。
@MainActor
final class StatusBarController: NSObject, NSMenuDelegate {
    private let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
    private let controller: QuickVMController

    init(controller: QuickVMController) {
        self.controller = controller
        super.init()
        let menu = NSMenu()
        menu.delegate = self
        item.menu = menu
        item.button?.image = Self.mark
        rebuildMenu()
    }

    /// 品牌 mark（全家共用的 zyx 標）。不隨執行/停止換樣 —— 狀態看選單文字。template：跟著選單列明暗。
    private static let mark: NSImage = {
        let img = Bundle.main.path(forResource: "MenubarIcon", ofType: "png")
            .flatMap { NSImage(contentsOfFile: $0) }
            ?? NSImage(systemSymbolName: "bolt.fill", accessibilityDescription: "QuicKVM")!
        img.isTemplate = true
        let h: CGFloat = 18
        img.size = NSSize(width: h * img.size.width / max(img.size.height, 1), height: h)
        return img
    }()

    func menuWillOpen(_: NSMenu) { rebuildMenu() }

    private func rebuildMenu() {
        guard let menu = item.menu else { return }
        menu.removeAllItems()

        let title = NSMenuItem(title: "QuicKVM", action: nil, keyEquivalent: "")
        title.isEnabled = false
        menu.addItem(title)

        let status = NSMenuItem(title: statusText(), action: nil, keyEquivalent: "")
        status.isEnabled = false
        menu.addItem(status)
        menu.addItem(.separator())

        add(menu, controller.isRunning ? "停止" : "啟動", #selector(toggleRun))
        if !AXIsProcessTrusted() {
            add(menu, "授權輔助使用…", #selector(grantAccessibility))
        }
        menu.addItem(.separator())

        add(menu, "開啟 log", #selector(openLog))
        add(menu, "開啟設定檔", #selector(openConfig))
        let launch = add(menu, "開機時啟動", #selector(toggleLaunchAtLogin))
        launch.state = (SMAppService.mainApp.status == .enabled) ? .on : .off
        menu.addItem(.separator())

        add(menu, "結束", #selector(quit), key: "q")
    }

    private func statusText() -> String {
        switch controller.status() {
        case .stopped: return "○ 已停止"
        case .running: return "● 執行中"
        case .needsAccessibility: return "⚠ 需要輔助使用權限"
        }
    }

    @discardableResult
    private func add(_ menu: NSMenu, _ title: String, _ action: Selector, key: String = "") -> NSMenuItem {
        let i = NSMenuItem(title: title, action: action, keyEquivalent: key)
        i.target = self
        menu.addItem(i)
        return i
    }

    @objc private func toggleRun() {
        controller.toggle()
    }

    /// 彈窗把 QuicKVM 加進「輔助使用」清單並開啟設定面板（CGEventTap 的 TCC 主體是本 app）。
    @objc private func grantAccessibility() {
        _ = AXIsProcessTrustedWithOptions(["AXTrustedCheckOptionPrompt": true] as CFDictionary)
        NSWorkspace.shared.open(URL(string:
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")!)
    }

    @objc private func openLog() { NSWorkspace.shared.open(controller.logURL) }

    @objc private func openConfig() {
        let url = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".config/quickvm/config.toml")
        NSWorkspace.shared.open(url)
    }

    @objc private func toggleLaunchAtLogin() {
        do {
            if SMAppService.mainApp.status == .enabled {
                try SMAppService.mainApp.unregister()
            } else {
                try SMAppService.mainApp.register()
            }
        } catch {
            NSLog("QuicKVM: 開機自啟切換失敗（app 需在 /Applications，見 make install）— \(error.localizedDescription)")
        }
        rebuildMenu()
    }

    @objc private func quit() { NSApp.terminate(nil) }
}

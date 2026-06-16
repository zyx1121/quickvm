import AppKit
import Foundation

/// 管理嵌入的 Rust helper（`quickvm connect`）：spawn / kill + 從 log 尾推斷狀態。
@MainActor
final class QuickVMController {
    private var process: Process?

    /// 狀態變更時通知選單刷新（啟動 / 停止 / helper 自行退出）。
    var onStateChange: (() -> Void)?

    /// 嵌在 bundle 裡的 Rust helper：Contents/Helpers/quickvm。
    /// 不放 MacOS/ —— APFS case-insensitive 下 `quickvm` 會跟主執行檔 `QuicKVM` 撞同一路徑互相覆蓋。
    private var helperURL: URL {
        Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/quickvm")
    }

    /// connect log → ~/Library/Logs/QuicKVM.log（helper 以 --log-file 寫入；選單「開啟 log」開它）。
    let logURL: URL = {
        let dir = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Logs", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("QuicKVM.log")
    }()

    var isRunning: Bool { process?.isRunning ?? false }

    func start() {
        guard !isRunning else { return }
        let p = Process()
        p.executableURL = helperURL
        // GUI app 無 shell env → QUICKVM_SECRET 走 ~/.config/quickvm/secret fallback；config 同層自動讀。
        p.arguments = ["connect", "--log-file", logURL.path]
        p.terminationHandler = { [weak self] _ in
            Task { @MainActor in self?.handleTermination() }
        }
        do {
            try p.run()
            process = p
        } catch {
            NSLog("QuicKVM: 啟動 helper 失敗 — \(error.localizedDescription)")
            process = nil
        }
        onStateChange?()
    }

    func stop() {
        guard let p = process, p.isRunning else {
            process = nil
            onStateChange?()
            return
        }
        // SIGTERM：CGEventTap 隨程序死由 kernel 自動拆、cursor hide 還原 → 輸入歸還本機，安全。
        p.terminate()
        onStateChange?() // process 由 terminationHandler 清 nil
    }

    func toggle() { isRunning ? stop() : start() }

    private func handleTermination() {
        process = nil
        onStateChange?()
    }

    enum Status { case stopped, running, needsAccessibility }

    /// best-effort 讀 log 尾：tap 失敗（沒授權）時程序仍活著但不捕捉輸入，必須跟「執行中」區分。
    func status() -> Status {
        guard isRunning else { return .stopped }
        let lines = tailLog(60)
        let failIdx = lines.lastIndex { $0.contains("需要「輔助使用") }
        let okIdx = lines.lastIndex { $0.contains("CGEventTap 啟動") }
        if let f = failIdx, okIdx == nil || f > okIdx! { return .needsAccessibility }
        return .running
    }

    private func tailLog(_ n: Int) -> [String] {
        guard let s = try? String(contentsOf: logURL, encoding: .utf8) else { return [] }
        return Array(s.split(separator: "\n", omittingEmptySubsequences: false).map(String.init).suffix(n))
    }
}

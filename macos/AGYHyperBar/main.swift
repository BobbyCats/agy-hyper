import AppKit
import Darwin
import Foundation
import SwiftUI

// MARK: - Daemon JSON Contracts

struct RadarSessionItem: Decodable, Identifiable, Equatable {
    let id: String
    let client: String
    let title: String
    let group: String
    let code: String
    let workspace: String
    let live_state: String
    let state_reason: String
    let updated_at_ms: UInt64
    let open_url: String?
}

struct SessionRadarPayload: Decodable, Equatable {
    let ok: Bool
    let waiting_count: Int
    let stalled_count: Int
    let unread_count: Int
    let running_count: Int
    let sound_enabled: Bool
    let ui_language: String?
    let glyph_style: String?
    let attention_items: [RadarSessionItem]
    let recent_items: [RadarSessionItem]
}

struct DaemonEnvelope<T: Decodable>: Decodable {
    let status: String
    let data: T?
}

// MARK: - Shared JSON i18n Loader (Zero Hardcoded UI Copy)

final class SharedLocalizer: @unchecked Sendable {
    static let supported = ["zh-CN", "en", "ja"]
    private let bundles: [String: [String: Any]]

    init() {
        var loaded: [String: [String: Any]] = [:]
        if let dir = Self.resolveLocalesDir() {
            for code in Self.supported {
                let url = dir.appendingPathComponent("\(code).json")
                if let data = try? Data(contentsOf: url),
                   let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
                    loaded[code] = obj
                }
            }
        }
        self.bundles = loaded
    }

    static func resolveLocalesDir() -> URL? {
        if let custom = ProcessInfo.processInfo.environment["AGY_HYPER_LOCALES_DIR"], !custom.isEmpty {
            let url = URL(fileURLWithPath: custom)
            if FileManager.default.fileExists(atPath: url.path) { return url }
        }
        let arg0 = URL(fileURLWithPath: CommandLine.arguments[0]).resolvingSymlinksInPath()
        let binDir = arg0.deletingLastPathComponent()
        let candidates = [
            binDir.deletingLastPathComponent().appendingPathComponent("sidecars/hyper_deck/locales"),
            binDir.deletingLastPathComponent().deletingLastPathComponent().appendingPathComponent("plugin/sidecars/hyper_deck/locales"),
            URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent(".gemini/config/plugins/agy-hyper/sidecars/hyper_deck/locales"),
        ]
        return candidates.first { FileManager.default.fileExists(atPath: $0.path) }
    }

    static func detectSystemLanguage() -> String {
        let pref = (Locale.preferredLanguages.first ?? "en").lowercased()
        if pref.hasPrefix("zh") { return "zh-CN" }
        if pref.hasPrefix("ja") { return "ja" }
        return "en"
    }

    func resolveLanguage(_ configured: String?) -> String {
        let raw = (configured ?? "auto").trimmingCharacters(in: .whitespacesAndNewlines)
        if Self.supported.contains(raw) { return raw }
        return Self.detectSystemLanguage()
    }

    func t(_ keyPath: String, lang: String, vars: [String: String] = [:]) -> String {
        let parts = keyPath.split(separator: ".").map(String.init)
        let raw = lookup(parts, in: bundles[lang])
            ?? lookup(parts, in: bundles["en"])
            ?? keyPath
        var out = raw
        for (k, v) in vars {
            out = out.replacingOccurrences(of: "{{\(k)}}", with: v)
        }
        return out
    }

    private func lookup(_ parts: [String], in dict: [String: Any]?) -> String? {
        guard var cur: Any = dict else { return nil }
        for p in parts {
            guard let next = (cur as? [String: Any])?[p] else { return nil }
            cur = next
        }
        return cur as? String
    }
}

// MARK: - Sub-Millisecond UDS Client

enum HyperUdsClient {
    static func socketPath() -> String {
        if let custom = ProcessInfo.processInfo.environment["AGY_HYPER_DATA_DIR"], !custom.isEmpty {
            return (custom as NSString).appendingPathComponent("run/agy-hyper-v1.sock")
        }
        return (NSHomeDirectory() as NSString)
            .appendingPathComponent(".gemini/antigravity/agy-hyper/run/agy-hyper-v1.sock")
    }

    static func sendRaw(_ jsonLine: String) -> Data? {
        let path = socketPath()
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return nil }
        defer { close(fd) }

        var tv = timeval(tv_sec: 1, tv_usec: 500_000)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let utf8 = Array(path.utf8CString)
        let maxLen = MemoryLayout.size(ofValue: addr.sun_path)
        guard utf8.count <= maxLen else { return nil }
        withUnsafeMutablePointer(to: &addr.sun_path) { ptr in
            ptr.withMemoryRebound(to: CChar.self, capacity: maxLen) { dst in
                for (idx, ch) in utf8.enumerated() { dst[idx] = ch }
            }
        }

        let connectRes = withUnsafePointer(to: &addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                connect(fd, sa, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard connectRes == 0 else { return nil }

        let payload = Array((jsonLine + "\n").utf8)
        let sent = payload.withUnsafeBytes { rawBuf in
            Darwin.write(fd, rawBuf.baseAddress, rawBuf.count)
        }
        guard sent == payload.count else { return nil }

        var out = Data()
        var chunk = [UInt8](repeating: 0, count: 8192)
        while true {
            let n = Darwin.read(fd, &chunk, chunk.count)
            if n <= 0 { break }
            out.append(chunk, count: n)
            if chunk[0..<n].contains(0x0A) { break }
        }
        return out.isEmpty ? nil : out
    }

    static func fetchRadar() -> SessionRadarPayload? {
        guard let raw = sendRaw(#"{"op":"session_radar","recent_limit":6}"#) else { return nil }
        return try? JSONDecoder().decode(DaemonEnvelope<SessionRadarPayload>.self, from: raw).data
    }

    static func sendJson(_ obj: [String: Any]) {
        if let data = try? JSONSerialization.data(withJSONObject: obj),
           let s = String(data: data, encoding: .utf8) {
            _ = sendRaw(s)
        }
    }
}

// MARK: - Observable Radar Store

@MainActor
final class RadarStore: ObservableObject {
    @Published var payload: SessionRadarPayload?
    @Published var lang: String
    @Published var glyphStyle: String = "waveform"
    @Published var isSyncingTitles: Bool = false
    let i18n = SharedLocalizer()

    init() {
        self.lang = SharedLocalizer.detectSystemLanguage()
    }

    func refresh() {
        let fetched = HyperUdsClient.fetchRadar()
        if let p = fetched {
            let resolved = i18n.resolveLanguage(p.ui_language)
            if lang != resolved { lang = resolved }
            if let gs = p.glyph_style, !gs.isEmpty, glyphStyle != gs {
                glyphStyle = gs
            }
        }
        if payload != fetched {
            payload = fetched
        }
    }

    func t(_ key: String, _ vars: [String: String] = [:]) -> String {
        i18n.t(key, lang: lang, vars: vars)
    }

    func selectLanguage(_ code: String) {
        lang = code
        HyperUdsClient.sendJson(["op": "set_language", "language": code])
        refresh()
    }

    func selectGlyphStyle(_ style: String) {
        glyphStyle = style
        HyperUdsClient.sendJson(["op": "set_glyph_style", "style": style])
        refresh()
    }

    func toggleSound() {
        let next = !(payload?.sound_enabled ?? true)
        HyperUdsClient.sendJson([
            "op": "set_toggle",
            "key": "enable_radar_sound",
            "value": next,
        ])
        refresh()
    }

    func syncTitles() {
        guard !isSyncingTitles else { return }
        isSyncingTitles = true
        let storeRef = self
        DispatchQueue.global(qos: .userInitiated).async {
            HyperUdsClient.sendJson(["op": "namer_sync", "limit": 16])
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.8) {
                storeRef.refresh()
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 5.0) {
                storeRef.isSyncingTitles = false
                storeRef.refresh()
            }
        }
    }

    func formatAge(_ tsMs: UInt64) -> String {
        guard tsMs > 0 else { return "—" }
        let nowMs = UInt64(Date().timeIntervalSince1970 * 1000)
        let sec = nowMs > tsMs ? Int((nowMs - tsMs) / 1000) : 0
        if sec < 5 { return t("menubar.age_now") }
        if sec < 60 { return t("menubar.age_s", ["n": "\(sec)"]) }
        let min = sec / 60
        if min < 60 { return t("menubar.age_m", ["n": "\(min)"]) }
        let hr = min / 60
        if hr < 24 { return t("menubar.age_h", ["n": "\(hr)"]) }
        return t("menubar.age_d", ["n": "\(hr / 24)"])
    }

    static func cleanTitle(_ raw: String, code: String, group: String) -> String {
        let s = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !s.isEmpty else { return s }
        // Strip duplicate `<Code>·` when group pill is already displayed right beneath
        let parts = s.split(separator: " ", maxSplits: 1, omittingEmptySubsequences: true)
        if parts.count == 2 {
            let emoji = String(parts[0])
            let body = String(parts[1])
            if let dotIdx = body.firstIndex(of: "·") {
                let prefix = String(body[..<dotIdx]).trimmingCharacters(in: .whitespaces)
                let rest = String(body[body.index(after: dotIdx)...]).trimmingCharacters(in: .whitespaces)
                if !prefix.isEmpty,
                   !rest.isEmpty,
                   (prefix.caseInsensitiveCompare(code) == .orderedSame || group.localizedCaseInsensitiveContains(prefix)) {
                    return "\(emoji) \(rest)"
                }
            }
        }
        return s
    }
}

// MARK: - 120Hz ProMotion Open-Source Inspired Animated Status Glyphs

enum RadarPalette {
    static func color(for state: String) -> Color {
        switch state {
        case "waiting": return Color(red: 0.96, green: 0.62, blue: 0.08)
        case "stalled": return Color(red: 0.94, green: 0.28, blue: 0.38)
        case "unread":  return Color(red: 0.16, green: 0.64, blue: 0.96)
        case "running": return Color(red: 0.14, green: 0.76, blue: 0.52)
        default:        return Color.secondary
        }
    }
}

struct LiveWaveformGlyph: View {
    let state: String
    let style: String
    let size: CGFloat

    var body: some View {
        let tint = RadarPalette.color(for: state)
        Group {
            switch state {
            case "running":
                TimelineView(.animation) { timeline in
                    let t = timeline.date.timeIntervalSinceReferenceDate
                    Canvas { ctx, sz in
                        if style == "pulse_rings" {
                            // uiball/ldrs `l-ripples`: dual expanding concentric wave rings
                            let center = CGPoint(x: sz.width / 2, y: sz.height / 2)
                            let maxR = min(sz.width, sz.height) * 0.46
                            for offset in [0.0, 0.5] {
                                let p = (t * 0.65 + offset).truncatingRemainder(dividingBy: 1.0)
                                let r = max(1.5, maxR * p)
                                let alpha = 1.0 - p
                                var ring = Path()
                                ring.addArc(center: center, radius: r, startAngle: .zero, endAngle: .degrees(360), clockwise: false)
                                ctx.stroke(ring, with: .color(tint.opacity(alpha)), lineWidth: 1.45)
                            }
                            let coreR: CGFloat = 1.9
                            ctx.fill(
                                Path(ellipseIn: CGRect(x: center.x - coreR, y: center.y - coreR, width: coreR * 2, height: coreR * 2)),
                                with: .color(tint)
                            )
                        } else if style == "bars_fade" {
                            // n3r4zzurr0/svg-spinners `bars-scale-fade`
                            let count = 3
                            let barW = sz.width * 0.19
                            let gap = (sz.width - barW * CGFloat(count)) / CGFloat(count + 1)
                            for i in 0..<count {
                                let phase = t * 6.2 - Double(i) * 0.85
                                let norm = (sin(phase) + 1.0) * 0.5
                                let h = sz.height * (0.34 + 0.56 * norm)
                                let x = gap + CGFloat(i) * (barW + gap)
                                let y = (sz.height - h) * 0.5
                                let rect = CGRect(x: x, y: y, width: barW, height: h)
                                ctx.fill(
                                    Path(roundedRect: rect, cornerRadius: barW * 0.5),
                                    with: .color(tint.opacity(0.42 + 0.58 * norm))
                                )
                            }
                        } else {
                            // Default: uiball/ldrs `l-waveform` 4-bar harmonic traveling sine wave
                            let count = 4
                            let barW = sz.width * 0.145
                            let totalBarsW = barW * CGFloat(count)
                            let gap = (sz.width - totalBarsW) / CGFloat(count + 1)
                            for i in 0..<count {
                                let phase = t * 6.8 - Double(i) * 1.05
                                let norm = (sin(phase) + 1.0) * 0.5
                                let h = sz.height * (0.26 + 0.64 * norm)
                                let x = gap + CGFloat(i) * (barW + gap)
                                let y = (sz.height - h) * 0.5
                                let rect = CGRect(x: x, y: y, width: barW, height: h)
                                ctx.fill(
                                    Path(roundedRect: rect, cornerRadius: barW * 0.5),
                                    with: .color(tint.opacity(0.56 + 0.44 * norm))
                                )
                            }
                        }
                    }
                }
            case "waiting":
                // uiball/ldrs `l-dot-wave`: 3 traveling prompt beads
                TimelineView(.animation) { timeline in
                    let t = timeline.date.timeIntervalSinceReferenceDate
                    Canvas { ctx, sz in
                        let r = sz.width * 0.115
                        let xs = [sz.width * 0.22, sz.width * 0.50, sz.width * 0.78]
                        for (i, cx) in xs.enumerated() {
                            let wave = sin(t * 5.2 - Double(i) * 0.95)
                            let cy = sz.height * 0.5 + CGFloat(wave) * (sz.height * 0.14)
                            let alpha = 0.55 + 0.45 * ((wave + 1.0) * 0.5)
                            ctx.fill(
                                Path(ellipseIn: CGRect(x: cx - r, y: cy - r, width: r * 2, height: r * 2)),
                                with: .color(tint.opacity(alpha))
                            )
                        }
                    }
                }
            case "unread":
                Image(systemName: "checkmark.circle.fill")
                    .font(.system(size: size * 0.88, weight: .semibold))
                    .foregroundStyle(tint)
            case "stalled":
                Image(systemName: "waveform.slash")
                    .font(.system(size: size * 0.84, weight: .semibold))
                    .foregroundStyle(tint)
            default:
                Image(systemName: "waveform")
                    .font(.system(size: size * 0.78, weight: .regular))
                    .foregroundStyle(Color.secondary.opacity(0.55))
            }
        }
        .frame(width: size, height: size)
    }
}

// MARK: - Native GPU MenuBar Status Capsule View

struct MenuBarStatusCapsule: View {
    @ObservedObject var store: RadarStore

    private var leadState: String {
        guard let p = store.payload else { return "idle" }
        if p.waiting_count > 0 { return "waiting" }
        if p.stalled_count > 0 { return "stalled" }
        if p.unread_count > 0  { return "unread" }
        if p.running_count > 0 { return "running" }
        return "idle"
    }

    private var countText: String {
        guard let p = store.payload else { return "" }
        var parts: [String] = []
        if p.waiting_count + p.stalled_count > 0 { parts.append("\(p.waiting_count + p.stalled_count)") }
        if p.unread_count > 0  { parts.append("\(p.unread_count)") }
        if p.running_count > 0 { parts.append("\(p.running_count)") }
        return parts.joined(separator: "·")
    }

    var body: some View {
        let lead = leadState
        let badge = countText
        HStack(spacing: 4) {
            if lead == "running" {
                Image(systemName: "waveform")
                    .font(.system(size: 12.5, weight: .semibold))
                    .foregroundStyle(RadarPalette.color(for: "running"))
                    .symbolEffect(.variableColor.iterative.reversing, options: .repeating, isActive: true)
            } else if lead == "waiting" {
                Image(systemName: "waveform.badge.exclamationmark")
                    .font(.system(size: 12.5, weight: .semibold))
                    .foregroundStyle(RadarPalette.color(for: "waiting"))
                    .symbolEffect(.pulse, options: .repeating, isActive: true)
            } else if lead == "stalled" {
                Image(systemName: "waveform.slash")
                    .font(.system(size: 12.5, weight: .semibold))
                    .foregroundStyle(RadarPalette.color(for: "stalled"))
            } else if lead == "unread" {
                Image(systemName: "checkmark.circle.fill")
                    .font(.system(size: 12.5, weight: .semibold))
                    .foregroundStyle(RadarPalette.color(for: "unread"))
            } else {
                Image(systemName: "waveform")
                    .font(.system(size: 12.5, weight: .medium))
                    .foregroundStyle(.primary.opacity(0.85))
            }

            if !badge.isEmpty {
                Text(badge)
                    .font(.system(size: 11, weight: .semibold, design: .monospaced))
                    .foregroundStyle(.primary)
            }
        }
        .padding(.horizontal, 4)
        .frame(height: 22)
        .fixedSize()
    }
}

final class PassthroughHostingView<Content: View>: NSHostingView<Content> {
    override func hitTest(_ point: NSPoint) -> NSView? {
        nil
    }
}

// MARK: - Popover Session Card & Full Layout

struct RadarControlButtonStyle: ButtonStyle {
    let pressScale: CGFloat

    init(pressScale: CGFloat = 0.96) {
        self.pressScale = pressScale
    }

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(configuration.isPressed ? pressScale : 1.0)
            .opacity(configuration.isPressed ? 0.84 : 1.0)
            .animation(.easeOut(duration: 0.10), value: configuration.isPressed)
    }
}

struct RadarSessionRowView: View {
    let item: RadarSessionItem
    @ObservedObject var store: RadarStore
    let onSelect: (RadarSessionItem, Bool) -> Void
    @State private var isHovered = false

    private var isCodex: Bool { item.client == "codex" }

    private var stateLabel: String {
        store.t("menubar.state_\(item.live_state)")
    }

    var body: some View {
        let stateColor = RadarPalette.color(for: item.live_state)
        let isUrgent = item.live_state == "waiting" || item.live_state == "stalled" || item.live_state == "unread"
        let displayTitle = RadarStore.cleanTitle(item.title, code: item.code, group: item.group)
        let biz = item.group.isEmpty ? item.code : item.group

        Button {
            let flags = NSEvent.modifierFlags
            let splitRight = flags.contains(.option) || flags.contains(.command)
            onSelect(item, splitRight)
        } label: {
            HStack(alignment: .center, spacing: 10) {
                // Fixed 26x26 icon badge -> strict left alignment across all rows
                ZStack {
                    RoundedRectangle(cornerRadius: 7, style: .continuous)
                        .fill(stateColor.opacity(item.live_state == "idle" ? 0.08 : 0.14))
                    LiveWaveformGlyph(state: item.live_state, style: store.glyphStyle, size: 15)
                }
                .frame(width: 26, height: 26)

                // Flexible 2-line content column
                VStack(alignment: .leading, spacing: 3) {
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                        Text(displayTitle)
                            .font(.system(size: 12.5, weight: isUrgent ? .semibold : .medium))
                            .foregroundStyle(.primary)
                            .lineLimit(1)
                            .truncationMode(.tail)
                        Spacer(minLength: 4)
                        Text(store.formatAge(item.updated_at_ms))
                            .font(.system(size: 10.5, weight: .regular, design: .monospaced))
                            .foregroundStyle(.secondary)
                    }

                    HStack(spacing: 5) {
                        Text(isCodex ? "Codex" : "AGY")
                            .font(.system(size: 9.5, weight: .bold, design: .monospaced))
                            .padding(.horizontal, 5)
                            .padding(.vertical, 1)
                            .background(
                                Capsule()
                                    .fill(isCodex ? Color.green.opacity(0.15) : Color.cyan.opacity(0.15))
                            )
                            .foregroundStyle(isCodex ? Color.green : Color.cyan)

                        if !biz.isEmpty {
                            Text(biz)
                                .font(.system(size: 10.5, weight: .medium))
                                .foregroundStyle(.secondary)
                                .lineLimit(1)
                        }

                        if item.live_state != "idle" {
                            Text("·")
                                .font(.system(size: 10))
                                .foregroundStyle(.tertiary)
                            Text(stateLabel)
                                .font(.system(size: 10.5, weight: .medium))
                                .foregroundStyle(stateColor)
                        }

                        Spacer(minLength: 0)

                        Image(systemName: isCodex ? "arrow.up.right" : "chevron.right")
                            .font(.system(size: 9.5, weight: .semibold))
                            .foregroundStyle(.secondary.opacity(isHovered ? 0.9 : 0.25))
                    }
                }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 7.5)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .fill(
                        isUrgent
                            ? stateColor.opacity(isHovered ? 0.14 : 0.08)
                            : Color.primary.opacity(isHovered ? 0.08 : 0.035)
                    )
            )
            .overlay(
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .strokeBorder(
                        isUrgent
                            ? stateColor.opacity(isHovered ? 0.42 : 0.24)
                            : Color.primary.opacity(isHovered ? 0.16 : 0.07),
                        lineWidth: 1
                    )
            )
            .contentShape(RoundedRectangle(cornerRadius: 9, style: .continuous))
        }
        .buttonStyle(RadarControlButtonStyle(pressScale: 0.988))
        .focusable(false)
        .focusEffectDisabled()
        .onHover { hovering in
            withAnimation(.easeOut(duration: 0.14)) {
                isHovered = hovering
            }
        }
        .help(isCodex ? store.t("menubar.row_codex_tip") : store.t("menubar.row_agy_tip"))
    }
}

struct SummaryChip: View {
    let text: String
    let color: Color

    var body: some View {
        HStack(spacing: 4) {
            Circle()
                .fill(color)
                .frame(width: 5.5, height: 5.5)
            Text(text)
                .font(.system(size: 10.5, weight: .semibold))
                .foregroundStyle(color)
        }
        .padding(.horizontal, 7)
        .padding(.vertical, 2.5)
        .background(
            Capsule()
                .fill(color.opacity(0.13))
        )
    }
}

struct RadarPopoverRootView: View {
    @ObservedObject var store: RadarStore
    let onSelectSession: (RadarSessionItem, Bool) -> Void
    let onQuit: () -> Void

    private let langChoices: [(code: String, label: String)] = [
        ("zh-CN", "中"),
        ("en", "EN"),
        ("ja", "日"),
    ]

    private var styleChoices: [(id: String, key: String)] {
        [
            ("waveform", "menubar.style_waveform"),
            ("bars_fade", "menubar.style_bars"),
            ("pulse_rings", "menubar.style_ripples"),
        ]
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            // Top Header Rail
            HStack(alignment: .center, spacing: 8) {
                HStack(spacing: 6) {
                    Image(systemName: "waveform.circle.fill")
                        .font(.system(size: 15, weight: .semibold))
                        .foregroundStyle(Color.cyan)
                    Text(store.t("menubar.title"))
                        .font(.system(size: 13.5, weight: .semibold))
                        .foregroundStyle(.primary)
                }

                Spacer(minLength: 6)

                // Compact i18n Switcher (synced with Hyper Deck)
                HStack(spacing: 2) {
                    ForEach(langChoices, id: \.code) { item in
                        let active = store.lang == item.code
                        Button {
                            store.selectLanguage(item.code)
                        } label: {
                            Text(item.label)
                                .font(.system(size: 10, weight: active ? .bold : .medium))
                                .padding(.horizontal, 5.5)
                                .padding(.vertical, 2.5)
                                .background(
                                    RoundedRectangle(cornerRadius: 5, style: .continuous)
                                        .fill(active ? Color.primary.opacity(0.12) : Color.clear)
                                )
                                .foregroundStyle(active ? .primary : .secondary)
                                .contentShape(RoundedRectangle(cornerRadius: 5, style: .continuous))
                        }
                        .buttonStyle(RadarControlButtonStyle())
                        .focusable(false)
                        .focusEffectDisabled()
                    }
                }
                .padding(2)
                .background(
                    RoundedRectangle(cornerRadius: 6.5, style: .continuous)
                        .fill(Color.primary.opacity(0.05))
                )
                .overlay(
                    RoundedRectangle(cornerRadius: 6.5, style: .continuous)
                        .strokeBorder(Color.primary.opacity(0.07), lineWidth: 0.75)
                )

                // 1-Click Smart Rename Sync Button
                Button {
                    store.syncTitles()
                } label: {
                    Image(systemName: store.isSyncingTitles ? "sparkles" : "wand.and.stars")
                        .font(.system(size: 11.5, weight: .medium))
                        .foregroundStyle(store.isSyncingTitles ? Color.cyan : .primary)
                        .symbolEffect(.pulse, options: .repeating, isActive: store.isSyncingTitles)
                        .frame(width: 26, height: 23)
                        .background(
                            RoundedRectangle(cornerRadius: 6.5, style: .continuous)
                                .fill(store.isSyncingTitles ? Color.cyan.opacity(0.14) : Color.primary.opacity(0.055))
                        )
                        .overlay(
                            RoundedRectangle(cornerRadius: 6.5, style: .continuous)
                                .strokeBorder(
                                    store.isSyncingTitles ? Color.cyan.opacity(0.32) : Color.primary.opacity(0.07),
                                    lineWidth: 0.75
                                )
                        )
                        .contentShape(RoundedRectangle(cornerRadius: 6.5, style: .continuous))
                }
                .buttonStyle(RadarControlButtonStyle())
                .focusable(false)
                .focusEffectDisabled()
                .help(store.t("menubar.sync_titles_tip"))

                // Chime Toggle Icon Button
                let soundOn = store.payload?.sound_enabled ?? true
                Button {
                    store.toggleSound()
                } label: {
                    Image(systemName: soundOn ? "speaker.wave.2.fill" : "speaker.slash.fill")
                        .font(.system(size: 11.5, weight: .medium))
                        .foregroundStyle(soundOn ? .primary : .tertiary)
                        .frame(width: 26, height: 23)
                        .background(
                            RoundedRectangle(cornerRadius: 6.5, style: .continuous)
                                .fill(Color.primary.opacity(0.055))
                        )
                        .overlay(
                            RoundedRectangle(cornerRadius: 6.5, style: .continuous)
                                .strokeBorder(Color.primary.opacity(0.07), lineWidth: 0.75)
                        )
                        .contentShape(RoundedRectangle(cornerRadius: 6.5, style: .continuous))
                }
                .buttonStyle(RadarControlButtonStyle())
                .focusable(false)
                .focusEffectDisabled()
                .help(soundOn ? store.t("menubar.sound_on_tip") : store.t("menubar.sound_off_tip"))
            }

            // Summary Chips + Motion Preset Switcher Rail (strictly left/right aligned)
            if let p = store.payload {
                HStack(alignment: .center, spacing: 5) {
                    if p.waiting_count > 0 {
                        SummaryChip(
                            text: store.t("menubar.waiting_chip", ["count": "\(p.waiting_count)"]),
                            color: RadarPalette.color(for: "waiting")
                        )
                    }
                    if p.stalled_count > 0 {
                        SummaryChip(
                            text: store.t("menubar.stalled_chip", ["count": "\(p.stalled_count)"]),
                            color: RadarPalette.color(for: "stalled")
                        )
                    }
                    if p.unread_count > 0 {
                        SummaryChip(
                            text: store.t("menubar.unread_chip", ["count": "\(p.unread_count)"]),
                            color: RadarPalette.color(for: "unread")
                        )
                    }
                    if p.running_count > 0 {
                        SummaryChip(
                            text: store.t("menubar.running_chip", ["count": "\(p.running_count)"]),
                            color: RadarPalette.color(for: "running")
                        )
                    }
                    if p.waiting_count + p.stalled_count + p.unread_count + p.running_count == 0 {
                        Text(store.t("menubar.subtitle_calm"))
                            .font(.system(size: 11))
                            .foregroundStyle(.secondary)
                    }

                    Spacer(minLength: 4)

                    // Open-source loader motion preset selector
                    HStack(spacing: 2) {
                        ForEach(styleChoices, id: \.id) { sc in
                            let on = store.glyphStyle == sc.id
                            Button {
                                store.selectGlyphStyle(sc.id)
                            } label: {
                                Text(store.t(sc.key))
                                    .font(.system(size: 9.5, weight: on ? .semibold : .regular))
                                    .padding(.horizontal, 5)
                                    .padding(.vertical, 2)
                                    .background(
                                        RoundedRectangle(cornerRadius: 4.5, style: .continuous)
                                            .fill(on ? Color.primary.opacity(0.11) : Color.clear)
                                    )
                                    .foregroundStyle(on ? .primary : .secondary)
                                    .contentShape(RoundedRectangle(cornerRadius: 4.5, style: .continuous))
                            }
                            .buttonStyle(RadarControlButtonStyle())
                            .focusable(false)
                            .focusEffectDisabled()
                        }
                    }
                    .padding(1.5)
                    .background(
                        RoundedRectangle(cornerRadius: 6, style: .continuous)
                            .fill(Color.primary.opacity(0.04))
                    )
                    .overlay(
                        RoundedRectangle(cornerRadius: 6, style: .continuous)
                            .strokeBorder(Color.primary.opacity(0.065), lineWidth: 0.75)
                    )
                }

                Divider().opacity(0.5)

                let attSlice = Array(p.attention_items.prefix(5))
                let recSlice = Array(p.recent_items.prefix(4))
                let totalRows = attSlice.count + recSlice.count

                let listContent = VStack(alignment: .leading, spacing: 10) {
                    if !attSlice.isEmpty {
                        VStack(alignment: .leading, spacing: 5) {
                            Text(store.t("menubar.sec_attention"))
                                .font(.system(size: 10.5, weight: .semibold))
                                .foregroundStyle(.secondary)
                                .padding(.leading, 2)

                            ForEach(attSlice) { item in
                                RadarSessionRowView(item: item, store: store, onSelect: onSelectSession)
                            }
                        }
                    }

                    if !recSlice.isEmpty {
                        VStack(alignment: .leading, spacing: 5) {
                            Text(store.t("menubar.sec_recent"))
                                .font(.system(size: 10.5, weight: .semibold))
                                .foregroundStyle(.secondary)
                                .padding(.leading, 2)

                            ForEach(recSlice) { item in
                                RadarSessionRowView(item: item, store: store, onSelect: onSelectSession)
                            }
                        }
                    }
                }

                if totalRows > 9 {
                    ScrollView(.vertical, showsIndicators: false) {
                        listContent
                            .padding(.horizontal, 1)
                    }
                    .frame(height: 410)
                } else {
                    listContent
                }
            } else {
                Text(store.t("menubar.offline"))
                    .font(.system(size: 11.5))
                    .foregroundStyle(.secondary)
                    .padding(.vertical, 8)
            }

            Divider().opacity(0.45)

            // Footer Rail
            HStack(alignment: .center) {
                Text(store.t("menubar.split_hint"))
                    .font(.system(size: 10.5))
                    .foregroundStyle(.tertiary)
                Spacer()
                Button {
                    onQuit()
                } label: {
                    Text(store.t("menubar.quit"))
                        .font(.system(size: 10.5, weight: .medium))
                        .foregroundStyle(.secondary)
                }
                .buttonStyle(RadarControlButtonStyle())
                .focusable(false)
                .focusEffectDisabled()
            }
        }
        .padding(13)
        .frame(width: 392)
    }
}

// MARK: - AppKit StatusBar Host + SwiftUI Popover Controller

@MainActor
final class RadarBarDelegate: NSObject, NSApplicationDelegate {
    private var statusItem: NSStatusItem!
    private var statusHosting: PassthroughHostingView<MenuBarStatusCapsule>!
    private let popover = NSPopover()
    private let store = RadarStore()
    private var pollTimer: Timer?

    func applicationDidFinishLaunching(_ notification: Notification) {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        if let btn = statusItem.button {
            btn.title = ""
            btn.image = nil
            btn.target = self
            btn.action = #selector(togglePopover(_:))

            let capsule = MenuBarStatusCapsule(store: store)
            statusHosting = PassthroughHostingView(rootView: capsule)
            statusHosting.translatesAutoresizingMaskIntoConstraints = false
            btn.addSubview(statusHosting)
            NSLayoutConstraint.activate([
                statusHosting.leadingAnchor.constraint(equalTo: btn.leadingAnchor),
                statusHosting.trailingAnchor.constraint(equalTo: btn.trailingAnchor),
                statusHosting.topAnchor.constraint(equalTo: btn.topAnchor),
                statusHosting.bottomAnchor.constraint(equalTo: btn.bottomAnchor),
            ])
        }

        let rootView = RadarPopoverRootView(
            store: store,
            onSelectSession: { [weak self] item, splitRight in
                self?.handleSelectSession(item, splitRight: splitRight)
            },
            onQuit: {
                NSApp.terminate(nil)
            }
        )
        let hostingController = NSHostingController(rootView: rootView)
        hostingController.sizingOptions = [.preferredContentSize]
        popover.contentViewController = hostingController
        popover.behavior = .transient
        popover.animates = true

        store.refresh()
        syncStatusLength()

        pollTimer = Timer.scheduledTimer(withTimeInterval: 2.0, repeats: true) { [weak self] _ in
            Task { @MainActor in
                self?.store.refresh()
                self?.syncStatusLength()
            }
        }
    }

    private func syncStatusLength() {
        let fitting = statusHosting.fittingSize
        statusItem.length = max(24, ceil(fitting.width))
        statusItem.button?.toolTip = store.t("menubar.title")
    }

    @objc private func togglePopover(_ sender: AnyObject?) {
        guard let btn = statusItem.button else { return }
        if popover.isShown {
            popover.performClose(sender)
        } else {
            store.refresh()
            syncStatusLength()
            popover.show(relativeTo: btn.bounds, of: btn, preferredEdge: .minY)
            if let win = popover.contentViewController?.view.window {
                win.initialFirstResponder = nil
                win.makeKey()
                win.makeFirstResponder(nil)
            }
            NSApp.activate(ignoringOtherApps: true)
        }
    }

    private func handleSelectSession(_ item: RadarSessionItem, splitRight: Bool) {
        if item.client == "codex" {
            popover.performClose(nil)
            HyperUdsClient.sendJson(["op": "radar_ack", "conversation_id": item.id])
            let uriStr = item.open_url ?? "codex://threads/\(item.id)"
            if let url = URL(string: uriStr) {
                NSWorkspace.shared.open(url)
            }
        } else {
            var req: [String: Any] = [
                "op": "radar_focus",
                "client": "agy",
                "conversation_id": item.id,
            ]
            if splitRight { req["split"] = "right" }
            HyperUdsClient.sendJson(req)
            activateHostApp()
            popover.performClose(nil)
        }
        store.refresh()
        syncStatusLength()
    }

    private func activateHostApp() {
        // Match strictly `.regular` GUI app (`com.google.antigravity`), never helper/XPC services
        // such as `LookupViewService (Antigravity)` or `Antigravity Helper`.
        let regularApp = NSWorkspace.shared.runningApplications.first(where: { app in
            guard app.activationPolicy == .regular else { return false }
            let bid = (app.bundleIdentifier ?? "").lowercased()
            let name = (app.localizedName ?? "").lowercased()
            return bid == "com.google.antigravity" || name == "antigravity"
        })

        if let appURL = regularApp?.bundleURL
            ?? NSWorkspace.shared.urlForApplication(withBundleIdentifier: "com.google.antigravity")
        {
            let cfg = NSWorkspace.OpenConfiguration()
            cfg.activates = true
            NSWorkspace.shared.openApplication(at: appURL, configuration: cfg, completionHandler: nil)
            return
        }

        if let agy = regularApp {
            _ = agy.activate(options: [.activateAllWindows])
            return
        }

        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/usr/bin/open")
        proc.arguments = ["-a", "Antigravity"]
        try? proc.run()
    }
}

let args = CommandLine.arguments
if let idx = args.firstIndex(of: "--render-mock"), idx + 1 < args.count {
    let outPath = args[idx + 1]
    let mockLang = (idx + 2 < args.count) ? args[idx + 2] : "en"
    MainActor.assumeIsolated {
        let store = RadarStore()
        store.lang = SharedLocalizer.supported.contains(mockLang) ? mockLang : "en"
        store.glyphStyle = "waveform"
        let nowMs = UInt64(Date().timeIntervalSince1970 * 1000)
        let isZh = store.lang == "zh-CN"
        store.payload = SessionRadarPayload(
            ok: true,
            waiting_count: 1,
            stalled_count: 0,
            unread_count: 1,
            running_count: 2,
            sound_enabled: true,
            ui_language: store.lang,
            glyph_style: "waveform",
            attention_items: [
                RadarSessionItem(
                    id: "00000000-0000-4000-8000-000000000001",
                    client: "agy",
                    title: isZh ? "🛠️ Auth·OAuth 令牌刷新｜修复并发重放竞态" : "🛠️ Auth·OAuth Refresh｜Fix Token Replay Race",
                    group: "Auth Core",
                    code: "Auth",
                    workspace: "/workspace/auth-core",
                    live_state: "waiting",
                    state_reason: "ask_user",
                    updated_at_ms: nowMs - 18_000,
                    open_url: nil
                ),
                RadarSessionItem(
                    id: "00000000-0000-4000-8000-000000000002",
                    client: "agy",
                    title: isZh ? "🔎 API·支付回调网关｜幂等键与签名审计" : "🔎 API·Webhook Gateway｜Idempotency & HMAC Audit",
                    group: "Gateway API",
                    code: "API",
                    workspace: "/workspace/gateway-api",
                    live_state: "unread",
                    state_reason: "completed",
                    updated_at_ms: nowMs - 120_000,
                    open_url: nil
                ),
                RadarSessionItem(
                    id: "00000000-0000-4000-8000-000000000003",
                    client: "agy",
                    title: isZh ? "🎨 UI·设计系统重构｜深浅双色语义变量" : "🎨 UI·Design System｜Semantic Dark-Mode Tokens",
                    group: "Web Studio",
                    code: "UI",
                    workspace: "/workspace/web-studio",
                    live_state: "running",
                    state_reason: "planner",
                    updated_at_ms: nowMs - 8_000,
                    open_url: nil
                ),
                RadarSessionItem(
                    id: "00000000-0000-4000-8000-000000000004",
                    client: "codex",
                    title: isZh ? "⚙️ CLI·跨平台构建矩阵｜ARM64 静态链接验证" : "⚙️ CLI·Release Matrix｜ARM64 Static Link Check",
                    group: "Rust SDK",
                    code: "CLI",
                    workspace: "/workspace/rust-sdk",
                    live_state: "running",
                    state_reason: "running",
                    updated_at_ms: nowMs - 34_000,
                    open_url: "codex://threads/00000000-0000-4000-8000-000000000004"
                ),
            ],
            recent_items: [
                RadarSessionItem(
                    id: "00000000-0000-4000-8000-000000000005",
                    client: "agy",
                    title: isZh ? "🧩 WAL·原子快照引擎｜ENOSPC 满盘回滚混沌测试" : "🧩 WAL·Snapshot Engine｜ENOSPC Chaos Rollback Test",
                    group: "Storage",
                    code: "WAL",
                    workspace: "/workspace/storage-wal",
                    live_state: "idle",
                    state_reason: "idle",
                    updated_at_ms: nowMs - 900_000,
                    open_url: nil
                ),
                RadarSessionItem(
                    id: "00000000-0000-4000-8000-000000000006",
                    client: "codex",
                    title: isZh ? "📊 Bench·缓存衰减曲线｜分模型 TTL 拐点回归" : "📊 Bench·Cache Decay｜Per-Model TTL Cliff Regression",
                    group: "Telemetry",
                    code: "Bench",
                    workspace: "/workspace/telemetry",
                    live_state: "idle",
                    state_reason: "idle",
                    updated_at_ms: nowMs - 2_400_000,
                    open_url: "codex://threads/00000000-0000-4000-8000-000000000006"
                ),
            ]
        )
        let preview = RadarPopoverRootView(
            store: store,
            onSelectSession: { _, _ in },
            onQuit: {}
        )
        .padding(6)
        .background(
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .fill(Color(nsColor: .windowBackgroundColor))
        )
        let renderer = ImageRenderer(content: preview)
        renderer.scale = 2.0
        if let nsImg = renderer.nsImage,
           let tiff = nsImg.tiffRepresentation,
           let rep = NSBitmapImageRep(data: tiff),
           let png = rep.representation(using: .png, properties: [:]) {
            try? png.write(to: URL(fileURLWithPath: outPath))
        }
    }
    exit(0)
}

if let idx = args.firstIndex(of: "--render-preview"), idx + 1 < args.count {
    let outPath = args[idx + 1]
    MainActor.assumeIsolated {
        let store = RadarStore()
        store.refresh()
        let preview = RadarPopoverRootView(
            store: store,
            onSelectSession: { _, _ in },
            onQuit: {}
        )
        .background(Color(nsColor: .windowBackgroundColor))
        let renderer = ImageRenderer(content: preview)
        renderer.scale = 2.0
        if let nsImg = renderer.nsImage,
           let tiff = nsImg.tiffRepresentation,
           let rep = NSBitmapImageRep(data: tiff),
           let png = rep.representation(using: .png, properties: [:]) {
            try? png.write(to: URL(fileURLWithPath: outPath))
        }
    }
    exit(0)
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let delegate = MainActor.assumeIsolated { RadarBarDelegate() }
app.delegate = delegate
app.run()

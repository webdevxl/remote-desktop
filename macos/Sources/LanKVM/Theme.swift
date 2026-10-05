import AppKit
import SwiftUI

// Visual language: native macOS structure (sidebar, toolbar, SF Symbols, system fonts) with a
// warm, Claude-like palette — ivory/charcoal surfaces, hairline borders, a clay-orange accent
// and serif page titles.

extension Color {
    static let lkAccent = Color(hex: 0xD97757)
    static let lkBackground = Color.dynamic(light: 0xFAF9F5, dark: 0x262624)
    static let lkSidebar = Color.dynamic(light: 0xF0EEE6, dark: 0x1F1E1D)
    static let lkCard = Color.dynamic(light: 0xFFFFFF, dark: 0x30302E)
    static let lkBorder = Color.dynamic(light: 0xE5E2D9, dark: 0x3E3E3A)
    static let lkText = Color.dynamic(light: 0x141413, dark: 0xF5F4EE)
    static let lkSecondary = Color.dynamic(light: 0x73726C, dark: 0xA6A39A)
    static let lkSuccess = Color.dynamic(light: 0x3F8A4E, dark: 0x6FBF7E)
    static let lkWarning = Color.dynamic(light: 0xB7791F, dark: 0xE0A64B)

    init(hex: UInt32) {
        self.init(nsColor: NSColor(hex: hex))
    }

    static func dynamic(light: UInt32, dark: UInt32) -> Color {
        Color(nsColor: NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? NSColor(hex: dark) : NSColor(hex: light)
        })
    }
}

extension NSColor {
    convenience init(hex: UInt32) {
        self.init(
            srgbRed: CGFloat((hex >> 16) & 0xFF) / 255,
            green: CGFloat((hex >> 8) & 0xFF) / 255,
            blue: CGFloat(hex & 0xFF) / 255,
            alpha: 1
        )
    }
}

/// Large serif title with an optional explanation, at the top of each page.
struct PageHeader: View {
    let title: String
    var subtitle: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title)
                .font(.system(size: 28, weight: .regular, design: .serif))
                .foregroundStyle(Color.lkText)
            if let subtitle {
                Text(subtitle)
                    .font(.system(size: 13))
                    .foregroundStyle(Color.lkSecondary)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// Small uppercase label above a group of cards.
struct SectionLabel: View {
    let title: String

    var body: some View {
        Text(title.uppercased())
            .font(.system(size: 11, weight: .semibold))
            .tracking(0.6)
            .foregroundStyle(Color.lkSecondary)
            .padding(.leading, 2)
    }
}

/// Rounded surface with a hairline border; rows inside are separated by dividers.
struct Card<Content: View>: View {
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            content
        }
        .background(Color.lkCard, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(Color.lkBorder))
    }
}

/// A row inside a [`Card`]: icon, title, optional detail, trailing accessory.
struct CardRow<Trailing: View>: View {
    let icon: String
    var tint: Color = .lkSecondary
    let title: String
    var detail: String?
    var monospacedDetail = false
    @ViewBuilder var trailing: Trailing

    var body: some View {
        HStack(spacing: 12) {
            IconBadge(systemName: icon, tint: tint)
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(Color.lkText)
                if let detail {
                    Text(detail)
                        .font(monospacedDetail ? .system(size: 12, design: .monospaced) : .system(size: 12))
                        .foregroundStyle(Color.lkSecondary)
                        .textSelection(.enabled)
                }
            }
            Spacer(minLength: 8)
            trailing
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 11)
    }
}

extension CardRow where Trailing == EmptyView {
    init(icon: String, tint: Color = .lkSecondary, title: String, detail: String? = nil, monospacedDetail: Bool = false) {
        self.init(icon: icon, tint: tint, title: title, detail: detail, monospacedDetail: monospacedDetail) { EmptyView() }
    }
}

struct CardDivider: View {
    var body: some View {
        Rectangle().fill(Color.lkBorder).frame(height: 1).padding(.leading, 54)
    }
}

/// SF Symbol on a soft rounded square.
struct IconBadge: View {
    let systemName: String
    var tint: Color = .lkSecondary

    var body: some View {
        Image(systemName: systemName)
            .font(.system(size: 13, weight: .medium))
            .foregroundStyle(tint)
            .frame(width: 28, height: 28)
            .background(tint.opacity(0.12), in: RoundedRectangle(cornerRadius: 7, style: .continuous))
    }
}

/// Filled clay-orange button for the main action on a page.
struct PrimaryButtonStyle: ButtonStyle {
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 13, weight: .semibold))
            .foregroundStyle(.white)
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
            .background(
                Color.lkAccent.opacity(isEnabled ? (configuration.isPressed ? 0.8 : 1) : 0.45),
                in: RoundedRectangle(cornerRadius: 8, style: .continuous)
            )
    }
}

/// Quiet bordered button for secondary actions.
struct SecondaryButtonStyle: ButtonStyle {
    var destructive = false
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 12, weight: .medium))
            .foregroundStyle(destructive ? Color.red.opacity(0.85) : Color.lkText)
            .padding(.horizontal, 10)
            .padding(.vertical, 5)
            .background(
                Color.lkText.opacity(configuration.isPressed ? 0.08 : 0.04),
                in: RoundedRectangle(cornerRadius: 7, style: .continuous)
            )
            .overlay(RoundedRectangle(cornerRadius: 7, style: .continuous).strokeBorder(Color.lkBorder))
            .opacity(isEnabled ? 1 : 0.45)
    }
}

/// Coloured dot + text, e.g. "Ready".
struct StatusPill: View {
    let text: String
    let color: Color

    var body: some View {
        HStack(spacing: 6) {
            Circle().fill(color).frame(width: 7, height: 7)
            Text(text).font(.system(size: 12, weight: .medium)).foregroundStyle(color)
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 4)
        .background(color.opacity(0.12), in: Capsule())
    }
}

/// Scrollable page body with the app's background and standard margins.
struct Page<Content: View>: View {
    @ViewBuilder var content: Content

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                content
            }
            .padding(.horizontal, 32)
            .padding(.vertical, 28)
            .frame(maxWidth: 680, alignment: .leading)
            .frame(maxWidth: .infinity)
        }
        .background(Color.lkBackground)
    }
}

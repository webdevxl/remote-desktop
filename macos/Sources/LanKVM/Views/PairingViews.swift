import SwiftUI

/// Shown on the Mac being connected to: the code to type on the other Mac.
struct PairingRequestSheet: View {
    @EnvironmentObject private var core: CoreModel
    let request: PairingRequest

    var body: some View {
        VStack(spacing: 18) {
            IconBadge(systemName: "lock.shield", tint: .lkAccent)
                .scaleEffect(1.4)
                .padding(.top, 6)
            VStack(spacing: 6) {
                Text("Pairing request")
                    .font(.system(size: 22, design: .serif))
                    .foregroundStyle(Color.lkText)
                Text("\(request.name) (\(request.address)) wants to view and control this Mac. To allow it, enter this code on that Mac.")
                    .font(.system(size: 13))
                    .foregroundStyle(Color.lkSecondary)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
            }
            PinDigits(pin: request.pin)
            Text("The code expires when this request is closed.")
                .font(.system(size: 11))
                .foregroundStyle(Color.lkSecondary)
            Button("Deny") { core.deny(request) }
                .buttonStyle(SecondaryButtonStyle(destructive: true))
                .keyboardShortcut(.cancelAction)
        }
        .padding(28)
        .frame(width: 420)
        .background(Color.lkBackground)
    }
}

/// Six digits in separate boxes, grouped 3 + 3.
struct PinDigits: View {
    let pin: String

    var body: some View {
        HStack(spacing: 8) {
            ForEach(Array(pin.enumerated()), id: \.offset) { index, digit in
                if index == 3 { Spacer().frame(width: 6) }
                Text(String(digit))
                    .font(.system(size: 30, weight: .medium, design: .monospaced))
                    .foregroundStyle(Color.lkText)
                    .frame(width: 42, height: 54)
                    .background(Color.lkCard, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                    .overlay(RoundedRectangle(cornerRadius: 9, style: .continuous).strokeBorder(Color.lkBorder))
            }
        }
    }
}

/// Shown on the viewing Mac the first time it connects to a host.
struct PinEntryView: View {
    let hostLabel: String
    let onSubmit: (String) -> Void
    let onCancel: () -> Void
    @State private var input = ""
    @FocusState private var focused: Bool

    private var digits: String { String(input.filter(\.isNumber).prefix(6)) }

    var body: some View {
        VStack(spacing: 18) {
            IconBadge(systemName: "lock.shield", tint: .lkAccent)
                .scaleEffect(1.4)
            VStack(spacing: 6) {
                Text("Pair with \(hostLabel)")
                    .font(.system(size: 22, design: .serif))
                    .foregroundStyle(Color.lkText)
                Text("Enter the 6-digit code shown on that Mac. You only need to do this once.")
                    .font(.system(size: 13))
                    .foregroundStyle(Color.lkSecondary)
                    .multilineTextAlignment(.center)
            }
            ZStack {
                // The visible boxes mirror an invisible text field that takes the typing.
                PinDigits(pin: digits.padding(toLength: 6, withPad: " ", startingAt: 0))
                TextField("", text: $input)
                    .textFieldStyle(.plain)
                    .foregroundStyle(.clear)
                    .tint(.clear)
                    .focused($focused)
                    .onSubmit(submit)
                    .onChange(of: input) { _, new in
                        let clean = String(new.filter(\.isNumber).prefix(6))
                        if clean != new { input = clean }
                    }
            }
            .onTapGesture { focused = true }
            HStack(spacing: 10) {
                Button("Cancel", action: onCancel)
                    .buttonStyle(SecondaryButtonStyle())
                    .keyboardShortcut(.cancelAction)
                Button("Pair", action: submit)
                    .buttonStyle(PrimaryButtonStyle())
                    .disabled(digits.count != 6)
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(32)
        .frame(width: 440)
        .background(Color.lkCard, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 16, style: .continuous).strokeBorder(Color.lkBorder))
        .onAppear { focused = true }
    }

    private func submit() {
        guard digits.count == 6 else { return }
        onSubmit(digits)
    }
}

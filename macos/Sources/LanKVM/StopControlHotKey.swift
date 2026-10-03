import Carbon.HIToolbox

/// ⌃⌥⌘. (period), anywhere: takes control back from every viewer controlling this Mac.
/// Registered only while someone controls it, so the shortcut is otherwise free.
@MainActor
final class StopControlHotKey {
    static let shared = StopControlHotKey()

    private var hotKey: EventHotKeyRef?
    private var handler: EventHandlerRef?
    private var action: (() -> Void)?

    func setEnabled(_ enabled: Bool, action: @escaping () -> Void) {
        self.action = action
        if enabled, hotKey == nil {
            register()
        } else if !enabled, let hotKey {
            UnregisterEventHotKey(hotKey)
            self.hotKey = nil
        }
    }

    private func register() {
        if handler == nil {
            var spec = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
            InstallEventHandler(GetApplicationEventTarget(), { _, _, _ in
                DispatchQueue.main.async {
                    MainActor.assumeIsolated { StopControlHotKey.shared.action?() }
                }
                return noErr
            }, 1, &spec, nil, &handler)
        }
        let id = EventHotKeyID(signature: OSType(0x4C4B_564D), id: 1) // "LKVM"
        let modifiers = UInt32(cmdKey | optionKey | controlKey)
        let status = RegisterEventHotKey(UInt32(kVK_ANSI_Period), modifiers, id, GetApplicationEventTarget(), 0, &hotKey)
        if status != noErr {
            hotKey = nil
        }
    }
}

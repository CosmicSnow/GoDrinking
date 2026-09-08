// winlist.swift — lista janelas on-screen (sem pixels, sem permissão
// especial) para evidência de estado da UI no E2E empacotado.
// Uso: swift scripts/winlist.swift  (imprime "owner :: title" por janela)
import CoreGraphics

guard let list = CGWindowListCopyWindowInfo(
    [.optionOnScreenOnly, .excludeDesktopElements],
    kCGNullWindowID
) as? [[String: Any]] else {
    print("NOLIST")
    exit(0)
}
for w in list {
    let owner = w[kCGWindowOwnerName as String] as? String ?? "?"
    let name = w[kCGWindowName as String] as? String ?? ""
    print("\(owner) :: \(name)")
}

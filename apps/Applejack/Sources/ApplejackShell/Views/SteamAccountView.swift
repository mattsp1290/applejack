import SwiftUI

public struct SteamAccountView: View {
    @Bindable var model: SteamAccountViewModel

    public init(model: SteamAccountViewModel) {
        self.model = model
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            FlowStateView(model.login, idle: "Sign-in state unknown. Sign in with QR to continue.", retry: { Task { await model.startQrLogin() } }) { session in
                switch session.step {
                case .qrReady(let payload):
                    Text("Scan with the Steam mobile app, then press Done.")
                    QRCodeView(payload: payload).frame(width: 240, height: 240)
                    Button("Done") { Task { await model.complete() } }
                case .guardRequired(let factor):
                    Text("Steam Guard (\(factor)) code:")
                    TextField("Code", text: $model.guardCode).frame(maxWidth: 160)
                    Button("Submit") { Task { await model.complete() } }
                case .done(let accountName):
                    Label("Signed in as \(accountName)", systemImage: "checkmark.circle")
                }
            }
            HStack {
                Button("Sign in with QR") { Task { await model.startQrLogin() } }
                Button("Sign out") { Task { await model.signOut() } }
            }
            FlowStateView(model.logout, idle: "") { _ in Text("Signed out.") }
            Spacer()
        }
        .padding()
        .navigationTitle("Steam Account")
    }
}

/// The login challenge as an image; the modules stay crisp at any size.
public struct QRCodeView: View {
    let payload: String

    public init(payload: String) {
        self.payload = payload
    }

    public var body: some View {
        if let image = QRCode.cgImage(for: payload) {
            Image(image, scale: 1, label: Text("Steam login QR code"))
                .interpolation(.none)
                .resizable()
                .aspectRatio(contentMode: .fit)
        } else {
            Text("The challenge could not be rendered.").foregroundStyle(.red)
        }
    }
}

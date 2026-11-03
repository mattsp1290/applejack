import SwiftUI

/// The Steam account (sign-in) and, below it, the Windows Steam client
/// runtime: both are the one Steam row in the sidebar, and the runtime's
/// `needs_login` state is a sign-in too, inside the client window.
public struct SteamAccountView: View {
    @Bindable var model: SteamAccountViewModel
    let runtime: SteamClientRuntimeViewModel

    public init(model: SteamAccountViewModel, runtime: SteamClientRuntimeViewModel) {
        self.model = model
        self.runtime = runtime
    }

    public var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                account
                Divider()
                SteamClientRuntimeView(model: runtime)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding()
        }
        .navigationTitle("Steam Account")
        .task {
            model.startPolling()
            await runtime.load()
        }
        .onDisappear { model.stopPolling() }
    }

    @ViewBuilder
    private var account: some View {
        FlowStateView(model.login, idle: "Sign-in state unknown. Sign in with QR to continue.", retry: { Task { await model.startQrLogin() } }) { session in
            switch session.step {
            case .qrReady(let payload):
                Text("Scan with the Steam mobile app; the sign-in completes on its own.")
                QRCodeView(payload: payload).frame(width: 240, height: 240)
            case .guardRequired(let factor):
                Text(SteamAccountViewModel.guardPrompt(factor: factor))
                if model.guardCodeWanted {
                    HStack {
                        TextField("Code", text: $model.guardCode).frame(maxWidth: 160)
                            .onSubmit { model.submitGuardCode() }
                        Button("Submit") { model.submitGuardCode() }
                            .disabled(model.guardCode.trimmingCharacters(in: .whitespaces).isEmpty)
                    }
                }
                if let rejection = model.guardRejection {
                    Text("Steam rejected the code: \(rejection)").foregroundStyle(.red)
                }
            case .waiting:
                Text("Waiting for Steam.")
            case .done(let accountName):
                Label("Signed in as \(accountName)", systemImage: "checkmark.circle")
            }
            if model.isPolling, !session.step.isDone {
                ProgressView().controlSize(.small)
            }
        }
        HStack {
            Button("Sign in with QR") { Task { await model.startQrLogin() } }
            Button("Sign out") { Task { await model.signOut() } }
        }
        FlowStateView(model.logout, idle: "") { _ in Text("Signed out.") }
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

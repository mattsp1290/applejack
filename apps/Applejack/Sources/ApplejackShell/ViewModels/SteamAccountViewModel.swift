import Foundation
import Observation

/// `steam.login.start` (QR), `steam.login.complete`, `steam.logout`.
@Observable @MainActor
public final class SteamAccountViewModel {
    public private(set) var login: FlowState<LoginSession> = .idle
    public private(set) var logout: FlowState<JSONValue> = .idle
    public var guardCode = ""
    private let app: AppModel

    public init(app: AppModel) {
        self.app = app
    }

    /// The challenge to render, while the session waits for the mobile app.
    public var qrPayload: String? {
        if case .ready(let session) = login, case .qrReady(let payload) = session.step {
            return payload
        }
        return nil
    }

    public var signedInAccount: String? {
        if case .ready(let session) = login, case .done(let accountName) = session.step {
            return accountName
        }
        return nil
    }

    public func startQrLogin() async {
        login = .loading
        logout = .idle
        if let next = await app.run({ try await $0.call("steam.login.start", params: SteamLoginStartParams(mode: .qr)) as LoginSession }) {
            login = next
        }
    }

    /// Finishes the session: after the QR was scanned, or with a Steam Guard
    /// code when the step asked for one.
    public func complete() async {
        guard case .ready(let session) = login else { return }
        let code = guardCode.trimmingCharacters(in: .whitespaces)
        let params = SteamLoginCompleteParams(sessionId: session.sessionId, steamGuardCode: code.isEmpty ? nil : code)
        login = .loading
        if let next = await app.run({ try await $0.call("steam.login.complete", params: params) as LoginSession }) {
            login = next
            guardCode = ""
        }
    }

    public func signOut() async {
        logout = .loading
        if let next = await app.run({ try await $0.call("steam.logout", params: EmptyParams()) as JSONValue }) {
            logout = next
            if case .ready = next {
                login = .idle
            }
        }
    }
}

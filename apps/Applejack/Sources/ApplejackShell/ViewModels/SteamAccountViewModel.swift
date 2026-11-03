import Foundation
import Observation

/// `steam.login.start` (QR), `steam.login.complete`, `steam.logout`.
@Observable @MainActor
public final class SteamAccountViewModel {
    public private(set) var login: FlowState<LoginSession> = .idle
    public private(set) var logout: FlowState<JSONValue> = .idle
    public var guardCode = ""
    private let app: AppModel
    private let pollDelay: Duration = .seconds(2)
    /// The session whose loop runs. A replaced login's loop sleeps on until
    /// its tick, and the new session must not wait for it.
    private var pollingSession: String?

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

    /// Drives the session to its end: one request per tick, each a poll, or
    /// a submit when `guardCode` holds something. An approval in the Steam
    /// mobile app and a typed code both end it, and a code typed while this
    /// runs goes out on the next tick. A login started meanwhile replaces
    /// this one: the loop ends, and a late answer does not land on the new
    /// session.
    public func complete() async {
        guard case .ready(var session) = login, pollingSession != session.sessionId else { return }
        let id = session.sessionId
        pollingSession = id
        defer {
            if pollingSession == id { pollingSession = nil }
        }
        while holds(session) {
            let code = guardCode.trimmingCharacters(in: .whitespaces)
            let params = SteamLoginCompleteParams(sessionId: session.sessionId, steamGuardCode: code.isEmpty ? nil : code)
            guard let next = await app.run({ try await $0.call("steam.login.complete", params: params) as LoginSession }) else { return }
            guard holds(session) else { return }
            login = next
            if !code.isEmpty { guardCode = "" }
            guard case .ready(let answer) = next, !answer.step.isDone else { return }
            session = answer
            try? await Task.sleep(for: pollDelay)
            if Task.isCancelled { return }
        }
    }

    private func holds(_ session: LoginSession) -> Bool {
        if case .ready(let current) = login {
            return current.sessionId == session.sessionId
        }
        return false
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

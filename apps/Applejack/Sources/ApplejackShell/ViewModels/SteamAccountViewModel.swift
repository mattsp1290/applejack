import Foundation
import Observation

/// `steam.login.start` (QR), `steam.login.complete` polled from the moment
/// the challenge is up, `steam.logout`.
@Observable @MainActor
public final class SteamAccountViewModel {
    public private(set) var login: FlowState<LoginSession> = .idle
    public private(set) var logout: FlowState<JSONValue> = .idle
    public var guardCode = ""
    private let app: AppModel
    /// "Every couple of seconds" (`docs/rpc-protocol.md`, "Steam").
    private let pollDelay: Duration
    /// The loop driving the current login; cancelled by the next login, by
    /// a submit, and by `stopPolling()` when the view goes away.
    private var poll: Task<Void, Never>?

    public init(app: AppModel, pollDelay: Duration = .seconds(2)) {
        self.app = app
        self.pollDelay = pollDelay
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

    /// Whether a code field is wanted: a `guard_required` step whose factor
    /// is not `device_confirmation`, which is approved in the mobile app.
    public var guardCodeWanted: Bool {
        if case .ready(let session) = login, case .guardRequired(let factor) = session.step {
            return factor != "device_confirmation"
        }
        return false
    }

    public var isPolling: Bool { poll != nil }

    /// Starts a login and, once a step is pending, polls it to its end.
    public func startQrLogin() async {
        stopPolling()
        login = .loading
        logout = .idle
        if let next = await app.run({ try await $0.call("steam.login.start", params: SteamLoginStartParams(mode: .qr)) as LoginSession }) {
            login = next
            startPolling()
        } else {
            login = .idle
        }
    }

    /// Sends `guardCode` now, instead of on the loop's next tick.
    public func submitGuardCode() {
        startPolling()
    }

    /// Waits for the current loop to end: `done`, an error, or a stop.
    public func waitForLogin() async {
        await poll?.value
    }

    /// Ends the loop; the login stays where it is, so a view that comes
    /// back can poll it again.
    public func stopPolling() {
        poll?.cancel()
        poll = nil
    }

    /// Polls the pending login, if there is one, replacing the loop in
    /// progress so the first request goes out at once.
    public func startPolling() {
        guard case .ready(let session) = login, !session.step.isDone else { return }
        stopPolling()
        let task = Task<Void, Never> { [weak self] in
            guard let self else { return }
            await drive(session)
        }
        poll = task
        Task { [weak self] in
            await task.value
            guard let self, poll == task else { return }
            poll = nil
        }
    }

    /// One request per tick, each a poll, or a submit when `guardCode` holds
    /// something, until the step is `done` or the login failed. A login
    /// started meanwhile cancels this loop, and a late answer does not land
    /// on the new session.
    private func drive(_ start: LoginSession) async {
        var session = start
        while holds(session), !Task.isCancelled {
            let code = guardCode.trimmingCharacters(in: .whitespaces)
            let params = SteamLoginCompleteParams(sessionId: session.sessionId, steamGuardCode: code.isEmpty ? nil : code)
            guard let next = await app.run({ try await $0.call("steam.login.complete", params: params) as LoginSession }) else { return }
            guard holds(session), !Task.isCancelled else { return }
            login = next
            if !code.isEmpty { guardCode = "" }
            guard case .ready(let answer) = next, !answer.step.isDone else { return }
            session = answer
            try? await Task.sleep(for: pollDelay)
        }
    }

    private func holds(_ session: LoginSession) -> Bool {
        if case .ready(let current) = login {
            return current.sessionId == session.sessionId
        }
        return false
    }

    public func signOut() async {
        stopPolling()
        logout = .loading
        if let next = await app.run({ try await $0.call("steam.logout", params: EmptyParams()) as JSONValue }) {
            logout = next
            if case .ready = next {
                login = .idle
            }
        }
    }
}

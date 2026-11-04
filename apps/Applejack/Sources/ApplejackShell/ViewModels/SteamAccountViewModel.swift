import Foundation
import Observation

/// `steam.login.start` (QR), `steam.login.complete` polled from the moment
/// the challenge is up, `steam.logout`.
@Observable @MainActor
public final class SteamAccountViewModel {
    public private(set) var login: FlowState<LoginSession> = .idle
    public private(set) var logout: FlowState<JSONValue> = .idle
    public var guardCode = ""
    /// The core's message for the last code Steam rejected
    /// (`SteamGuardRejected`); the login stays open for another one.
    public private(set) var guardRejection: String?
    private let app: AppModel
    /// "Every couple of seconds" (`docs/rpc-protocol.md`, "Steam").
    private let pollDelay: Duration
    /// The loop driving the current login; cancelled by the next login, by
    /// a submit, and by `stopPolling()` when the view goes away.
    private var poll: Task<Void, Never>?
    /// Which loop `poll` holds. A loop cannot name its own handle, so it
    /// clears `poll` by this instead, and only when no newer loop replaced it.
    private var pollGeneration = 0

    public init(app: AppModel, pollDelay: Duration = .seconds(2)) {
        self.app = app
        self.pollDelay = pollDelay
    }

    /// The loop holds the model only for the length of one request, so a
    /// model dropped without `stopPolling()` ends here rather than polling
    /// on until `done`.
    isolated deinit {
        poll?.cancel()
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

    /// What to ask for, by factor (`docs/rpc-protocol.md`, "Steam"): the
    /// mobile app can approve instead of a code only when it is the factor.
    public static func guardPrompt(factor: String) -> String {
        switch factor {
        case "email_code": "Steam Guard: type the code from your email."
        case "device_code": "Steam Guard: type the code from the Steam mobile app, or approve the sign-in there."
        case "device_confirmation": "Steam Guard: approve the sign-in in the Steam mobile app."
        default: "Steam Guard (\(factor)): type the code."
        }
    }

    public var isPolling: Bool { poll != nil }

    /// Starts a login and, once a step is pending, polls it to its end.
    public func startQrLogin() async {
        stopPolling()
        login = .loading
        logout = .idle
        guardRejection = nil
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
    /// progress so the first request goes out at once. One request per
    /// tick, each a poll, or a submit when `guardCode` holds something,
    /// until the step is `done` or the login failed. A login started
    /// meanwhile cancels this loop, and a late answer does not land on the
    /// new session. The sleep between ticks holds no reference to the
    /// model, so a dropped model is not kept alive by its own loop.
    public func startPolling() {
        guard case .ready(let session) = login, !session.step.isDone else { return }
        stopPolling()
        let delay = pollDelay
        pollGeneration += 1
        let generation = pollGeneration
        let task = Task<Void, Never> { [weak self] in
            // Cleared here, before the task completes, so `isPolling` is
            // false for anyone that awaited the loop, not one hop later.
            defer { self?.endPoll(generation) }
            var session = session
            var first = true
            while !Task.isCancelled {
                if !first {
                    try? await Task.sleep(for: delay)
                }
                first = false
                guard let self, let next = await tick(session) else { return }
                session = next
            }
        }
        poll = task
    }

    private func endPoll(_ generation: Int) {
        guard generation == pollGeneration else { return }
        poll = nil
    }

    /// One `steam.login.complete` for `session`; the session to poll next,
    /// or `nil` when the loop is over: `done`, an error other than a wrong
    /// code, a cancellation, or a login that is no longer this one.
    private func tick(_ session: LoginSession) async -> LoginSession? {
        let code = guardCode.trimmingCharacters(in: .whitespaces)
        let params = SteamLoginCompleteParams(sessionId: session.sessionId, steamGuardCode: code.isEmpty ? nil : code)
        guard let next = await app.run({ try await $0.call("steam.login.complete", params: params) as LoginSession }) else { return nil }
        guard holds(session), !Task.isCancelled else { return nil }
        if !code.isEmpty { guardCode = "" }
        if case .failed(.rpc(let error)) = next, error.errorCode == .steamGuardRejected {
            // The protocol keeps the login open for another code; the step
            // on screen is still the one that wants it.
            guardRejection = error.message
            return session
        }
        if !code.isEmpty { guardRejection = nil }
        login = next
        guard case .ready(let answer) = next, !answer.step.isDone else { return nil }
        return answer
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
                guardRejection = nil
            }
        }
    }
}

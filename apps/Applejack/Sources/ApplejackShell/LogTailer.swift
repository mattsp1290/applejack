import Foundation

/// Follows a log file the engine writes (`~/Library/Logs/applejack/<session>/wine.log`,
/// reported as `log_path` by `session.list`). Polls, so a file that does not
/// exist yet is picked up when it appears. The shell only reads; the core
/// owns the process that writes.
public enum LogTailer {
    /// Every complete line appended to `path`, from the start of the file.
    public static func lines(at path: String, interval: Duration = .milliseconds(250)) -> AsyncStream<String> {
        let (stream, continuation) = AsyncStream<String>.makeStream()
        let reader = Task.detached {
            var offset: UInt64 = 0
            var partial = Data()
            while !Task.isCancelled {
                if let handle = FileHandle(forReadingAtPath: path) {
                    defer { try? handle.close() }
                    if (try? handle.seek(toOffset: offset)) != nil, let data = try? handle.readToEnd(), !data.isEmpty {
                        offset += UInt64(data.count)
                        partial.append(data)
                        while let newline = partial.firstIndex(of: UInt8(ascii: "\n")) {
                            let line = partial[partial.startIndex..<newline]
                            partial.removeSubrange(partial.startIndex...newline)
                            continuation.yield(String(decoding: line, as: UTF8.self))
                        }
                    }
                }
                do {
                    try await Task.sleep(for: interval)
                } catch {
                    break
                }
            }
            continuation.finish()
        }
        continuation.onTermination = { _ in reader.cancel() }
        return stream
    }
}

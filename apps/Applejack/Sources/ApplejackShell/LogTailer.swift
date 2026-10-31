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
            var identity: ino_t?
            while !Task.isCancelled {
                if let handle = FileHandle(forReadingAtPath: path) {
                    defer { try? handle.close() }
                    // Another inode is a replacement, whatever its size; a
                    // file shorter than what was read was truncated. Either
                    // way start over instead of resuming mid-file.
                    let inode = fileIdentity(handle)
                    let replaced = identity != nil && inode != nil && inode != identity
                    let shorter = (try? handle.seekToEnd()).map { $0 < offset } ?? false
                    if replaced || shorter {
                        offset = 0
                        partial.removeAll()
                    }
                    identity = inode
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

    private static func fileIdentity(_ handle: FileHandle) -> ino_t? {
        var status = stat()
        return fstat(handle.fileDescriptor, &status) == 0 ? status.st_ino : nil
    }
}

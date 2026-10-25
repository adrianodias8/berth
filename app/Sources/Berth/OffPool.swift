import Foundation

/// Run blocking work (a process, a socket read, a keychain check) on a thread
/// of its own and resume with its result. Swift's shared pool for tasks has a
/// thread per core: blocking them stalls every other task in the app, which a
/// `berth start` or a long machine action would do for minutes.
func offPool<T>(_ work: @escaping () throws -> T) async throws -> T {
    try await withCheckedThrowingContinuation { continuation in
        Thread.detachNewThread { continuation.resume(with: Result(catching: work)) }
    }
}

func offPool<T>(_ work: @escaping () -> T) async -> T {
    await withCheckedContinuation { continuation in
        Thread.detachNewThread { continuation.resume(returning: work()) }
    }
}

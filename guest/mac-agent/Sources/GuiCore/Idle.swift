// Screen-idle detection for `wait {"idle":true}`: the executable grabs small
// RGBA thumbnails of the screen in a loop, and the screen counts as idle once
// successive thumbnails stop changing. A blinking caret or a clock ticking
// over changes a handful of pixels, so "unchanged" allows a small fraction.
import Foundation

public enum ImageDiff {
    /// The fraction of pixels (4 bytes each, RGBA) whose channels differ by
    /// more than `tolerance`. Buffers of different sizes differ completely.
    public static func changedFraction(_ a: [UInt8], _ b: [UInt8], tolerance: UInt8 = 8) -> Double {
        guard a.count == b.count, a.count >= 4 else { return 1 }
        let pixels = a.count / 4
        var changed = 0
        a.withUnsafeBufferPointer { pa in
            b.withUnsafeBufferPointer { pb in
                for p in 0..<pixels {
                    let i = p * 4
                    for c in 0..<3 {
                        let d = pa[i + c] > pb[i + c] ? pa[i + c] - pb[i + c] : pb[i + c] - pa[i + c]
                        if d > tolerance {
                            changed += 1
                            break
                        }
                    }
                }
            }
        }
        return Double(changed) / Double(pixels)
    }
}

/// Feeds thumbnails in; says when the screen has been still long enough.
public struct IdleDetector {
    /// Changed fraction at or under which two frames count as the same.
    public var threshold: Double
    /// How many still comparisons in a row make the screen idle.
    public var stillNeeded: Int
    private var last: [UInt8]?
    private var still = 0

    public init(threshold: Double = 0.002, stillNeeded: Int = 2) {
        self.threshold = threshold
        self.stillNeeded = stillNeeded
    }

    /// Adds a frame; true once the last `stillNeeded` comparisons were still.
    public mutating func add(_ frame: [UInt8]) -> Bool {
        defer { last = frame }
        guard let last else { return false }
        if ImageDiff.changedFraction(last, frame) <= threshold {
            still += 1
        } else {
            still = 0
        }
        return still >= stillNeeded
    }
}

// A small JSON value type. Request args and reply results are open-ended
// objects (each verb has its own shape), so the agent works with JSON values
// instead of a Codable struct per verb. Integers are kept apart from doubles
// so pids and sizes go back out as `123`, not `123.0`.
import Foundation

public enum JSON: Equatable, Codable {
    case null
    case bool(Bool)
    case int(Int)
    case double(Double)
    case string(String)
    case array([JSON])
    case object([String: JSON])

    public init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() {
            self = .null
        } else if let b = try? c.decode(Bool.self) {
            self = .bool(b)
        } else if let i = try? c.decode(Int.self) {
            self = .int(i)
        } else if let d = try? c.decode(Double.self) {
            self = .double(d)
        } else if let s = try? c.decode(String.self) {
            self = .string(s)
        } else if let a = try? c.decode([JSON].self) {
            self = .array(a)
        } else if let o = try? c.decode([String: JSON].self) {
            self = .object(o)
        } else {
            throw DecodingError.dataCorruptedError(in: c, debugDescription: "not a JSON value")
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let b): try c.encode(b)
        case .int(let i): try c.encode(i)
        case .double(let d):
            // JSON has no NaN or infinity; a frame from a broken AX element
            // must not make the whole reply unencodable.
            if d.isFinite { try c.encode(d) } else { try c.encodeNil() }
        case .string(let s): try c.encode(s)
        case .array(let a): try c.encode(a)
        case .object(let o): try c.encode(o)
        }
    }

    public subscript(key: String) -> JSON? {
        if case .object(let o) = self { return o[key] }
        return nil
    }

    /// The value, unless it's missing or null.
    public var nonNull: JSON? {
        if case .null = self { return nil }
        return self
    }

    public var string: String? {
        if case .string(let s) = self { return s }
        return nil
    }

    public var bool: Bool? {
        if case .bool(let b) = self { return b }
        return nil
    }

    public var double: Double? {
        switch self {
        case .int(let i): return Double(i)
        case .double(let d): return d
        default: return nil
        }
    }

    public var int: Int? {
        switch self {
        case .int(let i): return i
        case .double(let d) where d.isFinite && d.rounded() == d: return Int(d)
        default: return nil
        }
    }

    public var array: [JSON]? {
        if case .array(let a) = self { return a }
        return nil
    }

    public var object: [String: JSON]? {
        if case .object(let o) = self { return o }
        return nil
    }

    /// A point given as `[x, y]`.
    public var point: (Double, Double)? {
        guard let a = array, a.count == 2, let x = a[0].double, let y = a[1].double else { return nil }
        return (x, y)
    }

    /// One line of JSON with no trailing newline.
    public func encodedLine() -> Data {
        let enc = JSONEncoder()
        enc.outputFormatting = [.withoutEscapingSlashes, .sortedKeys]
        // Encoding a JSON value can only fail on non-finite doubles, which
        // encode(to:) already maps to null.
        return (try? enc.encode(self)) ?? Data("null".utf8)
    }

    public static func parse(_ data: Data) throws -> JSON {
        try JSONDecoder().decode(JSON.self, from: data)
    }
}

extension JSON: ExpressibleByStringLiteral, ExpressibleByIntegerLiteral, ExpressibleByBooleanLiteral,
    ExpressibleByArrayLiteral, ExpressibleByDictionaryLiteral, ExpressibleByFloatLiteral, ExpressibleByNilLiteral
{
    public init(stringLiteral value: String) { self = .string(value) }
    public init(integerLiteral value: Int) { self = .int(value) }
    public init(booleanLiteral value: Bool) { self = .bool(value) }
    public init(floatLiteral value: Double) { self = .double(value) }
    public init(nilLiteral: ()) { self = .null }
    public init(arrayLiteral elements: JSON...) { self = .array(elements) }
    public init(dictionaryLiteral elements: (String, JSON)...) {
        self = .object(Dictionary(elements, uniquingKeysWith: { _, last in last }))
    }
}

public extension JSON {
    /// `nil` becomes JSON null.
    static func from(_ s: String?) -> JSON { s.map { .string($0) } ?? .null }

    /// A number, written as an integer when it is one (screen points mostly
    /// are), so frames read `[0,25,1440,875]`.
    static func number(_ d: Double) -> JSON {
        if d.isFinite, d.rounded() == d, abs(d) < 1e15 { return .int(Int(d)) }
        return .double(d)
    }

    /// A frame `[x, y, w, h]`.
    static func frame(_ x: Double, _ y: Double, _ w: Double, _ h: Double) -> JSON {
        .array([number(x), number(y), number(w), number(h)])
    }
}

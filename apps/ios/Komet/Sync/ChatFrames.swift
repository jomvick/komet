// chat2 wire frames — the Swift twin of crates/sync/src/chat_frames.rs and
// edge/src/chat-frames.ts. Binary WS frames:
// `[type u8][headerLen u32 LE][header JSON][payload]`. Headers are tiny JSON;
// payloads are opaque bytes (Loro updates, the checkpoint frontier). The
// layout tests in ZeronTests pin the same vectors as the Rust/TS suites —
// change all three together.

import Foundation

/// Frame type bytes (shared client/server space; mirror `FRAME` in TS).
enum ChatFrameType {
    static let hello: UInt8 = 0x01
    static let state: UInt8 = 0x02
    static let rowsReq: UInt8 = 0x03
    static let row: UInt8 = 0x04
    static let rowsDone: UInt8 = 0x05
    static let push: UInt8 = 0x06
    static let ack: UInt8 = 0x07
    static let presence: UInt8 = 0x08
    static let probe: UInt8 = 0x09
    static let probeOk: UInt8 = 0x0a
    static let error: UInt8 = 0x0b
}

/// Headers are ids + a few integers; anything bigger is a peer bug.
let chatFrameMaxHeaderBytes = 4096

struct ChatWireFrame {
    var kind: UInt8
    var header: [String: Any]
    var payload: Data
}

enum ChatWire {
    static func encode(_ kind: UInt8, header: [String: Any], payload: Data = Data()) -> Data {
        // JSONSerialization never fails on the [String: JSON-primitive] maps
        // this file builds; sortedKeys keeps output stable for the tests.
        let headerData = (try? JSONSerialization.data(withJSONObject: header,
                                                      options: [.sortedKeys])) ?? Data("{}".utf8)
        var out = Data(capacity: 5 + headerData.count + payload.count)
        out.append(kind)
        var len = UInt32(headerData.count).littleEndian
        withUnsafeBytes(of: &len) { out.append(contentsOf: $0) }
        out.append(headerData)
        out.append(payload)
        return out
    }

    /// `nil` = malformed. Unknown type bytes are NOT rejected here (unlike
    /// the DO): the client tolerates future server frame types by skipping.
    static func decode(_ data: Data) -> ChatWireFrame? {
        guard data.count >= 5 else { return nil }
        let bytes = [UInt8](data.prefix(5))
        let kind = bytes[0]
        let headerLen = Int(UInt32(bytes[1]) | UInt32(bytes[2]) << 8
                            | UInt32(bytes[3]) << 16 | UInt32(bytes[4]) << 24)
        guard headerLen <= chatFrameMaxHeaderBytes, 5 + headerLen <= data.count else { return nil }
        let headerData = data.subdata(in: 5..<(5 + headerLen))
        guard let obj = try? JSONSerialization.jsonObject(with: headerData),
              let header = obj as? [String: Any] else { return nil }
        return ChatWireFrame(kind: kind, header: header,
                             payload: data.subdata(in: (5 + headerLen)..<data.count))
    }
}

// ── typed server headers (loose-dictionary reads; absent = 0) ───────────────

/// The hello answer: server log metadata only — the CLIENT plans catch-up.
struct ChatStateHeader: Equatable {
    var headSeq: UInt64
    var seqFloor: UInt64
    var checkpointSeq: UInt64
    var checkpointSize: UInt64
    var rowCount: UInt64
    var rowBytes: UInt64

    init?(_ header: [String: Any]) {
        func u64(_ key: String) -> UInt64 { (header[key] as? NSNumber)?.uint64Value ?? 0 }
        guard header["headSeq"] is NSNumber else { return nil }
        headSeq = u64("headSeq")
        seqFloor = u64("seqFloor")
        checkpointSeq = u64("checkpointSeq")
        checkpointSize = u64("checkpointSize")
        rowCount = u64("rowCount")
        rowBytes = u64("rowBytes")
    }
}

// ── catch-up planning (pure — the client-side precision rule) ───────────────

enum ChatCatchUpPlan: Equatable {
    /// Local doc already contains the checkpoint frontier (or there is no
    /// checkpoint): stream rows only.
    case rowsOnly(after: UInt64)
    /// Fetch + import the checkpoint first, then rows after it.
    case checkpointThenRows(after: UInt64)
}

/// Decide the catch-up path from the hello state (chat_client.rs
/// plan_catch_up, decision table pinned by its tests):
/// - cursor > headSeq means the server lost state — treat the cursor as fresh;
/// - checkpoint presence is the SIZE, not the seq (a freshly seeded room's
///   checkpoint legitimately covers seq 0);
/// - a contained frontier skips rows the checkpoint already covers.
func chatPlanCatchUp(cursor: UInt64, state: ChatStateHeader,
                     frontierContained: Bool) -> ChatCatchUpPlan {
    let cursor = cursor > state.headSeq ? 0 : cursor
    if state.checkpointSize == 0 {
        return .rowsOnly(after: cursor)
    }
    if frontierContained {
        return .rowsOnly(after: max(cursor, state.checkpointSeq))
    }
    return .checkpointThenRows(after: state.checkpointSeq)
}

// chat2 wire-frame conformance — pins the same layout vectors as
// crates/sync/src/chat_frames.rs and edge/src/chat-frames.test.ts. The three
// codecs must stay byte-compatible; change all suites together.

import XCTest
@testable import Zeron

final class ChatFramesTests: XCTestCase {
    func testPinsTheWireLayout() {
        // Must match the Rust/TS vector: [type][headerLen u32 LE][header][payload].
        let frame = ChatWire.encode(ChatFrameType.push,
                                    header: ["batchId": "b1"],
                                    payload: Data([9, 8, 7]))
        XCTAssertEqual(frame[0], ChatFrameType.push)
        let header = Data(#"{"batchId":"b1"}"#.utf8)
        XCTAssertEqual([UInt8](frame[1..<5]),
                       [UInt8(header.count), 0, 0, 0])
        XCTAssertEqual(frame.subdata(in: 5..<(5 + header.count)), header)
        XCTAssertEqual(frame.subdata(in: (5 + header.count)..<frame.count), Data([9, 8, 7]))
    }

    func testRoundTripsAndRejectsMalformed() {
        let payload = Data(repeating: 1, count: 1000)
        let frame = ChatWire.encode(ChatFrameType.row, header: ["seq": 7], payload: payload)
        let decoded = ChatWire.decode(frame)
        XCTAssertEqual(decoded?.kind, ChatFrameType.row)
        XCTAssertEqual((decoded?.header["seq"] as? NSNumber)?.uint64Value, 7)
        XCTAssertEqual(decoded?.payload, payload)

        XCTAssertNil(ChatWire.decode(Data()))
        XCTAssertNil(ChatWire.decode(Data([ChatFrameType.hello])))
        // Header length past the buffer.
        var truncated = ChatWire.encode(ChatFrameType.hello, header: [:])
        truncated.replaceSubrange(1..<5, with: withUnsafeBytes(of: UInt32(9999).littleEndian) { Data($0) })
        XCTAssertNil(ChatWire.decode(truncated))
        // Non-object header (raw array JSON in the header slot).
        var arr = Data([ChatFrameType.hello])
        let arrJSON = Data("[1]".utf8)
        arr.append(withUnsafeBytes(of: UInt32(arrJSON.count).littleEndian) { Data($0) })
        arr.append(arrJSON)
        XCTAssertNil(ChatWire.decode(arr))
        // Oversized header.
        let fat = ChatWire.encode(ChatFrameType.hello,
                                  header: ["pad": String(repeating: "x", count: chatFrameMaxHeaderBytes)])
        XCTAssertNil(ChatWire.decode(fat))
    }

    func testStateHeaderParsesServerShape() {
        let state = ChatStateHeader([
            "headSeq": 10, "seqFloor": 3, "checkpointSeq": 3,
            "checkpointSize": 160_000, "rowCount": 7, "rowBytes": 14_000,
        ])
        XCTAssertEqual(state?.headSeq, 10)
        XCTAssertEqual(state?.checkpointSeq, 3)
        XCTAssertEqual(state?.checkpointSize, 160_000)
        XCTAssertNil(ChatStateHeader(["nope": 1]))
    }

    /// The catch-up decision table from chat_client.rs plan_catch_up.
    func testPlanCatchUp() {
        func state(_ head: UInt64, _ ckSeq: UInt64, _ ckSize: UInt64) -> ChatStateHeader {
            ChatStateHeader(["headSeq": head, "seqFloor": 0,
                             "checkpointSeq": ckSeq, "checkpointSize": ckSize])!
        }
        // No checkpoint: rows from the cursor.
        XCTAssertEqual(chatPlanCatchUp(cursor: 4, state: state(10, 0, 0), frontierContained: false),
                       .rowsOnly(after: 4))
        // Contained frontier skips rows the checkpoint covers.
        XCTAssertEqual(chatPlanCatchUp(cursor: 2, state: state(10, 6, 100), frontierContained: true),
                       .rowsOnly(after: 6))
        XCTAssertEqual(chatPlanCatchUp(cursor: 8, state: state(10, 6, 100), frontierContained: true),
                       .rowsOnly(after: 8))
        // Missing frontier: fetch the checkpoint, then rows after it.
        XCTAssertEqual(chatPlanCatchUp(cursor: 2, state: state(10, 6, 100), frontierContained: false),
                       .checkpointThenRows(after: 6))
        // Server behind the cursor (reset/wipe): the cursor is meaningless.
        XCTAssertEqual(chatPlanCatchUp(cursor: 20, state: state(10, 0, 0), frontierContained: false),
                       .rowsOnly(after: 0))
        // A freshly SEEDED room: checkpoint covers seq 0 but has SIZE — it
        // must not be misread as "no checkpoint" (2026-08-10 cutover gauntlet).
        XCTAssertEqual(chatPlanCatchUp(cursor: 0, state: state(0, 0, 5_000), frontierContained: false),
                       .checkpointThenRows(after: 0))
    }
}

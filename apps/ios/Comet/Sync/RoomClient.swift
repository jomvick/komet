// Loro room client — a Swift port of crates/sync/src/room.rs.
//
// One client per room (workspace doc or session doc), one WebSocket carrying
// two sub-rooms: the `%LOR` doc room and the `%EPH` presence room. The client
// joins with its local oplog VV, imports the server's backfill, resubmits
// anything the server lacks (covers unacked updates across reconnects), and
// relays local commits as DocUpdate batches until acked.

import Foundation
import Loro

enum RoomEvent {
    case connected
    case disconnected
    case remoteUpdate
    case ephemeralUpdate
}

actor RoomClient {
    // Constants mirrored from room.rs.
    static let fragmentBytes = 200_000
    static let pingIntervalNs: UInt64 = 30_000_000_000
    static let silenceLeaseNs: UInt64 = 45_000_000_000
    static let backoffBaseMs = 250
    static let backoffCapMs = 30_000
    static let maxInvalidRejoins = 3
    static let maxFragmentCount: UInt64 = 4096
    static let maxReassembledBytes = 64 * 1024 * 1024
    // Room-level liveness (room.rs, 2026-07-30 incident): the silence lease
    // above is TRANSPORT-only — the CF runtime auto-answers our text pings
    // without ever waking the DO, so a wedged room looks healthy forever if
    // pongs are all we judge by. Room liveness is judged on %LOR frames plus
    // a hard join-answer deadline; %EPH presence traffic deliberately does
    // not count (the edge's eph path never touches the doc machinery).
    static let joinDeadlineNs: UInt64 = 15_000_000_000
    static let roomProbeAfterNs: UInt64 = 900_000_000_000
    static let roomProbeMaxNs: UInt64 = 4 * 3_600_000_000_000
    static let probeReplyGraceNs: UInt64 = 30_000_000_000
    static let livenessTickNs: UInt64 = 5_000_000_000

    let roomId: String
    let doc: LoroDoc
    let eph: EphemeralStore
    private let urlProvider: @Sendable () async -> URL?
    private let events: @Sendable (RoomEvent) -> Void

    private var socket: URLSessionWebSocketTask?
    private var receiveTask: Task<Void, Never>?
    private var pingTask: Task<Void, Never>?
    private var livenessTask: Task<Void, Never>?
    private var pending: [BatchId: [[UInt8]]] = [:]
    private var fragments: [BatchId: FragmentBuffer] = [:]
    private var joinedLor = false
    private var invalidRejoins = 0
    private var fullResyncRequested = false
    private var backoffMs = RoomClient.backoffBaseMs
    private var lastInbound = DispatchTime.now()
    private var closed = false
    private var generation = 0
    // Room-liveness state (room.rs Session::{join_sent_at, join_is_probe,
    // last_lor_rx}): the instant of the last %LOR JoinRequest still awaiting
    // JoinResponseOk, whether that join is a liveness probe on an established
    // session (its answer must not replay join side effects), and the last
    // inbound %LOR frame — the clock feeding both the deadline and the probe.
    private var joinSentAt: DispatchTime?
    private var joinIsProbe = false
    private var lastLorRx = DispatchTime.now()
    private var probeIntervalNs = RoomClient.roomProbeAfterNs
    private var lastProbeAt: DispatchTime?

    private struct FragmentBuffer {
        var crdt: CrdtType
        var parts: [[UInt8]?]
        var received: Int
        var totalSize: Int
    }

    init(roomId: String,
         doc: LoroDoc,
         ephTimeoutMs: Int64 = 30_000,
         urlProvider: @escaping @Sendable () async -> URL?,
         events: @escaping @Sendable (RoomEvent) -> Void) {
        self.roomId = roomId
        self.doc = doc
        self.eph = EphemeralStore(timeout: ephTimeoutMs)
        self.urlProvider = urlProvider
        self.events = events
    }

    // MARK: Lifecycle

    func start() {
        closed = false
        connect()
    }

    func stop() {
        closed = true
        generation += 1
        receiveTask?.cancel()
        pingTask?.cancel()
        livenessTask?.cancel()
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
        joinedLor = false
    }

    private func connect() {
        guard !closed else { return }
        generation += 1
        let gen = generation
        joinedLor = false
        fullResyncRequested = false
        fragments.removeAll()
        joinSentAt = nil
        joinIsProbe = false
        lastLorRx = .now()
        probeIntervalNs = RoomClient.roomProbeAfterNs
        lastProbeAt = nil

        Task {
            guard let url = await urlProvider() else {
                await self.scheduleReconnect(gen: gen)
                return
            }
            await self.openSocket(url: url, gen: gen)
        }
    }

    private func openSocket(url: URL, gen: Int) {
        guard gen == generation, !closed else { return }
        let task = URLSession.shared.webSocketTask(with: url)
        socket = task
        task.resume()
        lastInbound = .now()

        receiveTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                guard let sock = await self.currentSocket(gen: gen) else { return }
                do {
                    let message = try await sock.receive()
                    await self.handleInbound(message, gen: gen)
                } catch {
                    await self.onSocketError(gen: gen)
                    return
                }
            }
        }

        pingTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: RoomClient.pingIntervalNs)
                guard let self else { return }
                await self.pingTick(gen: gen)
            }
        }

        livenessTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: RoomClient.livenessTickNs)
                guard let self else { return }
                await self.livenessTick(gen: gen)
            }
        }

        // Join the doc room with our local VV (empty VV asks for a snapshot).
        Task { await self.sendJoinLoro(version: self.localVersionBytes()) }
    }

    private func currentSocket(gen: Int) -> URLSessionWebSocketTask? {
        gen == generation ? socket : nil
    }

    private func onSocketError(gen: Int) {
        guard gen == generation, !closed else { return }
        events(.disconnected)
        scheduleReconnect(gen: gen)
    }

    private func scheduleReconnect(gen: Int) {
        guard gen == generation, !closed else { return }
        socket?.cancel(with: .abnormalClosure, reason: nil)
        socket = nil
        receiveTask?.cancel()
        pingTask?.cancel()
        livenessTask?.cancel()
        let delay = backoffMs
        backoffMs = min(backoffMs * 2, RoomClient.backoffCapMs)
        Task {
            try? await Task.sleep(nanoseconds: UInt64(delay) * 1_000_000)
            await self.connect()
        }
    }

    private func pingTick(gen: Int) async {
        guard gen == generation, let socket else { return }
        let silence = DispatchTime.now().uptimeNanoseconds - lastInbound.uptimeNanoseconds
        if silence > RoomClient.silenceLeaseNs {
            onSocketError(gen: gen)
            return
        }
        try? await socket.send(.string("ping"))
    }

    /// Two-tier room liveness (room.rs run_session): an in-flight %LOR
    /// JoinRequest has a hard answer deadline; otherwise a long-quiet room
    /// gets an idempotent rejoin probe. The deadline runs from the LATER of
    /// join-sent and last %LOR frame, so a join queued behind a draining
    /// backfill that keeps producing %LOR acks is never killed mid-push. A
    /// hibernating-but-healthy DO is simply woken by the probe and answers —
    /// hibernation is NOT death, which is why probes run in minutes while
    /// the transport lease runs in seconds.
    private func livenessTick(gen: Int) async {
        guard gen == generation, socket != nil, !closed else { return }
        let now = DispatchTime.now().uptimeNanoseconds
        if let sent = joinSentAt {
            let base = max(sent.uptimeNanoseconds, lastLorRx.uptimeNanoseconds)
            if now - base > RoomClient.joinDeadlineNs {
                // The 2026-07-30 hang: a room that accepted the socket but
                // never answered the join. Redial via the backoff loop — one
                // fresh dial re-instantiates a wedged DO.
                onSocketError(gen: gen)
            }
            return
        }
        if now - lastLorRx.uptimeNanoseconds > probeIntervalNs {
            // Quiet room: rejoin as a liveness probe. Consecutive quiet
            // probes back off so a dormant chat costs a handful of DO wakes
            // a day; any organic %LOR traffic resets the cadence. Probe
            // state is armed BEFORE the send suspends — the actor is
            // reentrant across the await, so the answer could otherwise be
            // handled while joinIsProbe is still false and replay join side
            // effects.
            joinSentAt = .now()
            joinIsProbe = true
            lastProbeAt = .now()
            probeIntervalNs = min(probeIntervalNs * 2, RoomClient.roomProbeMaxNs)
            await send(.joinRequest(crdt: .loro, roomId: roomId, auth: [],
                                    version: localVersionBytes()))
        }
    }

    // MARK: Inbound

    private func handleInbound(_ message: URLSessionWebSocketTask.Message, gen: Int) async {
        guard gen == generation else { return }
        lastInbound = .now()
        switch message {
        case .string:
            return  // "pong" — lease already refreshed
        case .data(let data):
            guard let frame = LoroWire.decode(data) else { return }
            await handleFrame(frame, gen: gen)
        @unknown default:
            return
        }
    }

    private func handleFrame(_ frame: ProtocolMessage, gen: Int) async {
        // Advance the room-liveness clock for %LOR frames only: %EPH presence
        // keeps flowing from a doc-wedged DO, so letting it count silenced
        // the probe on exactly the room that wedged (room.rs, round-2
        // adversarial finding). Frames arriving inside a probe's own reply
        // window must not reset the probe backoff, or every probe would
        // reset its own decay.
        if crdtOf(frame) == .loro {
            lastLorRx = .now()
            if let at = lastProbeAt,
               DispatchTime.now().uptimeNanoseconds - at.uptimeNanoseconds
                   <= RoomClient.probeReplyGraceNs {
                // Probe's own reply — leave the backoff decaying.
            } else {
                probeIntervalNs = RoomClient.roomProbeAfterNs
            }
        }
        switch frame {
        case .joinResponseOk(let crdt, _, _, let version, _):
            await onJoinOk(crdt: crdt, version: version)

        case .joinError(let crdt, _, let code, _):
            if crdt == .loro {
                if code == .versionUnknown {
                    // Server can't diff from our VV — full snapshot backfill.
                    await sendJoinLoro(version: [])
                } else {
                    // AuthFailed / AppError: back off and retry (token refresh
                    // may fix it on the next dial).
                    onSocketError(gen: gen)
                }
            }

        case .docUpdate(let crdt, _, let updates, _):
            applyRemote(crdt: crdt, updates: updates)

        case .docUpdateFragmentHeader(let crdt, _, let batchId, let count, let total):
            guard count > 0, count <= RoomClient.maxFragmentCount,
                  total <= UInt64(RoomClient.maxReassembledBytes) else { return }
            fragments[batchId] = FragmentBuffer(crdt: crdt, parts: Array(repeating: nil, count: Int(count)),
                                                received: 0, totalSize: Int(total))

        case .docUpdateFragment(_, _, let batchId, let index, let fragment):
            onFragment(batchId: batchId, index: Int(index), fragment: fragment)

        case .ack(let crdt, _, let refId, let status):
            await onAck(crdt: crdt, refId: refId, status: status)

        case .roomError(_, _, let code, _):
            if code == .evicted {
                onSocketError(gen: gen)
            } else {
                await sendJoinLoro(version: localVersionBytes())
            }

        case .joinRequest, .leave:
            return
        }
    }

    private func crdtOf(_ frame: ProtocolMessage) -> CrdtType {
        switch frame {
        case .joinRequest(let crdt, _, _, _),
             .joinResponseOk(let crdt, _, _, _, _),
             .joinError(let crdt, _, _, _),
             .docUpdate(let crdt, _, _, _),
             .docUpdateFragmentHeader(let crdt, _, _, _, _),
             .docUpdateFragment(let crdt, _, _, _, _),
             .roomError(let crdt, _, _, _),
             .ack(let crdt, _, _, _),
             .leave(let crdt, _):
            return crdt
        }
    }

    private func onJoinOk(crdt: CrdtType, version: [UInt8]) async {
        switch crdt {
        case .loro:
            joinSentAt = nil  // join answered — disarm the deadline
            let wasProbe = joinIsProbe
            joinIsProbe = false
            joinedLor = true
            backoffMs = RoomClient.backoffBaseMs
            // Resubmit-from-VV: push everything the server lacks. Gated on
            // the VERSION VECTORS, not the export bytes: the export returns a
            // non-empty envelope even when there is nothing to say, so a
            // byte-length gate made every liveness probe upload a no-op
            // DocUpdate that dirtied the room's caches (room.rs finding).
            if !doc.oplogVv().isEmpty(), invalidRejoins < RoomClient.maxInvalidRejoins {
                let serverVv: VersionVector
                if version.isEmpty {
                    serverVv = VersionVector()
                } else {
                    serverVv = (try? VersionVector.decode(bytes: Data(version))) ?? VersionVector()
                }
                if !serverVv.includesVv(other: doc.oplogVv()),
                   let missing = try? doc.export(mode: .updates(from: serverVv)), !missing.isEmpty {
                    await sendLoroUpdates([[UInt8](missing)])
                }
            }
            if wasProbe {
                // A probe answer on an established session proves the room is
                // alive — that is ALL it is for. Re-running the side effects
                // below would re-join %EPH (re-uploading full presence) and
                // re-broadcast .connected on a timer.
                return
            }
            // Join presence once the doc room is up.
            await send(.joinRequest(crdt: .loroEphemeral, roomId: roomId, auth: [], version: []))
            events(.connected)
        case .loroEphemeral:
            let all = eph.encodeAll()
            if !all.isEmpty {
                await send(.docUpdate(crdt: .loroEphemeral, roomId: roomId,
                                      updates: [[UInt8](all)], batchId: .random()))
            }
        }
    }

    private func applyRemote(crdt: CrdtType, updates: [[UInt8]]) {
        switch crdt {
        case .loro:
            var imported = false
            for update in updates where !update.isEmpty {
                if let _ = try? doc.importWith(bytes: Data(update), origin: "remote") {
                    imported = true
                } else if !fullResyncRequested {
                    fullResyncRequested = true
                    Task { await self.sendJoinLoro(version: []) }
                }
            }
            if imported { events(.remoteUpdate) }
        case .loroEphemeral:
            var applied = false
            for update in updates where !update.isEmpty {
                if (try? eph.apply(data: Data(update))) != nil { applied = true }
            }
            if applied { events(.ephemeralUpdate) }
        }
    }

    private func onFragment(batchId: BatchId, index: Int, fragment: [UInt8]) {
        guard var buffer = fragments[batchId] else { return }
        guard index < buffer.parts.count else {
            fragments.removeValue(forKey: batchId)
            return
        }
        if buffer.parts[index] == nil { buffer.received += 1 }
        buffer.parts[index] = fragment
        if buffer.received < buffer.parts.count {
            fragments[batchId] = buffer
            return
        }
        fragments.removeValue(forKey: batchId)
        var total: [UInt8] = []
        total.reserveCapacity(buffer.totalSize)
        for part in buffer.parts { total.append(contentsOf: part ?? []) }
        applyRemote(crdt: buffer.crdt, updates: [total])
    }

    private func onAck(crdt: CrdtType, refId: BatchId, status: UpdateStatusCode) async {
        switch status {
        case .ok:
            pending.removeValue(forKey: refId)
        case .fragmentTimeout:
            // DO hibernated mid-batch — resend the whole batch.
            if let batch = pending.removeValue(forKey: refId) {
                await sendLoroUpdates(batch)
            }
        case .invalidUpdate, .permissionDenied:
            pending.removeValue(forKey: refId)
            if crdt == .loro, invalidRejoins < RoomClient.maxInvalidRejoins {
                invalidRejoins += 1
                await sendJoinLoro(version: localVersionBytes())
            }
        default:
            pending.removeValue(forKey: refId)
        }
    }

    // MARK: Outbound

    /// Called by the doc store on local commit (subscribeLocalUpdate bytes).
    func sendLocalUpdate(_ update: [UInt8]) async {
        guard joinedLor else { return }  // resubmit-from-VV covers pre-join commits
        await sendLoroUpdates([update])
    }

    /// Broadcast the presence store's local delta.
    func sendEphemeralUpdate(_ update: [UInt8]) async {
        guard joinedLor, !update.isEmpty else { return }
        await send(.docUpdate(crdt: .loroEphemeral, roomId: roomId, updates: [update], batchId: .random()))
    }

    private func sendJoinLoro(version: [UInt8]) async {
        // Arm the answer deadline BEFORE the frame leaves: an unanswered join
        // used to hang the session forever (room.rs, 2026-07-30). Joins
        // default to non-probe; the probe branch in livenessTick flags itself
        // after this returns.
        joinSentAt = .now()
        joinIsProbe = false
        await send(.joinRequest(crdt: .loro, roomId: roomId, auth: [], version: version))
    }

    /// Batch small updates, fragment any single update above the payload budget.
    private func sendLoroUpdates(_ updates: [[UInt8]]) async {
        var small: [[UInt8]] = []
        var smallBytes = 0
        for update in updates where !update.isEmpty {
            if update.count > RoomClient.fragmentBytes {
                await sendFragmented(update)
                continue
            }
            if smallBytes + update.count > RoomClient.fragmentBytes {
                await sendBatch(small)
                small = []
                smallBytes = 0
            }
            small.append(update)
            smallBytes += update.count
        }
        if !small.isEmpty { await sendBatch(small) }
    }

    private func sendBatch(_ updates: [[UInt8]]) async {
        let batchId = BatchId.random()
        pending[batchId] = updates
        await send(.docUpdate(crdt: .loro, roomId: roomId, updates: updates, batchId: batchId))
    }

    private func sendFragmented(_ update: [UInt8]) async {
        let batchId = BatchId.random()
        pending[batchId] = [update]
        let chunks = stride(from: 0, to: update.count, by: RoomClient.fragmentBytes).map {
            Array(update[$0..<min($0 + RoomClient.fragmentBytes, update.count)])
        }
        await send(.docUpdateFragmentHeader(crdt: .loro, roomId: roomId, batchId: batchId,
                                            fragmentCount: UInt64(chunks.count),
                                            totalSizeBytes: UInt64(update.count)))
        for (ix, chunk) in chunks.enumerated() {
            await send(.docUpdateFragment(crdt: .loro, roomId: roomId, batchId: batchId,
                                          index: UInt64(ix), fragment: chunk))
        }
    }

    private func send(_ message: ProtocolMessage) async {
        guard let socket, let data = LoroWire.encode(message) else { return }
        try? await socket.send(.data(data))
    }

    private func localVersionBytes() -> [UInt8] {
        let vv = doc.oplogVv()
        return vv.isEmpty() ? [] : [UInt8](vv.encode())
    }
}

private extension VersionVector {
    func isEmpty() -> Bool {
        // An empty VV encodes to a fixed small header with no entries; the
        // cheapest reliable emptiness probe the FFI exposes is comparing
        // against a fresh VV.
        self == VersionVector()
    }
}

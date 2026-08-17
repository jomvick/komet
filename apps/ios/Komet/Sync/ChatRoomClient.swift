// chat2 room client — a Swift port of crates/sync/src/chat_client.rs, the
// binary-frame sibling of RegistryClient. One WebSocket per chat to
// /chat2/{chatId}/ws: hello/state handshake with client-side checkpoint
// precision (the frontier payload decides fetch-vs-skip), cursor-based row
// backfill, push/ack with a pending-unacked batch queue, probe/redial
// liveness, reconnect with exponential backoff.
//
// The client owns no CRDT semantics: update bytes flow through main-actor
// delegate closures into SessionStore's doc, which persists doc content AND
// the room cursor in one file write (the C2 rule — they can never diverge).
//
// Liveness discipline is inherited from the registry client and its
// incidents: the text "ping" elicits a runtime auto-pong that proves NOTHING
// about the DO, so room health is judged only by protocol frames against
// hard deadlines.

import Foundation
import os

/// Sync must never fail silently (2026-07-31: a send that never left the
/// device was indistinguishable from a working one — `try?` all the way
/// down). Visible in Console.app / `log stream` under this subsystem.
let roomLog = Logger(subsystem: "sh.zeron.Zeron", category: "sync")

enum ChatRoomEvent: Sendable {
    /// Joined (or re-joined) and the initial catch-up (checkpoint if needed +
    /// row backfill) has been delivered through the apply closures.
    case connected
    /// The connection dropped; the client is backing off before redialing.
    case disconnected
}

actor ChatRoomClient {
    // Constants mirrored from crates/sync/src/chat_client.rs.
    static let pingIntervalNs: UInt64 = 15_000_000_000
    static let silenceLeaseNs: UInt64 = 45_000_000_000
    static let helloDeadlineNs: UInt64 = 15_000_000_000
    /// Checkpoint fetch + row backfill must complete within this deadline —
    /// post-strip docs are KB-scale, so this is generous even at 1.2 Mbps.
    static let backfillDeadlineNs: UInt64 = 120_000_000_000
    static let probeDeadlineNs: UInt64 = 10_000_000_000
    static let probeQuietNs: UInt64 = 900_000_000_000  // 15min quiet-room probe
    static let livenessTickNs: UInt64 = 1_000_000_000
    static let backoffBaseMs = 250
    static let backoffCapMs = 30_000
    /// Re-push cadence after a `quota` rejection (server window is 60s).
    static let quotaRetryNs: UInt64 = 5_000_000_000
    /// Client-side push cap: the DO's per-row cap (1 MiB) minus frame-header
    /// headroom — the runtime closes WS messages at 1 MiB BEFORE the DO runs,
    /// so an over-cap payload would die with no error frame to retire it.
    static let maxPushBytes = 1024 * 1024 - 4096

    /// MainActor-isolated bridge to SessionStore's doc. Every apply persists
    /// doc + cursor together; `applyCheckpoint` returns false on an import
    /// failure (the session redials rather than run blind).
    struct Delegate: Sendable {
        var cursor: @MainActor @Sendable () -> UInt64
        var containsFrontier: @MainActor @Sendable (Data) -> Bool
        var applyCheckpoint: @MainActor @Sendable (Data, UInt64) -> Bool
        var applyRow: @MainActor @Sendable (Data, UInt64) -> Void
        var advanceCursor: @MainActor @Sendable (UInt64) -> Void
        var event: @MainActor @Sendable (ChatRoomEvent) -> Void
    }

    private struct PendingPush {
        var batchId: String
        var bytes: Data
        /// In flight on the CURRENT connection (cleared on disconnect so
        /// reconnects re-push; the server dedupes replays by batchId).
        var inFlight = false
    }

    private let chatId: String
    private let device: String
    private let urlProvider: @Sendable () async -> URL?
    private let checkpointRequest: @Sendable () async -> URLRequest?
    private let delegate: Delegate

    private var socket: URLSessionWebSocketTask?
    private var receiveTask: Task<Void, Never>?
    private var pingTask: Task<Void, Never>?
    private var livenessTask: Task<Void, Never>?
    private var quotaTask: Task<Void, Never>?
    private var pending: [PendingPush] = []
    private var joined = false
    private var closed = false
    private var generation = 0
    private var backoffMs = ChatRoomClient.backoffBaseMs
    /// First backfill of THIS client instance must NOT exclude own rows: the
    /// pending queue doesn't survive restarts, and a restored device's doc
    /// may be missing its own post-backup writes — they exist only on the
    /// server. Loro re-import is a no-op, so redownloading own bytes once is
    /// pure safety; same-process reconnects skip them.
    private var resumed = false
    /// Transport clock — pongs count, so a healthy socket never trips it.
    private var lastInbound = DispatchTime.now()
    /// Protocol clock — only real frames count (pongs prove nothing).
    private var lastProtocolRx = DispatchTime.now()
    private var helloSentAt: DispatchTime?
    private var backfillStartedAt: DispatchTime?
    private var probeSentAt: DispatchTime?

    init(chatId: String,
         device: String,
         urlProvider: @escaping @Sendable () async -> URL?,
         checkpointRequest: @escaping @Sendable () async -> URLRequest?,
         delegate: Delegate) {
        self.chatId = chatId
        self.device = device
        self.urlProvider = urlProvider
        self.checkpointRequest = checkpointRequest
        self.delegate = delegate
    }

    // MARK: Lifecycle

    func start() {
        closed = false
        connect()
    }

    func stop() {
        closed = true
        generation += 1
        cancelTasks()
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
        joined = false
    }

    /// Queue one local update batch for push. The batch survives reconnects
    /// until acked — the server dedupes replays by batchId.
    ///
    /// Batches over `maxPushBytes` are refused here: the server can never
    /// accept them, and a queued-forever batch would replay on every
    /// reconnect — the exact wedge class chat2 replaces. The ops stay in the
    /// local doc; on a viewer device there is no checkpoint fallback, so
    /// this is loud (post-strip updates are KB-scale — an over-cap update is
    /// an upstream bug).
    func enqueue(update: Data) {
        guard update.count <= ChatRoomClient.maxPushBytes else {
            roomLog.error("chat2 \(self.chatId, privacy: .public): update \(update.count)B exceeds the row cap; not queued")
            return
        }
        pending.append(PendingPush(batchId: UUID().uuidString.lowercased(), bytes: update))
        if joined {
            Task { await self.pushPending() }
        }
    }

    /// Foreground hook (the chat2 twin of RegistryClient.kick): suspension
    /// kills the socket without running any failure path. A dead or unjoined
    /// session redials NOW on fresh backoff; a joined one gets an immediate
    /// deadline-checked probe (post-suspend sockets are half-open more often
    /// than not).
    func kick() async {
        guard !closed else { return }
        backoffMs = ChatRoomClient.backoffBaseMs
        if socket == nil {
            connect()
            return
        }
        if !joined {
            // A handshake/catch-up is already in flight on this socket — its
            // own deadlines police it (hello 15s, backfill 120s). Redialing
            // here abandoned a healthy catch-up and refetched the checkpoint
            // (observed on launch: foregrounded() fires during the first
            // join). A zombie socket with NO handshake pending is redialed.
            if helloSentAt == nil, backfillStartedAt == nil {
                connect()
            }
            return
        }
        guard probeSentAt == nil else { return }
        await sendProbe()
    }

    private func cancelTasks() {
        receiveTask?.cancel()
        pingTask?.cancel()
        livenessTask?.cancel()
        quotaTask?.cancel()
    }

    private func connect() {
        guard !closed else { return }
        generation += 1
        let gen = generation
        joined = false
        helloSentAt = nil
        backfillStartedAt = nil
        probeSentAt = nil
        lastProtocolRx = .now()
        for ix in pending.indices {
            pending[ix].inFlight = false
        }

        Task {
            guard let url = await urlProvider() else {
                // No URL = no token (refresh failed or signed out) — the most
                // confusing silent failure: everything cached renders,
                // nothing syncs. Say so and back off.
                roomLog.error("chat2 \(self.chatId, privacy: .public): no socket URL (token unavailable); backing off")
                await self.scheduleReconnect(gen: gen)
                return
            }
            await self.openSocket(url: url, gen: gen)
        }
    }

    private func openSocket(url: URL, gen: Int) async {
        guard gen == generation, !closed else { return }
        let task = URLSession.shared.webSocketTask(with: url)
        socket = task
        task.resume()
        lastInbound = .now()
        lastProtocolRx = .now()

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
                try? await Task.sleep(nanoseconds: ChatRoomClient.pingIntervalNs)
                guard let self else { return }
                await self.pingTick(gen: gen)
            }
        }

        livenessTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: ChatRoomClient.livenessTickNs)
                guard let self else { return }
                await self.livenessTick(gen: gen)
            }
        }

        // Hello with the persisted cursor. The deadline is armed BEFORE the
        // send — an unanswered hello must never hang the session.
        helloSentAt = .now()
        let cursor = await delegate.cursor()
        await send(ChatWire.encode(ChatFrameType.hello,
                                   header: ["cursor": cursor, "device": device]))
    }

    private func currentSocket(gen: Int) -> URLSessionWebSocketTask? {
        gen == generation ? socket : nil
    }

    private func onSocketError(gen: Int) async {
        guard gen == generation, !closed else { return }
        roomLog.warning("chat2 \(self.chatId, privacy: .public): session ended (joined=\(self.joined)); redialing in \(self.backoffMs)ms")
        joined = false
        await delegate.event(.disconnected)
        scheduleReconnect(gen: gen)
    }

    private func scheduleReconnect(gen: Int) {
        guard gen == generation, !closed else { return }
        socket?.cancel(with: .abnormalClosure, reason: nil)
        socket = nil
        cancelTasks()
        let delay = backoffMs
        backoffMs = min(backoffMs * 2, ChatRoomClient.backoffCapMs)
        Task {
            try? await Task.sleep(nanoseconds: UInt64(delay) * 1_000_000)
            await self.connect()
        }
    }

    // MARK: Timers

    private func pingTick(gen: Int) async {
        guard gen == generation, let socket else { return }
        let silence = DispatchTime.now().uptimeNanoseconds - lastInbound.uptimeNanoseconds
        if silence > ChatRoomClient.silenceLeaseNs {
            roomLog.warning("chat2 \(self.chatId, privacy: .public): socket silent past lease; treating as dead")
            await onSocketError(gen: gen)
            return
        }
        try? await socket.send(.string("ping"))
    }

    /// Hello answers, the catch-up, and probes run against hard deadlines; a
    /// long-quiet joined room gets a probe. Any protocol frame clears the
    /// probe deadline; catch-up progress (rows landing) feeds the protocol
    /// clock, so a draining backfill is never killed mid-stream.
    private func livenessTick(gen: Int) async {
        guard gen == generation, socket != nil, !closed else { return }
        let now = DispatchTime.now().uptimeNanoseconds
        if let sent = helloSentAt, now - sent.uptimeNanoseconds > ChatRoomClient.helloDeadlineNs {
            roomLog.warning("chat2 \(self.chatId, privacy: .public): no state frame within deadline; room presumed wedged, redialing")
            await onSocketError(gen: gen)
            return
        }
        if let started = backfillStartedAt,
           now - started.uptimeNanoseconds > ChatRoomClient.backfillDeadlineNs {
            roomLog.warning("chat2 \(self.chatId, privacy: .public): backfill did not complete within deadline; redialing")
            await onSocketError(gen: gen)
            return
        }
        if let sent = probeSentAt, now - sent.uptimeNanoseconds > ChatRoomClient.probeDeadlineNs {
            roomLog.warning("chat2 \(self.chatId, privacy: .public): probe unanswered past deadline; redialing")
            await onSocketError(gen: gen)
            return
        }
        if joined, probeSentAt == nil,
           now - lastProtocolRx.uptimeNanoseconds > ChatRoomClient.probeQuietNs {
            await sendProbe()
            // Don't re-arm the quiet timer against the same silence.
            lastProtocolRx = .now()
        }
    }

    private func sendProbe() async {
        // Armed BEFORE the send suspends — the actor is reentrant across the
        // await, and the answer must find the deadline already set.
        probeSentAt = .now()
        await send(ChatWire.encode(ChatFrameType.probe, header: [:]))
    }

    // MARK: Inbound

    private func handleInbound(_ message: URLSessionWebSocketTask.Message, gen: Int) async {
        guard gen == generation else { return }
        lastInbound = .now()
        guard case .data(let data) = message else { return }  // "pong" text
        guard let frame = ChatWire.decode(data) else {
            // Protocol breakdown — redial rather than run blind against a
            // server we can't parse.
            roomLog.error("chat2 \(self.chatId, privacy: .public): unparseable frame; redialing")
            await onSocketError(gen: gen)
            return
        }
        lastProtocolRx = .now()
        probeSentAt = nil

        switch frame.kind {
        case ChatFrameType.state:
            await handleState(frame, gen: gen)

        case ChatFrameType.row:
            guard let seq = (frame.header["seq"] as? NSNumber)?.uint64Value else { return }
            // Own-device rows can still arrive (first-backfill redownload, a
            // racing second socket) — Loro re-import is a no-op; the cursor
            // advance is what matters.
            await delegate.applyRow(frame.payload, seq)

        case ChatFrameType.rowsDone:
            guard backfillStartedAt != nil else { return }
            backfillStartedAt = nil
            let wasResumed = resumed
            resumed = true
            joined = true
            backoffMs = ChatRoomClient.backoffBaseMs
            roomLog.info("chat2 \(self.chatId, privacy: .public): joined (converged, resumed=\(wasResumed))")
            await delegate.event(.connected)
            // Anything pending (offline writes, reconnect re-pushes) goes
            // now — the server's batchId dedupe makes replays exact no-ops.
            await pushPending()

        case ChatFrameType.ack:
            guard let batchId = frame.header["batchId"] as? String,
                  let seq = (frame.header["seq"] as? NSNumber)?.uint64Value else { return }
            pending.removeAll { $0.batchId == batchId }
            await delegate.advanceCursor(seq)
            // A grant after a quota rejection: drain whatever the error
            // handler un-flagged (sparse mobile queues — no livelock risk).
            await pushPending()

        case ChatFrameType.probeOk:
            break  // liveness proven; clocks already advanced above

        case ChatFrameType.presence:
            break  // no consumer on mobile today

        case ChatFrameType.error:
            await handleErrorFrame(frame.header, gen: gen)

        default:
            // Unknown server frame: tolerate (future protocol additions).
            break
        }
    }

    /// The hello answer: plan the catch-up (chat_client.rs run_session).
    private func handleState(_ frame: ChatWireFrame, gen: Int) async {
        guard let state = ChatStateHeader(frame.header) else {
            roomLog.error("chat2 \(self.chatId, privacy: .public): malformed state header; redialing")
            await onSocketError(gen: gen)
            return
        }
        guard helloSentAt != nil else { return }  // late duplicate — ignore
        helloSentAt = nil
        backfillStartedAt = .now()

        let cursor = await delegate.cursor()
        if cursor > state.headSeq {
            // Server behind our cursor = the room was reset/wiped. A viewer
            // owes no re-seed (that is the host's checkpoint duty) — treat
            // the cursor as fresh and reload from what the room has.
            roomLog.warning("chat2 \(self.chatId, privacy: .public): server lost state (headSeq=\(state.headSeq) < cursor=\(cursor)); treating as fresh")
        }
        // Same presence rule as chatPlanCatchUp: SIZE, not seq — a seeded
        // room's checkpoint covers seq 0.
        var contained = state.checkpointSize == 0
        if !contained {
            contained = await delegate.containsFrontier(frame.payload)
        }
        let plan = chatPlanCatchUp(cursor: cursor, state: state, frontierContained: contained)
        let after: UInt64
        switch plan {
        case .rowsOnly(let a):
            after = a
        case .checkpointThenRows(let a):
            roomLog.info("chat2 \(self.chatId, privacy: .public): fetching checkpoint (seq=\(state.checkpointSeq), \(state.checkpointSize)B)")
            guard let bytes = await fetchCheckpoint(), gen == generation, !closed else {
                if gen == generation, !closed {
                    roomLog.warning("chat2 \(self.chatId, privacy: .public): checkpoint fetch failed; redialing")
                    await onSocketError(gen: gen)
                }
                return
            }
            guard await delegate.applyCheckpoint(bytes, state.checkpointSeq) else {
                roomLog.error("chat2 \(self.chatId, privacy: .public): checkpoint import failed; redialing")
                await onSocketError(gen: gen)
                return
            }
            after = a
        }
        await send(ChatWire.encode(ChatFrameType.rowsReq,
                                   header: ["after": after, "excludeOwn": resumed]))
    }

    private func handleErrorFrame(_ header: [String: Any], gen: Int) async {
        let code = header["code"] as? String ?? "?"
        let message = header["message"] as? String ?? ""
        let batchId = header["batchId"] as? String ?? ""
        roomLog.error("chat2 \(self.chatId, privacy: .public): server rejected a frame: \(code, privacy: .public): \(message, privacy: .public)")
        switch code {
        case "too_large", "empty", "bad_push":
            // Permanent verdicts on a specific batch: retire it, or it
            // replays on every reconnect forever — the wedge class this
            // design exists to kill. The ops stay in the local doc.
            if !batchId.isEmpty {
                pending.removeAll { $0.batchId == batchId }
            }
        case "quota":
            // Transient: the window passes on its own — keep the batch
            // queued and probe with the HEAD batch on a short clock
            // (one-per-grant; a full-queue replay would itself consume the
            // server's quota window).
            for ix in pending.indices {
                pending[ix].inFlight = false
            }
            quotaTask?.cancel()
            quotaTask = Task { [weak self] in
                try? await Task.sleep(nanoseconds: ChatRoomClient.quotaRetryNs)
                guard !Task.isCancelled, let self else { return }
                await self.pushHead(gen: gen)
            }
        case "hello_first":
            // Session state desynced from the server — start over.
            await onSocketError(gen: gen)
        default:
            break
        }
    }

    // MARK: Outbound

    private func pushPending() async {
        guard joined else { return }
        for ix in pending.indices where !pending[ix].inFlight {
            pending[ix].inFlight = true
            let push = pending[ix]
            await send(ChatWire.encode(ChatFrameType.push,
                                       header: ["batchId": push.batchId],
                                       payload: push.bytes))
        }
    }

    /// Send only the queue's head batch — the quota-probe path.
    private func pushHead(gen: Int) async {
        guard gen == generation, joined, let head = pending.first else { return }
        pending[0].inFlight = true
        await send(ChatWire.encode(ChatFrameType.push,
                                   header: ["batchId": head.batchId],
                                   payload: head.bytes))
    }

    private func send(_ frame: Data) async {
        guard let socket else { return }
        try? await socket.send(.data(frame))
    }

    // MARK: Checkpoint fetch (GET /chat2/{chatId}/checkpoint, Range resume)

    /// Range-resume loop (chat2_host.rs EdgeCheckpointFetcher): each attempt
    /// continues at the byte where the last one stopped; the DO stamps every
    /// response with the checkpoint's seq, and a mid-download replacement
    /// restarts from 0 (a Range against a new blob would splice two blobs).
    private func fetchCheckpoint() async -> Data? {
        var got = Data()
        var seenSeq: String?
        for _ in 0..<4 {
            guard var request = await checkpointRequest() else { return nil }
            if !got.isEmpty {
                request.setValue("bytes=\(got.count)-", forHTTPHeaderField: "Range")
            }
            guard let (stream, response) = try? await URLSession.shared.bytes(for: request),
                  let http = response as? HTTPURLResponse else { continue }
            let seq = http.value(forHTTPHeaderField: "x-chat2-checkpoint-seq")
            if let seq {
                if let prev = seenSeq, seq != prev {
                    roomLog.info("chat2 \(self.chatId, privacy: .public): checkpoint replaced mid-download; restarting")
                    got.removeAll()
                    seenSeq = seq
                    continue
                }
                seenSeq = seq
            }
            switch http.statusCode {
            case 200: got.removeAll()
            case 206: break
            default:
                roomLog.warning("chat2 \(self.chatId, privacy: .public): checkpoint HTTP \(http.statusCode)")
                return nil
            }
            do {
                for try await byte in stream {
                    got.append(byte)
                }
                return got
            } catch {
                // Mid-body drop: keep the bytes, resume via Range.
                roomLog.warning("chat2 \(self.chatId, privacy: .public): checkpoint stream dropped at \(got.count)B; resuming")
            }
        }
        return nil
    }
}

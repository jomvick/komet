/**
 * SessionRoom — one Durable Object per doc room, speaking loro-protocol over
 * hibernatable WebSockets (design §2, §3.1). Two doc kinds share this class:
 * chat session docs (room name = chatId, claim-on-first-join ownership) and
 * workspace docs (room name = `ws/{orgId}`, org-membership authz enforced by
 * the Worker — the DO sees the ROOM_KIND_HEADER stamp and skips ownership).
 *
 * Persistence model:
 * - `updates` — append-only incoming update log, buffered in memory during
 *   active streams and flushed every ~DO_FLUSH_MS (a crash losing buffered
 *   ops is healed by normal CRDT resync from the host on reconnect).
 * - `snapshot` blob — the doc's current snapshot. Two-level compaction:
 *   LOG FOLD (whenever the update log passes COMPACT_LOG_BYTES): re-export a
 *   full snapshot and clear the log — loses nothing. HISTORY TRIM (daily
 *   alarm): once a recorded frontier checkpoint is older than RETAIN_DAYS,
 *   re-export a *shallow* snapshot at that frontier — trimmed op history is
 *   discarded permanently, state is fully preserved (§3.1).
 * - `tail` blob — materialized last-N-messages JSON, recomputed lazily on
 *   GET /tail when dirty (§5 L2).
 * - `diff` blob — latest-only working-tree diff sidecar, overwritten on each
 *   host publish (§6.1).
 * - Ephemeral presence (%EPH room) is memory-only by construction.
 *
 * Hibernation discipline: no wall-clock JS timers except the flush debounce
 * (which only exists while traffic keeps the DO awake anyway); scheduled work
 * (checkpoints, history trim, R2 backup §3.3) rides the durable alarm.
 */
import { LoroDoc, EphemeralStore, VersionVector } from "loro-crdt";
import {
  CrdtType,
  JoinErrorCode,
  MAX_MESSAGE_SIZE,
  MessageType,
  UpdateStatusCode,
  bytesToHex,
  decode,
  encode,
  type DocUpdate,
  type DocUpdateFragmentHeader,
  type JoinRequest,
  type ProtocolMessage
} from "loro-protocol";
import {
  COMPACT_LOG_BYTES,
  COMPACT_LOG_ROWS,
  DO_FLUSH_MS,
  RETAIN_DAYS,
  materializeTail
} from "./session-doc";
import { createBlobStore, getJsonBlob, putJsonBlob, type BlobStore } from "./blobs";
import { AUTH_USER_HEADER, ROOM_KIND_HEADER, type Env } from "./env";

const DAY_MS = 24 * 60 * 60 * 1000;
const RETAIN_MS = RETAIN_DAYS * DAY_MS;
/** Consecutive cold-replay deaths (CPU-limit kills mid-`ensureDoc`) before the
 * room concludes it is wedged and drops its own log — see `ensureDoc`. */
const REPLAY_CRASH_LIMIT = 3;
/** Payload bytes per outbound fragment (leaves room for the envelope). */
const FRAGMENT_BYTES = 200_000;
/** Keep a rolling ~5 weeks of daily frontier checkpoints. */
const MAX_CHECKPOINTS = 36;
/** Isolate-wide poisoned-wasm strike counter — MODULE state on purpose: every
 * SessionRoom co-located in this isolate shares ONE loro-wasm linear memory,
 * so heap exhaustion poisons them all at once (2026-08-04: every
 * byte-exporting wasm call threw RangeError("Invalid array buffer length")
 * while imports/relays kept working, silently wedging all joins fleet-wide). */
let wasmPoisonStrikes = 0;
/** Wasm-boundary failures before `ctx.abort()` recycles the isolate. Wasm
 * memory only ever grows, so the poisoned state is permanent until the isolate
 * dies — and Cloudflare's own memory-limit reset arrives only after minutes of
 * thrash. Aborting early turns a silent hours-long wedge into a seconds-long
 * blip (sockets close, clients redial into a fresh isolate). */
const WASM_POISON_ABORT_AFTER = 3;
/** Free a quiet room's materialized doc after this long. Wasm linear memory
 * NEVER shrinks and outlives DO instances (one wasm module per isolate), so
 * docs held resident until instance eviction leak permanently — every
 * reconnect herd rematerialized every co-located room and the heap climbed
 * monotonically to the isolate limit in minutes (2026-08-04 thrash loop).
 * Freeing on idle returns blocks to the wasm allocator for reuse;
 * rematerialization is a 10-50ms cold replay. */
const DOC_IDLE_RELEASE_MS = 60_000;
/** Force-trim threshold: a room with NO trim-eligible checkpoint but a
 * full-history snapshot this large trims at its CURRENT frontier instead of
 * waiting days to age into RETAIN_DAYS eligibility. Behind/concurrent peers
 * take the §3.1 stale-peer full resync (designed-for). Without this, the
 * 2026-08-04 whale rooms (954KB / 1.8MB import chats, checkpoints first
 * recorded today) would have kept re-materializing their full history into
 * the pressed wasm heap for three more days of thrash. */
const TRIM_FORCE_BYTES = 512 * 1024;

interface SocketState {
  userId: string;
  /** Joined sub-rooms by crdt magic ("%LOR", "%EPH"). */
  rooms: string[];
  /** True for sockets on a workspace-doc room — org membership was enforced
   * by the Worker, so the per-chat ownership discipline does not apply. */
  workspace?: boolean;
  /** Dialing engine's device id (from `&device=`, Worker-validated) — pure
   * log attribution; never used for authz. */
  deviceId?: string;
}

interface FragmentBatch {
  parts: Uint8Array[];
  received: number;
  totalSize: number;
  header: DocUpdateFragmentHeader;
}

interface FrontierCheckpoint {
  at: number;
  frontiers: { peer: string; counter: number }[];
}

export class SessionRoom implements DurableObject {
  private readonly ctx: DurableObjectState;
  private readonly env: Env;
  private readonly blobs: BlobStore;
  /** Lazily materialized doc — the log is authoritative; this is a cache. */
  private doc: LoroDoc | undefined;
  private eph: EphemeralStore | undefined;
  private pending: Uint8Array[] = [];
  private pendingBytes = 0;
  private flushTimer: ReturnType<typeof setTimeout> | undefined;
  /** In-memory fragment reassembly. Lost on hibernation → the sender gets a
   * FragmentTimeout ack for the unknown batch and resends — self-healing. */
  private readonly fragments = new Map<WebSocket, Map<string, FragmentBatch>>();
  /** Idle-doc release bookkeeping (see DOC_IDLE_RELEASE_MS / touchDoc). */
  private docIdleTimer: ReturnType<typeof setTimeout> | undefined;
  private lastDocUse = 0;

  constructor(ctx: DurableObjectState, env: Env) {
    this.ctx = ctx;
    this.env = env;
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS updates (seq INTEGER PRIMARY KEY AUTOINCREMENT, bytes BLOB NOT NULL, received_at INTEGER NOT NULL)"
    );
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)"
    );
    this.blobs = createBlobStore(ctx.storage.sql);
    // Protocol-designed hibernation keepalive: ping → pong without waking us.
    // NOTE (2026-07-30 incident): precisely BECAUSE the runtime answers these
    // itself, a pong is NOT evidence this DO can still run — a wedged room
    // kept auto-ponging for hours while never processing a join. Clients judge
    // room liveness from protocol frames plus a join-response deadline
    // (crates/sync/src/room.rs), never from these pongs. Do not "upgrade" this
    // to an app-level handler: waking on every ping would abolish hibernation.
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  // ── meta helpers ──────────────────────────────────────────────────────────

  private getMeta(key: string): string | undefined {
    const rows = [...this.ctx.storage.sql.exec("SELECT value FROM meta WHERE key = ?", key)];
    return rows[0]?.value as string | undefined;
  }

  private setMeta(key: string, value: string): void {
    this.ctx.storage.sql.exec(
      "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
      key,
      value
    );
  }

  // ── HTTP surface (only reachable through the authed Worker) ──────────────

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const userId = request.headers.get(AUTH_USER_HEADER);
    if (!userId) return new Response("unauthenticated", { status: 401 });
    // Workspace rooms: the Worker already checked org membership; every
    // member may read/write, so the owner gates below are bypassed.
    const workspace = request.headers.get(ROOM_KIND_HEADER) === "workspace";

    if (url.pathname === "/ws") {
      const chatId = url.searchParams.get("chatId") ?? "";
      if (chatId && !this.getMeta("chatId")) this.setMeta("chatId", chatId);
      const deviceId = url.searchParams.get("device") ?? undefined;
      const pair = new WebSocketPair();
      this.ctx.acceptWebSocket(pair[1]);
      const state: SocketState = {
        userId,
        rooms: [],
        ...(workspace ? { workspace } : {}),
        ...(deviceId ? { deviceId } : {})
      };
      pair[1].serializeAttachment(state);
      console.log(
        "socket accepted",
        `room=${this.getMeta("chatId") ?? "?"}`,
        `device=${deviceId ?? "unattributed"}`,
        `sockets=${this.ctx.getWebSockets().length}`
      );
      return new Response(null, { status: 101, webSocket: pair[0] });
    }

    const owner = this.getMeta("owner");
    if (url.pathname === "/stats" && request.method === "GET") {
      // Observability: what this room holds and who's on it. Owner-gated like
      // every other read (org-membership-gated for workspace rooms).
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== userId) return json({ error: "forbidden" }, 403);
      }
      await this.flush();
      const updateRows = [...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM updates")][0]
        ?.n as number;
      const snapshot = this.blobs.get("snapshot");
      return json({
        chatId: this.getMeta("chatId") ?? null,
        connectedSockets: this.ctx.getWebSockets().length,
        updateRows,
        updateLogBytes: Number(this.getMeta("updateBytes") ?? "0"),
        snapshotBytes: snapshot?.length ?? 0,
        // Cold-start cost of the LAST materialization — the wedge-risk gauge
        // (2026-07-30: this creeping toward the CPU limit was invisible).
        lastReplayMs: Number(this.getMeta("lastReplayMs") ?? "0"),
        lastReplayRows: Number(this.getMeta("lastReplayRows") ?? "0"),
        // True between a wedge-break log drop and the first re-uploaded state
        // (the nightly backup is paused in that window).
        postReset: this.getMeta("postReset") === "1",
        tailCached: this.getMeta("tailDirty") !== "1" && this.blobs.get("tail") !== undefined,
        diffPublished: this.blobs.get("diff") !== undefined,
        checkpoints: (JSON.parse(this.getMeta("checkpoints") ?? "[]") as unknown[]).length,
        lastTrimAt: this.getMeta("lastTrimAt") ?? null,
        backupDirty: this.getMeta("backupDirty") === "1",
        // Non-zero while a cold replay is in flight or has been dying — the
        // wedge signature ensureDoc's automated reset watches for.
        replayAttempts: Number(this.getMeta("replayAttempts") ?? "0")
      });
    }
    if (url.pathname === "/tail" && request.method === "GET") {
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== userId) return json({ error: "forbidden" }, 403);
      }
      return json(await this.currentTail());
    }
    if (url.pathname === "/diff" && request.method === "GET") {
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== userId) return json({ error: "forbidden" }, 403);
      }
      const diff = getJsonBlob<unknown>(this.blobs, "diff");
      return diff === undefined ? json({ error: "not_found" }, 404) : json(diff);
    }
    if (url.pathname === "/diff" && request.method === "POST") {
      // The host may publish before any room join has claimed the doc.
      if (!workspace) {
        if (!owner) this.setMeta("owner", userId);
        else if (owner !== userId) return json({ error: "forbidden" }, 403);
      }
      putJsonBlob(this.blobs, "diff", await request.json());
      return json({ ok: true });
    }
    if (url.pathname === "/snapshot" && request.method === "GET") {
      // Repair/inspection read: the doc's full current snapshot bytes.
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== userId) return json({ error: "forbidden" }, 403);
      }
      await this.flush();
      const doc = await this.ensureDoc();
      const bytes = doc.export({ mode: "snapshot" });
      return new Response(bytes as unknown as BodyInit, {
        headers: { "content-type": "application/octet-stream" }
      });
    }
    if (url.pathname === "/append" && request.method === "POST") {
      // MERGE-safe repair write: import a Loro update (never replaces the
      // doc). Same durability bookkeeping as a WS DocUpdate.
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== userId) return json({ error: "forbidden" }, 403);
      }
      const body = new Uint8Array(await request.arrayBuffer());
      const doc = await this.ensureDoc();
      try {
        if (body.length > 0) doc.import(body);
      } catch {
        return json({ error: "invalid_update" }, 400);
      }
      this.recordLoroUpdates([body]);
      // Converge live peers: relay the update to connected %LOR sockets.
      const roomId = this.getMeta("chatId") ?? "";
      for (const ws of this.ctx.getWebSockets()) {
        const state = ws.deserializeAttachment() as SocketState | null;
        if (!state?.rooms.includes(CrdtType.Loro)) continue;
        this.sendUpdates(ws, CrdtType.Loro, roomId, [body]);
      }
      return json({ ok: true });
    }
    if (url.pathname === "/reset-log" && request.method === "POST") {
      // WEDGE BREAK: drop the persisted update log + snapshot so the NEXT cold
      // `ensureDoc` starts from empty instead of replaying a log so large it
      // exceeds the DO CPU limit and resets before any client can join (which
      // also blocks the compaction that would have shrunk it — a permanent
      // wedge). Deliberately does NOT call `ensureDoc`, so it stays cheap
      // enough to land on an already-wedged DO. State is not lost: every engine
      // holds the full workspace doc locally and re-uploads it on the next join
      // (CRDT merge), exactly like the `ws3` fresh-namespace recovery. Presence
      // is ephemeral and simply re-published. Owner/chatId meta are preserved.
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== userId) return json({ error: "forbidden" }, 403);
      }
      const before = [...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM updates")][0]?.n as
        | number
        | undefined;
      this.dropLog();
      this.doc?.free(); // release the wasm memory, don't wait on GC finalizers
      this.doc = undefined; // force a fresh (empty) materialization next join
      // Boot any currently-attached %LOR/%EPH sockets so their hung/half-cold
      // sessions bail and reconnect into the now-empty doc.
      for (const sock of this.ctx.getWebSockets()) {
        try {
          sock.close(4410, "room reset");
        } catch {
          /* already gone */
        }
      }
      return json({ ok: true, clearedUpdateRows: before ?? 0 });
    }
    return new Response("not found", { status: 404 });
  }

  // ── WebSocket protocol ────────────────────────────────────────────────────

  async webSocketMessage(ws: WebSocket, message: ArrayBuffer | string): Promise<void> {
    if (typeof message === "string") return; // ping/pong handled by auto-response
    let decoded: ProtocolMessage;
    try {
      decoded = decode(new Uint8Array(message));
    } catch {
      ws.close(1002, "Protocol error");
      return;
    }
    const state = ws.deserializeAttachment() as SocketState;
    try {
      switch (decoded.type) {
        case MessageType.JoinRequest:
          await this.handleJoin(ws, state, decoded);
          break;
        case MessageType.DocUpdate:
          await this.handleDocUpdate(ws, state, decoded);
          break;
        case MessageType.DocUpdateFragmentHeader:
          this.handleFragmentHeader(ws, state, decoded);
          break;
        case MessageType.DocUpdateFragment:
          await this.handleFragment(ws, state, decoded);
          break;
        case MessageType.Leave:
          state.rooms = state.rooms.filter((r) => r !== decoded.crdt);
          ws.serializeAttachment(state);
          break;
        case MessageType.Ack:
        case MessageType.RoomError:
          break;
        default:
          ws.close(1002, "Unsupported message");
      }
    } catch (e) {
      // A handler that dies pre-answer used to fail in SILENCE: the client
      // waits out its 15s join deadline, redials, and dies the same way —
      // the 2026-08-04 fleet-wide join wedge. Log attributed, answer an
      // outstanding join so clients fail fast and VISIBLY (JoinError →
      // long-backoff rejoin instead of a hot 15s dial loop), then escalate
      // suspected wasm-heap poisoning to an isolate recycle.
      console.error(
        "ws message handler failed",
        `room=${this.getMeta("chatId") ?? "?"}`,
        `device=${state?.deviceId ?? "unattributed"}`,
        `type=${decoded.type}`,
        String(e)
      );
      if (decoded.type === MessageType.JoinRequest) {
        this.send(ws, {
          type: MessageType.JoinError,
          crdt: decoded.crdt,
          roomId: decoded.roomId,
          code: JoinErrorCode.AppError,
          message: "internal error"
        });
      }
      this.escalateWasmPoisoning(e);
    }
  }

  async webSocketClose(ws: WebSocket): Promise<void> {
    this.logSocketEnd(ws, "closed");
    this.fragments.delete(ws);
    try {
      await this.flush();
    } catch (e) {
      // Flush can fold the log (a wasm snapshot export); an uncaught throw
      // here is invisible in a close handler. Same discipline as above.
      console.error("flush on socket close failed", `room=${this.getMeta("chatId") ?? "?"}`, String(e));
      this.escalateWasmPoisoning(e);
    }
  }

  async webSocketError(ws: WebSocket): Promise<void> {
    this.logSocketEnd(ws, "errored");
    this.fragments.delete(ws);
    try {
      await this.flush();
    } catch (e) {
      console.error("flush on socket error failed", `room=${this.getMeta("chatId") ?? "?"}`, String(e));
      this.escalateWasmPoisoning(e);
    }
  }

  /** RangeError("Invalid array buffer length") / wasm RuntimeError are the
   * signature of an exhausted loro-wasm heap (see `wasmPoisonStrikes`).
   * Strike out and `ctx.abort()` so clients redial into a fresh isolate
   * within seconds instead of hot-looping against a deaf room until
   * Cloudflare's memory-limit reset finally fires. */
  private escalateWasmPoisoning(e: unknown): void {
    if (!(e instanceof RangeError || e instanceof WebAssembly.RuntimeError)) return;
    wasmPoisonStrikes++;
    if (wasmPoisonStrikes < WASM_POISON_ABORT_AFTER) return;
    console.error(`wasm heap poisoned (${wasmPoisonStrikes} strikes); aborting isolate for a fresh heap`);
    // Best effort: if abort recycles only the DO instance (not the whole
    // isolate), at least this room's doc goes back to the wasm allocator.
    try {
      this.doc?.free();
    } catch {
      /* already poisoned beyond freeing */
    }
    this.doc = undefined;
    this.ctx.abort("loro-wasm heap exhausted; recycling isolate");
  }

  private logSocketEnd(ws: WebSocket, how: string): void {
    const state = ws.deserializeAttachment() as SocketState | null;
    console.log(
      `socket ${how}`,
      `room=${this.getMeta("chatId") ?? "?"}`,
      `device=${state?.deviceId ?? "unattributed"}`,
      `sockets=${Math.max(0, this.ctx.getWebSockets().length - 1)}`
    );
  }

  private async handleJoin(ws: WebSocket, state: SocketState, message: JoinRequest): Promise<void> {
    if (!state.workspace) {
      // Chat rooms: claim-on-first-join ownership, then owner-only forever.
      const owner = this.getMeta("owner");
      if (!owner) this.setMeta("owner", state.userId);
      else if (owner !== state.userId) {
        this.send(ws, {
          type: MessageType.JoinError,
          crdt: message.crdt,
          roomId: message.roomId,
          code: JoinErrorCode.AuthFailed,
          message: "not the room owner"
        });
        return;
      }
    }
    if (!this.getMeta("chatId") && message.roomId) this.setMeta("chatId", message.roomId);

    if (message.crdt === CrdtType.Loro) {
      const doc = await this.ensureDoc();
      if (!state.rooms.includes(message.crdt)) state.rooms.push(message.crdt);
      ws.serializeAttachment(state);
      // Wasm-bindgen objects (VersionVector here and below) free their wasm
      // memory only via GC finalizers — and V8 has no reason to collect when
      // the pressure is in WASM linear memory, not the JS heap. Under join
      // storms these leaked per-answer until the isolate hit its memory
      // limit (2026-08-04 exhaustion). Free explicitly.
      const vv = doc.version();
      try {
        this.send(ws, {
          type: MessageType.JoinResponseOk,
          crdt: message.crdt,
          roomId: message.roomId,
          permission: "write",
          version: vv.encode()
        });
      } finally {
        vv.free();
      }
      let backfill: Uint8Array;
      if (message.version.length > 0) {
        let from: VersionVector | undefined;
        try {
          from = VersionVector.decode(message.version);
          backfill = doc.export({ mode: "update", from });
        } catch {
          // Unknown/garbled client version — fall back to a full snapshot.
          backfill = doc.export({ mode: "snapshot" });
        } finally {
          from?.free();
        }
      } else {
        backfill = doc.export({ mode: "snapshot" });
      }
      if (backfill.length > 0) {
        this.sendUpdates(ws, message.crdt, message.roomId, [backfill]);
      }
      // A full join answer just exported cleanly — the wasm heap is healthy.
      // Without this reset, occasional transient RangeErrors accumulated
      // over an isolate's lifetime and the tripwire aborted HEALTHY
      // isolates, each abort causing a reconnect herd that produced more
      // transient errors (observed 2026-08-04: ~1 abort/min with all rooms
      // already trimmed small). Poisoning is CONSECUTIVE failures.
      wasmPoisonStrikes = 0;
      return;
    }

    if (message.crdt === CrdtType.LoroEphemeralStore) {
      const eph = this.ensureEph();
      if (!state.rooms.includes(message.crdt)) state.rooms.push(message.crdt);
      ws.serializeAttachment(state);
      this.send(ws, {
        type: MessageType.JoinResponseOk,
        crdt: message.crdt,
        roomId: message.roomId,
        permission: "write",
        version: new Uint8Array()
      });
      const all = eph.encodeAll();
      if (all.length > 0) this.sendUpdates(ws, message.crdt, message.roomId, [all]);
      return;
    }

    this.send(ws, {
      type: MessageType.JoinError,
      crdt: message.crdt,
      roomId: message.roomId,
      code: JoinErrorCode.Unknown,
      message: "unsupported crdt"
    });
  }

  private async handleDocUpdate(ws: WebSocket, state: SocketState, message: DocUpdate): Promise<void> {
    if (message.updates.some((u) => u.length > MAX_MESSAGE_SIZE)) {
      this.ack(ws, message, UpdateStatusCode.PayloadTooLarge);
      return;
    }
    if (!state.rooms.includes(message.crdt)) {
      // A write from a socket that never (re)joined: PermissionDenied makes
      // the client rejoin, but the condition itself is the `rooms: []`
      // broadcast-exclusion state — log who hit it.
      console.warn(
        "update from non-member socket",
        `room=${this.getMeta("chatId") ?? "?"}`,
        `device=${state.deviceId ?? "unattributed"}`,
        `crdt=${message.crdt}`
      );
      this.ack(ws, message, UpdateStatusCode.PermissionDenied);
      return;
    }
    await this.applyUpdates(ws, state, message.crdt, message.roomId, message.batchId, message.updates);
  }

  /** Shared apply path for whole and reassembled updates. */
  private async applyUpdates(
    ws: WebSocket,
    _state: SocketState,
    crdt: CrdtType,
    roomId: string,
    batchId: `0x${string}`,
    updates: Uint8Array[]
  ): Promise<void> {
    if (crdt === CrdtType.Loro) {
      const doc = await this.ensureDoc();
      try {
        for (const update of updates) if (update.length > 0) doc.import(update);
      } catch {
        // Includes imports concurrent to a shallow-snapshot start (§3.1 stale
        // peer) — the client resyncs fresh and re-submits at the app layer.
        this.ack(ws, { crdt, roomId }, UpdateStatusCode.InvalidUpdate, batchId);
        return;
      }
      this.recordLoroUpdates(updates);
      this.ack(ws, { crdt, roomId }, UpdateStatusCode.Ok, batchId);
      this.relay(ws, crdt, roomId, updates);
      return;
    }
    if (crdt === CrdtType.LoroEphemeralStore) {
      const eph = this.ensureEph();
      try {
        for (const update of updates) if (update.length > 0) eph.apply(update);
      } catch {
        this.ack(ws, { crdt, roomId }, UpdateStatusCode.InvalidUpdate, batchId);
        return;
      }
      this.ack(ws, { crdt, roomId }, UpdateStatusCode.Ok, batchId);
      this.relay(ws, crdt, roomId, updates);
      return;
    }
    this.ack(ws, { crdt, roomId }, UpdateStatusCode.Unknown, batchId);
  }

  /** Durability bookkeeping for accepted %LOR updates: buffer for the flush
   * batch, dirty the tail/backup caches, keep the daily alarm armed. */
  private recordLoroUpdates(updates: Uint8Array[]): void {
    let real = false;
    for (const update of updates) {
      if (update.length === 0) continue;
      real = true;
      this.pending.push(update);
      this.pendingBytes += update.length;
    }
    // A batch of only zero-length updates (empty POST /append body, empty
    // DocUpdate frame) recorded nothing: it must not dirty caches, arm the
    // alarm, or — critically — clear postReset, which would re-expose the
    // disaster backup to an empty-doc overwrite (round-2 review finding).
    if (!real) return;
    this.setMeta("tailDirty", "1");
    this.setMeta("backupDirty", "1");
    // Real state landed — the backup may advance past a wedge-break drop
    // (the monotonic VV gate in alarm() still has the final say).
    this.setMeta("postReset", "0");
    this.scheduleFlush();
    this.markActivity();
  }

  private handleFragmentHeader(
    ws: WebSocket,
    state: SocketState,
    message: DocUpdateFragmentHeader
  ): void {
    if (!state.rooms.includes(message.crdt)) {
      this.ack(ws, message, UpdateStatusCode.PermissionDenied, message.batchId);
      return;
    }
    let batches = this.fragments.get(ws);
    if (!batches) {
      batches = new Map();
      this.fragments.set(ws, batches);
    }
    batches.set(message.batchId, {
      parts: Array.from({ length: message.fragmentCount }, () => new Uint8Array()),
      received: 0,
      totalSize: message.totalSizeBytes,
      header: message
    });
  }

  private async handleFragment(
    ws: WebSocket,
    state: SocketState,
    message: { crdt: CrdtType; roomId: string; batchId: `0x${string}`; index: number; fragment: Uint8Array }
  ): Promise<void> {
    const batch = this.fragments.get(ws)?.get(message.batchId);
    if (!batch) {
      // Unknown batch (e.g. header lost to hibernation) — tell the sender to
      // retry the whole batch.
      this.ack(ws, message, UpdateStatusCode.FragmentTimeout, message.batchId);
      return;
    }
    batch.parts[message.index] = message.fragment;
    batch.received++;
    if (batch.received < batch.parts.length) return;
    this.fragments.get(ws)?.delete(message.batchId);
    const total = new Uint8Array(batch.totalSize);
    let off = 0;
    for (const part of batch.parts) {
      total.set(part, off);
      off += part.length;
    }
    await this.applyUpdates(ws, state, message.crdt, message.roomId, message.batchId, [total]);
  }

  // ── doc/ephemeral materialization ────────────────────────────────────────

  private async ensureDoc(): Promise<LoroDoc> {
    this.touchDoc();
    if (this.doc) return this.doc;
    // AUTOMATED WEDGE BREAK: a cold replay that exceeds the DO CPU limit kills
    // the invocation before `replayAttempts` is cleared below — and every
    // reconnecting client cold-starts the room into the same death, forever
    // (the manual escape is POST /reset-log). Count consecutive replay deaths;
    // past the limit, drop the log+snapshot exactly like /reset-log does.
    // Recovery is by design lossless-enough: every engine holds the full doc
    // locally and re-uploads whatever the server lacks on its next join.
    const attempts = Number(this.getMeta("replayAttempts") ?? "0");
    if (attempts >= REPLAY_CRASH_LIMIT) {
      this.dropLog();
      // Boot every attached socket, exactly like POST /reset-log. The
      // automated wedge break used to swap the doc out from UNDER live
      // sessions: their next writes carried deps the emptied doc lacks,
      // imports failed, clients burned their capped invalid-rejoin resyncs
      // and then sat LATCHED — rows frozen on a healthy-looking socket
      // (2026-08-04: work-metal's workspace status never updated again
      // after the 20:16Z wedge-break while its chat rooms streamed fine).
      // A close → redial → empty-VV join re-uploads full state instead.
      for (const sock of this.ctx.getWebSockets()) {
        try {
          sock.close(4410, "room reset");
        } catch {
          /* already gone */
        }
      }
    }
    this.setMeta("replayAttempts", String(attempts + 1));
    // INCIDENT (2026-07-30): a CPU-limit kill ROLLS BACK the event's
    // uncommitted storage writes — so the increment above died with every
    // crash, the count never reached the limit, and the wedge break never
    // fired on the exact failure it was built for. The ws3 workspace room
    // died 7 times in two minutes and then sat wedged for 3+ hours until a
    // manual engine restart. sync() makes the count durable BEFORE the risky
    // replay below, so consecutive deaths are actually counted; clients
    // redialing on their join deadline (crates/sync/src/room.rs) supply the
    // attempts, and the room self-heals within REPLAY_CRASH_LIMIT dials.
    await this.ctx.storage.sync();
    const started = Date.now();
    const doc = new LoroDoc();
    const snapshot = this.blobs.get("snapshot");
    if (snapshot && snapshot.length > 0) doc.import(snapshot);
    let rows = 0;
    for (const row of this.ctx.storage.sql.exec("SELECT bytes FROM updates ORDER BY seq")) {
      rows++;
      try {
        doc.import(new Uint8Array(row.bytes as ArrayBuffer));
      } catch {
        // A poisoned row cannot be applied; skip it rather than brick the room.
      }
    }
    for (const update of this.pending) {
      try {
        doc.import(update);
      } catch {
        /* same */
      }
    }
    this.setMeta("replayAttempts", "0");
    // Scope the crash budget to the replay ALONE: without this second sync, a
    // CPU kill later in the same event (a backfill export for a fresh client,
    // the alarm's shallow trim) would roll back this reset while the synced
    // increment above survives — three such deaths would wedge-break a room
    // whose replay is perfectly healthy (adversarial-review finding). One
    // extra sync, cold path only. Deliberate consequence: a deterministic
    // POST-replay death (a doc so big its snapshot export blows the CPU
    // limit) gets no automatic wedge-break — destroying state over an export
    // problem is worse than looping loudly. That class is watched via
    // lastReplayMs creep and escaped manually with POST /reset-log.
    await this.ctx.storage.sync();
    // Cold-start telemetry (Workers Logs + /stats): the replay cost is the
    // wedge risk — watch lastReplayMs trend toward the CPU limit to catch the
    // next 2026-07-30 while it is still a statistic, not an incident.
    const replayMs = Date.now() - started;
    this.setMeta("lastReplayMs", String(replayMs));
    this.setMeta("lastReplayRows", String(rows));
    console.log(
      `cold replay: ${replayMs}ms, ${rows} rows, snapshot ${snapshot?.length ?? 0}B, attempt ${attempts + 1}`,
      `room=${this.getMeta("chatId") ?? "?"}`
    );
    this.doc = doc;
    // Record a frontier checkpoint on cold start too: the alarm only records
    // while WRITES keep it armed, so an idle room never aged into trim
    // eligibility — it could never shrink, ever. One checkpoint a day max.
    const checkpoints = JSON.parse(this.getMeta("checkpoints") ?? "[]") as FrontierCheckpoint[];
    const newest = checkpoints[checkpoints.length - 1];
    if (!newest || Date.now() - newest.at >= DAY_MS) {
      checkpoints.push({
        at: Date.now(),
        frontiers: doc.frontiers().map((f) => ({ peer: String(f.peer), counter: f.counter }))
      });
      while (checkpoints.length > MAX_CHECKPOINTS) checkpoints.shift();
      this.setMeta("checkpoints", JSON.stringify(checkpoints));
    }
    // Trim on cold materialization too: fold and alarm both ride WRITES, so
    // an idle-but-watched room NEVER trimmed — yet every isolate restart
    // re-materializes its full history into the shared wasm heap (the
    // 2026-08-04 exhaustion recurred post-fix on exactly those rooms). The
    // one-off export cost here permanently shrinks the room.
    if (await this.trimHistoryIfDue(doc, Date.now())) {
      console.log(`history trimmed on cold start room=${this.getMeta("chatId") ?? "?"}`);
    }
    return this.doc;
  }

  /** Idle-doc release (see DOC_IDLE_RELEASE_MS): a debounced timer frees the
   * materialized doc after a quiet minute. Timer only exists while traffic
   * keeps the DO awake — same hibernation discipline as the flush debounce.
   * Buffered `pending` updates survive a release: cold replay re-imports
   * them (see ensureDoc). */
  private touchDoc(): void {
    this.lastDocUse = Date.now();
    if (this.docIdleTimer) return;
    this.docIdleTimer = setTimeout(() => this.releaseIdleDoc(), DOC_IDLE_RELEASE_MS + 500);
  }

  private releaseIdleDoc(): void {
    this.docIdleTimer = undefined;
    if (!this.doc) return;
    const idle = Date.now() - this.lastDocUse;
    if (idle < DOC_IDLE_RELEASE_MS) {
      this.docIdleTimer = setTimeout(
        () => this.releaseIdleDoc(),
        Math.max(DOC_IDLE_RELEASE_MS - idle, 1_000) + 500
      );
      return;
    }
    this.doc.free();
    this.doc = undefined;
  }

  /** Drop the persisted update log + snapshot (the /reset-log storage clear):
   * the next materialization starts empty and engines re-upload state on
   * rejoin. Preserves owner/chatId meta. */
  private dropLog(): void {
    this.ctx.storage.sql.exec("DELETE FROM updates");
    this.blobs.delete("snapshot");
    this.setMeta("updateBytes", "0");
    this.setMeta("checkpoints", "[]");
    this.setMeta("lastTrimAt", "");
    this.pending = [];
    this.pendingBytes = 0;
    // Until an engine re-uploads real state, anything materialized from here
    // is empty — postReset gates the nightly R2 put so the DISASTER backup
    // cannot be overwritten by the emptied doc. Without it, the durable crash
    // counter let alarm auto-retries complete a wedge break with ZERO clients
    // connected and then back up the empty doc in the same invocation,
    // destroying the one copy that exists for the engine-never-returns case
    // (adversarial-review finding). Cleared by recordLoroUpdates.
    this.setMeta("postReset", "1");
  }

  private ensureEph(): EphemeralStore {
    if (!this.eph) this.eph = new EphemeralStore(30_000);
    return this.eph;
  }

  // ── durability: flush, compaction, backups ───────────────────────────────

  private scheduleFlush(): void {
    if (this.flushTimer) return;
    this.flushTimer = setTimeout(() => {
      this.flushTimer = undefined;
      void this.flush();
    }, DO_FLUSH_MS);
  }

  private async flush(): Promise<void> {
    if (this.flushTimer) {
      clearTimeout(this.flushTimer);
      this.flushTimer = undefined;
    }
    if (this.pending.length === 0) return;
    const now = Date.now();
    for (const update of this.pending) {
      this.ctx.storage.sql.exec(
        "INSERT INTO updates (bytes, received_at) VALUES (?, ?)",
        update.buffer.slice(update.byteOffset, update.byteOffset + update.byteLength),
        now
      );
    }
    const logBytes = Number(this.getMeta("updateBytes") ?? "0") + this.pendingBytes;
    this.setMeta("updateBytes", String(logBytes));
    this.pending = [];
    this.pendingBytes = 0;
    // Fold on EITHER budget: bytes bounds one huge update, rows bounds many
    // tiny ones — a cold `ensureDoc` replay pays per-import overhead per row,
    // so a high row count is as expensive as a high byte count (see
    // COMPACT_LOG_ROWS). COUNT(*) is a cheap indexed read, once per flush.
    if (logBytes > COMPACT_LOG_BYTES) {
      await this.foldLog();
      return;
    }
    const rows = [...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM updates")][0]?.n as
      | number
      | undefined;
    if ((rows ?? 0) > COMPACT_LOG_ROWS) await this.foldLog();
  }

  /** LOG FOLD: snapshot re-export + clear the update log. Prefers a shallow
   * trim when one is due — waiting for the DAILY alarm meant a heap-pressed
   * colo (2026-08-04 wasm exhaustion) kept thrash-cycling for up to a day
   * after the retention fix deployed; a high-churn room folds every ~400
   * rows, so trimming here converges in minutes instead. Falls back to the
   * lossless full snapshot when no trim is due (or the trim export fails). */
  private async foldLog(): Promise<void> {
    const doc = await this.ensureDoc();
    if (await this.trimHistoryIfDue(doc, Date.now())) return;
    this.blobs.put("snapshot", doc.export({ mode: "snapshot" }));
    this.ctx.storage.sql.exec("DELETE FROM updates");
    this.setMeta("updateBytes", "0");
  }

  /** HISTORY TRIM (§3.1): shallow snapshot at the newest recorded frontier
   * checkpoint older than RETAIN_DAYS — history before it is discarded
   * permanently, state fully preserved. Returns whether a trim landed (the
   * snapshot + log + materialized doc were all replaced — the passed `doc`
   * is CONSUMED: its wasm memory is freed, callers must switch to
   * `this.doc`). Best-effort: any export failure leaves the room to the
   * caller's lossless fold. */
  private async trimHistoryIfDue(doc: LoroDoc, now: number): Promise<boolean> {
    const checkpoints = JSON.parse(this.getMeta("checkpoints") ?? "[]") as FrontierCheckpoint[];
    const cutoff = checkpoints.filter((c) => now - c.at >= RETAIN_MS).pop();
    let frontiers: { peer: `${number}`; counter: number }[];
    // lastTrimAt alone gates the cutoff trim: the trim is durable (sync()
    // below), and requiring doc.isShallow() re-fired it on EVERY cold start
    // once a log fold re-exported the once-shallow doc as a regular
    // snapshot (isShallow reads false after rematerializing from it) —
    // observed as the same rooms "trimming" every few minutes all evening.
    if (cutoff && this.getMeta("lastTrimAt") !== String(cutoff.at)) {
      frontiers = cutoff.frontiers.map((f) => ({ peer: f.peer as `${number}`, counter: f.counter }));
    } else if (
      (this.blobs.get("snapshot")?.length ?? 0) + Number(this.getMeta("updateBytes") ?? "0") >
      TRIM_FORCE_BYTES
    ) {
      // Snapshot AND log bytes: after a wedge-break reset the re-uploaded
      // full histories live as LOG ROWS against an empty snapshot (observed
      // live: 0B snapshot + 119 rows replaying for 7 SECONDS), so a
      // snapshot-only gate never fired while every cold start ballooned the
      // heap with the same megabytes.
      // No aged checkpoint but the full history is already a heap hazard:
      // trim at the current frontier (see TRIM_FORCE_BYTES).
      frontiers = doc.frontiers().map((f) => ({ peer: String(f.peer) as `${number}`, counter: f.counter }));
    } else {
      return false;
    }
    try {
      const shallow = doc.export({
        mode: "shallow-snapshot",
        frontiers
      });
      this.blobs.put("snapshot", shallow);
      this.ctx.storage.sql.exec("DELETE FROM updates");
      this.setMeta("updateBytes", "0");
      this.setMeta("lastTrimAt", String(cutoff?.at ?? now));
      // Make the trim durable NOW: a later kill in the same event (a join's
      // backfill export on a pressed isolate — observed live 2026-08-04)
      // rolls back uncommitted storage writes, silently resurrecting the
      // full-history snapshot the trim just replaced.
      await this.ctx.storage.sync();
      const fresh = new LoroDoc();
      fresh.import(shallow);
      this.doc = fresh;
      // Free the replaced full-history doc NOW — waiting on GC finalizers
      // leaks it into the shared wasm heap exactly when trimming was
      // supposed to relieve it (see handleJoin).
      if (doc !== fresh) doc.free();
      return true;
    } catch {
      return false;
    }
  }

  /** Daily alarm: frontier checkpoint, history trim, R2 backup. */
  async alarm(): Promise<void> {
    await this.flush();
    if (this.getMeta("backupDirty") !== "1") return; // idle: stop the chain
    const doc = await this.ensureDoc();
    const now = Date.now();

    // 1. Record today's frontier checkpoint.
    const checkpoints = JSON.parse(this.getMeta("checkpoints") ?? "[]") as FrontierCheckpoint[];
    checkpoints.push({
      at: now,
      frontiers: doc.frontiers().map((f) => ({ peer: String(f.peer), counter: f.counter }))
    });
    while (checkpoints.length > MAX_CHECKPOINTS) checkpoints.shift();

    // 2. HISTORY TRIM — must see today's checkpoint list, so persist first.
    //    (Also fires from foldLog, which is what usually gets there first on
    //    a high-churn room.)
    this.setMeta("checkpoints", JSON.stringify(checkpoints));
    await this.trimHistoryIfDue(doc, now);

    // 3. Nightly R2 backup (§3.3) — full current snapshot, disaster hatch.
    // Two guards (round-2 review): postReset pauses the put between a
    // wedge-break drop and the first re-uploaded state, and the put is
    // MONOTONIC — the new snapshot must version-include the previously
    // backed-up one, so even a post-drop doc that took a few fresh writes
    // (clearing postReset) can never replace the last good copy with a
    // hollow one. CRDT merge guarantees a genuinely recovered doc includes
    // the old VV, at which point the put resumes; until then backupDirty
    // stays set and the alarm chain keeps trying.
    const chatId = this.getMeta("chatId");
    if (chatId && this.getMeta("postReset") !== "1") {
      const current = this.doc ?? doc;
      const prevVV = this.getMeta("backupVV");
      let advances = true;
      if (prevVV) {
        let prev: VersionVector | undefined;
        let cur: VersionVector | undefined;
        try {
          prev = VersionVector.decode(Uint8Array.from(atob(prevVV), (c) => c.charCodeAt(0)));
          cur = current.version();
          const cmp = cur.compare(prev);
          advances = cmp !== undefined && cmp >= 0;
        } catch {
          /* unreadable meta: allow the put and rewrite it below */
        } finally {
          // Explicit frees: see handleJoin — GC finalizers don't run under
          // wasm-side memory pressure.
          prev?.free();
          cur?.free();
        }
      }
      if (advances) {
        const snapshot = current.export({ mode: "snapshot" });
        await this.env.BLOBS.put(`backup/${chatId}/latest.loro`, snapshot);
        const vv = current.version();
        try {
          this.setMeta("backupVV", btoa(String.fromCharCode(...vv.encode())));
        } finally {
          vv.free();
        }
        this.setMeta("backupDirty", "0");
      }
    }
    // Re-arm only while there is a reason to wake again; markActivity re-arms
    // on the next write otherwise.
  }

  /** Arm the daily alarm if none is scheduled (called on every write). */
  private markActivity(): void {
    void this.ctx.storage.getAlarm().then((existing) => {
      if (existing === null) void this.ctx.storage.setAlarm(Date.now() + DAY_MS);
    });
  }

  private async currentTail(): Promise<unknown> {
    await this.flush();
    if (this.getMeta("tailDirty") !== "1") {
      const cached = getJsonBlob<unknown>(this.blobs, "tail");
      if (cached !== undefined) return cached;
    }
    const doc = await this.ensureDoc();
    const tail = materializeTail(doc, Date.now());
    putJsonBlob(this.blobs, "tail", tail);
    this.setMeta("tailDirty", "0");
    return tail;
  }

  // ── wire helpers ─────────────────────────────────────────────────────────

  /** Returns false when the frame could not be delivered (socket gone /
   * runtime refused the send). Encode failures throw out instead — they are
   * OUR bug, never the peer's, and must not be mistaken for a deaf socket. */
  private send(ws: WebSocket, message: ProtocolMessage): boolean {
    const bytes = encode(message);
    try {
      ws.send(bytes);
      return true;
    } catch {
      /* socket already gone; hibernation API cleans it up */
      return false;
    }
  }

  /** Send updates, fragmenting any single update above FRAGMENT_BYTES and
   * chunking small ones so no encoded frame approaches the loro-protocol
   * 256KB message cap (envelope overhead included). Returns false if any
   * frame failed to deliver. */
  private sendUpdates(ws: WebSocket, crdt: CrdtType, roomId: string, updates: Uint8Array[]): boolean {
    let ok = true;
    let batch: Uint8Array[] = [];
    let batchBytes = 0;
    const flushBatch = () => {
      if (batch.length === 0) return;
      ok =
        this.send(ws, {
          type: MessageType.DocUpdate,
          crdt,
          roomId,
          updates: batch,
          batchId: this.newBatchId()
        }) && ok;
      batch = [];
      batchBytes = 0;
    };
    for (const u of updates) {
      if (u.length > FRAGMENT_BYTES) continue;
      if (batchBytes + u.length > FRAGMENT_BYTES) flushBatch();
      batch.push(u);
      batchBytes += u.length;
    }
    flushBatch();
    for (const update of updates) {
      if (update.length <= FRAGMENT_BYTES) continue;
      const batchId = this.newBatchId();
      const fragmentCount = Math.ceil(update.length / FRAGMENT_BYTES);
      ok =
        this.send(ws, {
          type: MessageType.DocUpdateFragmentHeader,
          crdt,
          roomId,
          batchId,
          fragmentCount,
          totalSizeBytes: update.length
        }) && ok;
      for (let i = 0; i < fragmentCount; i++) {
        ok =
          this.send(ws, {
            type: MessageType.DocUpdateFragment,
            crdt,
            roomId,
            batchId,
            index: i,
            fragment: update.subarray(
              i * FRAGMENT_BYTES,
              Math.min((i + 1) * FRAGMENT_BYTES, update.length)
            )
          }) && ok;
      }
    }
    return ok;
  }

  /** Relay accepted updates to every other member socket via sendUpdates —
   * NOT a single pre-encoded frame. broadcast() used to encode the batch
   * once, so a reassembled >256KB client push (a device re-uploading its
   * full workspace history after a server reset) blew the loro-protocol
   * message cap and NEVER reached peers live; they only converged via a
   * later rejoin backfill (2026-08-04, the last silent-staleness path). */
  private relay(from: WebSocket, crdt: CrdtType, roomId: string, updates: Uint8Array[]): void {
    for (const ws of this.ctx.getWebSockets()) {
      if (ws === from) continue;
      const state = ws.deserializeAttachment() as SocketState | null;
      if (!state?.rooms.includes(crdt)) continue;
      if (!this.sendUpdates(ws, crdt, roomId, updates)) {
        // A member socket we cannot send to is a DEAF PEER, not a skippable
        // one: swallowing the failure left it looking alive (runtime
        // auto-pongs, accepted writes) while it silently missed every
        // broadcast until an app restart (2026-08-04 incident). Close it so
        // the client's session ends and its redial + VV backfill heal the
        // gap within seconds.
        console.warn(
          "relay send failed; closing socket",
          `room=${this.getMeta("chatId") ?? "?"}`,
          `device=${state.deviceId ?? "unattributed"}`
        );
        try {
          ws.close(1011, "broadcast delivery failed");
        } catch {
          /* already gone */
        }
      }
    }
  }

  private ack(
    ws: WebSocket,
    message: { crdt: CrdtType; roomId: string; batchId?: `0x${string}` },
    status: UpdateStatusCode,
    refId?: `0x${string}`
  ): void {
    this.send(ws, {
      type: MessageType.Ack,
      crdt: message.crdt,
      roomId: message.roomId,
      refId: refId ?? message.batchId ?? "0x0000000000000000",
      status
    });
  }

  private newBatchId(): `0x${string}` {
    const bytes = new Uint8Array(8);
    crypto.getRandomValues(bytes);
    return bytesToHex(bytes);
  }
}

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });

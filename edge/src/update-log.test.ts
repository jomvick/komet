import { describe, expect, it } from "vitest";
import { CHUNK_BYTES } from "./blobs";
import { appendUpdateRow, ensureUpdateLog, readUpdateRows } from "./update-log";

/** Minimal SqlStorage fake covering exactly the statements update-log.ts
 * issues, including the ~2MB row cap that motivated chunking. */
const ROW_CAP = 2 * 1024 * 1024;

class FakeSql {
  rows: { seq: number; bytes: ArrayBuffer; received_at: number; cont: number }[] = [];
  private seq = 0;
  private hasCont;

  constructor({ legacy = false } = {}) {
    // legacy=true simulates a table created before the `cont` column existed.
    this.hasCont = !legacy;
  }

  exec(query: string, ...params: unknown[]): Iterable<Record<string, unknown>> {
    if (query.startsWith("CREATE TABLE")) return [];
    if (query.startsWith("ALTER TABLE")) {
      if (this.hasCont) throw new Error("duplicate column name: cont");
      this.hasCont = true;
      for (const row of this.rows) row.cont = 0;
      return [];
    }
    if (query.startsWith("INSERT INTO updates")) {
      const bytes = params[0] as ArrayBuffer;
      if (bytes.byteLength > ROW_CAP) throw new Error("string or blob too big: SQLITE_TOOBIG");
      if (!this.hasCont && query.includes("cont")) throw new Error("table updates has no column named cont");
      this.rows.push({
        seq: ++this.seq,
        bytes,
        received_at: params[1] as number,
        cont: (params[2] as number) ?? 0
      });
      return [];
    }
    if (query.startsWith("SELECT bytes, cont FROM updates")) {
      return this.rows.map((r) => ({ bytes: r.bytes, cont: r.cont }));
    }
    throw new Error(`FakeSql: unhandled query: ${query}`);
  }
}

const asSql = (fake: FakeSql) => fake as unknown as SqlStorage;

const bytesOf = (len: number, seed: number): Uint8Array => {
  const out = new Uint8Array(len);
  for (let i = 0; i < len; i++) out[i] = (seed + i * 31) & 0xff;
  return out;
};

const readAll = (fake: FakeSql): Uint8Array[] => [...readUpdateRows(asSql(fake))];

/** Byte-identical check that stays fast on multi-MB arrays (vitest's deep
 * equality walks element-by-element and times out). */
const sameBytes = (a: Uint8Array | undefined, b: Uint8Array): boolean =>
  a !== undefined && Buffer.from(a.buffer, a.byteOffset, a.byteLength).equals(Buffer.from(b.buffer, b.byteOffset, b.byteLength));

describe("update log chunking", () => {
  it("stores a small update as a single non-continuation row", () => {
    const sql = new FakeSql();
    const update = bytesOf(1000, 1);
    appendUpdateRow(asSql(sql), update, 42);
    expect(sql.rows.length).toBe(1);
    expect(sql.rows[0]!.cont).toBe(0);
    expect(readAll(sql)).toEqual([update]);
  });

  it("splits an update above the row cap and reassembles it byte-identically", () => {
    const sql = new FakeSql();
    const whale = bytesOf(3 * CHUNK_BYTES + 123, 7); // >2 rows, ragged tail
    appendUpdateRow(asSql(sql), whale, 42);
    expect(sql.rows.length).toBe(4);
    expect(sql.rows.map((r) => r.cont)).toEqual([0, 1, 1, 1]);
    // The bug this guards: every row must fit under the SQL value cap.
    for (const row of sql.rows) expect(row.bytes.byteLength).toBeLessThanOrEqual(ROW_CAP);
    const back = readAll(sql);
    expect(back.length).toBe(1);
    expect(sameBytes(back[0], whale)).toBe(true);
  });

  it("keeps interleaved small and chunked updates in order", () => {
    const sql = new FakeSql();
    const a = bytesOf(10, 1);
    const b = bytesOf(CHUNK_BYTES + 5, 2);
    const c = bytesOf(20, 3);
    for (const u of [a, b, c]) appendUpdateRow(asSql(sql), u, 42);
    const back = readAll(sql);
    expect(back.length).toBe(3);
    expect(sameBytes(back[0], a)).toBe(true);
    expect(sameBytes(back[1], b)).toBe(true);
    expect(sameBytes(back[2], c)).toBe(true);
  });

  it("respects a subarray view's offset when slicing chunks", () => {
    const sql = new FakeSql();
    const backing = bytesOf(CHUNK_BYTES + 200, 9);
    const view = backing.subarray(100, CHUNK_BYTES + 150);
    appendUpdateRow(asSql(sql), view, 42);
    const back = readAll(sql);
    expect(back.length).toBe(1);
    expect(sameBytes(back[0], view)).toBe(true);
  });

  it("migrates a legacy table and reads its rows as one update each", () => {
    const sql = new FakeSql({ legacy: true });
    // Legacy rows written before the cont column existed.
    sql.rows.push(
      { seq: 1, bytes: bytesOf(10, 1).buffer as ArrayBuffer, received_at: 1, cont: 0 },
      { seq: 2, bytes: bytesOf(11, 2).buffer as ArrayBuffer, received_at: 2, cont: 0 }
    );
    ensureUpdateLog(asSql(sql));
    expect(readAll(sql).length).toBe(2);
    // And post-migration appends work, including chunked ones.
    appendUpdateRow(asSql(sql), bytesOf(CHUNK_BYTES + 1, 3), 3);
    expect(readAll(sql).length).toBe(3);
  });

  it("ensureUpdateLog is idempotent on an already-migrated table", () => {
    const sql = new FakeSql();
    ensureUpdateLog(asSql(sql));
    ensureUpdateLog(asSql(sql));
    appendUpdateRow(asSql(sql), bytesOf(10, 1), 1);
    expect(readAll(sql).length).toBe(1);
  });
});

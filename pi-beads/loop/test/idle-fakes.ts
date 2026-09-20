/**
 * Shared fakes for the idle surface, extracted from `test/idle.test.ts` so a
 * second test file can boot a *real* `createIdleMode` without a TTY.
 *
 * `FakeTerminal` implements pi-tui's real `Terminal` contract and feeds input
 * through pi's own `StdinBuffer`, so a submitted line reaches the editor the
 * way a real keystroke would — same split, same Enter handling. Faking less
 * than that quietly changes what Enter does.
 */

import { StdinBuffer } from "@earendil-works/pi-tui";

import type { IdleSignalAdapter } from "../src/idle.ts";

/** A `Terminal` we can drive and inspect, implementing pi-tui's real contract. */
export class FakeTerminal {
  writes: string[] = [];
  /** Ordered log of terminal method calls, for teardown-order assertions. */
  calls: string[] = [];
  /** How many writes had happened by the time stop() was called. */
  writesAtStop = -1;
  columnsValue: number;
  rowsValue: number;
  private inputHandler?: (data: string) => void;
  private resizeHandler?: () => void;
  private _kittyActive: boolean;

  constructor(columns = 80, rows = 24, kittyActive = false) {
    this.columnsValue = columns;
    this.rowsValue = rows;
    this._kittyActive = kittyActive;
  }

  get kittyProtocolActive(): boolean {
    return this._kittyActive;
  }

  start(onInput: (data: string) => void, onResize: () => void): void {
    this.calls.push("start");
    this.inputHandler = onInput;
    this.resizeHandler = onResize;
  }

  stop(): void {
    this.calls.push("stop");
    this.writesAtStop = this.writes.length;
  }

  async drainInput(_maxMs?: number, _idleMs?: number): Promise<void> {
    this.calls.push("drainInput");
  }

  write(data: string): void {
    this.writes.push(data);
  }

  get columns(): number {
    return this.columnsValue;
  }

  get rows(): number {
    return this.rowsValue;
  }

  moveBy(_lines: number): void {
    this.calls.push("moveBy");
  }

  hideCursor(): void {
    this.calls.push("hideCursor");
  }

  showCursor(): void {
    this.calls.push("showCursor");
  }

  clearLine(): void {
    this.calls.push("clearLine");
  }

  clearFromCursor(): void {
    this.calls.push("clearFromCursor");
  }

  clearScreen(): void {
    this.calls.push("clearScreen");
  }

  setTitle(title: string): void {
    this.calls.push(`setTitle:${title}`);
  }

  setProgress(_active: boolean): void {
    this.calls.push("setProgress");
  }

  /**
   * Feed input the way a real terminal does: `ProcessTerminal` runs everything
   * through pi-tui's `StdinBuffer`, so the editor receives one complete key
   * sequence per call — `"hello\r"` arrives as `h`,`e`,`l`,`l`,`o`,`\r`, not as
   * one string. Faking the terminal without that split silently changes what the
   * editor does with Enter, so we reuse pi's own splitter.
   */
  input(data: string): void {
    if (this.inputHandler === undefined) throw new Error("terminal not started");
    const buffer = new StdinBuffer();
    const sequences: string[] = [];
    buffer.on("data", (sequence) => sequences.push(sequence));
    buffer.process(data);
    for (const sequence of sequences) this.inputHandler(sequence);
  }

  resize(columns: number, rows: number): void {
    this.columnsValue = columns;
    this.rowsValue = rows;
    this.calls.push("resize");
    this.resizeHandler?.();
  }

  get output(): string {
    return this.writes.join("");
  }

  /** How many writes have happened so far — a bookmark for {@link outputSince}. */
  get writeCount(): number {
    return this.writes.length;
  }

  /**
   * Only what was written after a bookmark. pi renders diffs, so "what the user
   * can see now" is the latest frame, not everything ever written; tests that
   * assert something disappeared must look at the delta, not the whole buffer.
   */
  outputSince(from: number): string {
    return this.writes.slice(from).join("");
  }

  count(call: string): number {
    return this.calls.filter((c) => c === call).length;
  }

  indexOf(call: string): number {
    return this.calls.indexOf(call);
  }
}

/** Signal source we can fire without killing the test process. */
export class FakeSignals implements IdleSignalAdapter {
  private handlers = new Map<string, Set<() => void>>();

  on(signal: string, handler: () => void): () => void {
    let set = this.handlers.get(signal);
    if (set === undefined) {
      set = new Set();
      this.handlers.set(signal, set);
    }
    set.add(handler);
    return () => {
      this.handlers.get(signal)?.delete(handler);
    };
  }

  emit(signal: string): void {
    for (const handler of [...(this.handlers.get(signal) ?? [])]) handler();
  }

  listening(signal: string): number {
    return this.handlers.get(signal)?.size ?? 0;
  }
}

/** Manually advanced clock so the double-press window is deterministic. */
export function fakeClock(start = 1_000): {
  now: () => number;
  advance(ms: number): void;
} {
  let current = start;
  return {
    now: () => current,
    advance(ms: number) {
      current += ms;
    },
  };
}

/**
 * Let the queued work actually run. pi-tui schedules a render as
 * `process.nextTick(...)` followed by a `setTimeout` of a frame, so real time
 * has to pass — `setImmediate` alone can observe a frame that has not painted
 * yet, which would make these assertions timing-luck rather than true.
 */
export async function settle(rounds = 3): Promise<void> {
  for (let i = 0; i < rounds; i += 1) {
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
}

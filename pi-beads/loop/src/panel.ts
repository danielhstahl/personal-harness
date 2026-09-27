/**
 * The panel contract — how a strip of the screen plugs into the work surface.
 *
 * Until now the presenter knew its HUDs by name. `WorkPresenterOptions` carried
 * `monitor`, `monitorPlacement`, `monitorLines`, `kanban`, `kanbanMode`,
 * `kanbanLines`, `kanbanPlacement`: seven options describing two particular
 * strips, filled from two config sections by the composition root. A third strip
 * meant changing the presenter, the root, the config and the environment reader
 * together, which is the signature of a missing abstraction rather than of a
 * missing feature.
 *
 * So: a panel is a thing that can draw itself into a width, says where it wants
 * to sit and how many rows it may take, and can tell a surface when its content
 * changed. That is the whole contract, and it is exactly what
 * `MonitorComponent` and `KanbanComponent` already were — which is the point.
 * Nothing here invents a capability; it names the one two modules already
 * agreed on without either saying the other's name.
 *
 * What a panel is *not*:
 *
 * - **Not a decider.** A panel shows what its own source already holds. It gets
 *   a width, a theme and the id of the item the surface is working, and it
 *   returns lines. It takes no board handle into the presenter, no session and
 *   no git, which is rule 1 of the presenter surviving the refactor.
 * - **Not the owner of its source's lifecycle.** The composition root builds
 *   the pollers, starts them and stops them, because the same monitor is read by
 *   the work surface *and* the idle prompt. {@link Panel.dispose} releases what
 *   the *panel* allocated; it never stops a shared poller. A strip that owns its
 *   own lifecycle can build a panel that stops it — the contract permits that,
 *   it does not require it.
 * - **Not required to be colourful, live, or even present.** A strip that is
 *   switched off is a panel that renders no lines
 *   ({@link createNullPanel}), so a surface never has to ask whether a HUD
 *   *exists* — only what it drew.
 *
 * Colour: {@link PanelTheme} is structurally the presenter's
 * `PresenterTheme` (and pi's `Theme` under it). Both strips happen to carry
 * their own theme already — a `MonitorTheme` baked into the source — so they
 * ignore the argument. A strip that wants the live surface theme, wanting it
 * *at render time* so a theme switch repaints it, takes it here.
 *
 * **No pi import.** ADR-010 counts the files allowed to name a pi package
 * symbol, and a contract module that plugs strips into a renderer is not one of
 * them. So {@link PanelRenderable} states the two members of pi-tui's
 * `Component` that drawing actually needs, structurally: a real `Component`
 * satisfies it without this file saying so, and the boundary stays exactly
 * where that ADR put it — six files name pi, and this is not a seventh.
 */

// ── the contract ───────────────────────────────────────────────────────────

/**
 * Where a panel sits. The presenter lays its content column out around one
 * rule:
 *
 * - `"band"` — in the fixed chrome directly above the footer, which is always
 *   on screen. This is the default for a reason spelled out in
 *   `WorkPresenterOptions`: `TuiMainScreen` keeps the *bottom* `rows` lines of
 *   the content column visible, so anything pinned at the true top scrolls into
 *   scrollback as soon as the transcript outgrows the terminal — which it does,
 *   every unit, early. Above the footer a panel is always visible, covers no
 *   content, and never forces the full-screen redraw the diff renderer falls
 *   back to when something above the viewport changes.
 * - `"top"` — above the transcript, for a surface whose content is known to
 *   stay short, or for a run whose operator wants it read before the text.
 *
 * A panel keeps its placement in every combination: a knob that made a strip
 * *vanish* rather than move it would be a knob nobody could debug.
 */
export type PanelPlacement = "band" | "top";

/**
 * What a {@link Panel.render} may hand back: plain lines, or something that
 * draws itself.
 *
 * Both forms are accepted because both existing strips were already one of each
 * kind of drawable — `KanbanComponent` and `MonitorComponent` are pi-tui
 * components — and forcing them through a lines-only adapter would mean
 * rewriting the layout code that works.
 */
export type PanelRender = readonly string[] | PanelRenderable;

/**
 * The drawable half of pi-tui's `Component`, declared here rather than
 * imported. Deliberate: ADR-010 keeps pi named in six files, and this contract
 * module is not one of them. `Component` is assignable to this shape, and a
 * value of this shape that carries exactly these two members is assignable to
 * `Component`, so the two ends meet without this module naming the middle.
 */
export interface PanelRenderable {
  render(width: number): string[];
  invalidate(): void;
}

/**
 * The least a panel can ask of the surface's colours: two styled-string
 * factories, role-in / string-out. Deliberately wider than the presenter's own
 * closed role list, so `PresenterTheme` — and pi's live `Theme` behind it —
 * satisfies it without either module importing the other.
 */
export interface PanelTheme {
  color(role: string, text: string): string;
  bold(text: string): string;
}

/**
 * One pluggable strip of the work surface.
 *
 * `id` is for the operator and for tests: two panels must not be
 * indistinguishable in a captured frame. `placement` and `lines` are the whole
 * layout vocabulary the presenter needs — beyond that it knows nothing about
 * boards, servers, or anything else a panel might be.
 */
export interface Panel {
  /** What this strip is called. Stable, lowercase, unique per surface. */
  readonly id: string;
  /** Which side of the transcript it goes on. See {@link PanelPlacement}. */
  readonly placement: PanelPlacement;
  /** Row budget. A panel never draws more than this many lines. */
  readonly lines: number;
  /**
   * Draw at `width`. Plain lines or a `Component` — both forms are accepted
   * because both existing strips were already one of each kind of drawable, and
   * forcing the second one through a lines-only adapter would mean rewriting the
   * layout code that is working.
   */
  render(width: number, theme: PanelTheme): PanelRender;
  /**
   * Tell the surface this panel's content changed. Mirrors the `subscribe` both
   * strips already have: the pull model is the surface's (nothing may wait on a
   * network mid-frame), but *when* to repaint is the panel's news.
   *
   * The returned function detaches the listener, and is safe to call twice.
   */
  subscribe?(listener: () => void): () => void;
  /**
   * Read now, out of band with whatever cadence the panel has — the same
   * "this run just changed the board, so ask again" call
   * `KanbanSource.refresh()` and `BackendMonitor.poll()` are.
   */
  refresh?(): void | Promise<void>;
  /**
   * The surface is gone. Release what the *panel* allocated (cached views,
   * child components, its own timers). Not a stop order for a shared source:
   * see the module doc.
   */
  dispose?(): void;
  /**
   * Which work item the surface holding this panel is about. The board marks
   * that ticket `▸`; a panel with no notion of "current" ignores it. It is
   * deliberately phrased as *the item this surface is about* rather than as a
   * board concept, so a strip about a server, a PR, or a person can take the
   * same slot.
   */
  setCurrent?(id: string | undefined): void;
}

// ── helpers ────────────────────────────────────────────────────────────────

/** Is what a panel rendered a drawable rather than lines? */
export function isPanelComponent(render: PanelRender): render is PanelRenderable {
  return (
    typeof render === "object" &&
    render !== null &&
    typeof (render as PanelRenderable).render === "function" &&
    typeof (render as PanelRenderable).invalidate === "function"
  );
}

/**
 * A panel's drawn lines, clamped to its declared budget.
 *
 * The clamp lives here rather than in each panel so the presenter can trust the
 * number it laid out with: `lines` is not a hint, it is what the surface
 * reserved.
 */
export function panelLines(panel: Panel, width: number, theme: PanelTheme): string[] {
  if (panel.lines <= 0) return [];
  const drawn = panel.render(width, theme);
  const lines = isPanelComponent(drawn) ? drawn.render(width) : drawn;
  return clampLines(lines, panel.lines);
}

function clampLines(lines: readonly string[], budget: number): string[] {
  if (budget <= 0) return [];
  return lines.length > budget ? [...lines.slice(0, budget)] : [...lines];
}

/**
 * A panel drawn as a pi-tui `Component`, for adding to a container.
 *
 * It caches nothing between frames, like both strips' own components: the lines
 * are produced at render time from a snapshot already in memory, so a slow
 * poll behind a panel can never hold up a frame — it can only make the panel
 * draw an older number, which is what `—` and an age are for. The one thing it
 * remembers is the component a lines-returning panel never handed back, so
 * `invalidate()` reaches a real cache when there is one and is a no-op when
 * there is not.
 */
export class PanelComponent implements PanelRenderable {
  readonly id: string;
  private readonly panel: Panel;
  private readonly theme: PanelTheme;
  /** The component form, kept so `invalidate()` has something to invalidate. */
  private held: PanelRenderable | null = null;

  constructor(panel: Panel, theme: PanelTheme) {
    this.panel = panel;
    this.theme = theme;
    this.id = panel.id;
  }

  render(width: number): string[] {
    const drawn = this.panel.render(width, this.theme);
    if (isPanelComponent(drawn)) {
      this.held = drawn;
      const lines = drawn.render(width);
      return clampLines(lines, this.panel.lines);
    }
    this.held = null;
    return clampLines(drawn, this.panel.lines);
  }

  invalidate(): void {
    this.held?.invalidate();
  }
}

/**
 * A stable partition by placement — input order preserved inside each band,
 * which is what makes "the monitor stays outermost, the board sits between it
 * and the transcript" a property of the array the root passes in rather than a
 * rule buried in the presenter.
 */
export function splitByPlacement(panels: readonly Panel[]): {
  readonly top: readonly Panel[];
  readonly band: readonly Panel[];
} {
  const top: Panel[] = [];
  const band: Panel[] = [];
  for (const panel of panels) {
    (panel.placement === "top" ? top : band).push(panel);
  }
  return { top, band };
}

// ── the null forms ─────────────────────────────────────────────────────────

/** A {@link Panel} that draws nothing, and says why. */
export interface NullPanel extends Panel {
  /** Why this strip is off — the string that made it off. */
  readonly reason: string;
}

export interface NullPanelOptions {
  readonly id?: string;
  readonly placement?: PanelPlacement;
  /** Row budget. `0` is the honest default: a panel that draws nothing owns nothing. */
  readonly lines?: number;
}

/**
 * The panel for a run that must not show one.
 *
 * One implementation of "absent", for every kind of strip there is or will be:
 * it satisfies {@link Panel}, renders nothing, has nothing to dispose, and
 * carries the reason it was switched off so a diagnostic can print it rather
 * than leaving an operator to guess whether the board is empty or turned off.
 * `createNullKanban()` and `createNullMonitor()` are the *source*-shaped
 * versions of the same trick — those exist because a poller's consumers want a
 * poller; this is the surface-shaped one.
 */
export function createNullPanel(
  reason = "not configured",
  options: NullPanelOptions = {},
): NullPanel {
  return {
    id: options.id ?? "none",
    placement: options.placement ?? "band",
    lines: Math.max(0, options.lines ?? 0),
    reason,
    render(): PanelRender {
      return [];
    },
    subscribe(): () => void {
      return () => undefined;
    },
    refresh(): void {},
    dispose(): void {},
    setCurrent(): void {},
  };
}

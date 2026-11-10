// The diff Tracks shows beside the agent: a changed file, a checkout's uncommitted changes, a
// commit (or one file of it) or a pull request. The sidebar puts the request in shared state
// `tracks.diff.<tab>` (props { tab }) and sends `tracks-diff.show`; this pane follows it, takes
// the keyboard (j/k, space/b, g/G scroll; q or Esc close), and reads changing diffs again.

import { View, div } from "gpui-kit";
import { v_flex, h_flex, Scrollbar, v_virtual_list, VirtualListScrollHandle } from "gpui-base";
// herdr's exec takes an argv array and runs no shell, like execFile.
import { exec as execFile, shared, panes, props, run, theme, settings, nextCommand } from "herdr";

const BIN = "herdr-tracks";
const ROW = 20;
const MAX_ROWS = 20000;
// How often a diff that can change is read again; pull requests come from GitHub.
const AGAIN_MS = { local: 2000, worktree: 2000, pr: 30000 };

function title(request) {
  switch (request?.kind) {
    case "local": return request.path;
    case "worktree": return `${request.root.split("/").pop()} · uncommitted changes`;
    case "commit": return `${request.commit.hash.slice(0, 7)}${request.path ? ` · ${request.path}` : ""}`;
    case "pr": return request.url.replace("https://github.com/", "");
    default: return "Tracks diff";
  }
}

// Numbers each line once, so a frame only draws the rows on screen.
function rows(text) {
  let old = 0, next = 0;
  return text.replace(/\n$/, "").split("\n").slice(0, MAX_ROWS).map((line) => {
    line = line.replaceAll("\t", "    ");
    const hunk = line.match(/^@@ -(\d+)(?:,\d+)? \+(\d+)/);
    if (hunk) { old = +hunk[1]; next = +hunk[2]; return { kind: "@", text: line }; }
    if (/^(diff --git|\+\+\+ |--- |new file|deleted file|rename |similarity |index )/.test(line)) return { kind: "h", text: line };
    const kind = line[0];
    if (kind === "-") return { kind, number: old++, text: line.slice(1) || " " };
    if (kind === "+") return { kind, number: next++, text: line.slice(1) || " " };
    if (kind === " ") { old++; return { kind, number: next++, text: line.slice(1) || " " }; }
    return { kind: "", text: line };
  });
}

export default class TracksDiff extends View {
  init(_props, cx) {
    this.handle = cx.focus_handle();
    this.request = null;
    this.key = null;
    this.rows = [];
    this.width = 0;
    this.error = null;
    this.next = 0;
    this.scroll = VirtualListScrollHandle.new();
    // The row kept in the middle of the view, and how many rows the view holds.
    this.center = 0;
    this.page = 20;
    cx.timer.every(1000, (cx) => this.load(cx, true));
    this.focus(cx);
    cx.spawn(async (cx) => {
      for (;;) {
        const command = await nextCommand();
        if (command === "show" || command === "focus") this.focus(cx);
        if (command === "show") this.load(cx, true);
        cx.notify();
      }
    });
  }

  // Takes the keyboard, again once the pane is drawn: a handle not on screen yet holds nothing.
  focus(cx) {
    this.handle.focus();
    for (const ms of [30, 150, 400]) cx.timer.after(ms, (cx) => {
      if (!this.handle.is_focused()) this.handle.focus();
      cx.notify();
    });
  }

  // Moves the view `by` rows (±Infinity: to an end), keeping its middle row on screen.
  move(by) {
    const half = Math.floor(this.page / 2);
    const last = Math.max(half, this.rows.length - 1 - half);
    const target = by === Infinity ? last : by === -Infinity ? half : Math.round(this.center) + by;
    this.center = Math.min(last, Math.max(half, target));
    this.scroll.scroll_to_item(this.center, "center");
  }

  key(stroke) {
    const page = Math.max(1, this.page - 2);
    const moves = { j: 1, down: 1, k: -1, up: -1, space: page, pagedown: page, b: -page, pageup: -page, g: -Infinity, home: -Infinity, "shift-g": Infinity, end: Infinity };
    if (!(stroke in moves)) return false;
    this.move(moves[stroke]);
    return true;
  }

  // From render, `notify` is false: the frame being drawn shows the change already.
  load(cx, notify) {
    const request = shared(`tracks.diff.${props()?.tab}`) ?? null;
    const key = JSON.stringify(request);
    const changed = key !== this.key;
    if (!changed && (this.loading || Date.now() < this.next || !AGAIN_MS[request?.kind])) return;
    this.key = key;
    this.request = request;
    this.next = Date.now() + (AGAIN_MS[request?.kind] ?? 0);
    if (changed) {
      this.rows = []; this.text = null; this.error = null;
      this.center = Math.floor(this.page / 2);
      this.scroll.scroll_to_item(0, "top");
      if (notify) cx.notify();
    }
    if (!request) return;
    this.loading = true;
    cx.spawn(async (cx) => {
      try {
        // A remote tab's id is `<machine>/<tab>`: its repos are there.
        const tab = String(props()?.tab ?? "");
        const on = tab.includes("/") ? tab.slice(0, tab.indexOf("/")) : null;
        const r = await execFile(BIN, ["app", "patch", key], { on });
        if (this.key !== key) return;
        if (r.code !== 0) { this.error = r.stderr.trim() || "no diff"; this.rows = []; this.text = null; }
        else {
          this.error = r.stdout.trim() ? null : "no changes";
          if (r.stdout !== this.text) {
            this.text = r.stdout;
            this.rows = rows(r.stdout);
            this.width = Math.max(300, ...this.rows.slice(0, 5000).map((l) => l.text.length * 7 + 60));
          }
        }
      } catch (e) {
        if (this.key === key) this.error = String(e);
      } finally {
        this.loading = false;
        cx.notify();
      }
    });
  }

  closePane() {
    const me = panes().find((p) => p.tab === props()?.tab && p.mod === "tracks-diff");
    if (me) run("pane.close", { pane: me.pane });
  }

  row({ kind, number, text }, t) {
    if (kind === "@") return div().w_full().px(12).h(ROW).flex_none().truncate().bg(t.raised).text_color(t.dim).child(text);
    if (kind === "h") return div().w_full().px(12).h(ROW).flex_none().truncate().font_semibold().child(text);
    return h_flex().w_full().h(ROW).flex_none().gap(10)
      .when(kind === "+", (e) => e.bg(t.ansi2 + "18")).when(kind === "-", (e) => e.bg(t.ansi1 + "18"))
      .child(div().w(36).flex_none().text_right().text_color(t.dim).child(number === undefined ? "" : String(number)))
      .child(div().flex_1().min_w(0).whitespace_nowrap().text_color(kind === "-" ? t.ansi1 : kind === "+" ? t.ansi2 : t.foreground).child(text));
  }

  render(cx) {
    this.load(cx, false);
    const t = theme();
    return v_flex().id("tracks-diff").size_full().min_h(0).bg(t.background).text_color(t.foreground).text_size(12).track_focus(this.handle)
      .on_size_change((size) => { this.page = Math.max(1, Math.floor((size.height - 30) / ROW)); })
      .on_key_down((e, cx) => {
        if (e.keystroke === "escape" || e.keystroke === "q") { this.closePane(); cx.stop_propagation(); }
        else if (this.key(e.keystroke)) cx.stop_propagation();
      })
      .on_mouse_down("left", () => { if (!this.handle.is_focused()) this.handle.focus(); })
      .on_scroll_wheel((e) => {
        const half = Math.floor(this.page / 2);
        const dy = e.delta_lines ? e.delta_lines.y : e.delta.y / ROW;
        this.center = Math.min(Math.max(half, this.rows.length - 1 - half), Math.max(half, this.center - dy));
      })
      .child(h_flex().h(30).flex_none().px(12).gap(8).items_center().border_b(1).border_color(t.border)
        .child(div().flex_1().min_w(0).truncate().font_semibold().child(title(this.request)))
        .when(Boolean(this.loading), (e) => e.child(div().text_size(11).text_color(t.dim).child("…")))
        .child(div().id("close").px(6).rounded(4).cursor_pointer().text_color(t.muted).hover((s) => s.bg(t.hover)).on_click(() => this.closePane()).child("✕")))
      .when(Boolean(this.error), (e) => e.child(div().px(12).py(6).text_color(t.dim).child(this.error)))
      .child(div().flex_1().min_h(0).relative().font_family(settings().font_family ?? "JetBrains Mono").text_size(11.5)
        .child(div().id("tracks-diff-x").size_full().overflow_x_scrollbar()
          .child(div().w_full().min_w(this.width).h_full().child(v_virtual_list("tracks-diff-rows", this.rows.length, ROW, (i) => String(i),
            (range) => this.rows.slice(range.start, range.end).map((row) => this.row(row, t))).track_scroll(this.scroll).size_full())))
        .child(Scrollbar.vertical("tracks-diff-rows").viewport_from_layout().absolute().inset_0()));
  }
}

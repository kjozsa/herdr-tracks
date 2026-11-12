// Tracks: the repos, uncommitted files, pull requests, commits and links of the agent chat in
// the focused tab. `herdr-tracks app serve` tracks every agent and writes one model file per
// terminal; this draws the one followed. Opened as a popup with props { menu }, it is the
// right-click menu instead.

import { View, div, svg } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
// herdr's exec takes an argv array and runs no shell, like execFile.
import { agents, focused, panes, settings, exec as execFile, readTextFile, watchFolder, theme, run, setShared, openOverlay, closeOverlay, props } from "herdr";

const BIN = "herdr-tracks";
// Commits listed per repo and per pull request; older ones are counted on one more row.
const COMMITS_SHOWN = 10;
// Rows and icons as in Herdr's own Files, Changes and Spaces views.
const ROW = 25;
// uncommitted files, and files under an expanded commit, sit tighter
const CHANGE_ROW = 22;
// commits and pull requests in a repo's history
const COMMIT_ROW = 25;
const LINK_ROW = 22;
const FILE_ROW = 20;
const INDENT = 18;
// Pastel bars per repo, tinted like the labeled Spaces view: [rest, hover] alpha.
const PASTELS = ["#f5c2e7", "#a6e3a1", "#89b4fa", "#f9e2af", "#cba6f7", "#94e2d5", "#fab387", "#f38ba8", "#b4befe", "#89dceb"];
const BAR_TINT = { dark: ["47", "61"], light: ["7a", "a0"] };
const NAME_DIM = 0.22;
const pastel = (name) => PASTELS[[...name].reduce((h, ch) => (h * 31 + ch.charCodeAt(0)) >>> 0, 7) % PASTELS.length];
function mix(a, b, k) {
  const rgb = (h) => /^#[0-9a-f]{6}/i.test(h ?? "") ? [1, 3, 5].map((i) => parseInt(h.slice(i, i + 2), 16)) : null;
  const x = rgb(a), y = rgb(b);
  if (!x || !y) return a;
  return "#" + x.map((v, i) => Math.round(v + (y[i] - v) * k).toString(16).padStart(2, "0")).join("");
}
const ICON = (name) => `mods/workspace-chrome/${name}.svg`;
const CHEVRON = (open) => `mods/files/chevron-${open ? "down" : "right"}.svg`;
// Link rows in view; more scroll.
const LINKS_SHOWN = 8;

// Merge commits say nothing the PR or branch doesn't; GitHub gives no subject for a PR's own.
const isMerge = (c) => c.subject === "merge commit" || /^Merge (pull request|branch|remote-tracking branch|commit) /.test(c.subject ?? "");
const short = (hash) => hash.slice(0, 7);
const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;
const linkText = (url) => url.replace(/^https?:\/\//, "").replace(/^www\./, "");

function open(url) {
  execFile("xdg-open", [url]).catch(() => {});
}

// A remote terminal's address is `<machine>/<terminal>`: its serve, files and diffs are there.
export function machineOf(address) {
  const i = address ? address.indexOf("/") : -1;
  return i < 0 ? { on: null, local: address ?? null } : { on: address.slice(0, i), local: address.slice(i + 1) };
}

function send(terminal, command) {
  const { on, local } = machineOf(terminal);
  return execFile(BIN, ["app", "send", local, JSON.stringify(command)], { on });
}

// The tab's agent to follow: the focused one, else the one followed so far, else the first.
function followed(previous) {
  const f = focused();
  if (!f) return null;
  const mine = agents().filter((a) => a.tab === f.tab);
  return mine.find((a) => a.address === f.terminal) ?? mine.find((a) => a.address === previous) ?? mine[0] ?? null;
}

// Shows a diff in the tab's diff pane, splitting one off the agent's pane the first time, and
// gives it the keyboard.
function showDiff(agent, request) {
  setShared(`tracks.diff.${agent.tab}`, request);
  const open = panes().find((p) => p.tab === agent.tab && p.mod === "tracks-diff");
  if (!open) return run("pane.split", { direction: "right", pane: agent.pane, mod: "tracks-diff", props: { tab: agent.tab } });
  run("pane.focus", { pane: open.pane });
  run("tracks-diff.show");
}

function changeColor(code, t) {
  if (code === "??") return t.ansi3;
  if (code.includes("U") || code === "AA" || code === "DD") return t.ansi5;
  return code[1] === " " ? t.ansi2 : t.ansi1;
}

function statText(stat, t) {
  if (!stat) return [];
  if (stat.binary) return [div().text_size(11).text_color(t.dim).child("bin")];
  return [
    stat.added > 0 && div().flex_none().text_size(11).text_color(t.ansi2).child(`+${stat.added}`),
    stat.removed > 0 && div().flex_none().text_size(11).text_color(t.ansi1).child(`−${stat.removed}`),
  ].filter(Boolean);
}

const PR_STATE = { open: "ansi2", draft: "dim", merged: "ansi5", closed: "ansi1" };
const CHECKS = { passing: ["✓", "ansi2"], failing: ["✗", "ansi1"], pending: ["◔", "ansi3"] };

export default class Tracks extends View {
  init(_props, cx) {
    if (props()?.menu) return;
    // Per machine ("" for this one): where its serve writes the models, and why it can't.
    this.dirs = new Map();
    this.errors = new Map();
    this.model = null;
    this.text = null;
    this.agent = null;
    // Pull requests (by URL) expanded into their commits, commits (by hash) into their files.
    this.expanded = new Set();
    this.selected = null;
    try { this.folded = new Set(JSON.parse(localStorage.getItem("repos.folded") ?? "[]")); } catch { this.folded = new Set(); }
    this.ensure(cx);
    cx.timer.every(60000, (cx) => this.ensure(cx));
    cx.timer.every(2000, (cx) => this.reload(cx));
  }

  // The followed agent's machine, "" for this one.
  machine() {
    return machineOf(this.agent?.address).on ?? "";
  }

  // Keeps `serve` running on the followed agent's machine; learns where it writes, and follows
  // its writes (watched here, polled by the reload timer elsewhere).
  ensure(cx) {
    const m = this.machine();
    cx.spawn(async (cx) => {
      try {
        const r = await execFile(BIN, ["app", "ensure"], { on: m || null });
        if (r.code !== 0) throw new Error(r.stderr.trim() || `${BIN} app ensure failed`);
        const dir = r.stdout.trim();
        this.errors.delete(m);
        if (dir !== this.dirs.get(m)) {
          this.dirs.set(m, dir);
          this.reload(cx);
          if (!m) this.watch(dir, cx);
        }
      } catch (e) {
        this.errors.set(m, m ? `${m}: ${e}` : String(e));
        cx.notify();
      }
    });
  }

  watch(dir, cx) {
    cx.spawn(async (cx) => {
      try {
        while (this.dirs.get("") === dir) {
          await watchFolder(dir);
          this.reload(cx);
        }
      } catch {}
    });
  }

  // From render, `notify` is false: the frame being drawn shows the change already.
  reload(cx, notify = true) {
    const agent = followed(this.agent?.address);
    if (agent?.address !== this.agent?.address) {
      this.agent = agent;
      this.model = null;
      this.text = null;
      this.expanded = new Set();
      this.selected = null;
      if (notify) cx.notify();
      if (agent && !this.dirs.has(this.machine())) this.ensure(cx);
    } else {
      this.agent = agent;
    }
    const dir = this.dirs.get(this.machine());
    if (!dir || !agent) return;
    const address = agent.address;
    const { on, local } = machineOf(address);
    cx.spawn(async (cx) => {
      const text = await readTextFile(`${dir}/${local}.json`, { on }).catch(() => null);
      if (this.agent?.address !== address || text === this.text) return;
      this.text = text;
      try { this.model = text ? JSON.parse(text) : null; } catch { this.model = null; }
      cx.notify();
    });
  }

  select(key, request) {
    this.selected = key;
    if (this.agent) showDiff(this.agent, request);
  }

  toggle(key) {
    if (!this.expanded.delete(key)) this.expanded.add(key);
  }

  expandCommit(commit) {
    if (this.expanded.delete(commit.hash)) return;
    this.expanded.add(commit.hash);
    if (!this.model?.files?.[commit.hash] && this.agent) send(this.agent.address, { files: commit.at }).catch(() => {});
  }

  menu(items) {
    openOverlay({ mod: "tracks", kind: "popup", width: 210, height: items.length * 30 + 12, props: { menu: { items, terminal: this.agent?.address } } });
  }

  row(id, t, selected, depth = 0, height = ROW) {
    return h_flex().relative().id(id).h(height).flex_none().pl(8 + Math.max(0, depth - 1) * INDENT).pr(8).gap(9).items_center().rounded(8)
      .when(selected, (e) => e.bg(mix(t.selected, t.chrome, 0.45)).text_color(t.foreground)
        .child(div().absolute().left(0).top(height > 24 ? 6 : 3).bottom(height > 24 ? 6 : 3).w(3).rounded(2).bg(t.accent)))
      .when(!selected, (e) => e.hover((s) => s.bg(t.hover)));
  }

  chevron(id, open, toggle, t) {
    return div().id(id).size(16).flex_none().flex().items_center().justify_center().rounded(4).cursor_pointer()
      .hover((s) => s.bg(t.hover)).on_click((_e, cx) => { cx.stop_propagation(); toggle(); cx.notify(); })
      .child(svg(CHEVRON(open)).size(12).text_color(t.dim));
  }

  // "src/app/" dim before the name, as the Changes view shows a path.
  path(text, t) {
    const cut = text.includes(" → ") ? -1 : text.replace(/\/$/, "").lastIndexOf("/");
    return h_flex().flex_1().min_w(0)
      .when(cut >= 0, (e) => e.child(div().min_w(0).flex_shrink(1).truncate().text_color(t.dim).child(text.slice(0, cut + 1))))
      .child(div().min_w(0).flex_shrink(0).max_w(260).truncate().child(text.slice(cut + 1)));
  }

  code(text, color) {
    return div().w(16).flex_none().text_size(11).text_color(color).child(text);
  }

  dim(id, t, depth, text) {
    return this.row(id, t, false, depth).child(div().pl(23).truncate().text_size(12).text_color(t.dim).child(text));
  }

  // A bar like a label's in the Spaces view; a click folds it.
  head(id, t, name, key, first) {
    const dark = settings().dark !== false;
    const c = pastel(name), tint = BAR_TINT[dark ? "dark" : "light"], ink = dark ? c : t.foreground;
    const folded = this.folded.has(key);
    return h_flex().id(id).h(24).flex_none().when(!first, (e) => e.mt(2)).px(8).gap(7).items_center().text_size(12).font_bold()
      .rounded(7).bg(`${c}${tint[0]}`).text_color(ink).cursor_pointer().hover((s) => s.bg(`${c}${tint[1]}`))
      .on_click((_e, cx) => {
        if (!this.folded.delete(key)) this.folded.add(key);
        try { localStorage.setItem("repos.folded", JSON.stringify([...this.folded])); } catch {}
        cx.notify();
      })
      .child(svg(ICON("folder")).size(13).flex_none())
      .child(div().flex_none().max_w(160).truncate().child(name));
  }

  repoRows(repo, t, first) {
    const rows = [];
    const folded = this.folded.has(repo.root);
    const tint = BAR_TINT[settings().dark !== false ? "dark" : "light"][1];
    rows.push(this.head(`repo-${repo.root}`, t, repo.name, repo.root, first)
      .on_mouse_down("right", () => this.menu([["Dismiss repo", { dismiss: { repo: repo.root } }]]))
      .child(h_flex().flex_1().min_w(0).gap(5).justify_end().items_center().font_normal().text_size(11.5)
        .when(Boolean(repo.branch), (e) => e.child(svg(ICON("git-branch")).size(12).flex_none()).child(div().min_w(0).truncate().child(repo.branch)))
        .when(repo.ahead > 0, (e) => e.child(div().flex_none().text_color(t.ansi2).child(`↑${repo.ahead}`)))
        .when(repo.behind > 0, (e) => e.child(div().flex_none().text_color(t.ansi1).child(`↓${repo.behind}`))))
      .child(div().id(`repo-diff-${repo.root}`).flex_none().p(3).rounded(4).hover((s) => s.bg(`${pastel(repo.name)}${tint}`))
        .tooltip("Show all changes").on_click((_e, cx) => { cx.stop_propagation(); this.select(repo.root, repo.diff); cx.notify(); })
        .child(svg(ICON("git-compare")).size(12)))
      .child(svg(CHEVRON(!folded)).size(11).flex_none()));
    if (folded) return rows;
    if (repo.error) rows.push(this.row(`repo-err-${repo.root}`, t, false, 1).child(div().truncate().text_size(12).text_color(t.ansi1).child(repo.error)));
    for (const c of repo.changes ?? []) {
      const key = `${repo.root}:${c.path}`;
      rows.push(this.row(`file-${key}`, t, this.selected === key, 1, CHANGE_ROW).cursor_pointer()
        .on_click(() => this.select(key, c.diff))
        .child(this.code(c.code.trim() === "??" ? "U" : c.code.trim(), changeColor(c.code, t)))
        .child(this.path(c.display, t))
        .children(statText(c.stat, t)));
    }
    let loose = 0;
    const history = repo.history.filter((h) => !(h.commit && isMerge(h.commit)));
    const looseTotal = history.filter((h) => h.commit).length;
    for (const entry of history) {
      if (entry.pr) rows.push(...this.prRows(entry.pr, t, 1));
      else if (++loose <= COMMITS_SHOWN) rows.push(...this.commitRows(entry.commit, t, 1));
      else if (loose === COMMITS_SHOWN + 1) rows.push(this.dim(`more-${repo.root}`, t, 1, `${plural(looseTotal - COMMITS_SHOWN, "earlier commit")}…`));
    }
    return rows;
  }

  prRows(pr, t, depth) {
    const expanded = this.expanded.has(pr.url);
    const check = pr.checks && CHECKS[pr.checks];
    const state = t[PR_STATE[pr.state]] ?? t.dim;
    const rows = [this.row(`pr-${pr.url}`, t, this.selected === pr.url, depth, COMMIT_ROW)
      .on_mouse_down("right", () => this.menu([["Open on GitHub", { open: pr.url }], ["Dismiss PR", { dismiss: { pr: pr.target } }]]))
      .child(this.chevron(`pr-mark-${pr.url}`, expanded, () => this.toggle(pr.url), t))
      .child(svg(ICON("git-compare")).size(13).flex_none().text_color(state))
      .child(div().id(`pr-link-${pr.url}`).flex_none().underline().text_color(t.foreground).cursor_pointer().hover((s) => s.text_color(t.accent))
        .tooltip("Open on GitHub").on_click(() => open(pr.url)).child(`PR #${pr.number}`))
      .child(div().id(`pr-title-${pr.url}`).flex_1().min_w(0).truncate().cursor_pointer().on_click(() => this.select(pr.url, pr.diff))
        .when(Boolean(pr.error && !pr.title), (e) => e.text_color(t.ansi1))
        .child(pr.title ?? pr.error ?? "…"))
      .when(Boolean(check), (e) => e.child(div().flex_none().text_size(11.5).text_color(t[check[1]]).child(check[0])))
      .when(Boolean(pr.state), (e) => e.child(div().flex_none().px(7).py(1).rounded(5).bg(state).text_size(10.5).font_bold().text_color(t.chrome).child(pr.state)))];
    if (!expanded) return rows;
    if (!pr.commits) return [...rows, this.dim(`pr-wait-${pr.url}`, t, depth + 1, "Loading…")];
    const commits = pr.commits.filter((c) => !isMerge(c));
    for (const c of commits.slice(0, COMMITS_SHOWN)) rows.push(...this.commitRows(c, t, depth + 1, pr.url));
    if (commits.length > COMMITS_SHOWN) rows.push(this.dim(`pr-more-${pr.url}`, t, depth + 1, `${plural(commits.length - COMMITS_SHOWN, "earlier commit")}…`));
    return rows;
  }

  commitRows(c, t, depth, under = "") {
    const expanded = this.expanded.has(c.hash);
    const id = `${under}:${c.hash}`;
    const rows = [this.row(`commit-${id}`, t, this.selected === c.hash, depth, COMMIT_ROW)
      .child(this.chevron(`commit-mark-${id}`, expanded, () => this.expandCommit(c), t))
      .child(div().id(`commit-body-${id}`).flex_1().min_w(0).flex().items_center().gap(7).cursor_pointer()
        .on_click((e) => (e.modifiers.control && c.url ? open(c.url) : this.select(c.hash, c.diff)))
        .child(div().min_w(0).flex_1().truncate().text_color(c.chat ? mix(t.foreground, t.chrome, 0.38) : mix(t.dim, t.chrome, 0.2)).child(c.subject))
        .child(div().flex_none().text_size(11.5).text_color(t.dim).child(short(c.hash))))];
    if (!expanded) return rows;
    const files = this.model?.files?.[c.hash];
    if (!files) rows.push(this.dim(`files-wait-${id}`, t, depth + 1, "Loading…"));
    else if (files.error) rows.push(this.row(`files-err-${id}`, t, false, depth + 1).child(div().pl(23).truncate().text_size(12).text_color(t.ansi1).child(files.error)));
    else for (const f of files.files) {
      const key = `${c.hash}:${f.path}`;
      rows.push(this.row(`cfile-${id}-${f.path}`, t, this.selected === key, depth + 1, FILE_ROW).text_size(12).cursor_pointer()
        .on_click(() => this.select(key, { kind: "commit", commit: c.at, path: f.path }))
        .child(div().w(16).flex_none())
        .child(this.code(f.code, f.code === "A" ? t.ansi2 : f.code === "D" ? t.ansi1 : t.blocked))
        .child(this.path(f.path, t))
        .children(statText(f.stat, t)));
    }
    return rows;
  }

  linkRows(links, t) {
    return links.map((url) => this.row(`link-${url}`, t, false, 0, LINK_ROW).cursor_pointer()
      .on_click(() => open(url))
      .on_mouse_down("right", () => this.menu([["Open in browser", { open: url }], ["Dismiss link", { dismiss: { link: url } }], ["Dismiss all links", { dismiss: "links" }]]))
      .child(div().min_w(0).truncate().text_color(t.ansi4).child(linkText(url))));
  }

  menuView(t) {
    const { items, terminal } = props().menu;
    return v_flex().w_full().p(5).bg(t.popover).text_size(12.8).children(items.map(([label, action], i) =>
      h_flex().id(`item-${i}`).h(30).px(8).rounded(7).items_center().text_color(t.foreground).cursor_pointer().hover((el) => el.bg(t.hover))
        .on_click(() => {
          closeOverlay(props().overlay);
          if (action.open) open(action.open);
          else if (terminal) send(terminal, action).catch(() => {});
        })
        .child(label)));
  }

  render(cx) {
    const t = theme();
    if (props()?.menu) return this.menuView(t);
    const agent = followed(this.agent?.address);
    if (agent?.address !== this.agent?.address) this.reload(cx, false);
    const m = this.model;
    const rows = [];
    const note = (text, color = t.dim) => rows.push(div().p(8).text_size(12).text_color(color).child(text));
    const error = this.errors.get(this.machine());
    if (error) note(error, t.ansi1);
    if (m?.error) note(m.error, t.ansi1);
    if (!agent) note("No agent in this tab");
    else if (!m) note(this.dirs.has(this.machine()) ? "Loading…" : "Starting…");
    else {
      if (!m.repos.length && !m.orphans.length) note("No changes yet");
      m.repos.forEach((repo, i) => rows.push(...this.repoRows(repo, t, i === 0)));
      const heads = new Set();
      for (const pr of m.orphans) {
        const key = `orphan:${pr.repo}`;
        if (!heads.has(key)) {
          heads.add(key);
          rows.push(this.head(`orphan-${pr.repo}`, t, pr.repo, key, rows.length === 0).child(div().flex_1())
            .child(svg(CHEVRON(!this.folded.has(key))).size(11).flex_none()));
        }
        if (!this.folded.has(key)) rows.push(...this.prRows(pr, t, 1));
      }
    }
    const links = m?.links ?? [];
    // Drawn like the Spaces sidebar: flat on the chrome, a small dim title, label-style bars.
    return v_flex().id("tracks").size_full().min_h(0).rounded(10).overflow_hidden().bg(t.chrome)
      .text_color(mix(t.foreground, t.chrome, NAME_DIM)).text_size(13)
      .child(h_flex().h(31).flex_none().pl(16).pr(12).gap(8).items_center().text_size(11.5).text_color(t.dim)
        .child(div().flex_none().child("Tracks"))
        .child(div().flex_1().min_w(0).truncate().text_right().child(m?.title ?? "")))
      .child(v_flex().id("tracks-list").flex_1().min_h(0).px(8).pb(14).overflow_y_scrollbar().children(rows))
      .when(links.length > 0, (e) => e.child(v_flex().flex_none().mx(8).border_t_1().border_color(t.border)
        .child(h_flex().id("links-rule").h(26).flex_none().px(8).gap(7).items_center().text_size(11).text_color(t.dim)
          .on_mouse_down("right", () => this.menu([["Dismiss all links", { dismiss: "links" }]]))
          .child(div().flex_1().child("Links"))
          .child(div().flex_none().child(String(links.length))))
        .child(v_flex().id("tracks-links").h(Math.min(links.length, LINKS_SHOWN) * LINK_ROW + 6).flex_none().pb(6).overflow_y_scrollbar().children(this.linkRows(links, t)))));
  }
}

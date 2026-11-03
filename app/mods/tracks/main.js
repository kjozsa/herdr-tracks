// Tracks: the repos, uncommitted files, pull requests, commits and links of the agent chat in
// the focused tab. `herdr-tracks app serve` tracks every agent and writes one model file per
// terminal; this draws the one followed. Opened as a popup with props { menu }, it is the
// right-click menu instead.

import { View, div } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
// herdr's exec takes an argv array and runs no shell, like execFile.
import { agents, focused, panes, exec as execFile, readTextFile, watchFolder, theme, run, setShared, openOverlay, closeOverlay, props } from "herdr";

const BIN = "herdr-tracks";
// Commits listed per repo and per pull request; older ones are counted on one more row.
const COMMITS_SHOWN = 10;
const ROW = 22;
// Link rows in view; more scroll.
const LINKS_SHOWN = 8;

const short = (hash) => hash.slice(0, 7);
const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;
const linkText = (url) => url.replace(/^https?:\/\//, "").replace(/^www\./, "");

function open(url) {
  execFile("xdg-open", [url]).catch(() => {});
}

function send(terminal, command) {
  return execFile(BIN, ["app", "send", terminal, JSON.stringify(command)]);
}

// The tab's agent to follow: the focused one, else the one followed so far, else the first.
function followed(previous) {
  const f = focused();
  if (!f) return null;
  const mine = agents().filter((a) => a.tab === f.tab && !a.address.includes("/"));
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
  if (stat.binary) return [div().text_color(t.dim).child("bin")];
  return [
    stat.added > 0 && div().text_color(t.ansi2).child(`+${stat.added}`),
    stat.removed > 0 && div().text_color(t.ansi1).child(`-${stat.removed}`),
  ].filter(Boolean);
}

const PR_STATE = { open: "ansi2", draft: "dim", merged: "ansi5", closed: "ansi1" };
const CHECKS = { passing: ["✓", "ansi2"], failing: ["✗", "ansi1"], pending: ["◔", "ansi3"] };

export default class Tracks extends View {
  init(_props, cx) {
    if (props()?.menu) return;
    this.dir = null;
    this.model = null;
    this.text = null;
    this.agent = null;
    // Pull requests (by URL) expanded into their commits, commits (by hash) into their files.
    this.expanded = new Set();
    this.selected = null;
    this.ensure(cx);
    cx.timer.every(60000, (cx) => this.ensure(cx));
    cx.timer.every(2000, (cx) => this.reload(cx));
  }

  // Keeps `serve` running; learns where it writes, and follows its writes.
  ensure(cx) {
    cx.spawn(async (cx) => {
      try {
        const r = await execFile(BIN, ["app", "ensure"]);
        if (r.code !== 0) throw new Error(r.stderr.trim() || `${BIN} app ensure failed`);
        const dir = r.stdout.trim();
        this.error = null;
        if (dir !== this.dir) {
          this.dir = dir;
          this.reload(cx);
          this.watch(dir, cx);
        }
      } catch (e) {
        this.error = String(e);
        cx.notify();
      }
    });
  }

  watch(dir, cx) {
    cx.spawn(async (cx) => {
      try {
        while (this.dir === dir) {
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
    } else {
      this.agent = agent;
    }
    if (!this.dir || !agent) return;
    const address = agent.address;
    cx.spawn(async (cx) => {
      const text = await readTextFile(`${this.dir}/${address}.json`).catch(() => null);
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

  row(id, t, selected) {
    return h_flex().id(id).h(ROW).flex_none().px(10).gap(6).items_center().when(selected, (e) => e.bg(t.selected)).hover((s) => s.bg(t.hover));
  }

  repoRows(repo, t) {
    const rows = [];
    const tracking = [repo.ahead > 0 && `↑${repo.ahead}`, repo.behind > 0 && `↓${repo.behind}`].filter(Boolean).join(" ");
    rows.push(this.row(`repo-${repo.root}`, t, this.selected === repo.root).mt(6).cursor_pointer()
      .on_click(() => this.select(repo.root, repo.diff))
      .on_mouse_down("right", () => this.menu([["Dismiss repo", { dismiss: { repo: repo.root } }]]))
      .child(div().font_semibold().flex_none().child(repo.name))
      .when(Boolean(repo.branch), (e) => e.child(div().min_w(0).truncate().text_color(t.ansi6).child(repo.branch)))
      .when(Boolean(tracking), (e) => e.child(div().flex_none().text_color(t.ansi3).child(tracking))));
    if (repo.error) rows.push(this.row(`repo-err-${repo.root}`, t).child(div().text_color(t.ansi1).truncate().child(`! ${repo.error}`)));
    for (const c of repo.changes ?? []) {
      const key = `${repo.root}:${c.path}`;
      rows.push(this.row(`file-${key}`, t, this.selected === key).cursor_pointer()
        .on_click(() => this.select(key, c.diff))
        .child(div().w(18).flex_none().text_color(changeColor(c.code, t)).font_family("JetBrains Mono").child(c.code.replace(" ", "·")))
        .child(div().flex_1().min_w(0).text_ellipsis_start().whitespace_nowrap().overflow_hidden().child(c.display))
        .children(statText(c.stat, t)));
    }
    let loose = 0;
    const looseTotal = repo.history.filter((h) => h.commit).length;
    for (const entry of repo.history) {
      if (entry.pr) rows.push(...this.prRows(entry.pr, t, 0));
      else if (++loose <= COMMITS_SHOWN) rows.push(...this.commitRows(entry.commit, t, 0));
      else if (loose === COMMITS_SHOWN + 1) rows.push(this.row(`more-${repo.root}`, t).child(div().pl(16).text_color(t.dim).child(`… ${plural(looseTotal - COMMITS_SHOWN, "earlier commit")}`)));
    }
    return rows;
  }

  prRows(pr, t, depth) {
    const expanded = this.expanded.has(pr.url);
    const check = pr.checks && CHECKS[pr.checks];
    const rows = [this.row(`pr-${pr.url}`, t, this.selected === pr.url).pl(10 + depth * 14)
      .on_mouse_down("right", () => this.menu([["Open on GitHub", { open: pr.url }], ["Dismiss PR", { dismiss: { pr: pr.target } }]]))
      .child(div().id(`pr-mark-${pr.url}`).w(12).flex_none().text_color(t.dim).cursor_pointer().on_click((_e, cx) => { this.toggle(pr.url); cx.notify(); }).child(expanded ? "▾" : "▸"))
      .child(h_flex().id(`pr-link-${pr.url}`).flex_none().gap(5).cursor_pointer().on_click(() => open(pr.url))
        .child(div().underline().child(`PR#${pr.number}`))
        .when(Boolean(pr.state), (e) => e.child(div().text_color(t[PR_STATE[pr.state]]).child(pr.state)))
        .when(Boolean(check), (e) => e.child(div().text_color(t[check[1]]).child(check[0]))))
      .child(div().id(`pr-title-${pr.url}`).flex_1().min_w(0).truncate().cursor_pointer().on_click(() => this.select(pr.url, pr.diff))
        .text_color(pr.error && !pr.title ? t.ansi1 : t.foreground)
        .child(pr.title ?? (pr.error ? `! ${pr.error}` : "…")))];
    if (!expanded) return rows;
    if (!pr.commits) return [...rows, this.row(`pr-wait-${pr.url}`, t).child(div().pl(28).text_color(t.dim).child("…"))];
    for (const c of pr.commits.slice(0, COMMITS_SHOWN)) rows.push(...this.commitRows(c, t, depth + 1, pr.url));
    if (pr.commits.length > COMMITS_SHOWN)
      rows.push(this.row(`pr-more-${pr.url}`, t).child(div().pl(30).text_color(t.dim).child(`… ${plural(pr.commits.length - COMMITS_SHOWN, "earlier commit")}`)));
    return rows;
  }

  commitRows(c, t, depth, under = "") {
    const expanded = this.expanded.has(c.hash);
    const id = `${under}:${c.hash}`;
    const rows = [this.row(`commit-${id}`, t, this.selected === c.hash).pl(10 + depth * 14)
      .child(div().id(`commit-mark-${id}`).w(12).flex_none().cursor_pointer().text_color(c.chat ? t.ansi2 : t.dim)
        .on_click((_e, cx) => { this.expandCommit(c); cx.notify(); }).child(expanded ? "▾" : "▸"))
      .child(div().id(`commit-body-${id}`).flex_1().min_w(0).flex().gap(6).cursor_pointer()
        .on_click((e) => (e.modifiers.control && c.url ? open(c.url) : this.select(c.hash, c.diff)))
        .child(div().flex_none().text_color(t.dim).font_family("JetBrains Mono").child(short(c.hash)))
        .child(div().min_w(0).truncate().text_color(c.chat ? t.foreground : t.dim).child(c.subject)))];
    if (!expanded) return rows;
    const files = this.model?.files?.[c.hash];
    const pad = 38 + depth * 14;
    if (!files) rows.push(this.row(`files-wait-${id}`, t).child(div().pl(pad).text_color(t.dim).child("…")));
    else if (files.error) rows.push(this.row(`files-err-${id}`, t).child(div().pl(pad).truncate().text_color(t.ansi1).child(`! ${files.error}`)));
    else for (const f of files.files) {
      const key = `${c.hash}:${f.path}`;
      rows.push(this.row(`cfile-${id}-${f.path}`, t, this.selected === key).cursor_pointer()
        .on_click(() => this.select(key, { kind: "commit", commit: c.at, path: f.path }))
        .child(div().pl(pad).w(pad + 14).flex_none().text_color(f.code === "A" ? t.ansi2 : f.code === "D" ? t.ansi1 : t.ansi3).child(f.code))
        .child(div().flex_1().min_w(0).text_ellipsis_start().whitespace_nowrap().overflow_hidden().child(f.path))
        .children(statText(f.stat, t)));
    }
    return rows;
  }

  linkRows(links, t) {
    return links.map((url) => this.row(`link-${url}`, t).cursor_pointer()
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
    const note = (text, color = t.dim) => rows.push(this.row(`note-${rows.length}`, t).child(div().truncate().text_color(color).child(text)));
    if (this.error) note(this.error, t.ansi1);
    if (m?.error) note(`! ${m.error}`, t.ansi1);
    if (!agent) note("no agent in this tab");
    else if (!m) note(this.dir ? "…" : "starting…");
    else {
      if (!m.repos.length && !m.orphans.length) note("no changes yet");
      for (const repo of m.repos) rows.push(...this.repoRows(repo, t));
      for (const pr of m.orphans) {
        rows.push(this.row(`orphan-${pr.url}`, t).mt(6).child(div().font_semibold().child(pr.repo)));
        rows.push(...this.prRows(pr, t, 0));
      }
    }
    const links = m?.links ?? [];
    // A card like the Files and Changes panels it sits beside.
    return v_flex().id("tracks").size_full().min_h(0).rounded(10).overflow_hidden().bg(t.background).border_1().border_color(t.border)
      .text_color(t.foreground).text_size(12.5)
      .child(h_flex().h(42).flex_none().px(14).gap(8).items_center().border_b(1).border_color(t.border)
        .child(div().font_semibold().flex_none().child("Tracks"))
        .child(div().flex_1().min_w(0).truncate().text_size(11).text_color(t.dim).child(m?.title ?? "")))
      .child(v_flex().id("tracks-list").flex_1().min_h(0).py(4).overflow_y_scrollbar().children(rows))
      .when(links.length > 0, (e) => e.child(v_flex().flex_none().border_t(1).border_color(t.border)
        .child(h_flex().id("links-rule").h(ROW).flex_none().px(10).items_center().text_size(11).text_color(t.dim)
          .on_mouse_down("right", () => this.menu([["Dismiss all links", { dismiss: "links" }]]))
          .child(`links · ${links.length}`))
        .child(v_flex().id("tracks-links").h(Math.min(links.length, LINKS_SHOWN) * ROW).flex_none().overflow_y_scrollbar().children(this.linkRows(links, t)))));
  }
}

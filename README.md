# Tracks

A [herdr](https://herdr.dev) plugin that docks a sidebar on the right of every tab running a
coding agent and shows the tracks the agent's chat leaves behind: the git repos it changed,
their uncommitted files with line counts, the pull requests it opened or is working on, and
the commits it made. Select a file, pull request or commit to see its diff next to the agent.

```
herdr-tracks main ↑1
  M src/sidebar.rs                     +268 -105
 ?? src/ansi.rs                              +80
 ▾ PR#42 open ✓ dock the sidebar
   ▸ 9e1c2a4 dock the sidebar on the right
 ▾ 3326d43 list pull requests newest first
     M src/sidebar/render.rs                +12 -4
```

Linux only. Reads omp and Claude Code chat logs; other agents get the repos their processes
work in.

## Install

```sh
herdr plugin install kjozsa/herdr-tracks
```

The install builds the plugin with `cargo`, so it needs a Rust toolchain. Pull-request status
and diffs use the [GitHub CLI](https://cli.github.com) (`gh`), logged in.

From a local checkout:

```sh
cargo build --release
herdr plugin link .
```

## What it shows

The sidebar follows the focused agent pane of its tab and lists, per repo:

| Row | Meaning |
|---|---|
| `herdr-tracks main ↑1 ↓2` | Repo, checked-out branch, commits ahead of / behind its upstream; select it to see all its uncommitted changes in one diff |
| ` ▸ PR#42 open ✓ dock the sidebar` | A pull request the chat opened or referred to: state, CI checks (`✓` all pass, `✗` one failed, `◔` still running; none when the PR has no checks) and title; expands into its commits |
| `  M`, `A `, `??` … | Uncommitted changes, as in `git status`, with lines added/removed |
| ` ▸ 3326d43 …` | A commit; expands into the files it changes. Green marker: the chat made it |

Under each repo, uncommitted work comes first, then pull requests and the chat's other commits
in one list, freshest first. A pull request counts as fresh as the newest of the chat's
commits it holds, or its last activity on GitHub. Ten commits outside pull requests are
listed; one more row counts the older ones. An expanded pull request lists all its commits,
its merge commit included; those the chat did not make are dimmed. Commits the checkout does
not have are read from GitHub.

A commit counts as the chat's when it was committed while one of the chat's tool calls ran:
its own commits, merges and rebases, and pull requests it merged on GitHub; not older commits
a merge brought in. Commits on any local or remote branch count, pushed or not.

Several checkouts of one repo (git worktrees share their branches) list each pull request and
commit once. A pull request goes under the checkout that has its branch checked out, otherwise
under the first one the chat touched. A commit goes inside the pull request that holds it,
otherwise under the checkout whose branch has it. A rebased or cherry-picked commit can leave its
old copy on a branch no checkout has out. That copy is not listed when a pull request holds a
commit of the chat's with the same subject.

A repo is listed once the chat **changed** it: edited or wrote a file there, committed, the
repo's git state moved after the chat first touched it, or one of its uncommitted files was
last written while one of the chat's tool calls ran (shell edits in a chat begun outside
herdr). Repos the chat only read are not
listed, unless one of its pull requests belongs to them. Pull requests are those the chat
created with `gh pr create`, or referred to: a pull request URL in your message, omp's `pr://`
reads, or `gh pr view/diff/checkout/review/…`. What Claude Code's subagents do (a forked skill
like `/code-review 104`, a background agent) counts as the chat's: their pull requests, edits
and tool calls show too.

The bottom of the sidebar collects the **links** of the chat, newest first: web links in your
messages, in the agent's replies, and pages its tools read or fetched (not tool output). It
takes at most half of the pane; when more links than that are collected, the mouse wheel
scrolls them and the rule shows which are in view (`─ links 4–25 of 29`). Pull requests
already listed above are left out.

The repo list belongs to the chat and is kept across herdr restarts. The sidebar docks at the
width of herdr's left sidebar (`[ui] sidebar_width`, `sidebar_min_width`, `sidebar_max_width`).
Drag its edge to change that; it keeps the dragged width when panes open or close next to it.

## Keys and mouse

In the sidebar:

| Input | Action |
|---|---|
| `↓` `↑` (`j` `k`) | Select the next / previous file, commit or pull request; its diff opens next to the agent |
| `→` `←` (`l` `h`) | Expand / collapse a pull request into its commits, a commit into its files; `←` on a row inside one collapses it |
| Click | Select a row and show its diff (a pull request's title: the whole pull request; a commit: the whole commit; a file of a commit: that file) |
| Click on `▸` / `▾` | Expand / collapse a pull request or commit, without showing its diff |
| Click on a pull request's number or status, `o`, Ctrl+click | Open the pull request (or the selected commit) on GitHub |
| Click on a link | Open it in the browser |
| Right-click | On a repo, pull request or link: dismiss it, or open the pull request / link in the browser; on a link or the links rule: dismiss all links |
| `C` | Dismiss all links |
| `Enter` | Move focus into the diff to scroll it |
| `Esc` | Close the diff |

In the diff pane: `↓` `↑`, PgDn/PgUp, space/`b`, `g`/`G`, mouse wheel; `w` wraps long lines (again
to cut them at the edge); `q` or `Esc` closes it.
Diffs are formatted with [delta](https://github.com/dandavison/delta) when it is your git pager.
An open diff of uncommitted changes updates within a second of an edit or commit, keeping its
scroll position. A pull request's diff updates within 30 seconds of a push. A commit's diff
never changes: amending makes a new commit, listed as its own row.

A dismissed repo returns when the chat touches it again, a dismissed link when the chat
mentions it again; a dismissed pull request stays hidden for that chat. The sidebar cannot be
closed in a tab with an agent: it docks again right away. Once every other pane of its tab is
closed, it closes too, and so does the tab.
The `show` action docks it in any tab:

```toml
[[keys.command]]
key = "prefix+g"
type = "plugin_action"
command = "kjozsa.tracks.show"
```

## Herdr app

The same tracks show in the [Herdr app](https://herdr.dev) (`herdr-app`) as a **Tracks** panel
beside Files and Changes, following the agent of the focused tab. Click a file, repo, pull
request title or commit to open its diff in a pane next to the agent; that pane takes the
keyboard: `j`/`k` or arrows scroll a line, `space`/`b` a page, `g`/`G` to either end, `q` or
`Esc` close. The markers expand pull requests and commits, a pull request's number opens it in
the browser, and right-click dismisses repos, pull requests and links. Claude Code chats are
read from their transcript; other agents get the repos their processes work in.

```sh
cargo build --release
app/install.sh   # copies the mods into ~/.config/herdr-app/mods, links herdr-tracks onto the PATH
```

Herdr's own layout and tab bar are read-only, so the panel needs copies of them: duplicate the
`main` layout and the `tabs` mod (`herdr-app do layout.clone '{"name":"main","as":"main-tracks"}'`,
`herdr-app do mod.clone '{"name":"tabs","as":"tabs-tracks"}'`), add
`{ "id": "tracks", "type": "block", "content": { "mod": "tracks" } }` to the layout's
`panel-body` stack, point its `tabs` block at `tabs-tracks`, and add a
`panelButton("tracks", "Tracks", …)` next to the Changes one in `tabs-tracks`.

Mods cannot follow transcripts, so the panel starts `herdr-tracks app serve` as a transient user
unit (`herdr-tracks-app`). It tracks every agent terminal of the local herdr-core, writes one
model per terminal to `~/.local/state/herdr/plugins/kjozsa.tracks/app/models/`, and exits ten
minutes after the panel last asked for it, or when the binary is rebuilt (the panel starts the
new one within a minute). Chats keep the same state as in the classic plugin.

## Files

Plugin state lives in `~/.local/state/herdr/plugins/kjozsa.tracks/`: one file per chat with its
repos and pull requests, and the docked sidebar of each tab. herdr's startup hook removes files
of chats untouched for 30 days.

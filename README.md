# Tracks

A [herdr](https://herdr.dev) plugin that docks a sidebar on the right of every tab running a
coding agent and shows the tracks the agent's chat leaves behind: the git repos it changed,
their uncommitted and unpushed files with line counts, the commits it made, and the pull
requests it opened or is working on. Select a file or commit to see its diff next to the agent.

```
herdr-tracks main ↑1
  M src/sidebar.rs                     +268 -105
 ?? src/ansi.rs                              +80
 ↑  src/git.rs                           +32 -15
 ▾ #42 open ✓ dock the sidebar
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
| `herdr-tracks main ↑1 ↓2` | Repo, checked-out branch, commits ahead of / behind its upstream |
| ` ▸ #42 open ✓ dock the sidebar` | A pull request the chat opened or referred to: state, CI checks and title; expands into its commits |
| `  M`, `A `, `??` … | Uncommitted changes, as in `git status`, with lines added/removed |
| ` ↑ ` | Files changed by commits not pushed yet |
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

A repo is listed once the chat **changed** it: edited or wrote a file there, committed, or the
repo's git state moved after the chat first touched it. Repos the chat only read are not
listed, unless one of its pull requests belongs to them. Pull requests are those the chat
created with `gh pr create`, or referred to: a pull request URL in your message, omp's `pr://`
reads, or `gh pr view/diff/checkout/review/…`.

The bottom of the sidebar collects the **links** of the chat, newest first: web links in your
messages, in the agent's replies, and pages its tools read or fetched (not tool output). It
takes at most half of the pane; when more links than that are collected, the mouse wheel
scrolls them and the rule shows which are in view (`─ links 4–25 of 29`). Pull requests
already listed above are left out.

The repo list belongs to the chat and is kept across herdr restarts. The sidebar matches the
width of herdr's left sidebar (`[ui] sidebar_width`, `sidebar_min_width`, `sidebar_max_width`).

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

## Files

Plugin state lives in `~/.local/state/herdr/plugins/kjozsa.tracks/`: one file per chat with its
repos and pull requests, and the docked sidebar of each tab. herdr's startup hook removes files
of chats untouched for 30 days.

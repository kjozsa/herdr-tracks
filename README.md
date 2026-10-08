# Tracks

A [herdr](https://herdr.dev) plugin that docks a sidebar on the right of every tab running a
coding agent and shows the tracks the agent's chat leaves behind: the git repos it changed,
their uncommitted and unpushed files with line counts, and the pull requests it opened or is
working on. Select a file to see its diff next to the agent.

```
herdr-tracks main ↑1
 ▸ #42 open ✓
  M src/sidebar.rs                     +268 -105
 ?? src/ansi.rs                              +80
 ↑  src/git.rs                           +32 -15
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
| ` ▸ #42 open ✓` | A pull request the chat opened or referred to: state and CI checks |
| `  M`, `A `, `??` … | Uncommitted changes, as in `git status`, with lines added/removed |
| ` ↑ ` | Files changed by commits not pushed yet |

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
| `↓` `↑` (`j` `k`) | Select the next / previous file or pull request; its diff opens next to the agent |
| `→` `←` (`l` `h`) | Expand / collapse a pull request into its files |
| Click | Select a row and show its diff; on a pull request, also expand or collapse it |
| `o`, Ctrl+click | Open the selected pull request in the browser |
| Click on a link | Open it in the browser |
| Right-click | On a repo, pull request or link: dismiss it, or open the pull request / link in the browser |
| `Enter` | Move focus into the diff to scroll it |
| `Esc` | Close the diff |

In the diff pane: `↓` `↑`, PgDn/PgUp, space/`b`, `g`/`G`, mouse wheel; `q` or `Esc` closes it.
Diffs are formatted with [delta](https://github.com/dandavison/delta) when it is your git pager.

A dismissed repo returns when the chat touches it again, a dismissed link when the chat
mentions it again; a dismissed pull request stays hidden for that chat. The sidebar cannot be
closed in a tab with an agent: it docks again right away.
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

//! The sidebar pane: follows one agent chat session in its tab and lists the git repos that
//! session changed (edit/write/commit tool calls in its omp or Claude Code transcript, or a
//! repo's git state moving after the session first touched it), with branch + changed files.
//!
//! This module holds the state, the event loop and input handling; [`collect`] gathers the
//! data and [`render`] lays it out and draws it.

mod render;
mod source;

use crate::classic::diff;
use crate::classic::{dock, herdr, term};
use anyhow::{anyhow, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use crossterm::terminal;
use herdr_tracks::engine::git::Change;
use herdr_tracks::engine::patch::{CommitAt, DiffRequest};
use herdr_tracks::engine::{Target, Tracker};
use render::{Row, Screen};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;

/// Links scrolled per mouse-wheel notch.
const WHEEL_LINKS: usize = 3;

/// What the sidebar shows (the [`Tracker`]'s), and how.
#[derive(Default)]
struct Model {
    tracker: Tracker,
    /// The diff pane currently shown; the next diff replaces it.
    diff_pane: Option<String>,
    /// Row chosen with the arrow keys or a click; its diff is shown.
    selected: Option<Key>,
    /// Pull requests (by URL) expanded to list their commits, and commits (by hash) expanded
    /// to list their files.
    expanded: HashSet<String>,
    /// This sidebar's own pane id; names the request file its diff viewer follows.
    me: String,
    /// First rendered row on screen when the list is taller than the pane.
    scroll: usize,
    /// Right-click menu, while open.
    menu: Option<Menu>,
    /// The link or pull request under the mouse pointer, drawn underlined and bold so it reads
    /// as clickable.
    hover: Option<String>,
    /// How many of the newest links the links block is scrolled past.
    links_scroll: usize,
    /// Whether the selection was made with the keys: only then is it drawn highlighted (and
    /// kept in view). A clicked row needs no marking.
    keyboard: bool,
    /// The width to keep: a dragged one, or herdr's left-sidebar width.
    width: dock::WidthHold,
}

/// What a sidebar row is: selecting or clicking it shows its diff.
#[derive(Debug, Clone, PartialEq)]
enum Click {
    /// A changed file in a local checkout.
    File { root: PathBuf, change: Change },
    /// A pull request; expands to its commits.
    Pr { url: String },
    /// A repo header: all its uncommitted changes.
    Repo { root: PathBuf },
    /// A commit, listed on its own or (`under`) in an expanded pull request; expands to its
    /// files.
    Commit { commit: CommitAt, under: Option<String> },
    /// One file of an expanded commit.
    CommitFile { commit: CommitAt, path: String },
}

/// Identity of a selectable row that survives refreshes (a file's status code may change).
#[derive(Debug, Clone, PartialEq)]
enum Key {
    File(PathBuf, String),
    Pr(String),
    Repo(PathBuf),
    Commit(String),
    CommitFile(String, String),
}

impl Click {
    fn key(&self) -> Key {
        match self {
            Click::File { root, change } => Key::File(root.clone(), change.path.clone()),
            Click::Pr { url } => Key::Pr(url.clone()),
            Click::Repo { root } => Key::Repo(root.clone()),
            Click::Commit { commit, .. } => Key::Commit(commit.hash.clone()),
            Click::CommitFile { commit, path } => Key::CommitFile(commit.hash.clone(), path.clone()),
        }
    }

    fn diff_request(&self) -> DiffRequest {
        match self {
            Click::File { root, change } => DiffRequest::local(root, change),
            Click::Pr { url } => DiffRequest::Pr { url: url.clone() },
            Click::Repo { root } => DiffRequest::Worktree { root: root.clone() },
            Click::Commit { commit, .. } => DiffRequest::Commit { commit: commit.clone(), path: None },
            Click::CommitFile { commit, path } => DiffRequest::Commit { commit: commit.clone(), path: Some(path.clone()) },
        }
    }

    /// The page it opens in the browser: a pull request's, or a commit's on GitHub.
    fn url(&self) -> Option<String> {
        match self {
            Click::Pr { url } => Some(url.clone()),
            Click::Commit { commit, .. } | Click::CommitFile { commit, .. } => commit.url(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum MenuItem {
    /// Open the pull request or link in the browser.
    Open,
    Dismiss,
    DismissAllLinks,
}

/// An open right-click menu, anchored below (or above) the on-screen row it was opened on.
struct Menu {
    target: Target,
    row: usize,
}

impl Menu {
    /// Items with the on-screen rows they occupy in a `rows`-high pane.
    fn placed(&self, rows: usize) -> Vec<(usize, MenuItem, &'static str)> {
        let items: &[(MenuItem, &str)] = match self.target {
            Target::Repo(_) => &[(MenuItem::Dismiss, "dismiss repo")],
            Target::Pr(_) => &[(MenuItem::Open, "open on GitHub"), (MenuItem::Dismiss, "dismiss PR")],
            Target::Link(_) => &[
                (MenuItem::Open, "open in browser"),
                (MenuItem::Dismiss, "dismiss link"),
                (MenuItem::DismissAllLinks, "dismiss all links"),
            ],
            Target::Links => &[(MenuItem::DismissAllLinks, "dismiss all links")],
        };
        let start = if self.row + 1 + items.len() <= rows { self.row + 1 } else { self.row.saturating_sub(items.len()) };
        items.iter().enumerate().map(|(i, (item, label))| (start + i, *item, *label)).collect()
    }
}

pub fn run() -> Result<()> {
    let me = std::env::var("HERDR_PANE_ID").map_err(|_| anyhow!("HERDR_PANE_ID not set"))?;
    // herdr shows its own pane menu on right-click unless the pane asks for the clicks.
    herdr::forward_right_click(&me)?;
    let _term = term::Term::enter()?;
    event_loop(&me)
}

/// Runs for the pane's lifetime. There is no quit key: the sidebar is mandatory.
fn event_loop(me: &str) -> Result<()> {
    let mut model = Model { me: me.to_string(), ..Model::default() };
    let watched = dock::watch_width(me, &mut model.width);
    model.tracker.note(watched);
    let mut shown: (Vec<Row>, Option<usize>, Option<String>) = (Vec::new(), None, None);
    loop {
        refresh(&mut model);
        let (cols, rows) = terminal::size()?;
        let (cols, rows) = (usize::from(cols), usize::from(rows));
        let screen = render::render(&model, cols);
        let selected_row = screen.row_of(model.selected.as_ref()).filter(|_| model.keyboard);
        // The links block takes at most half the pane; the list scrolls above it.
        let mut links_scroll = model.links_scroll;
        let block = render::links(&model, cols, rows / 2, &mut links_scroll);
        model.links_scroll = links_scroll;
        let mut view = render::window(&screen, cols, rows - block.len(), &mut model.scroll, selected_row);
        view.pin(block, rows);
        if let Some(menu) = &model.menu {
            view.overlay(menu, cols, rows);
        }
        if (&view.rows, view.highlight, &model.hover) != (&shown.0, shown.1, &shown.2) {
            render::draw(&view, cols, model.hover.as_deref())?;
            shown = (view.rows.clone(), view.highlight, model.hover.clone());
        }
        if event::poll(model.tracker.next_due().saturating_duration_since(Instant::now()))? {
            let event = event::read()?;
            if matches!(event, Event::Resize(..)) {
                shown = (Vec::new(), None, None);
            }
            let result = handle_event(&mut model, &screen, &view, rows, event);
            model.tracker.note(result);
        }
    }
}

/// Refreshes the tracker, following the agent pane of this sidebar's tab.
fn refresh(model: &mut Model) {
    let Model { tracker, diff_pane, selected, me, .. } = model;
    tracker.refresh(|previous| {
        // A diff pane closed from its own side (`q`, or herdr) leaves nothing selected.
        if diff_pane.as_deref().is_some_and(|pane| !herdr::pane_exists(pane)) {
            *diff_pane = None;
            *selected = None;
        }
        source::find(me, previous)
    });
}

/// One key, click or resize. An open right-click menu takes the next click or key: an item
/// runs, anything else closes it; a right-click elsewhere opens a menu there instead.
fn handle_event(model: &mut Model, screen: &Screen, view: &render::View, rows: usize, event: Event) -> Result<()> {
    match (model.menu.take(), event) {
        (Some(menu), Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), row, .. })) => {
            match menu.placed(rows).into_iter().find(|(r, ..)| *r == usize::from(row)) {
                Some((_, item, _)) => run_menu(model, &menu.target, item),
                None => Ok(()),
            }
        }
        (Some(_), Event::Key(_) | Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Middle), .. })) => {
            Ok(())
        }
        (_, Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Right), row, .. })) => {
            let row = usize::from(row);
            model.menu = view.rows.get(row).and_then(|r| r.menu.clone()).map(|target| Menu { target, row });
            Ok(())
        }
        (None, Event::Key(KeyEvent { code, .. })) => {
            model.keyboard = true;
            handle_key(model, screen, code)
        }
        (None, Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), row, column, modifiers })) => {
            let Some(row) = view.rows.get(usize::from(row)) else { return Ok(()) };
            if let Some(url) = &row.link {
                return open_url(url);
            }
            match &row.click {
                Some(click) => {
                    model.keyboard = false;
                    let zone = render::zone(&row.line, usize::from(column));
                    handle_click(model, click, modifiers.contains(KeyModifiers::CONTROL), zone)
                }
                None => Ok(()),
            }
        }
        (menu, Event::Resize(..)) => {
            model.menu = menu;
            dock::hold_width(&model.me, &mut model.width)
        }
        // The wheel over the links block scrolls it (and closes a menu anchored to its rows).
        (_, Event::Mouse(MouseEvent { kind: kind @ (MouseEventKind::ScrollDown | MouseEventKind::ScrollUp), row, .. }))
            if view.links_top.is_some_and(|top| usize::from(row) >= top) =>
        {
            model.links_scroll = match kind {
                MouseEventKind::ScrollDown => model.links_scroll + WHEEL_LINKS,
                _ => model.links_scroll.saturating_sub(WHEEL_LINKS),
            };
            model.hover = None;
            Ok(())
        }
        (menu, Event::Mouse(MouseEvent { kind: MouseEventKind::Moved, row, column, .. })) => {
            model.menu = menu;
            model.hover = view.pointer_url(usize::from(row), usize::from(column)).map(str::to_string);
            Ok(())
        }
        // Mouse releases and drags, focus changes: an open menu stays open.
        (menu, _) => {
            model.menu = menu;
            Ok(())
        }
    }
}

/// ↑/↓ (`k`/`j`) select rows, →/← (`l`/`h`) expand and collapse pull requests and commits,
/// `o` opens the selected pull request or commit in the browser, Enter focuses the diff, Esc
/// closes it.
fn handle_key(model: &mut Model, screen: &Screen, code: KeyCode) -> Result<()> {
    let selected = model.selected.as_ref().and_then(|key| screen.clicks().find(|(_, c)| c.key() == *key)).map(|(_, c)| c.clone());
    match code {
        KeyCode::Down | KeyCode::Char('j') => match move_selection(model, screen, 1) {
            Some(request) => show_diff(model, &request),
            None => Ok(()),
        },
        KeyCode::Up | KeyCode::Char('k') => match move_selection(model, screen, -1) {
            Some(request) => show_diff(model, &request),
            None => Ok(()),
        },
        KeyCode::Right | KeyCode::Char('l') => {
            match selected {
                Some(Click::Pr { url }) => {
                    model.expanded.insert(url);
                }
                Some(Click::Commit { commit, .. }) => expand_commit(model, &commit),
                _ => {}
            }
            Ok(())
        }
        // Collapses the selected row, or else the one it is listed in, and selects that.
        KeyCode::Left | KeyCode::Char('h') => match selected {
            Some(Click::Pr { url }) => {
                model.expanded.remove(&url);
                Ok(())
            }
            Some(Click::Commit { commit, under }) => {
                if model.expanded.remove(&commit.hash) {
                    return Ok(());
                }
                let Some(url) = under else { return Ok(()) };
                model.expanded.remove(&url);
                handle_select(model, &Click::Pr { url })
            }
            Some(Click::CommitFile { commit, .. }) => {
                model.expanded.remove(&commit.hash);
                let under = screen.clicks().find_map(|(_, c)| match c {
                    Click::Commit { commit: c, under } if c.hash == commit.hash => Some(under.clone()),
                    _ => None,
                });
                handle_select(model, &Click::Commit { commit, under: under.flatten() })
            }
            _ => Ok(()),
        },
        KeyCode::Char('C') => model.tracker.dismiss(&Target::Links),
        KeyCode::Char('o') => match selected.and_then(|c| c.url()) {
            Some(url) => open_url(&url),
            None => Ok(()),
        },
        KeyCode::Enter => match &model.diff_pane {
            Some(pane) => herdr::pane_focus(pane),
            None => Ok(()),
        },
        KeyCode::Esc => {
            model.selected = None;
            match model.diff_pane.take() {
                Some(pane) => herdr::pane_close(&pane),
                None => Ok(()),
            }
        }
        _ => Ok(()),
    }
}

/// Moves the selection `step` rows down (or up), clamped to the list. Returns the newly
/// selected row's diff, or `None` when the selection did not move.
fn move_selection(model: &mut Model, screen: &Screen, step: isize) -> Option<DiffRequest> {
    let clicks: Vec<&Click> = screen.clicks().map(|(_, click)| click).collect();
    let last = clicks.len().checked_sub(1)?;
    let current = model.selected.as_ref().and_then(|key| clicks.iter().position(|click| click.key() == *key));
    let next = match current {
        Some(i) => i.saturating_add_signed(step).min(last),
        None if step > 0 => 0,
        None => last,
    };
    if Some(next) == current {
        return None;
    }
    let click = clicks[next];
    model.selected = Some(click.key());
    Some(click.diff_request())
}

/// Lists a commit's files under it, fetching them in the background the first time.
fn expand_commit(model: &mut Model, commit: &CommitAt) {
    model.expanded.insert(commit.hash.clone());
    model.tracker.fetch_files(commit);
}

fn run_menu(model: &mut Model, target: &Target, item: MenuItem) -> Result<()> {
    match (item, target) {
        (MenuItem::Open, Target::Pr(pr)) => open_url(&pr.url()),
        (MenuItem::Open, Target::Link(url)) => open_url(url),
        (MenuItem::Open, Target::Repo(_) | Target::Links) => Ok(()),
        (MenuItem::Dismiss, target) => model.tracker.dismiss(target),
        (MenuItem::DismissAllLinks, _) => model.tracker.dismiss(&Target::Links),
    }
}

/// Opens the URL in the desktop's browser without waiting for it.
fn open_url(url: &str) -> Result<()> {
    let mut opener = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("xdg-open")?;
    // Collect it when it exits, or every click would leave a zombie process behind.
    std::thread::spawn(move || opener.wait());
    Ok(())
}

/// A click on a row: its expand marker expands or collapses a pull request or commit; a pull
/// request's number and status open it in the browser; anywhere else selects the row and
/// shows its diff. Ctrl+click opens a pull request or commit in the browser.
fn handle_click(model: &mut Model, click: &Click, ctrl: bool, zone: render::Zone) -> Result<()> {
    if let (Some(url), true) = (click.url(), ctrl) {
        return open_url(&url);
    }
    match (click, zone) {
        (Click::Pr { url }, render::Zone::Marker) => {
            if !model.expanded.remove(url) {
                model.expanded.insert(url.clone());
            }
            Ok(())
        }
        (Click::Pr { url }, render::Zone::Link) => open_url(url),
        (Click::Commit { commit, .. }, render::Zone::Marker) => {
            if !model.expanded.remove(&commit.hash) {
                expand_commit(model, commit);
            }
            Ok(())
        }
        _ => handle_select(model, click),
    }
}

/// Selects the row and shows its diff.
fn handle_select(model: &mut Model, click: &Click) -> Result<()> {
    model.selected = Some(click.key());
    show_diff(model, &click.diff_request())
}

/// Shows a diff without taking focus from the sidebar: points the running diff viewer at it
/// (it redraws in place), or opens the viewer next to the agent.
fn show_diff(model: &mut Model, request: &DiffRequest) -> Result<()> {
    let file = diff::request_file(&model.me);
    herdr_tracks::engine::state::write_atomic(&file, &serde_json::to_vec(request)?)?;
    if model.diff_pane.as_deref().is_some_and(herdr::pane_exists) {
        return Ok(());
    }
    let agent = model.tracker.session.as_ref().map(|s| s.pane.clone()).ok_or_else(|| anyhow!("no agent pane"))?;
    let env = [(diff::ENV, file.display().to_string()), (diff::OWNER_ENV, model.me.clone())];
    model.diff_pane =
        Some(herdr::open_plugin_pane(herdr_tracks::PLUGIN_ID, crate::DIFF_ENTRYPOINT, &agent, false, &env)?);
    Ok(())
}

#[cfg(test)]
mod fixture {
    use super::render::Line;
    use super::Model;
    use herdr_tracks::engine::{fixture, Session};

    pub(super) use herdr_tracks::engine::fixture::{clean, gh, pr, root, view};

    /// A sidebar following [`fixture::fixture`]'s chat.
    pub(super) fn fixture(repos: &[&str], prs: &[herdr_tracks::engine::github::PrRef]) -> Model {
        Model { tracker: fixture::fixture(repos, prs), ..Model::default() }
    }

    pub(super) fn session(model: &mut Model) -> &mut Session {
        fixture::session(&mut model.tracker)
    }

    pub(super) fn text(line: &Line) -> String {
        line.iter().map(|(t, _)| t.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::fixture::{clean, fixture, pr, root, session};
    use herdr_tracks::engine::git::RepoStatus;
    use super::render::{links, render, window};

    #[test]
    fn arrows_walk_the_selectable_rows_and_stop_at_the_ends() {
        let change = |path: &str| Change { code: *b" M", path: path.into(), orig: None, stat: None };
        let mut model = fixture(&["app"], &[pr("app", 7)]);
        model.tracker.statuses.insert(root("app"), Ok(RepoStatus { changes: vec![change("a.rs"), change("b.rs")], ..clean() }));
        let screen = render(&model, 40);
        // The repo header comes first: it shows all uncommitted changes.
        assert_eq!(move_selection(&mut model, &screen, 1), Some(DiffRequest::Worktree { root: root("app") }));
        assert_eq!(move_selection(&mut model, &screen, 1), Some(DiffRequest::local(&root("app"), &change("a.rs"))));
        move_selection(&mut model, &screen, 1);
        assert_eq!(model.selected, Some(Key::File(root("app"), "b.rs".into())));
        assert_eq!(move_selection(&mut model, &screen, 1), Some(DiffRequest::Pr { url: pr("app", 7).url() }));
        assert_eq!(move_selection(&mut model, &screen, 1), None, "already on the last row");
        assert_eq!(move_selection(&mut model, &screen, -1), Some(DiffRequest::local(&root("app"), &change("b.rs"))));
    }

    #[test]
    fn link_rows_hover_scroll_and_dismiss() {
        let mut model = fixture(&[], &[]);
        let (new, mid, old) = ("https://www.new.io/a", "https://mid.io/b", "https://old.io/c");
        session(&mut model).links = [new, mid, old].map(String::from).into();
        let screen = render(&model, 30);
        let block = links(&model, 30, 3, &mut 0);
        let mut view = window(&screen, 30, 10 - block.len(), &mut 0, None);
        view.pin(block, 10);
        let rule = view.links_top.unwrap();
        let mouse = |kind, row| Event::Mouse(MouseEvent { kind, column: 3, row, modifiers: KeyModifiers::NONE });
        let (new_row, mid_row) = (rule as u16 + 1, rule as u16 + 2);

        // A link row right-clicks to its own menu; dismissing the link lets the next one move up.
        handle_event(&mut model, &screen, &view, 10, mouse(MouseEventKind::Down(MouseButton::Right), new_row)).unwrap();
        let menu = model.menu.take().unwrap();
        assert_eq!(menu.target, Target::Link(new.into()));
        let items: Vec<MenuItem> = menu.placed(10).into_iter().map(|(_, item, _)| item).collect();
        assert_eq!(items, [MenuItem::Open, MenuItem::Dismiss, MenuItem::DismissAllLinks]);
        run_menu(&mut model, &menu.target, MenuItem::Dismiss).unwrap();
        let urls: Vec<String> = links(&model, 30, 3, &mut 0).into_iter().filter_map(|r| r.link).collect();
        assert_eq!(urls, [mid, old]);

        // Moving the pointer onto a link row hovers that link; anywhere else clears it, and an
        // open menu stays open meanwhile.
        handle_event(&mut model, &screen, &view, 10, mouse(MouseEventKind::Moved, mid_row)).unwrap();
        assert_eq!(model.hover.as_deref(), Some(mid));
        model.menu = Some(Menu { target: Target::Link(mid.into()), row: usize::from(mid_row) });
        handle_event(&mut model, &screen, &view, 10, mouse(MouseEventKind::Moved, 0)).unwrap();
        assert_eq!(model.hover, None);
        assert!(model.menu.is_some());

        // The wheel scrolls the links block, not the list above it.
        handle_event(&mut model, &screen, &view, 10, mouse(MouseEventKind::ScrollDown, rule as u16 - 1)).unwrap();
        assert_eq!(model.links_scroll, 0, "above the links block");
        handle_event(&mut model, &screen, &view, 10, mouse(MouseEventKind::ScrollDown, mid_row)).unwrap();
        assert_eq!(model.links_scroll, WHEEL_LINKS);
        handle_event(&mut model, &screen, &view, 10, mouse(MouseEventKind::ScrollUp, mid_row)).unwrap();
        assert_eq!(model.links_scroll, 0);

        // `C`, or the rule's right-click menu, dismisses all links.
        handle_event(&mut model, &screen, &view, 10, mouse(MouseEventKind::Down(MouseButton::Right), rule as u16)).unwrap();
        let menu = model.menu.take().unwrap();
        assert_eq!(menu.target, Target::Links);
        assert_eq!(menu.placed(10).into_iter().map(|(_, item, _)| item).collect::<Vec<_>>(), [MenuItem::DismissAllLinks]);
        handle_event(&mut model, &screen, &view, 10, Event::Key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT))).unwrap();
        assert!(links(&model, 30, 3, &mut 0).is_empty());
    }

    #[test]
    fn menus_open_next_to_their_row_and_dismiss_what_they_were_opened_on() {
        let mut model = fixture(&["mine", "e2e"], &[pr("e2e", 36), pr("infra", 133)]);
        session(&mut model).saved.repos[1].changed = false;
        // The menu opens below the clicked row, or above it at the bottom of the pane.
        let below = Menu { target: Target::Pr(pr("infra", 133)), row: 3 };
        assert_eq!(below.placed(10).iter().map(|(r, i, _)| (*r, *i)).collect::<Vec<_>>(), [(4, MenuItem::Open), (5, MenuItem::Dismiss)]);
        let above = Menu { target: Target::Pr(pr("infra", 133)), row: 9 };
        assert_eq!(above.placed(10).iter().map(|(r, ..)| *r).collect::<Vec<_>>(), [7, 8]);

        // Dismissing #36 also hides its repo, which was only shown for that PR; dismissing a
        // repo hides it even though the chat changed it.
        run_menu(&mut model, &Target::Pr(pr("E2E", 36)), MenuItem::Dismiss).unwrap();
        run_menu(&mut model, &Target::Repo(root("mine")), MenuItem::Dismiss).unwrap();
        let screen = render(&model, 40);
        let menus: Vec<&Target> = screen.rows.iter().filter_map(|r| r.menu.as_ref()).collect();
        assert_eq!(menus, [&Target::Pr(pr("infra", 133)), &Target::Pr(pr("infra", 133))]);
    }
}

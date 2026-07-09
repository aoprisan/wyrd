//! An interactive terminal UI over a recording.
//!
//! `wyrd tui <recording>` ingests a `.wyrd` file and presents its async
//! causality interactively: the same [`Stats`], [`WorldState`], and
//! [`BlockedReport`] the one-shot subcommands print, but browsable — pick a
//! task and see *why it is blocked*, and scrub a time cursor across the
//! recording to watch task/resource state evolve.
//!
//! With `--follow`, it re-reads the file on an interval (like `tail -f`) so you
//! can watch a *running* app's async state live. This is deliberately the
//! zero-producer-overhead design: the recorded program is untouched — it just
//! keeps appending frames to the file as always, and all the folding /
//! rendering cost lives here, in a separate process. The trade-off is latency
//! (bounded by the writer's flush cadence) and that history is whatever the
//! file holds; it does not attach to the process or read its memory.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Gauge, Padding, Paragraph, Row, Table, TableState, Tabs, Wrap,
};
use ratatui::{DefaultTerminal, Frame};

use wyrd_core::model::{BlockedOutcome, BlockedReport, Stats, TaskState, TaskStatus, WorldState};
use wyrd_core::{Recording, TaskId};

use crate::follow::load_follow;
use crate::render::ms;

/// How often `--follow` re-folds the growing recording. Doubles as the input
/// poll timeout, so the UI stays responsive between ticks.
const REFRESH: Duration = Duration::from_millis(250);

/// Entry point for the `tui` subcommand.
///
/// With `follow`, the recording is re-read on an interval (like `tail -f`): the
/// producer is never touched — this side just re-folds whatever complete frames
/// are on disk so far.
pub fn run(file: &Path, top: usize, follow: bool) -> Result<(), Box<dyn std::error::Error>> {
    let (rec, follow_path) = if follow {
        (load_follow(file), Some(file.to_path_buf()))
    } else {
        // One-shot: ingest strictly, so a corrupt file is a hard error.
        (Recording::open(file)?, None)
    };
    let app = App::new(rec, top, follow_path)?;
    let mut terminal = ratatui::init();
    let res = app.run(&mut terminal);
    ratatui::restore();
    res
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Stats,
    Tasks,
    Tree,
    Resources,
    WhyBlocked,
}

impl Tab {
    const ALL: [Tab; 5] = [
        Tab::Stats,
        Tab::Tasks,
        Tab::Tree,
        Tab::Resources,
        Tab::WhyBlocked,
    ];

    fn title(self) -> &'static str {
        match self {
            Tab::Stats => "Stats",
            Tab::Tasks => "Tasks",
            Tab::Tree => "Tree",
            Tab::Resources => "Resources",
            Tab::WhyBlocked => "Why-blocked",
        }
    }

    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    fn next(self) -> Tab {
        Tab::ALL[(self.index() + 1) % Tab::ALL.len()]
    }

    fn prev(self) -> Tab {
        Tab::ALL[(self.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]
    }
}

struct App {
    rec: Recording,
    /// When set, re-fold this file on every tick (`--follow`).
    follow: Option<PathBuf>,
    /// Time cursor pinned to the growing tail: `at` tracks `end_ts` as it grows.
    /// Scrubbing unpins it; `G`/End re-pins.
    live: bool,
    /// In follow mode, keep the Why-blocked view on the currently most-stuck
    /// task as the world changes — until the user steers with ↑/↓.
    auto_select: bool,
    /// How many longest-parks the Stats tab requests (kept for reloads).
    top: usize,
    /// A transient note for the footer, e.g. "waiting for data".
    status: Option<String>,
    /// Last timestamp of the recording (right edge of the time cursor).
    end_ts: u64,
    /// Current query time; everything but `Stats` is evaluated here.
    at: u64,
    stats: Stats,
    tab: Tab,
    /// World snapshot at `at`, recomputed whenever `at` moves.
    world: WorldState,
    /// Selected task, tracked by id so it survives a time scrub.
    sel_task: Option<TaskId>,
    /// `why_blocked` for `sel_task` at `at`; recomputed on selection/time change.
    blocked: Option<BlockedReport>,
}

impl App {
    fn new(
        rec: Recording,
        top: usize,
        follow: Option<PathBuf>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let end_ts = rec.end_ts()?;
        let stats = rec.stats(top)?;
        let world = rec.world_state(Some(end_ts))?;
        let sel_task = rec.pick_blocked_task(Some(end_ts))?;
        let mut app = App {
            rec,
            follow,
            live: true,
            auto_select: true,
            top,
            status: None,
            end_ts,
            at: end_ts,
            stats,
            tab: Tab::Stats,
            world,
            sel_task,
            blocked: None,
        };
        app.refresh_blocked();
        Ok(app)
    }

    fn run(mut self, terminal: &mut DefaultTerminal) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            terminal.draw(|f| self.draw(f))?;
            // No key within REFRESH → a follow tick (or just loop again).
            if !event::poll(REFRESH)? {
                if self.follow.is_some() {
                    self.reload();
                }
                continue;
            }
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Right | KeyCode::Char('l') => self.tab = self.tab.next(),
                    KeyCode::Left | KeyCode::Char('h') => self.tab = self.tab.prev(),
                    KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
                    KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
                    KeyCode::Enter => {
                        if self.tab == Tab::Tasks {
                            self.tab = Tab::WhyBlocked;
                        }
                    }
                    KeyCode::Char(']') => self.scrub(self.step()),
                    KeyCode::Char('[') => self.scrub(-self.step()),
                    KeyCode::Char('G') | KeyCode::End => {
                        // Jump to the live tail and resume auto-tracking.
                        self.live = true;
                        self.auto_select = true;
                        self.set_at(self.end_ts);
                    }
                    KeyCode::Char('g') | KeyCode::Home => {
                        self.live = false;
                        self.set_at(0);
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Re-fold the followed recording. Cheap to call: it rebuilds an in-memory
    /// snapshot from the file's complete frames and re-derives the views. The
    /// producer is untouched — all of this cost is in the observer.
    fn reload(&mut self) {
        let Some(path) = self.follow.clone() else {
            return;
        };
        let rec = load_follow(&path);
        let Ok(end_ts) = rec.end_ts() else {
            return; // transient read; keep the last good snapshot
        };
        self.rec = rec;
        self.end_ts = end_ts;
        if let Ok(stats) = self.rec.stats(self.top) {
            self.stats = stats;
        }
        // Follow the tail when live; otherwise stay put, but never point past
        // the (possibly shorter) new end.
        if self.live || self.at > end_ts {
            self.at = end_ts;
        }
        if let Ok(world) = self.rec.world_state(Some(self.at)) {
            self.world = world;
        }
        // In live+auto mode, keep pointing at whatever is most stuck right now;
        // otherwise only re-pick if the selected task has vanished.
        let sel_valid = self
            .sel_task
            .is_some_and(|id| self.world.tasks.iter().any(|t| t.ident.id == id));
        if (self.live && self.auto_select) || !sel_valid {
            self.sel_task = self
                .rec
                .pick_blocked_task(Some(self.at))
                .ok()
                .flatten()
                .or_else(|| self.world.tasks.first().map(|t| t.ident.id));
        }
        self.refresh_blocked();
        self.status = self
            .world
            .tasks
            .is_empty()
            .then(|| "waiting for recording data…".to_string());
    }

    /// A time step for the scrubber: ~2% of the recording, at least 1ns.
    fn step(&self) -> i128 {
        (self.end_ts / 50).max(1) as i128
    }

    fn scrub(&mut self, delta: i128) {
        let next = (self.at as i128 + delta).clamp(0, self.end_ts as i128) as u64;
        // Scrubbing back unpins the live tail; scrubbing all the way forward
        // to the end re-pins it.
        self.live = next == self.end_ts;
        self.set_at(next);
    }

    fn set_at(&mut self, at: u64) {
        if at == self.at {
            return;
        }
        self.at = at;
        // Fold to the new instant; a query failure leaves the old view in place.
        if let Ok(world) = self.rec.world_state(Some(at)) {
            self.world = world;
        }
        // Keep the selection valid at the new time.
        if !self
            .sel_task
            .is_some_and(|id| self.world.tasks.iter().any(|t| t.ident.id == id))
        {
            self.sel_task = self.world.tasks.first().map(|t| t.ident.id);
        }
        self.refresh_blocked();
    }

    fn sel_index(&self) -> Option<usize> {
        let id = self.sel_task?;
        self.world.tasks.iter().position(|t| t.ident.id == id)
    }

    fn move_sel(&mut self, delta: isize) {
        // Selection only drives the Tasks list.
        if self.tab != Tab::Tasks || self.world.tasks.is_empty() {
            return;
        }
        // The user is steering: stop auto-tracking the most-stuck task.
        self.auto_select = false;
        let len = self.world.tasks.len() as isize;
        let cur = self.sel_index().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, len - 1) as usize;
        self.sel_task = Some(self.world.tasks[next].ident.id);
        self.refresh_blocked();
    }

    fn refresh_blocked(&mut self) {
        self.blocked = match self.sel_task {
            Some(id) => self.rec.why_blocked(id, Some(self.at)).ok(),
            None => None,
        };
    }

    // ---- rendering ----------------------------------------------------

    fn draw(&self, f: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .areas(f.area());

        self.draw_tabs(f, header);
        match self.tab {
            Tab::Stats => self.draw_stats(f, body),
            Tab::Tasks => self.draw_tasks(f, body),
            Tab::Tree => self.draw_tree(f, body),
            Tab::Resources => self.draw_resources(f, body),
            Tab::WhyBlocked => self.draw_blocked(f, body),
        }
        self.draw_footer(f, footer);
    }

    fn draw_tabs(&self, f: &mut Frame, area: Rect) {
        // Title carries the follow indicator: a green ● when tracking the live
        // tail, a yellow ⏸ when the user has scrubbed away from it.
        let title: Line = match (self.follow.is_some(), self.live) {
            (false, _) => " wyrd ".bold().into(),
            (true, true) => Line::from(vec![" wyrd ".bold(), "● live ".fg(Color::Green).bold()]),
            (true, false) => {
                Line::from(vec![" wyrd ".bold(), "⏸ frozen ".fg(Color::Yellow).bold()])
            }
        };
        let titles = Tab::ALL.iter().map(|t| t.title());
        let tabs = Tabs::new(titles)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan).bold())
            .select(self.tab.index());
        f.render_widget(tabs, area);
    }

    fn draw_stats(&self, f: &mut Frame, area: Rect) {
        let s = &self.stats;
        let mut lines: Vec<Line> = vec![
            kv("recording span", ms(s.duration_ns)),
            kv("tasks", s.task_count.to_string()),
            kv("resources", s.resource_count.to_string()),
            Line::raw(""),
            kv(
                "poll time",
                format!(
                    "n={} p50={} p90={} p99={} max={}",
                    s.poll_time.count,
                    ms(s.poll_time.p50),
                    ms(s.poll_time.p90),
                    ms(s.poll_time.p99),
                    ms(s.poll_time.max),
                ),
            ),
        ];

        if !s.longest_parks.is_empty() {
            lines.push(Line::raw(""));
            lines.push("longest parks".bold().into());
            for p in &s.longest_parks {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:>10}  ", ms(p.dur_ns)),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::raw(format!(
                        "{} on {} [{}]",
                        p.task.label(),
                        p.resource.label(),
                        p.op_name
                    )),
                ]));
            }
        }

        if !s.channel_depths.is_empty() {
            lines.push(Line::raw(""));
            lines.push("channel depths".bold().into());
            for c in &s.channel_depths {
                lines.push(Line::raw(format!(
                    "  {} peak {}/{}",
                    c.resource.label(),
                    c.max_depth,
                    c.capacity,
                )));
            }
        }

        let p = Paragraph::new(lines)
            .block(titled(" overview (whole recording) "))
            .wrap(Wrap { trim: false });
        f.render_widget(p, area);
    }

    fn draw_tasks(&self, f: &mut Frame, area: Rect) {
        let rows = self.world.tasks.iter().map(|t| {
            let (status, style) = status_cell(&t.status, &self.world);
            Row::new(vec![
                Span::raw(t.ident.label()),
                Span::styled(status, style),
            ])
        });
        let table = Table::new(
            rows,
            [Constraint::Percentage(55), Constraint::Percentage(45)],
        )
        .header(
            Row::new(vec!["task", "status"])
                .style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)),
        )
        .block(titled(format!(" tasks @ t={} ", ms(self.at))))
        .row_highlight_style(Style::default().bg(Color::Cyan).fg(Color::Black).bold())
        .highlight_symbol("▶ ");

        let mut state = TableState::default();
        state.select(self.sel_index());
        f.render_stateful_widget(table, area, &mut state);
    }

    fn draw_tree(&self, f: &mut Frame, area: Rect) {
        let lines = tree_lines(&self.world);
        let p = Paragraph::new(lines)
            .block(titled(format!(" spawn tree @ t={} ", ms(self.at))))
            .wrap(Wrap { trim: false });
        f.render_widget(p, area);
    }

    fn draw_resources(&self, f: &mut Frame, area: Rect) {
        let rows = self.world.resources.iter().map(|r| {
            let holder = r
                .holder
                .map(|h| task_label(h, &self.world))
                .unwrap_or_else(|| "—".to_string());
            let locked = match r.locked {
                Some(true) => "locked",
                Some(false) => "free",
                None => "",
            };
            let depth = match (r.depth, r.capacity) {
                (Some(d), Some(c)) => format!("{d}/{c}"),
                (Some(d), None) => d.to_string(),
                _ => String::new(),
            };
            Row::new(vec![r.ident.label(), holder, locked.to_string(), depth])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Percentage(45),
                Constraint::Percentage(30),
                Constraint::Length(8),
                Constraint::Min(6),
            ],
        )
        .header(
            Row::new(vec!["resource", "holder", "lock", "depth"])
                .style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)),
        )
        .block(titled(format!(" resources @ t={} ", ms(self.at))));
        f.render_widget(table, area);
    }

    fn draw_blocked(&self, f: &mut Frame, area: Rect) {
        let lines = match &self.blocked {
            Some(report) => blocked_lines(report, &self.world),
            None => vec![Line::from(
                "select a task on the Tasks tab (↑/↓, Enter)".italic(),
            )],
        };
        let title = match self.sel_task {
            Some(id) => format!(
                " why-blocked: {} @ t={} ",
                task_label(id, &self.world),
                ms(self.at)
            ),
            None => " why-blocked ".to_string(),
        };
        let p = Paragraph::new(lines)
            .block(titled(title))
            .wrap(Wrap { trim: false });
        f.render_widget(p, area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let [gauge_area, hint_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

        let ratio = if self.end_ts == 0 {
            1.0
        } else {
            self.at as f64 / self.end_ts as f64
        };
        let label = if self.follow.is_some() && self.live {
            format!("t = {} / {}  (now)", ms(self.at), ms(self.end_ts))
        } else {
            format!("t = {} / {}", ms(self.at), ms(self.end_ts))
        };
        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(Color::Cyan))
            .ratio(ratio)
            .label(label);
        f.render_widget(gauge, gauge_area);

        // A transient follow note (e.g. "waiting for data") preempts the hints.
        let hints = match &self.status {
            Some(note) => Line::from(note.clone().fg(Color::Yellow).italic()),
            None => Line::from(vec![
                key("◂ ▸"),
                Span::raw(" tabs  "),
                key("↑ ↓"),
                Span::raw(" select  "),
                key("↵"),
                Span::raw(" why-blocked  "),
                key("[ ]"),
                Span::raw(" scrub  "),
                key("g/G"),
                Span::raw(if self.follow.is_some() {
                    " start/live  "
                } else {
                    " start/end  "
                }),
                key("q"),
                Span::raw(" quit"),
            ]),
        };
        f.render_widget(Paragraph::new(hints), hint_area);
    }
}

// ---- small view helpers -----------------------------------------------

fn titled(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .title(title.into().bold())
}

fn kv(k: &str, v: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{k:<16}"), Style::default().fg(Color::DarkGray)),
        Span::raw(v),
    ])
}

fn key(k: &str) -> Span<'static> {
    Span::styled(k.to_string(), Style::default().fg(Color::Cyan).bold())
}

/// Resolve a task id to its display label within a world snapshot.
fn task_label(id: TaskId, world: &WorldState) -> String {
    world
        .tasks
        .iter()
        .find(|t| t.ident.id == id)
        .map(|t| t.ident.label())
        .unwrap_or_else(|| format!("task#{id}"))
}

/// A task's status rendered as a labelled, coloured cell.
fn status_cell(status: &TaskStatus, world: &WorldState) -> (String, Style) {
    match status {
        TaskStatus::Running => ("running".into(), Style::default().fg(Color::Green)),
        TaskStatus::Idle => ("idle".into(), Style::default().fg(Color::DarkGray)),
        TaskStatus::Done => ("done".into(), Style::default().fg(Color::Blue)),
        TaskStatus::Parked { resource } => {
            let res = world
                .resources
                .iter()
                .find(|r| r.ident.id == *resource)
                .map(|r| r.ident.label())
                .unwrap_or_else(|| format!("resource#{resource}"));
            (
                format!("parked on {res}"),
                Style::default().fg(Color::Yellow),
            )
        }
    }
}

/// Render the spawn tree at the cursor time: parents indent their children
/// (box-drawing branches), each task with its coloured status. Tasks whose
/// parent is unknown or absent from the snapshot are roots, in spawn order.
fn tree_lines(world: &WorldState) -> Vec<Line<'static>> {
    use std::collections::{HashMap, HashSet};

    if world.tasks.is_empty() {
        return vec![Line::from("no tasks at this time".italic())];
    }
    let present: HashSet<TaskId> = world.tasks.iter().map(|t| t.ident.id).collect();
    let mut children: HashMap<TaskId, Vec<&TaskState>> = HashMap::new();
    let mut roots: Vec<&TaskState> = Vec::new();
    // world.tasks is spawn-ordered, so sibling order is spawn order.
    for t in &world.tasks {
        match t.parent.filter(|p| present.contains(p) && *p != t.ident.id) {
            Some(p) => children.entry(p).or_default().push(t),
            None => roots.push(t),
        }
    }

    let mut lines = Vec::new();
    let mut visited: HashSet<TaskId> = HashSet::new();
    for root in roots {
        push_tree(root, "", "", &children, world, &mut visited, &mut lines);
    }
    lines
}

fn push_tree(
    t: &TaskState,
    lead: &str,
    branch: &str,
    children: &std::collections::HashMap<TaskId, Vec<&TaskState>>,
    world: &WorldState,
    visited: &mut std::collections::HashSet<TaskId>,
    lines: &mut Vec<Line<'static>>,
) {
    if !visited.insert(t.ident.id) {
        return; // defensive: a malformed recording could claim a parent cycle
    }
    let (status, style) = status_cell(&t.status, world);
    lines.push(Line::from(vec![
        Span::raw(format!("{lead}{branch}")),
        Span::styled(t.ident.label(), Style::default().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled(status, style),
    ]));
    let kids: &[&TaskState] = children.get(&t.ident.id).map_or(&[], Vec::as_slice);
    for (i, kid) in kids.iter().enumerate() {
        // Roots carry no branch glyph, so their children need no continuation
        // rail either; below a "├─" the rail continues, below a "└─" it stops.
        let child_lead = if branch.is_empty() {
            lead.to_string()
        } else if branch.starts_with('└') {
            format!("{lead}   ")
        } else {
            format!("{lead}│  ")
        };
        let child_branch = if i + 1 == kids.len() {
            "└─ "
        } else {
            "├─ "
        };
        push_tree(
            kid,
            &child_lead,
            child_branch,
            children,
            world,
            visited,
            lines,
        );
    }
}

/// Build the styled why-blocked view: a headline, the park → holder chain, and
/// (for a deadlock) the cycle summary. Mirrors `render::render_blocked`.
fn blocked_lines(report: &BlockedReport, world: &WorldState) -> Vec<Line<'static>> {
    let head = report
        .chain
        .first()
        .map(|l| l.task.label())
        .unwrap_or_else(|| format!("task#{}", report.task));

    let mut lines: Vec<Line> = Vec::new();
    match &report.outcome {
        BlockedOutcome::NotBlocked => {
            lines.push(Line::from(
                format!("✓ {head} is not blocked at t={}ns.", report.at)
                    .fg(Color::Green)
                    .bold(),
            ));
            return lines;
        }
        BlockedOutcome::Deadlock { cycle } => {
            lines.push(Line::from(
                format!("⛔ DEADLOCK — {head} is in a {}-task cycle:", cycle.len())
                    .fg(Color::Red)
                    .bold(),
            ));
        }
        BlockedOutcome::ResourceRoot { .. } => {
            let root = report
                .chain
                .last()
                .map(|l| l.waiting_on.label())
                .unwrap_or_default();
            lines.push(Line::from(
                format!("⏳ {head} is blocked; root cause is {root} (no tracked holder — timer, full channel, or external):")
                    .fg(Color::Yellow),
            ));
        }
        BlockedOutcome::ActiveHolder { .. } => {
            lines.push(Line::from(
                format!("⏳ {head} is blocked behind an active (running/idle) holder:")
                    .fg(Color::Yellow),
            ));
        }
    }
    lines.push(Line::raw(""));

    for (i, link) in report.chain.iter().enumerate() {
        let arrow = if i == 0 { "  " } else { "  ↳ " };
        let holder = match &link.holder {
            Some(h) => format!("held by {}", h.label()),
            None => "no holder (channel full / timer / external)".to_string(),
        };
        lines.push(Line::from(vec![
            Span::raw(arrow.to_string()),
            Span::styled(link.task.label(), Style::default().fg(Color::Cyan)),
            Span::raw(format!(
                "  --[{}, parked {}]-->  ",
                link.op_name,
                ms(link.wait_ns)
            )),
            Span::styled(link.waiting_on.label(), Style::default().fg(Color::Magenta)),
            Span::raw(format!("  ({holder})")),
        ]));
    }

    if let BlockedOutcome::Deadlock { cycle } = &report.outcome {
        let names: Vec<String> = report
            .chain
            .iter()
            .filter(|l| cycle.contains(&l.task.id))
            .map(|l| l.task.label())
            .collect();
        lines.push(Line::raw(""));
        lines.push(Line::from(
            format!("cycle: {} → (back to start)", names.join(" → ")).fg(Color::Red),
        ));
        lines.push(Line::from("resources involved:".bold()));
        for link in report.chain.iter().filter(|l| cycle.contains(&l.task.id)) {
            lines.push(Line::raw(format!(
                "  • {} at {}",
                link.waiting_on.concrete_type, link.waiting_on.loc
            )));
        }
    }

    let _ = world; // reserved for future holder cross-referencing
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::follow::recording_from_bytes;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use wyrd_weave::{Event, FrameWriter, Loc, Record, StateOp, TaskKind, FIELD_ACQUIRED_BY};

    /// The canonical two-mutex deadlock: t1 holds A wants B, t2 holds B wants A.
    fn deadlock_recording() -> Recording {
        Recording::from_records(deadlock_records()).expect("ingest synthetic recording")
    }

    /// The same recording as a raw record stream, for the on-disk/follow tests.
    fn deadlock_records() -> Vec<Record> {
        let loc = |line: u32| Loc {
            file: Some("src/main.rs".into()),
            line: Some(line),
            col: None,
        };
        let events = vec![
            (
                1,
                Event::ResourceNew {
                    id: 100,
                    parent: None,
                    concrete_type: "Mutex".into(),
                    loc: loc(10),
                    is_internal: false,
                },
            ),
            (
                2,
                Event::ResourceNew {
                    id: 200,
                    parent: None,
                    concrete_type: "Mutex".into(),
                    loc: loc(20),
                    is_internal: false,
                },
            ),
            (
                3,
                Event::TaskSpawn {
                    id: 1,
                    parent: None,
                    name: Some("t1".into()),
                    loc: loc(1),
                    kind: TaskKind::Task,
                },
            ),
            (
                4,
                Event::TaskSpawn {
                    id: 2,
                    parent: None,
                    name: Some("t2".into()),
                    loc: loc(1),
                    kind: TaskKind::Task,
                },
            ),
            (5, Event::PollStart { task: 1 }),
            (
                6,
                Event::ResourceState {
                    id: 100,
                    field: FIELD_ACQUIRED_BY.into(),
                    value: 1,
                    op: StateOp::Override,
                },
            ),
            (7, Event::PollEnd { task: 1 }),
            (8, Event::PollStart { task: 2 }),
            (
                9,
                Event::ResourceState {
                    id: 200,
                    field: FIELD_ACQUIRED_BY.into(),
                    value: 2,
                    op: StateOp::Override,
                },
            ),
            (10, Event::PollEnd { task: 2 }),
            (11, Event::PollStart { task: 1 }),
            (
                12,
                Event::Park {
                    task: 1,
                    resource: 200,
                    op_name: "poll_acquire".into(),
                },
            ),
            (13, Event::PollEnd { task: 1 }),
            (14, Event::PollStart { task: 2 }),
            (
                15,
                Event::Park {
                    task: 2,
                    resource: 100,
                    op_name: "poll_acquire".into(),
                },
            ),
            (16, Event::PollEnd { task: 2 }),
        ];
        events
            .into_iter()
            .map(|(ts, event)| Record { ts, event })
            .collect()
    }

    /// Serialize records into the on-disk frame format (header + frames).
    fn to_frames(records: &[Record]) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut w = FrameWriter::new(&mut buf).expect("header");
        for r in records {
            w.write_record(r).expect("frame");
        }
        w.flush().expect("flush");
        buf
    }

    /// Flatten a rendered frame into a single searchable string.
    fn render(app: &App) -> String {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(|f| app.draw(f)).expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn stats_tab_shows_overview() {
        let app = App::new(deadlock_recording(), 10, None).unwrap();
        let text = render(&app);
        assert!(text.contains("wyrd"), "tab bar present");
        assert!(text.contains("recording span"));
        assert!(text.contains("tasks"));
    }

    #[test]
    fn tasks_tab_lists_named_tasks() {
        let mut app = App::new(deadlock_recording(), 10, None).unwrap();
        app.tab = Tab::Tasks;
        let text = render(&app);
        assert!(text.contains("t1"));
        assert!(text.contains("t2"));
        // Both tasks are parked on a mutex at the end of the recording.
        assert!(text.contains("parked on"));
    }

    #[test]
    fn why_blocked_tab_names_the_deadlock() {
        // Default selection lands on a task blocked behind another — the cycle.
        let mut app = App::new(deadlock_recording(), 10, None).unwrap();
        app.tab = Tab::WhyBlocked;
        let text = render(&app);
        assert!(text.contains("DEADLOCK"), "got: {text}");
        assert!(text.contains("cycle"));
    }

    #[test]
    fn tree_tab_indents_children_under_parents() {
        let mut records = deadlock_records();
        records.push(Record {
            ts: 17,
            event: Event::TaskSpawn {
                id: 3,
                parent: Some(1),
                name: Some("child-of-t1".into()),
                loc: Loc {
                    file: Some("src/main.rs".into()),
                    line: Some(2),
                    col: None,
                },
                kind: TaskKind::Task,
            },
        });
        let rec = Recording::from_records(records).expect("ingest");
        let mut app = App::new(rec, 10, None).unwrap();
        app.tab = Tab::Tree;
        let text = render(&app);
        assert!(text.contains("spawn tree"), "got: {text}");
        // The child hangs off t1 with a branch glyph; the roots carry none.
        assert!(text.contains("└─ child-of-t1"), "got: {text}");
        assert!(text.contains("t1") && text.contains("t2"));
    }

    #[test]
    fn scrub_clamps_to_recording_bounds() {
        let mut app = App::new(deadlock_recording(), 10, None).unwrap();
        app.set_at(0);
        assert_eq!(app.at, 0);
        app.scrub(-1_000); // cannot go below zero
        assert_eq!(app.at, 0);
        app.set_at(app.end_ts);
        app.scrub(1_000_000); // cannot exceed the end
        assert_eq!(app.at, app.end_ts);
    }

    #[test]
    fn tab_navigation_wraps() {
        assert_eq!(Tab::Stats.prev(), Tab::WhyBlocked);
        assert_eq!(Tab::WhyBlocked.next(), Tab::Stats);
    }

    #[test]
    fn follow_loader_reads_complete_frames() {
        let records = deadlock_records();
        let rec = recording_from_bytes(&to_frames(&records));
        // Same content as ingesting the records directly.
        assert_eq!(rec.end_ts().unwrap(), 16);
        assert_eq!(rec.world_state(None).unwrap().tasks.len(), 2);
    }

    #[test]
    fn follow_loader_tolerates_a_torn_tail_frame() {
        let records = deadlock_records();
        let mut bytes = to_frames(&records);
        // Simulate the writer being caught mid-frame: a length prefix claiming
        // more bytes than actually follow.
        bytes.extend_from_slice(&999u32.to_le_bytes());
        bytes.extend_from_slice(b"\x01\x02\x03"); // truncated body
        let rec = recording_from_bytes(&bytes);
        // The complete prefix still ingests fully; the torn frame is ignored.
        assert_eq!(rec.end_ts().unwrap(), 16);
        assert_eq!(rec.world_state(None).unwrap().tasks.len(), 2);
    }

    #[test]
    fn follow_loader_tolerates_an_unflushed_header() {
        // Only part of the "WYRD" magic has hit disk so far.
        let rec = recording_from_bytes(b"WY");
        assert_eq!(rec.end_ts().unwrap(), 0);
        assert!(rec.world_state(None).unwrap().tasks.is_empty());
        // A missing file folds to the same empty recording.
        let missing = load_follow(Path::new("/nonexistent/wyrd/does-not-exist.wyrd"));
        assert_eq!(missing.end_ts().unwrap(), 0);
    }

    #[test]
    fn reload_picks_up_appended_frames_and_tracks_the_tail() {
        // Write a growing recording to a temp file and drive reload() over it.
        let path =
            std::env::temp_dir().join(format!("wyrd-cli-follow-{}.wyrd", std::process::id()));
        let records = deadlock_records();

        // Start with just the two spawns visible (indices 2..4 in the stream).
        std::fs::write(&path, to_frames(&records[..4])).unwrap();
        let mut app = App::new(load_follow(&path), 10, Some(path.clone())).unwrap();
        assert!(app.live);
        assert_eq!(app.end_ts, 4);
        assert_eq!(app.world.tasks.len(), 2);
        // No deadlock yet — nobody is parked.
        assert!(!app.blocked.as_ref().unwrap().is_deadlock());

        // The app runs on: the full deadlock is now on disk.
        std::fs::write(&path, to_frames(&records)).unwrap();
        app.reload();
        // Live cursor advanced to the new tail and the deadlock is now visible.
        assert_eq!(app.end_ts, 16);
        assert_eq!(app.at, 16);
        assert!(
            app.blocked.as_ref().unwrap().is_deadlock(),
            "reload should surface the freshly-formed deadlock"
        );

        // Freeze at an earlier instant: reload must not drag the cursor forward.
        app.live = false;
        app.set_at(4);
        std::fs::write(&path, to_frames(&records)).unwrap(); // (unchanged, but re-read)
        app.reload();
        assert_eq!(app.at, 4, "a frozen cursor stays put across reloads");

        let _ = std::fs::remove_file(&path);
    }
}

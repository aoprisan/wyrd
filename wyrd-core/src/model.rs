//! Serde-serializable query results shared by the CLI, the MCP server, and
//! the `wyrd tui` viewer.

use serde::{Deserialize, Serialize};
use wyrd_weave::{Loc, ResourceId, TaskId, TaskKind};

/// Identity of a task, resolved for display.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskIdent {
    pub id: TaskId,
    /// `task::Builder` name, if the task was named.
    pub name: Option<String>,
    pub kind: TaskKind,
    pub loc: Loc,
}

impl TaskIdent {
    /// A short human label: the name if present, else `kind@loc`, else the id.
    pub fn label(&self) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }
        if !self.loc.is_empty() {
            return format!("{}@{}", kind_str(self.kind), self.loc);
        }
        format!("task#{}", self.id)
    }
}

/// Identity of a resource, resolved for display.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceIdent {
    pub id: ResourceId,
    pub concrete_type: String,
    pub loc: Loc,
}

impl ResourceIdent {
    /// A short human label: `Type@loc`, falling back to the id.
    pub fn label(&self) -> String {
        if !self.loc.is_empty() {
            format!("{}@{}", self.concrete_type, self.loc)
        } else {
            format!("{}#{}", self.concrete_type, self.id)
        }
    }
}

fn kind_str(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Task => "task",
        TaskKind::Blocking => "blocking",
        TaskKind::BlockOn => "block_on",
        TaskKind::Other => "task",
    }
}

/// A task's status at an instant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TaskStatus {
    /// Currently being polled.
    Running,
    /// Alive but not being polled and not parked on a resource.
    Idle,
    /// Parked waiting on a resource.
    Parked { resource: ResourceId },
    /// Completed.
    Done,
}

/// A task and its status at an instant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskState {
    #[serde(flatten)]
    pub ident: TaskIdent,
    #[serde(flatten)]
    pub status: TaskStatus,
    /// The task that spawned this one, if recorded — the spawn-tree edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<TaskId>,
}

/// A resource and its folded state at an instant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceStateView {
    #[serde(flatten)]
    pub ident: ResourceIdent,
    /// Presumed holder (last successful acquirer, not yet released).
    pub holder: Option<TaskId>,
    /// Whether a mutex is currently locked (if it tracks `locked`).
    pub locked: Option<bool>,
    /// Current permit count (semaphores / channels).
    pub permits: Option<i64>,
    /// Initial permit count / capacity, if known.
    pub capacity: Option<i64>,
    /// For a bounded channel, `capacity - permits`.
    pub depth: Option<i64>,
}

/// Whole-world snapshot at a timestamp.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorldState {
    pub at: u64,
    pub tasks: Vec<TaskState>,
    pub resources: Vec<ResourceStateView>,
}

/// One hop in a blocked-chain: a task waiting on a resource held by someone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockedLink {
    pub task: TaskIdent,
    pub waiting_on: ResourceIdent,
    pub op_name: String,
    /// When the task parked on this resource.
    pub since_ts: u64,
    /// How long it has been parked, at the query time.
    pub wait_ns: u64,
    /// The presumed holder of the resource, if any.
    pub holder: Option<TaskIdent>,
}

/// Why a chain of blocked tasks terminates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum BlockedOutcome {
    /// The queried task is not parked.
    NotBlocked,
    /// A cycle of tasks each holding what the next needs: a deadlock.
    Deadlock { cycle: Vec<TaskId> },
    /// The chain bottoms out at a resource with no known holder (e.g. a full
    /// channel, or a timer): backpressure / external wait.
    ResourceRoot { resource: ResourceId },
    /// The chain bottoms out at a task that is running or idle (not parked):
    /// it holds the resource and simply hasn't released yet.
    ActiveHolder { task: TaskId },
}

/// Result of a `why_blocked` query.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockedReport {
    pub task: TaskId,
    pub at: u64,
    pub chain: Vec<BlockedLink>,
    pub outcome: BlockedOutcome,
}

impl BlockedReport {
    /// Whether the report describes a deadlock.
    pub fn is_deadlock(&self) -> bool {
        matches!(self.outcome, BlockedOutcome::Deadlock { .. })
    }
}

/// Thresholds for [`lint`](crate::Recording::lint) findings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LintConfig {
    /// A single poll longer than this is flagged as blocking-in-async.
    pub long_poll_ns: u64,
    /// A park (on a non-timer resource) longer than this is flagged.
    pub long_park_ns: u64,
}

impl Default for LintConfig {
    fn default() -> Self {
        Self {
            long_poll_ns: 1_000_000,     // 1ms: a poll should never block
            long_park_ns: 1_000_000_000, // 1s: parked this long looks stuck
        }
    }
}

/// Severity of a lint finding.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum LintSeverity {
    Error,
    Warning,
}

/// What a lint finding is about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LintKind {
    /// Tasks in a hold-and-wait cycle.
    Deadlock {
        cycle: Vec<TaskIdent>,
        resources: Vec<ResourceIdent>,
    },
    /// A task whose polls exceed the threshold: blocking (or heavy compute)
    /// inside async code, starving the executor.
    LongPoll {
        task: TaskIdent,
        /// How many polls exceeded the threshold.
        count: u64,
        /// The worst offending poll.
        max_ns: u64,
        threshold_ns: u64,
    },
    /// A task parked on a (non-timer) resource beyond the threshold.
    LongPark {
        task: TaskIdent,
        resource: ResourceIdent,
        op_name: String,
        /// How many park episodes exceeded the threshold.
        count: u64,
        /// The longest episode.
        max_ns: u64,
        threshold_ns: u64,
    },
    /// A bounded channel/semaphore that hit its capacity: backpressure.
    SaturatedChannel {
        resource: ResourceIdent,
        capacity: i64,
        max_depth: i64,
    },
}

/// One lint finding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LintFinding {
    pub severity: LintSeverity,
    #[serde(flatten)]
    pub kind: LintKind,
}

/// Result of a `lint` query.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LintReport {
    pub at: u64,
    pub config: LintConfig,
    pub findings: Vec<LintFinding>,
}

impl LintReport {
    /// Whether any finding is an error (a deadlock).
    pub fn has_errors(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity == LintSeverity::Error)
    }

    /// Whether the recording is clean.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

// --- why-slow: causal latency attribution -----------------------------------

/// What a holder task was doing while another task waited on its resource.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HolderActivity {
    pub task: TaskIdent,
    /// Time the holder spent being polled during the wait window.
    pub polling_ns: u64,
    /// Time the holder spent parked during the wait window.
    pub parked_ns: u64,
    /// The resource the holder was (dominantly) parked on, if any — the next
    /// hop of the latency chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_on: Option<ResourceIdent>,
}

/// One wait episode in a task's lifetime: a park, split at the wake into
/// resource-wait and scheduler-lag portions, with the blame attached.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WaitEpisode {
    pub resource: ResourceIdent,
    pub op_name: String,
    /// When the task parked.
    pub since_ts: u64,
    /// Park → wake (or → next poll if no wake was recorded).
    pub wait_ns: u64,
    /// Wake → next poll: the task was runnable but the executor hadn't
    /// polled it yet.
    pub sched_lag_ns: u64,
    /// Whether the resource is a timer (`Sleep`/`Interval`/`Timeout`) — an
    /// intentional wait.
    pub is_timer: bool,
    /// Who held the resource during the wait, and what they were doing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<HolderActivity>,
}

/// Result of a `why_slow` query: a task's lifetime decomposed into where the
/// time actually went, with the dominant waits blamed on their holders.
///
/// The buckets partition `total_ns`:
/// `own_poll_ns + resource_wait_ns + timer_wait_ns + sched_lag_ns + idle_ns
///  == total_ns`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LatencyReport {
    pub task: TaskIdent,
    /// Window start: the task's spawn (or the recording start if it clips).
    pub from_ts: u64,
    /// Window end: the task's end, or the query time if still alive.
    pub to_ts: u64,
    pub total_ns: u64,
    /// Time spent inside `poll` — the task's own compute.
    pub own_poll_ns: u64,
    pub poll_count: u64,
    /// Time parked on non-timer resources (locks, channels, semaphores).
    pub resource_wait_ns: u64,
    /// Time parked on timers (`Sleep`/`Interval`/`Timeout`) — intentional.
    pub timer_wait_ns: u64,
    /// Time between being woken and actually being polled: executor lag.
    pub sched_lag_ns: u64,
    /// Alive but neither polled, parked, nor known-runnable (e.g. waiting on
    /// something wyrd can't see, or yielded).
    pub idle_ns: u64,
    /// The longest waits, longest first (both resource and timer waits).
    pub waits: Vec<WaitEpisode>,
}

// --- diff: run-over-run regression detection ---------------------------------

/// Thresholds for [`diff`](crate::diff) findings: a metric regresses when it
/// grows by more than `ratio` **and** by more than `abs_floor_ns`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiffConfig {
    /// Relative growth needed to flag (1.5 = +50%).
    pub ratio: f64,
    /// Absolute growth (ns) needed to flag — silences noise on tiny values.
    pub abs_floor_ns: u64,
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self {
            ratio: 1.5,
            abs_floor_ns: 1_000_000, // 1ms
        }
    }
}

/// One run's headline numbers, for the diff banner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunSummary {
    pub duration_ns: u64,
    pub task_count: u64,
    pub resource_count: u64,
    /// Total time spent inside polls, across all tasks.
    pub total_poll_ns: u64,
    /// Total time spent parked on non-timer resources, across all tasks.
    pub total_wait_ns: u64,
    /// Distinct deadlock cycles detected.
    pub deadlocks: u64,
}

/// Aggregate behavior of one task group (tasks sharing a stable identity:
/// their name, else `kind@file:line`) within one run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskGroupStats {
    /// How many task instances the group had.
    pub instances: u64,
    pub total_poll_ns: u64,
    pub max_poll_ns: u64,
    /// Total time parked on non-timer resources.
    pub total_wait_ns: u64,
    pub max_wait_ns: u64,
}

impl TaskGroupStats {
    /// Mean poll time per instance — the regression-checked metric.
    pub fn mean_poll_ns(&self) -> u64 {
        self.total_poll_ns / self.instances.max(1)
    }
    /// Mean non-timer wait per instance — the regression-checked metric.
    pub fn mean_wait_ns(&self) -> u64 {
        self.total_wait_ns / self.instances.max(1)
    }
}

/// A task group across the two runs. `None` on a side means the group did not
/// exist in that run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskGroupDiff {
    /// The stable identity: task name, else `kind@file:line`.
    pub key: String,
    pub baseline: Option<TaskGroupStats>,
    pub current: Option<TaskGroupStats>,
}

/// Aggregate behavior of one resource group (resources sharing
/// `Type@file:line`) within one run. Timer resources are excluded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceGroupStats {
    pub instances: u64,
    /// Total time tasks spent parked on this group's resources.
    pub total_wait_ns: u64,
    /// Distinct tasks that parked on it.
    pub waiters: u64,
    /// Largest capacity seen, for bounded resources.
    pub capacity: Option<i64>,
    /// Deepest observed depth, for bounded resources.
    pub max_depth: Option<i64>,
}

/// A resource group across the two runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceGroupDiff {
    /// The stable identity: `Type@file:line`, else the type alone.
    pub key: String,
    pub baseline: Option<ResourceGroupStats>,
    pub current: Option<ResourceGroupStats>,
}

/// How serious a diff finding is.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum DiffSeverity {
    /// The current run has a defect the baseline didn't (a new deadlock).
    Error,
    /// A behavioral regression beyond the thresholds.
    Warning,
    /// An improvement or notable change.
    Info,
}

/// What changed between the runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffKind {
    /// The current run deadlocks and the baseline didn't (for this cycle).
    NewDeadlock { cycle: Vec<String> },
    /// A baseline deadlock is gone.
    FixedDeadlock { cycle: Vec<String> },
    /// A task group's mean poll time grew past the thresholds.
    PollRegression {
        key: String,
        baseline_ns: u64,
        current_ns: u64,
    },
    /// A task group's mean non-timer wait grew past the thresholds.
    WaitRegression {
        key: String,
        baseline_ns: u64,
        current_ns: u64,
    },
    /// A resource group newly hit its capacity.
    NewSaturation {
        key: String,
        capacity: i64,
        max_depth: i64,
    },
    /// A task group's mean poll time shrank past the thresholds.
    PollImprovement {
        key: String,
        baseline_ns: u64,
        current_ns: u64,
    },
    /// A task group's mean non-timer wait shrank past the thresholds.
    WaitImprovement {
        key: String,
        baseline_ns: u64,
        current_ns: u64,
    },
    /// A task group only present in the current run (with meaningful time).
    NewTaskGroup { key: String },
    /// A task group only present in the baseline (with meaningful time).
    RemovedTaskGroup { key: String },
}

/// One diff finding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffFinding {
    pub severity: DiffSeverity,
    #[serde(flatten)]
    pub kind: DiffKind,
}

/// Result of diffing two recordings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiffReport {
    pub config: DiffConfig,
    pub baseline: RunSummary,
    pub current: RunSummary,
    /// All task groups from either run, biggest behavioral change first.
    pub task_groups: Vec<TaskGroupDiff>,
    /// All non-timer resource groups from either run, biggest change first.
    pub resource_groups: Vec<ResourceGroupDiff>,
    /// Verdicts: errors first, then warnings, then info.
    pub findings: Vec<DiffFinding>,
}

impl DiffReport {
    /// Whether any finding is an error (a new deadlock).
    pub fn has_errors(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity == DiffSeverity::Error)
    }

    /// Whether any finding is a regression warning.
    pub fn has_regressions(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity == DiffSeverity::Warning)
    }
}

/// A parked interval, for the "longest parks" report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParkStat {
    pub task: TaskIdent,
    pub resource: ResourceIdent,
    pub op_name: String,
    pub since_ts: u64,
    pub dur_ns: u64,
}

/// Max observed depth of a channel-like resource.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelDepth {
    pub resource: ResourceIdent,
    pub capacity: i64,
    pub max_depth: i64,
}

/// Poll-duration percentiles, in nanoseconds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PollPercentiles {
    pub count: u64,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,
}

/// Result of a `stats` query.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stats {
    pub duration_ns: u64,
    pub task_count: u64,
    pub resource_count: u64,
    pub poll_time: PollPercentiles,
    pub longest_parks: Vec<ParkStat>,
    pub channel_depths: Vec<ChannelDepth>,
}

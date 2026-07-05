//! Serde-serializable query results shared by the CLI (and, later, an MCP
//! server and a TUI).

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

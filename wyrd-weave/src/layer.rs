//! The [`WeaveLayer`]: a `tracing_subscriber::Layer` that normalizes tokio's
//! internal instrumentation into the wyrd event vocabulary.
//!
//! ## Task attribution
//!
//! tokio's `poll_op` and `waker` events reach only the *resource* through their
//! explicit span chain — never the task. The task is recovered from a
//! per-thread stack of entered `runtime.spawn` spans maintained in
//! [`on_enter`](WeaveLayer::on_enter)/[`on_exit`](WeaveLayer::on_exit): the
//! innermost such span is the task currently being polled on that thread.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event as TracingEvent, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::error::WeaveError;
use crate::event::{Event, Loc, StateOp, TaskKind, FIELD_ACQUIRED_BY};
use crate::recorder::Recorder;
use crate::writer::FlushGuard;

thread_local! {
    /// Ids of `runtime.spawn` spans currently entered on this thread, outermost
    /// first. The last element is the task being polled right now.
    static TASK_STACK: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };

    /// Whether a `poll_acquire` event has already been seen in the current
    /// `async_op.poll`. tokio emits *two* `poll_acquire` events per semaphore
    /// poll: the first traces the cooperative-budget check
    /// (`poll_proceed`, essentially always ready) and the second traces the
    /// real acquire result. Only the second reflects contention, so we skip the
    /// first. Reset whenever an `async_op.poll` span is entered.
    static COOP_ACQUIRE_SEEN: Cell<bool> = const { Cell::new(false) };
}

fn task_stack_top() -> Option<u64> {
    TASK_STACK.with(|s| s.borrow().last().copied())
}

const ASYNC_OP_POLL_SPAN: &str = "runtime.resource.async_op.poll";

/// Marker stored in a span's extensions so later callbacks can classify it
/// without re-parsing fields.
#[derive(Clone, Copy)]
enum SpanTag {
    /// A `runtime.spawn` task span.
    Task,
    /// A `runtime.resource` span. `effective` is the id this resource collapses
    /// into — itself, unless it is an internal child (e.g. a `Mutex`'s backing
    /// `Semaphore`) whose parent is another resource.
    Resource { effective: u64 },
}

/// Builder for [`WeaveLayer`].
pub struct WeaveLayerBuilder {
    path: Option<PathBuf>,
    queue_capacity: usize,
    batch_size: usize,
    record_waker_clone_drop: bool,
}

impl Default for WeaveLayerBuilder {
    fn default() -> Self {
        Self {
            path: None,
            queue_capacity: 64 * 1024,
            batch_size: 256,
            record_waker_clone_drop: false,
        }
    }
}

impl WeaveLayerBuilder {
    /// Destination recording file (required).
    pub fn file(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Bounded queue depth between the runtime and the writer thread. Records
    /// beyond this are dropped (and counted) rather than blocking a worker.
    pub fn queue_capacity(mut self, capacity: usize) -> Self {
        self.queue_capacity = capacity;
        self
    }

    /// How many records to buffer before flushing to disk.
    pub fn batch_size(mut self, batch: usize) -> Self {
        self.batch_size = batch;
        self
    }

    /// Record high-volume, causality-free `waker.clone` / `waker.drop` events.
    /// Off by default.
    pub fn record_waker_clone_drop(mut self, yes: bool) -> Self {
        self.record_waker_clone_drop = yes;
        self
    }

    /// Build the layer and its finalization guard.
    pub fn build(self) -> Result<(WeaveLayer, FlushGuard), WeaveError> {
        let path = self.path.ok_or(WeaveError::NoPath)?;
        let (recorder, guard) = Recorder::builder()
            .file(path)
            .queue_capacity(self.queue_capacity)
            .batch_size(self.batch_size)
            .build()?;
        let layer = WeaveLayer {
            recorder,
            record_waker_clone_drop: self.record_waker_clone_drop,
        };
        Ok((layer, guard))
    }
}

/// A `tracing_subscriber::Layer` that records normalized tokio causality events.
///
/// Add it to a `registry()` and keep the returned [`FlushGuard`] alive:
///
/// ```no_run
/// use tracing_subscriber::prelude::*;
/// let (layer, _guard) = wyrd_weave::WeaveLayer::builder()
///     .file("run.wyrd")
///     .build()
///     .expect("open recording");
/// tracing_subscriber::registry().with(layer).init();
/// ```
pub struct WeaveLayer {
    recorder: Recorder,
    record_waker_clone_drop: bool,
}

impl WeaveLayer {
    /// Start configuring a layer.
    pub fn builder() -> WeaveLayerBuilder {
        WeaveLayerBuilder::default()
    }

    fn emit(&self, event: Event) {
        self.recorder.emit(event);
    }
}

/// Find the resource an event fired against by walking its span scope
/// (leaf → root) to the first resource span, and resolving its effective id.
fn nearest_resource<S>(ctx: &Context<'_, S>, event: &TracingEvent<'_>) -> Option<u64>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let scope = ctx.event_scope(event)?;
    for span in scope {
        if let Some(SpanTag::Resource { effective }) = span.extensions().get::<SpanTag>().copied() {
            return Some(effective);
        }
    }
    None
}

impl<S> Layer<S> for WeaveLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let meta = attrs.metadata();
        match meta.name() {
            "runtime.spawn" => {
                let mut v = TaskVisitor::default();
                attrs.record(&mut v);
                // The spawner is the task currently polling on this thread.
                let parent = task_stack_top();
                let loc = v.loc();
                let kind = TaskKind::parse(v.kind.as_deref());
                if let Some(span) = ctx.span(id) {
                    span.extensions_mut().insert(SpanTag::Task);
                }
                self.emit(Event::TaskSpawn {
                    id: id.into_u64(),
                    parent,
                    name: v.task_name.filter(|s| !s.is_empty()),
                    loc,
                    kind,
                });
            }
            "runtime.resource" => {
                let mut v = ResourceVisitor::default();
                attrs.record(&mut v);

                // The contextual parent span, if it is itself a resource, is the
                // resource we may collapse into.
                let parent_resource = ctx.current_span().id().and_then(|pid| {
                    ctx.span(pid)
                        .and_then(|s| match s.extensions().get::<SpanTag>().copied() {
                            Some(SpanTag::Resource { effective }) => Some(effective),
                            _ => None,
                        })
                });

                let self_id = id.into_u64();
                let effective = if v.is_internal {
                    parent_resource.unwrap_or(self_id)
                } else {
                    self_id
                };

                if let Some(span) = ctx.span(id) {
                    span.extensions_mut()
                        .insert(SpanTag::Resource { effective });
                }

                // Only emit a resource for the surviving (non-collapsed) id.
                if effective == self_id {
                    let loc = v.loc();
                    self.emit(Event::ResourceNew {
                        id: self_id,
                        parent: parent_resource,
                        concrete_type: v.concrete_type.unwrap_or_default(),
                        loc,
                        is_internal: v.is_internal,
                    });
                }
            }
            // runtime.resource.async_op[.poll] and everything else: transparent.
            _ => {}
        }
    }

    fn on_event(&self, event: &TracingEvent<'_>, ctx: Context<'_, S>) {
        match event.metadata().target() {
            "runtime::resource::poll_op" => {
                let mut v = PollOpVisitor::default();
                event.record(&mut v);
                let (Some(op_name), Some(is_ready)) = (v.op_name, v.is_ready) else {
                    return;
                };
                // Drop the cooperative-budget `poll_acquire` (always the first
                // one per poll); only the acquire result matters for causality.
                if op_name == "poll_acquire" && !COOP_ACQUIRE_SEEN.replace(true) {
                    return;
                }
                let (Some(resource), Some(task)) =
                    (nearest_resource(&ctx, event), task_stack_top())
                else {
                    return;
                };
                if is_ready {
                    // A successful acquire establishes the presumed holder.
                    if op_name == "poll_acquire" {
                        self.emit(Event::ResourceState {
                            id: resource,
                            field: FIELD_ACQUIRED_BY.into(),
                            value: task as i64,
                            op: StateOp::Override,
                        });
                    }
                } else {
                    self.emit(Event::Park {
                        task,
                        resource,
                        op_name,
                    });
                }
            }
            "tokio::task::waker" => {
                let mut v = WakerVisitor::default();
                event.record(&mut v);
                let Some(op) = v.op else { return };
                let clone_or_drop = op == "waker.clone" || op == "waker.drop";
                if clone_or_drop && !self.record_waker_clone_drop {
                    return;
                }
                let Some(woken) = v.task_id else { return };
                self.emit(Event::Wake {
                    woken,
                    by: task_stack_top(),
                });
            }
            "runtime::resource::state_update" => {
                let mut v = StateUpdateVisitor::default();
                event.record(&mut v);
                let Some(resource) = nearest_resource(&ctx, event) else {
                    return;
                };
                for (field, value) in v.values {
                    let op = StateOp::parse(v.ops.get(&field).map(String::as_str));
                    self.emit(Event::ResourceState {
                        id: resource,
                        field,
                        value,
                        op,
                    });
                }
            }
            _ => {}
        }
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            // A fresh poll of a resource op begins: the next `poll_acquire`
            // (if any) is the cooperative-budget check.
            if span.name() == ASYNC_OP_POLL_SPAN {
                COOP_ACQUIRE_SEEN.set(false);
            }
            if matches!(span.extensions().get::<SpanTag>(), Some(SpanTag::Task)) {
                let raw = id.into_u64();
                TASK_STACK.with(|s| s.borrow_mut().push(raw));
                self.emit(Event::PollStart { task: raw });
            }
        }
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            if matches!(span.extensions().get::<SpanTag>(), Some(SpanTag::Task)) {
                let raw = id.into_u64();
                TASK_STACK.with(|s| {
                    let mut st = s.borrow_mut();
                    if let Some(pos) = st.iter().rposition(|&x| x == raw) {
                        st.remove(pos);
                    }
                });
                self.emit(Event::PollEnd { task: raw });
            }
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(&id) {
            match span.extensions().get::<SpanTag>().copied() {
                Some(SpanTag::Task) => self.emit(Event::TaskEnd { id: id.into_u64() }),
                Some(SpanTag::Resource { effective }) if effective == id.into_u64() => {
                    self.emit(Event::ResourceDrop { id: effective });
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Field visitors
// ---------------------------------------------------------------------------

/// Strip one layer of surrounding quotes left by `{:?}` on a string value.
fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(s)
}

#[derive(Default)]
struct TaskVisitor {
    task_name: Option<String>,
    kind: Option<String>,
    loc_file: Option<String>,
    loc_line: Option<u32>,
    loc_col: Option<u32>,
}

impl TaskVisitor {
    fn loc(&self) -> Loc {
        Loc {
            file: self.loc_file.clone(),
            line: self.loc_line,
            col: self.loc_col,
        }
    }
}

impl Visit for TaskVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "task.name" => self.task_name = Some(value.to_owned()),
            "kind" => self.kind = Some(value.to_owned()),
            "loc.file" => self.loc_file = Some(value.to_owned()),
            _ => {}
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "loc.line" => self.loc_line = Some(value as u32),
            "loc.col" => self.loc_col = Some(value as u32),
            _ => {}
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        if value >= 0 {
            self.record_u64(field, value as u64);
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        match field.name() {
            "task.name" if self.task_name.is_none() => {
                self.task_name = Some(unquote(&s).to_owned())
            }
            "kind" if self.kind.is_none() => self.kind = Some(unquote(&s).to_owned()),
            "loc.file" if self.loc_file.is_none() => self.loc_file = Some(unquote(&s).to_owned()),
            "loc.line" if self.loc_line.is_none() => self.loc_line = s.parse().ok(),
            "loc.col" if self.loc_col.is_none() => self.loc_col = s.parse().ok(),
            _ => {}
        }
    }
}

#[derive(Default)]
struct ResourceVisitor {
    concrete_type: Option<String>,
    is_internal: bool,
    loc_file: Option<String>,
    loc_line: Option<u32>,
    loc_col: Option<u32>,
}

impl ResourceVisitor {
    fn loc(&self) -> Loc {
        Loc {
            file: self.loc_file.clone(),
            line: self.loc_line,
            col: self.loc_col,
        }
    }
}

impl Visit for ResourceVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "is_internal" {
            self.is_internal = value;
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "concrete_type" => self.concrete_type = Some(value.to_owned()),
            "loc.file" => self.loc_file = Some(value.to_owned()),
            _ => {}
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "loc.line" => self.loc_line = Some(value as u32),
            "loc.col" => self.loc_col = Some(value as u32),
            _ => {}
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        if value >= 0 {
            self.record_u64(field, value as u64);
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        match field.name() {
            "concrete_type" if self.concrete_type.is_none() => {
                self.concrete_type = Some(unquote(&s).to_owned())
            }
            "is_internal" => self.is_internal = s.parse().unwrap_or(self.is_internal),
            "loc.file" if self.loc_file.is_none() => self.loc_file = Some(unquote(&s).to_owned()),
            "loc.line" if self.loc_line.is_none() => self.loc_line = s.parse().ok(),
            "loc.col" if self.loc_col.is_none() => self.loc_col = s.parse().ok(),
            _ => {}
        }
    }
}

#[derive(Default)]
struct PollOpVisitor {
    op_name: Option<String>,
    is_ready: Option<bool>,
}

impl Visit for PollOpVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "is_ready" {
            self.is_ready = Some(value);
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "op_name" {
            self.op_name = Some(value.to_owned());
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        match field.name() {
            "op_name" if self.op_name.is_none() => self.op_name = Some(unquote(&s).to_owned()),
            "is_ready" if self.is_ready.is_none() => self.is_ready = s.parse().ok(),
            _ => {}
        }
    }
}

#[derive(Default)]
struct WakerVisitor {
    op: Option<String>,
    task_id: Option<u64>,
}

impl Visit for WakerVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "op" {
            self.op = Some(value.to_owned());
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "task.id" {
            self.task_id = Some(value);
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        if field.name() == "task.id" && value >= 0 {
            self.task_id = Some(value as u64);
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        match field.name() {
            "op" if self.op.is_none() => self.op = Some(unquote(&s).to_owned()),
            "task.id" if self.task_id.is_none() => self.task_id = s.parse().ok(),
            _ => {}
        }
    }
}

/// Collects resource `state_update` fields. A value field `x` may be paired
/// with a companion `x.op` (add/sub/override) and/or `x.unit` (ignored).
#[derive(Default)]
struct StateUpdateVisitor {
    values: Vec<(String, i64)>,
    ops: std::collections::HashMap<String, String>,
}

impl StateUpdateVisitor {
    fn push_value(&mut self, name: &str, value: i64) {
        if name.ends_with(".op") || name.ends_with(".unit") {
            return;
        }
        if !self.values.iter().any(|(k, _)| k == name) {
            self.values.push((name.to_owned(), value));
        }
    }
}

impl Visit for StateUpdateVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push_value(field.name(), value as i64);
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push_value(field.name(), value as i64);
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push_value(field.name(), value);
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if let Some(base) = field.name().strip_suffix(".op") {
            self.ops.insert(base.to_owned(), value.to_owned());
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let name = field.name();
        let s = format!("{value:?}");
        if let Some(base) = name.strip_suffix(".op") {
            self.ops
                .entry(base.to_owned())
                .or_insert_with(|| unquote(&s).to_owned());
            return;
        }
        if name.ends_with(".unit") {
            return;
        }
        if let Ok(b) = s.parse::<bool>() {
            self.push_value(name, b as i64);
        } else if let Ok(i) = s.parse::<i64>() {
            self.push_value(name, i);
        }
    }
}

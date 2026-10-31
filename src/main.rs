//! The gpui shell — variant B layout as approved in the prototype
//! (bd importer-cb5.1 verdict). All import behavior lives in the `kanistr`
//! core; this file wires it up — import view plus the settings form
//! (the text-input widget lives in the `field` module) — and decides whether
//! and when the opt-in USB power fix runs before an Import.

// No console window behind the app on Windows (but keep stdout for tests).
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Application, Bounds, Context, DispatchPhase, Entity, KeyBinding, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Render, ScrollHandle, ScrollWheelEvent,
    Task, TitlebarOptions, Window, WindowBounds, WindowOptions, canvas, div, point, prelude::*, px,
    quad, relative, rgb, size, transparent_black,
};

use field::{Backspace, Commit, Delete, Field, Paste, SelectAll};
use kanistr::{
    Outcome, Settings, clamp_concurrency, detect_roots, format_date, import_file, load_settings,
    save_settings, scan, settings_from_form, usb,
};

mod field;

/// Accent pill colours (background, hover): Save, Import, USB fix On.
const ACCENT: (u32, u32) = (0x2f6feb, 0x4f8cff);
/// Neutral pill colours: Back, Cancel, Refresh, stepper, USB fix Off.
const NEUTRAL: (u32, u32) = (0x2a3242, 0x3a465e);

/// File-list scrollbar: vertical inset of the bar from the list's edges, and
/// how far the painted thumb sits inside the bar on each side.
const BAR_INSET: f32 = 6.;
const THUMB_INSET: f32 = 1.;

#[derive(Clone, Copy, PartialEq)]
enum RowState {
    Pending,
    Imported,
    Failed,
    Skipped,
}

/// Which page the window shows: the import view or the settings form.
#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Import,
    Settings,
}

/// This run's pre-Import USB phase: nothing happening, the privileged call
/// waiting on sudo, or the fix not applied with the header's reason.
#[derive(Default)]
enum UsbPhase {
    #[default]
    Idle,
    Applying,
    NotApplied(String),
}

struct Row {
    src: PathBuf,
    dest: Option<PathBuf>,
    state: RowState,
}

struct Main {
    settings: Settings,
    rows: Vec<Row>,
    imported: usize,
    failed: usize,
    skipped: usize,
    running: bool,
    cancelled: bool,
    finished: bool,
    source_lost: bool,
    /// When the first file of this run launched; `None` outside a run.
    started_at: Option<Instant>,
    /// This run's duration, frozen when it ends so the summary stops moving.
    elapsed: Option<Duration>,
    /// This run's pre-Import USB phase; also feeds the status line and the
    /// header note, so the two can never disagree.
    usb_phase: UsbPhase,
    task: Option<Task<()>>,
    screen: Screen,
    dest_field: Entity<Field>,
    format_field: Entity<Field>,
    /// Stepper value while the settings form is open; only Save writes it
    /// into [`Settings`].
    concurrency: usize,
    /// USB power fix toggle while the settings form is open; only Save writes
    /// it into [`Settings`].
    usb_power_fix: bool,
    save_error: Option<String>,
    /// Scroll state of the file list, shared with the list element so the
    /// scrollbar overlay can read the current offset.
    scroll_handle: ScrollHandle,
    /// Where the pointer grabbed the scrollbar thumb, in px from the thumb's
    /// top; `None` when no drag is in progress. Lives on the view so a repaint
    /// mid-drag cannot reset it.
    bar_grab: Option<f32>,
    /// Whether the file list follows the tail as Import rows land. Set when a
    /// run starts or the list is re-scanned, and by user input: wheel-up (or a
    /// bar grab) leaves the tail, wheel-down to the bottom or a bar grab at
    /// the bottom rejoins it. Only acts while `running`.
    follow: bool,
}

impl Main {
    fn new(cx: &mut Context<Self>) -> Self {
        let settings = load_settings();
        let rows = Self::scan_rows();
        Self {
            concurrency: settings.concurrency,
            usb_power_fix: settings.usb_power_fix,
            settings,
            rows,
            imported: 0,
            failed: 0,
            skipped: 0,
            running: false,
            cancelled: false,
            finished: false,
            source_lost: false,
            started_at: None,
            elapsed: None,
            usb_phase: UsbPhase::Idle,
            task: None,
            screen: Screen::Import,
            dest_field: cx.new(|cx| Field::new("Destination root", cx)),
            format_field: cx.new(|cx| Field::new("YYYYMMDD", cx)),
            save_error: None,
            scroll_handle: ScrollHandle::new(),
            bar_grab: None,
            follow: false,
        }
    }

    /// Height of the rows' content in logical pixels.
    fn content_height(&self, line_height: Pixels) -> f32 {
        content_height(f32::from(line_height), self.rows.len())
    }

    fn status(&self) -> &'static str {
        if self.source_lost && !self.running {
            return "source lost — import stopped";
        }
        match (&self.usb_phase, self.running, self.cancelled, self.finished) {
            (UsbPhase::Applying, ..) => "applying USB power fix…",
            (_, true, false, _) => "importing…",
            (_, true, true, _) => "cancelling…",
            (_, false, true, _) => "cancelled",
            (_, false, false, true) => "done",
            (_, false, false, false) => "ready",
        }
    }

    fn summary(&self) -> Option<String> {
        (!self.running && self.finished).then(|| {
            let mut summary = format!(
                "{} imported · {} failed · {} skipped",
                self.imported, self.failed, self.skipped
            );
            // Only runs that launched at least one file have a duration.
            if let Some(elapsed) = self.elapsed {
                summary.push_str(&format!(" · took {:.1}s", elapsed.as_secs_f64()));
            }
            summary
        })
    }

    /// Every row the run actually handled — skipped and failed rows consumed
    /// their slot too, so a re-run of already-imported media still reaches
    /// the total instead of parking at `imported/total`.
    fn processed(&self) -> usize {
        self.imported + self.failed + self.skipped
    }

    /// A fresh scan as pending rows — Launch and Refresh share this.
    fn scan_rows() -> Vec<Row> {
        scan(&detect_roots())
            .into_iter()
            .map(|src| Row {
                src,
                dest: None,
                state: RowState::Pending,
            })
            .collect()
    }

    /// Launch/Refresh: re-detect mounts, re-scan, reset the run. Sync on
    /// purpose — shortcut: a mount with a huge tree will jank the UI thread;
    /// move the scan to the background executor if that ever bites.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        self.rows = Self::scan_rows();
        // A fresh scan starts at the first file. `set_offset` does not notify,
        // so the `cx.notify()` below is what repaints the list and the bar.
        self.scroll_handle.set_offset(point(px(0.), px(0.)));
        // Refreshing rejoins the tail, ready for the next run. Nothing scrolls
        // until one starts.
        self.follow = true;
        self.reset_run();
        cx.notify();
    }

    fn reset_run(&mut self) {
        self.imported = 0;
        self.failed = 0;
        self.skipped = 0;
        self.cancelled = false;
        self.finished = false;
        self.source_lost = false;
        self.started_at = None;
        self.elapsed = None;
        self.usb_phase = UsbPhase::Idle;
    }

    /// Import starts only on click. Files launch in windows of
    /// `Settings.concurrency` on the background executor and the next window
    /// waits for the previous one to drain, so no more than N copies run at
    /// once. Each row paints its own outcome as it lands (out of order is
    /// fine); counts and the bar are derived from row outcomes. At 1 this is
    /// one window per file, today's loop.
    fn start(&mut self, cx: &mut Context<Self>) {
        if self.running || self.rows.is_empty() {
            return;
        }
        for row in &mut self.rows {
            row.state = RowState::Pending;
            row.dest = None;
        }
        self.reset_run();
        self.running = true;
        // An Import watches its own tail until the user scrolls away.
        self.follow = true;
        // A few unprivileged sysfs reads, cheap enough for the UI thread: a
        // run that needs nothing must never show "applying…" or spawn sudo.
        // The fix is Linux-only; elsewhere the toggle is hidden and this is
        // false without reading anything.
        let apply_fix =
            cfg!(target_os = "linux") && self.settings.usb_power_fix && usb::is_fix_needed();
        if apply_fix {
            // Marked before the task spawns, so no repaint can pair
            // "importing…" with a step that has not started yet.
            self.usb_phase = UsbPhase::Applying;
        }
        let total = self.rows.len();
        self.task = Some(cx.spawn(async move |this, cx| {
            if apply_fix {
                // Off the UI thread: sudo waits on the terminal while the
                // window keeps painting.
                let applied = cx
                    .background_executor()
                    .spawn(async move { usb::apply() })
                    .await;
                this.update(cx, |this, cx| {
                    // Never abort: a cancelled prompt still imports.
                    this.usb_phase = match applied {
                        Ok(()) => UsbPhase::Idle,
                        Err(reason) => {
                            UsbPhase::NotApplied(format!("USB power fix not applied — {reason}"))
                        }
                    };
                    cx.notify();
                })
                .ok();
            }
            let mut next = 0;
            while next < total {
                // Take the next window, or break: cancelled, or Source gone.
                let window = this.update(cx, |this, _| {
                    if this.cancelled {
                        return None;
                    }
                    let end = (next + clamp_concurrency(this.settings.concurrency)).min(total);
                    let window = (next..end)
                        .filter_map(|i| {
                            this.rows
                                .get(i)
                                .map(|row| (i, row.src.clone(), this.settings.clone()))
                        })
                        .collect::<Vec<_>>();
                    // Every Source in the window is checked before launching it
                    // (importer-tk5.3): files already copying when the Source
                    // vanishes fail on their own, while a row whose Source is
                    // already gone stops the run here with its rows still
                    // pending and earlier results intact.
                    if !window.iter().all(|(_, src, _)| src.exists()) {
                        this.source_lost = true;
                        return None;
                    }
                    // The clock starts with the first launch, so any wait
                    // before the copy loop is excluded.
                    this.started_at.get_or_insert_with(Instant::now);
                    Some((end, window))
                });
                let Ok(window) = window else { return };
                let Some((end, window)) = window else { break };
                // The window's own end, not its length: the driver no longer
                // depends on the window matching `end - next` row for row.
                next = end;
                let mut launches = Vec::with_capacity(window.len());
                for (i, src, settings) in window {
                    // Separate task per row so a fast file paints its row
                    // without waiting for the slowest one in the window.
                    let this = this.clone();
                    launches.push(cx.spawn(async move |cx| {
                        let outcome = cx
                            .background_executor()
                            .spawn(async move { import_file(&src, &settings) })
                            .await;
                        this.update(cx, |this, cx| {
                            this.apply(i, outcome);
                            cx.notify();
                        })
                        .ok();
                    }));
                }
                for launch in launches {
                    launch.await;
                }
            }
            this.update(cx, |this, cx| {
                this.running = false;
                this.finished = true;
                // The run is over, so the list stays where the user left it.
                this.follow = false;
                this.elapsed = this.started_at.take().map(|started| started.elapsed());
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Flag-based cancel: the in-flight window finishes (never a partial copy
    /// at the Destination), no new window starts, and the status stays
    /// "cancelling…" until those files drain.
    fn cancel(&mut self, cx: &mut Context<Self>) {
        if !self.running || self.cancelled {
            return;
        }
        self.cancelled = true;
        // Stop following now: the in-flight rows still land, but the list must
        // stay where the user left it.
        self.follow = false;
        cx.notify();
    }

    /// Gear: swap to the settings form, fields loaded from the current
    /// (persisted) Settings.
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dest = self.settings.dest_root.to_string_lossy().into_owned();
        let fmt = self.settings.date_format.clone();
        self.dest_field.update(cx, |field, cx| field.set_text(&dest, cx));
        self.format_field.update(cx, |field, cx| field.set_text(&fmt, cx));
        self.concurrency = self.settings.concurrency;
        self.usb_power_fix = self.settings.usb_power_fix;
        self.save_error = None;
        self.screen = Screen::Settings;
        cx.focus_view(&self.dest_field, window);
        cx.notify();
    }

    /// Save button / Enter inside a field: persist, then swap back. The
    /// in-memory Settings is replaced too, so the next import uses the new
    /// values without an app restart (`start` reads `self.settings`).
    fn save_settings_view(&mut self, cx: &mut Context<Self>) {
        let dest = self.dest_field.read(cx).text();
        let fmt = self.format_field.read(cx).text();
        let new = settings_from_form(
            &dest,
            &fmt,
            self.concurrency,
            self.usb_power_fix,
            &self.settings,
        );
        match save_settings(&new) {
            Ok(()) => {
                self.settings = new;
                self.save_error = None;
                self.screen = Screen::Import;
            }
            Err(err) => self.save_error = Some(format!("could not save settings: {err}")),
        }
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.save_error = None;
        self.screen = Screen::Import;
        cx.notify();
    }

    /// One file's outcome drives its row directly — no re-derivation from
    /// Report counts. Skipped rows carry the Destination that already holds
    /// the content (US13: verify where every file went).
    fn apply(&mut self, i: usize, outcome: Outcome) {
        let Some(row) = self.rows.get_mut(i) else { return };
        match outcome {
            Outcome::Imported(dest) => {
                row.state = RowState::Imported;
                row.dest = Some(dest);
                self.imported += 1;
            }
            Outcome::Skipped(dest) => {
                row.state = RowState::Skipped;
                row.dest = Some(dest);
                self.skipped += 1;
            }
            Outcome::Failed => {
                row.state = RowState::Failed;
                self.failed += 1;
            }
        }
        // Following the tail: the request is consumed in prepaint, so the
        // caller's notify is all the repaint it needs. A cancelled run's
        // in-flight rows land without moving the list.
        if self.running && !self.cancelled && self.follow {
            self.scroll_handle.scroll_to_bottom();
        }
    }

    fn button(
        cx: &mut Context<Self>,
        id: &'static str,
        label: &'static str,
        colors: (u32, u32),
        f: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div().id(id)
            .px_5()
            .py_2()
            .rounded_full()
            .cursor_pointer()
            .bg(rgb(colors.0))
            .text_color(rgb(0xffffff))
            .text_size(px(13.))
            .hover(move |s| s.bg(rgb(colors.1)))
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| f(this, cx)))
    }

    fn header(&self) -> impl IntoElement {
        let total = self.rows.len();
        let frac = if total == 0 {
            0.0
        } else {
            self.processed() as f32 / total as f32
        };
        div().flex()
            .flex_col()
            .gap_3()
            .px_6()
            .py_5()
            .child(
                div().flex()
                    .items_baseline()
                    .justify_between()
                    .child(
                        div().flex()
                            .items_baseline()
                            .gap_2()
                            .child(
                                div().text_2xl()
                                    .child(format!("{}", self.processed())),
                            )
                            .child(
                                div().text_xl()
                                    .text_color(rgb(0x7d8aa0))
                                    .child(format!("/ {total}")),
                            ),
                    )
                    .child(
                        div().flex()
                            .items_baseline()
                            .gap_4()
                            .child(
                                div().text_sm()
                                    .text_color(rgb(0x7d8aa0))
                                    .child(self.status()),
                            )
                            .child(match self.summary() {
                                Some(summary) => div()
                                    .text_sm()
                                    .text_color(rgb(0x33dd88))
                                    .child(summary),
                                None => div(),
                            }),
                    ),
            )
            // Present only when this run could not apply the fix.
            .children(match &self.usb_phase {
                UsbPhase::NotApplied(note) => Some(
                    div()
                        .text_sm()
                        .text_color(rgb(0xff6b6b))
                        .child(note.clone()),
                ),
                _ => None,
            })
            .child(
                div().w_full()
                    .h(px(8.))
                    .rounded_full()
                    .bg(rgb(0x232b3a))
                    .child(
                        div().h_full()
                            .rounded_full()
                            .bg(rgb(ACCENT.1))
                            .w(relative(frac.max(0.004))),
                    ),
            )
    }

    fn rows_area(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        if self.rows.is_empty() {
            return div().flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div().text_color(rgb(0x7d8aa0))
                        .child("No media found — connect a card, then press Refresh"),
                )
                .into_any_element();
        }
        let content = self.content_height(window.line_height());
        let scroll_handle = self.scroll_handle.clone();
        let weak = cx.weak_entity();
        div().relative()
            .flex_1()
            .min_h_0()
            .child(
                div().id("rows")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    // Leaving or rejoining the tail is derived from the wheel
                    // like a pager: up pauses following, down at the bottom
                    // resumes it. Only during a run. gpui's own scroll
                    // listener handles the same event in an unspecified
                    // order, so predict the landing offset from the current
                    // one; an overshoot on either ordering still satisfies
                    // the predicate.
                    .on_scroll_wheel(cx.listener(
                        |this, event: &ScrollWheelEvent, window, _cx| {
                            if !this.running {
                                return;
                            }
                            let line_height = window.line_height();
                            let delta = f32::from(event.delta.pixel_delta(line_height).y);
                            if delta == 0. {
                                return;
                            }
                            let viewport =
                                f32::from(this.scroll_handle.bounds().size.height);
                            let limit = max_scroll(this.content_height(line_height), viewport);
                            let scrolled = scrolled(&this.scroll_handle);
                            if delta > 0. {
                                this.follow = false;
                            } else if at_bottom(scrolled - delta, limit) {
                                this.follow = true;
                            }
                        },
                    ))
                    .children(self.rows.iter().map(|row| {
                        let (marker, marker_color, text, text_color) = match row.state {
                            RowState::Pending => (
                                "·",
                                0x44505e,
                                row.src.to_string_lossy().into_owned(),
                                0x77828f,
                            ),
                            RowState::Imported => (
                                "✓",
                                0x33dd88,
                                format!(
                                    "{}  →  {}",
                                    row.src.display(),
                                    row.dest.as_ref().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default()
                                ),
                                0xd7e0ea,
                            ),
                            RowState::Failed => (
                                "✗",
                                0xff6b6b,
                                format!("{}  (failed)", row.src.display()),
                                0xff6b6b,
                            ),
                            RowState::Skipped => (
                                "=",
                                0x7d8aa0,
                                format!(
                                    "{}  →  {} (already in Destination)",
                                    row.src.display(),
                                    row.dest.as_ref().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default()
                                ),
                                0x9fb0c3,
                            ),
                        };
                        div().flex()
                            .gap_3()
                            .px_6()
                            .py(px(6.))
                            .child(div().w(px(14.)).text_color(rgb(marker_color)).child(marker))
                            .child(div().flex_1().truncate().text_color(rgb(text_color)).child(text))
                    })),
            )
            .child(
                // The bar is a canvas because gpui 0.2.2 ships no scrollbar
                // element. It deliberately has no id and no hitbox, so it never
                // becomes a mouse target and the wheel reaches the list under
                // the track. It overlays the rows' right margin only.
                // Declared after the rows so its prepaint already reads this
                // frame's clamped scroll offset.
                canvas(
                    move |bounds, _window, _cx| {
                        let track = f32::from(bounds.size.height);
                        // The bar is inset top and bottom, so the list viewport
                        // is taller than the track; a list that only overflows
                        // into that inset still fits and draws no bar.
                        let viewport = bar_viewport(track);
                        (content > viewport).then(|| {
                            thumb_geometry(track, content, viewport, scrolled(&scroll_handle))
                        })
                    },
                    move |bounds, thumb, window, _cx| {
                        let Some(thumb) = thumb else { return };
                        let track = f32::from(bounds.size.height);
                        let viewport = bar_viewport(track);
                        let draw = |window: &mut Window,
                                    left: f32,
                                    width: f32,
                                    height: f32,
                                    offset: f32,
                                    radius: f32,
                                    color: u32| {
                            window.paint_quad(quad(
                                Bounds::new(
                                    point(bounds.left() + px(left), bounds.top() + px(offset)),
                                    size(px(width), px(height)),
                                ),
                                px(radius),
                                rgb(color),
                                px(0.),
                                transparent_black(),
                                Default::default(),
                            ));
                        };
                        // Track full width; thumb inset 1px each side with a
                        // tighter radius so it reads as sitting inside the bar.
                        let width = f32::from(bounds.size.width);
                        draw(window, 0., width, track, 0., 3., 0x232b3a);
                        draw(
                            window,
                            THUMB_INSET,
                            width - THUMB_INSET * 2.,
                            thumb.height,
                            thumb.offset,
                            2.,
                            0x44505e,
                        );

                        // Window-level listeners are legal only during paint.
                        // They are not tied to a hitbox, so the bar never
                        // swallows the wheel. Drag state lives on the view, so
                        // the repaints a drag triggers cannot reset the grab.
                        window.on_mouse_event({
                            let weak = weak.clone();
                            move |event: &MouseDownEvent, phase, _window, cx| {
                                if phase != DispatchPhase::Bubble
                                    || event.button != MouseButton::Left
                                {
                                    return;
                                }
                                // A press anywhere else ends any drag whose
                                // release we missed (e.g. let go off-window).
                                if !bounds.contains(&event.position) {
                                    let _ = weak.update(cx, |this, _| this.bar_grab = None);
                                    return;
                                }
                                let pointer =
                                    f32::from(event.position.y) - f32::from(bounds.top());
                                let _ = weak.update(cx, |this, cx| {
                                    let scrolled = scrolled(&this.scroll_handle);
                                    let bar =
                                        thumb_geometry(track, content, viewport, scrolled);
                                    if pointer >= bar.offset && pointer <= bar.offset + bar.height {
                                        this.bar_grab = Some(pointer - bar.offset);
                                        // A press on the thumb pauses following
                                        // where the list already is, so the
                                        // next row cannot yank it.
                                        this.follow = at_bottom(
                                            scrolled,
                                            max_scroll(content, viewport),
                                        );
                                        return;
                                    }
                                    let offset = scroll_offset_for_pointer(
                                        track,
                                        content,
                                        viewport,
                                        pointer,
                                        bar.height / 2.,
                                    );
                                    // A click at the bottom rejoins the tail.
                                    this.follow =
                                        at_bottom(offset, max_scroll(content, viewport));
                                    this.scroll_handle
                                        .set_offset(point(px(0.), px(-offset)));
                                    cx.notify();
                                });
                            }
                        });
                        window.on_mouse_event({
                            let weak = weak.clone();
                            move |event: &MouseMoveEvent, phase, _window, cx| {
                                if phase != DispatchPhase::Bubble || !event.dragging() {
                                    return;
                                }
                                let pointer =
                                    f32::from(event.position.y) - f32::from(bounds.top());
                                let _ = weak.update(cx, |this, cx| {
                                    let Some(grab) = this.bar_grab else { return };
                                    let offset = scroll_offset_for_pointer(
                                        track, content, viewport, pointer, grab,
                                    );
                                    // Dragging away from the bottom leaves the
                                    // tail; dragging back rejoins it.
                                    this.follow =
                                        at_bottom(offset, max_scroll(content, viewport));
                                    let scrolled = scrolled(&this.scroll_handle);
                                    if (offset - scrolled).abs() > f32::EPSILON {
                                        this.scroll_handle
                                            .set_offset(point(px(0.), px(-offset)));
                                        cx.notify();
                                    }
                                });
                            }
                        });
                        window.on_mouse_event({
                            let weak = weak.clone();
                            move |event: &MouseUpEvent, phase, _window, cx| {
                                if phase != DispatchPhase::Bubble
                                    || event.button != MouseButton::Left
                                {
                                    return;
                                }
                                let _ = weak.update(cx, |this, _| this.bar_grab = None);
                            }
                        });
                    },
                )
                .absolute()
                .top(px(BAR_INSET))
                .bottom(px(BAR_INSET))
                .right(px(4.))
                .w(px(6.)),
            )
            .into_any_element()
    }

    /// The settings form: two editable fields populated from the persisted
    /// Settings, the parallel-imports stepper, a live preview from the core's
    /// own formatter, Save/Back. Enter inside a field bubbles up here as
    /// Commit and saves.
    fn settings_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let dest = self.dest_field.read(cx).text();
        let fmt = self.format_field.read(cx).text();
        let sample = chrono::NaiveDate::from_ymd_opt(2026, 10, 10)
            .unwrap()
            .and_hms_opt(14, 32, 55)
            .unwrap();
        let stamped = format_date(sample, &fmt);
        let preview = format!("{dest}/{stamped}/{stamped}_IMG_1234.jpg");
        // Same three-band skeleton as the main view: header → full-width
        // body → footer bar with pills. No centered card.
        div().size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x141a24))
            .text_color(rgb(0xe6edf3))
            .on_action(cx.listener(|this, _: &Commit, _, cx| this.save_settings_view(cx)))
            .child(
                div().flex()
                    .items_baseline()
                    .justify_between()
                    .px_6()
                    .py_5()
                    .child(div().text_2xl().child("Settings"))
                    .child(
                        div().text_sm()
                            .text_color(rgb(0x7d8aa0))
                            .child("Changes apply to the next import"),
                    ),
            )
            .child(
                div().id("settings-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .px_6()
                    .py_4()
                    .child(Self::form_row("Destination", self.dest_field.clone()))
                    .child(Self::form_row("Date format", self.format_field.clone()))
                    .child(Self::form_row(
                        "Parallel imports",
                        div().flex()
                            .flex_col()
                            .gap_2()
                            .child(self.stepper(cx))
                            .child(
                                div().text_xs()
                                    .text_color(rgb(0x556677))
                                    .child("How many files are copied at once — more in flight is not a speed guarantee"),
                            ),
                    ))
                    .when(cfg!(target_os = "linux"), |el| {
                        el.child(Self::form_row("USB power fix", self.usb_power_fix_row(cx)))
                    })
                    .child(
                        div().flex()
                            .items_start()
                            .gap_4()
                            .child(
                                div().w(px(220.))
                                    .flex_shrink_0()
                                    .text_sm()
                                    .text_color(rgb(0x7d8aa0))
                                    .child("Next import will write"),
                            )
                            .child(
                                div().flex_1()
                                    .min_w_0()
                                    .px_4()
                                    .py_3()
                                    .rounded_md()
                                    .bg(rgb(0x1a2029))
                                    .font_family("monospace")
                                    .text_size(px(12.))
                                    .text_color(rgb(0x33dd88))
                                    .truncate()
                                    .child(preview),
                            ),
                    )
                    .child(
                        div().text_xs()
                            .text_color(rgb(0x556677))
                            .child("Tokens: YYYY year · MM month · DD day · HH:MM:SS time — everything else is literal"),
                    )
                    .child(match &self.save_error {
                        Some(err) => div().text_sm().text_color(rgb(0xff6b6b)).child(err.clone()),
                        None => div(),
                    }),
            )
            .child(
                div().flex()
                    .gap_3()
                    .items_center()
                    .px_6()
                    .py_4()
                    .child(Self::button(cx, "save", "Save", ACCENT, |t, cx| {
                        t.save_settings_view(cx)
                    }))
                    .child(Self::button(cx, "back", "Back", NEUTRAL, |t, cx| {
                        t.close_settings(cx)
                    })),
            )
            .into_any_element()
    }

    /// Off/On pill plus the known-issue warning. The toggle only rides into
    /// the persisted Settings; `start` reads it and applies the fix.
    fn usb_power_fix_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = if self.usb_power_fix { ACCENT } else { NEUTRAL };
        div().flex()
            .flex_col()
            .gap_2()
            .child(Self::button(
                cx,
                "usb-power-fix",
                if self.usb_power_fix { "On" } else { "Off" },
                colors,
                |this, cx| {
                    this.usb_power_fix = !this.usb_power_fix;
                    cx.notify();
                },
            ))
            .child(
                div().text_sm()
                    .text_color(rgb(0x9fb0c3))
                    .child("Known issue: an idle USB card reader can be powered down by the kernel, which unmounts the Source mid-Import. Turning this on re-applies the temporary fix before each Import."),
            )
            .child(
                div().text_xs()
                    .text_color(rgb(0x556677))
                    .child("Asks for authentication with sudo or polkit only when needed. The fix lasts until you reboot."),
            )
    }

    /// `− N +` built from the footer pill buttons; stops at the bounds instead
    /// of wrapping, so 1 and 10 are visible dead ends.
    fn stepper(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex()
            .items_center()
            .gap_3()
            .child(Self::button(
                cx,
                "concurrency-down",
                "−",
                NEUTRAL,
                |this, cx| {
                    this.concurrency = clamp_concurrency(this.concurrency.saturating_sub(1));
                    cx.notify();
                },
            ))
            .child(
                div().w(px(28.))
                    .text_center()
                    .text_size(px(15.))
                    .child(self.concurrency.to_string()),
            )
            .child(Self::button(
                cx,
                "concurrency-up",
                "+",
                NEUTRAL,
                |this, cx| {
                    this.concurrency = clamp_concurrency(this.concurrency.saturating_add(1));
                    cx.notify();
                },
            ))
    }

    /// Label-left / control-right row; the control takes the remaining width,
    /// so the form tracks window resizing instead of floating in dead space.
    fn form_row(label: &'static str, control: impl IntoElement) -> impl IntoElement {
        div().flex()
            .items_center()
            .gap_4()
            .child(
                div().w(px(220.))
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(rgb(0x7d8aa0))
                    .child(label),
            )
            .child(div().flex_1().min_w_0().child(control))
    }
}

/// Height of the rows' content in logical pixels, given the line height and
/// row count. shortcut: one text line plus the 12px of vertical padding each
/// row carries (`py(px(6.))`). Reading `scroll_handle.max_offset()` instead
/// would be empty on the first paint and stale after a Refresh, because gpui
/// updates the handle in prepaint, after render. Teach rows a second line or
/// more padding and this formula must follow.
fn content_height(line_height: f32, rows: usize) -> f32 {
    (line_height + 12.) * rows as f32
}

/// How far the list is scrolled from the top, in logical pixels.
fn scrolled(handle: &ScrollHandle) -> f32 {
    (-f32::from(handle.offset().y)).max(0.)
}

/// Height of the list viewport a `track`-tall bar sits in. The bar is inset
/// top and bottom, so the list runs on past the track by that inset.
fn bar_viewport(track: f32) -> f32 {
    track + BAR_INSET * 2.
}

/// The largest scroll offset the list can reach, never negative.
fn max_scroll(content: f32, viewport: f32) -> f32 {
    (content - viewport).max(0.)
}

/// Height and top offset of the file list's scrollbar thumb, in logical pixels.
#[derive(Debug, PartialEq)]
struct Thumb {
    height: f32,
    offset: f32,
}

/// Thumb geometry for a `track`-tall bar over `content`-tall rows in a
/// `viewport`-tall list scrolled `offset` from the top. The bar is inset, so
/// the viewport is taller than the track. A list that fits gets a full-height
/// thumb at the top (the caller then draws no bar); a longer list gets a thumb
/// proportional to the visible fraction, never shorter than the grab minimum,
/// and pinned to the track's ends at the scroll extremes.
fn thumb_geometry(track: f32, content: f32, viewport: f32, offset: f32) -> Thumb {
    /// Comfortable to grab even on a very long list.
    const MIN_THUMB: f32 = 24.;
    let track = track.max(0.);
    if content <= viewport {
        return Thumb {
            height: track,
            offset: 0.,
        };
    }
    let height = (track * (viewport / content)).clamp(MIN_THUMB.min(track), track);
    let progress = (offset / (content - viewport)).clamp(0., 1.);
    Thumb {
        height,
        offset: progress * (track - height),
    }
}

/// Exact inverse of [`thumb_geometry`]: the scroll position that puts the
/// thumb's top at `pointer - grab` on a `track`-tall bar, where `grab` is how
/// far into the thumb the pointer is. A track click passes half the thumb
/// height so the thumb centres on the click. Clamped to the scroll range.
fn scroll_offset_for_pointer(
    track: f32,
    content: f32,
    viewport: f32,
    pointer: f32,
    grab: f32,
) -> f32 {
    let limit = max_scroll(content, viewport);
    let travel = (track - thumb_geometry(track, content, viewport, 0.).height).max(0.);
    if limit == 0. || travel == 0. {
        return 0.;
    }
    let top = (pointer - grab).clamp(0., travel);
    (top / travel * limit).clamp(0., limit)
}

/// Whether a list scrolled `offset` logical pixels from the top has reached
/// its last row, with a pixel of slack for rounding.
fn at_bottom(offset: f32, max_scroll: f32) -> bool {
    offset >= max_scroll - 1.
}

impl Render for Main {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.screen == Screen::Settings {
            return self.settings_view(cx);
        }
        let header = self.header();
        let rows_area = self.rows_area(window, cx);
        div().size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x141a24))
            .text_color(rgb(0xe6edf3))
            .child(header)
            .child(rows_area)
            .child(
                div().flex()
                    .gap_3()
                    .items_center()
                    .px_6()
                    .py_4()
                    // Running (including cancelling…): Cancel alone; hiding
                    // Import/Refresh matches what's actually actionable.
                    .child(if self.running {
                        Self::button(cx, "cancel", "Cancel", NEUTRAL, |t, cx| {
                            t.cancel(cx)
                        })
                        .into_any_element()
                    } else {
                        Self::button(cx, "import", "Import", ACCENT, |t, cx| {
                            t.start(cx)
                        })
                        .into_any_element()
                    })
                    .child(if self.running {
                        div().into_any_element()
                    } else {
                        Self::button(cx, "refresh", "Refresh", NEUTRAL, |t, cx| {
                            t.rescan(cx)
                        })
                        .into_any_element()
                    })
                    // Gear, pushed to the right; opens the settings form.
                    // Hidden while an import runs: settings take effect on
                    // the next import (cb5.6), so a mid-run save would split
                    // one run across two destinations.
                    .child(div().flex_1())
                    .child(if self.running {
                        div().into_any_element()
                    } else {
                        div().id("settings")
                            .px_5()
                            .py_2()
                            .rounded_full()
                            .cursor_pointer()
                            .bg(rgb(NEUTRAL.0))
                            .text_color(rgb(0xffffff))
                            .text_size(px(13.))
                            .hover(move |s| s.bg(rgb(NEUTRAL.1)))
                            .child("⚙ Settings")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_settings(window, cx)
                            }))
                            .into_any_element()
                    }),
            )
            .into_any_element()
    }
}


#[cfg(test)]
mod tests {
    use gpui::{ScrollHandle, point, px};

    use super::{
        Thumb, at_bottom, bar_viewport, content_height, max_scroll, scroll_offset_for_pointer,
        scrolled, thumb_geometry,
    };

    #[test]
    fn content_height_is_one_text_line_plus_padding_per_row() {
        // A row is one line of text plus 12px of vertical padding.
        assert_eq!(content_height(20., 0), 0.);
        assert_eq!(content_height(20., 1), 32.);
        assert_eq!(content_height(20., 7), 7. * 32.);
    }

    #[test]
    fn the_bar_inset_belongs_to_the_list_viewport() {
        assert_eq!(bar_viewport(388.), 400.);
        assert_eq!(max_scroll(500., 400.), 100.);
        // A list shorter than its viewport cannot scroll.
        assert_eq!(max_scroll(300., 400.), 0.);
    }

    #[test]
    fn scrolled_distance_is_positive_in_the_scroll_direction() {
        let handle = ScrollHandle::new();
        handle.set_offset(point(px(0.), px(-120.)));
        assert_eq!(scrolled(&handle), 120.);
        handle.set_offset(point(px(0.), px(0.)));
        assert_eq!(scrolled(&handle), 0.);
    }

    #[test]
    fn the_top_of_a_scrollable_list_is_not_the_bottom() {
        assert!(!at_bottom(0., 400.));
    }

    #[test]
    fn exactly_at_the_bottom_counts_as_the_bottom() {
        assert!(at_bottom(400., 400.));
    }

    #[test]
    fn a_pixel_of_rounding_slack_still_counts_as_the_bottom() {
        assert!(at_bottom(399.5, 400.));
        assert!(!at_bottom(398.9, 400.));
    }

    #[test]
    fn a_list_with_no_scroll_range_is_always_at_the_bottom() {
        assert!(at_bottom(0., 0.));
    }

    #[test]
    fn a_list_that_fits_gets_a_full_height_thumb_at_the_top() {
        assert_eq!(
            thumb_geometry(300., 200., 300., 0.),
            Thumb {
                height: 300.,
                offset: 0.
            }
        );
    }

    #[test]
    fn the_thumb_is_proportional_to_the_visible_fraction() {
        // Half the content fits: a 400px track over 800px of rows.
        let thumb = thumb_geometry(400., 800., 400., 0.);
        assert_eq!(thumb.height, 200.);

        // Halfway down the list, the thumb sits halfway along its travel.
        let thumb = thumb_geometry(400., 800., 400., 200.);
        assert_eq!(thumb.offset, 100.);
    }

    #[test]
    fn the_thumb_never_shrinks_below_the_grabbable_minimum() {
        // A 300px track over a 30,000px list would be a 3px thumb.
        let thumb = thumb_geometry(300., 30_000., 300., 0.);
        assert_eq!(thumb.height, 24.);
    }

    #[test]
    fn the_thumb_is_pinned_to_the_ends() {
        let top = thumb_geometry(400., 800., 400., 0.);
        assert_eq!(top.offset, 0.);

        // At the bottom of the scroll range the thumb's bottom meets the track.
        let bottom = thumb_geometry(400., 800., 400., 400.);
        assert_eq!(bottom.height, 200.);
        assert_eq!(bottom.offset, 200.);

        // Overscroll past the end stays pinned instead of running off the track.
        assert_eq!(thumb_geometry(400., 800., 400., 9_999.).offset, 200.);
    }

    #[test]
    fn an_inset_bar_reaches_the_track_bottom_at_the_end() {
        // The 388px track sits inside a 400px list viewport (6px inset top and
        // bottom), and 500px of rows just overflows it.
        let track = 388.;
        let viewport = 400.;
        let content = 500.;
        let max_scroll = content - viewport;

        let bottom = thumb_geometry(track, content, viewport, max_scroll);
        assert!((bottom.offset + bottom.height - track).abs() < 0.01);

        // The pointer mapping is the exact inverse: the track's bottom maps to
        // the scroll range's end, and back to a thumb flush with the bottom.
        let offset = scroll_offset_for_pointer(track, content, viewport, track, 0.);
        assert!((offset - max_scroll).abs() < 0.01);
        let round_trip = thumb_geometry(track, content, viewport, offset);
        assert!((round_trip.offset + round_trip.height - track).abs() < 0.01);
    }

    #[test]
    fn a_track_click_maps_proportionally() {
        // Half the content fits, so the middle of the track is halfway down
        // the list. A click is centred on the thumb: grab = thumb.height / 2.
        assert_eq!(
            scroll_offset_for_pointer(400., 800., 400., 200., 100.),
            200.
        );
    }

    #[test]
    fn a_drag_past_either_end_clamps_to_the_scroll_range() {
        assert_eq!(scroll_offset_for_pointer(400., 800., 400., -50., 0.), 0.);
        assert_eq!(
            scroll_offset_for_pointer(400., 800., 400., 9_999., 0.),
            400.
        );
    }

    #[test]
    fn the_bottom_clamp_uses_the_list_viewport() {
        // The bar is inset inside the list viewport, so the list runs out of
        // scroll before the pointer reaches the track's bottom.
        assert_eq!(
            scroll_offset_for_pointer(400., 800., 412., 9_999., 0.),
            388.
        );
    }

    #[test]
    fn grabbing_the_thumb_keeps_the_grab_offset() {
        // Thumb at scroll 200 sits 100px down the track, 200px tall. Grab it
        // 50px from its top and the drag maps straight back to scroll 200.
        let thumb = thumb_geometry(400., 800., 400., 200.);
        assert_eq!(thumb.offset, 100.);
        assert_eq!(
            scroll_offset_for_pointer(400., 800., 400., thumb.offset + 50., 50.),
            200.
        );
    }

    #[test]
    fn a_list_that_fits_stays_at_the_top() {
        assert_eq!(scroll_offset_for_pointer(300., 200., 300., 150., 0.), 0.);
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, None),
            KeyBinding::new("delete", Delete, None),
            KeyBinding::new("cmd-a", SelectAll, None),
            KeyBinding::new("ctrl-a", SelectAll, None),
            KeyBinding::new("ctrl-v", Paste, None),
            KeyBinding::new("enter", Commit, None),
        ]);
        let bounds = Bounds::centered(None, size(px(960.), px(640.)), cx);
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Kanistr".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(Main::new),
        )
        .unwrap();
        cx.activate(true);
    });
}

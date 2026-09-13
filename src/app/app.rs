use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use async_channel::{bounded, Receiver, Sender};
use chrono::{NaiveDate, Utc};
use gpui::{
    px, size, App, AppContext, Bounds, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, Styled, Task, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions,
};
use gpui_component::calendar::Date;
use gpui_component::date_picker::{DatePickerEvent, DatePickerState};
use gpui_component::select::{SelectEvent, SelectState};
use gpui_component::{v_flex, Colorize, IndexPath};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use rusqlite::Connection;

use crate::collector::{scheduler, Collector, CollectorEvent};
use crate::core::aggregation::SumStats;
use crate::core::model::{Provider, ThemeColor, TimeWindow};
use crate::platform::{
    get_floating_hwnd, install_ball_drag, is_floating_visible, register_floating_hwnd,
    set_always_on_top, set_floating_visible, show_window,
};
use crate::storage::default_db_path;
use crate::storage::repository::UsageRepo;
use crate::storage::sqlite;
use crate::ui;
#[cfg(target_os = "windows")]
use crate::ui::floating::FloatingView;

use super::state::{
    ActivePage, AppState, ChartApp, ChartMetric, ChartRange, ChartRangeItem, ChartsSnapshot,
    ReportSnapshot, ScanInterval, ScanStatus, TimeTab, ViewSnapshot,
};
use super::update_check::UpdateCheckUiState;

/// One aggregate request: everything the background worker needs to compute
/// a `ViewSnapshot` without touching app state (so the main thread is never
/// blocked). Produced on the main thread, consumed on the worker thread.
struct AggRequest {
    seq: u64,
    time_tab: TimeTab,
    db_path: std::path::PathBuf,
    window: TimeWindow,
    charts: Option<(TimeWindow, ChartApp)>,
    report: Option<TimeWindow>,
}

/// Root application entity: owns app state, the collector, and the window focus.
pub struct TokenMonitorApp {
    pub state: AppState,
    pub collector: Arc<Collector>,
    pub focus_handle: FocusHandle,
    pub weak_self: WeakEntity<TokenMonitorApp>,
    /// Keeps the periodic auto-rescan thread alive for the app's lifetime.
    _scheduler: std::thread::JoinHandle<()>,
    _view_tx: Sender<ViewSnapshot>,
    view_rx: Receiver<ViewSnapshot>,
    view_seq: u64,
    /// Highest snapshot seq applied so far; snapshots older than this are stale.
    applied_seq: u64,
    /// Aggregate-worker control channel: bounded(1), latest-wins. The worker
    /// stays alive for the app's lifetime, reusing a single read-only SQLite
    /// connection instead of spawning a fresh thread + open per refresh.
    agg_req_tx: Sender<AggRequest>,
    _agg_worker: std::thread::JoinHandle<()>,
    /// Stateful dropdown / date-picker entities for the charts page controls.
    pub chart_metric_select: Entity<SelectState<Vec<ChartMetric>>>,
    pub chart_app_select: Entity<SelectState<Vec<ChartApp>>>,
    pub chart_range_select: Entity<SelectState<Vec<ChartRangeItem>>>,
    pub chart_range_picker: Entity<DatePickerState>,
    /// Auto-update state and preferences.
    pub update_check: UpdateCheckUiState,
    pub check_updates_on_startup: bool,
    pub skipped_update_version: Option<String>,
    /// Periodic rescan interval (seconds), read live by the scheduler thread.
    pub scan_interval: Arc<AtomicU64>,
    /// App accent theme color, applied to dashboard highlights and chart colors.
    pub theme_color: ThemeColor,
    /// Wakes the scheduler thread when the interval changes so the new value
    /// takes effect immediately instead of after the old cycle elapses.
    scheduler_wake: std::sync::mpsc::Sender<()>,
    /// Weak handle to the floating usage-ball window, if it has been opened.
    /// Windows-only: the ball is a separate always-on-top GPUI window.
    #[cfg(target_os = "windows")]
    pub floating: Option<WeakEntity<FloatingView>>,
    /// Keeps the tray-command listener alive for the app's lifetime. Windows-only.
    #[cfg(target_os = "windows")]
    pub tray_task: Option<Task<()>>,
}

impl TokenMonitorApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let db_path = default_db_path().expect("resolve app data dir");
        let collector = Arc::new(Collector::open(&db_path).expect("open collector"));
        // Share the collector with the floating ball (which has no window of
        // its own to open a DB connection) so it can persist its visibility
        // preference on close.
        COLLECTOR.set(collector.clone()).ok();
        let scan_interval_secs = collector.scan_interval_seconds();
        let scan_interval = Arc::new(AtomicU64::new(scan_interval_secs));
        let (scheduler_wake, scheduler_wake_rx) = std::sync::mpsc::channel();
        let scheduler =
            scheduler::start_scheduler(collector.clone(), scan_interval.clone(), scheduler_wake_rx);
        let (view_tx, view_rx) = bounded(1);

        // Long-lived aggregate worker: one background thread owns a single
        // read-only SQLite connection and drains an `AggRequest` queue bounded
        // to 1 (latest-wins). This replaces the old scheme where every
        // `refresh_view` spawned a fresh thread + opened a new connection,
        // which could fire 14+ times during one full scan cycle.
        let (agg_req_tx, agg_req_rx) = async_channel::bounded::<AggRequest>(1);
        let agg_db_path = db_path.clone();
        let agg_view_tx = view_tx.clone();
        let _agg_worker = std::thread::Builder::new()
            .name("tokenmonitor-aggregate".into())
            .spawn(move || {
                let conn = match sqlite::open_read(&agg_db_path) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("[TokenMonitor] aggregate worker: open db failed: {e}");
                        return;
                    }
                };
                while let Ok(req) = agg_req_rx.recv_blocking() {
                    let snapshot = compute_view_snapshot(
                        &conn,
                        req.seq,
                        req.time_tab,
                        &req.db_path,
                        req.window,
                        req.charts,
                        req.report,
                    );
                    // bounded(1): if the UI is busy, the latest snapshot
                    // displaces the stale one in the channel.
                    let _ = agg_view_tx.try_send(snapshot);
                }
            })
            .expect("spawn aggregate worker");

        let check_updates_on_startup = collector.check_updates_on_startup();
        let skipped_update_version = collector.skipped_update_version();
        let theme_color = collector.theme_color();

        // Stateful dropdown / date-picker entities live for the app's lifetime;
        // recreating them each render would reset open state on every notify.
        let chart_metric_select = cx.new(|cx| {
            SelectState::new(
                ChartMetric::ALL.to_vec(),
                Some(IndexPath::default()),
                window,
                cx,
            )
        });
        let mut chart_app_options = vec![ChartApp::All];
        chart_app_options.extend(Provider::ALL.into_iter().map(ChartApp::One));
        let chart_app_select = cx
            .new(|cx| SelectState::new(chart_app_options, Some(IndexPath::default()), window, cx));
        let chart_range_select = cx.new(|cx| {
            SelectState::new(
                ChartRangeItem::all(),
                Some(IndexPath::default()),
                window,
                cx,
            )
        });
        let chart_range_picker = cx.new(|cx| DatePickerState::range(window, cx));

        let mut app = TokenMonitorApp {
            state: AppState::default(),
            collector,
            focus_handle,
            weak_self: cx.weak_entity(),
            _scheduler: scheduler,
            _view_tx: view_tx,
            view_rx,
            view_seq: 0,
            applied_seq: 0,
            agg_req_tx,
            _agg_worker,
            chart_metric_select,
            chart_app_select,
            chart_range_select,
            chart_range_picker,
            update_check: UpdateCheckUiState::default(),
            check_updates_on_startup,
            skipped_update_version,
            scan_interval,
            scheduler_wake,
            theme_color,
            #[cfg(target_os = "windows")]
            floating: None,
            #[cfg(target_os = "windows")]
            tray_task: None,
        };
        // Expose a weak handle to this app so the floating ball can link
        // itself back onto the app when it is opened from the tray menu.
        APP_WEAK.set(app.weak_self.clone()).ok();
        app.sync_chart_app_select(window, cx);

        // Dispatch dropdown / date-picker events back into app handlers.
        {
            let metric = app.chart_metric_select.clone();
            cx.subscribe_in(
                &metric,
                window,
                |this, _, ev: &SelectEvent<Vec<ChartMetric>>, _, cx| {
                    if let SelectEvent::Confirm(Some(m)) = ev {
                        this.select_chart_metric(*m, cx);
                    }
                },
            )
            .detach();
        }
        {
            let app_select = app.chart_app_select.clone();
            cx.subscribe_in(
                &app_select,
                window,
                |this, _, ev: &SelectEvent<Vec<ChartApp>>, _, cx| {
                    if let SelectEvent::Confirm(Some(app)) = ev {
                        this.select_chart_app(*app, cx);
                    }
                },
            )
            .detach();
        }
        {
            let range_select = app.chart_range_select.clone();
            cx.subscribe_in(
                &range_select,
                window,
                |this, _, ev: &SelectEvent<Vec<ChartRangeItem>>, window, cx| {
                    if let SelectEvent::Confirm(Some(range)) = ev {
                        this.select_chart_range(*range, window, cx);
                    }
                },
            )
            .detach();
        }
        {
            let picker = app.chart_range_picker.clone();
            cx.subscribe_in(
                &picker,
                window,
                |this, _, ev: &DatePickerEvent, window, cx| match ev {
                    DatePickerEvent::Change(Date::Range(Some(start), Some(end))) => {
                        this.select_chart_custom_range(*start, *end, window, cx);
                    }
                    DatePickerEvent::Change(Date::Range(_, _)) => {
                        // Cleared: no custom dates picked yet; the placeholder
                        // window takes over until a new range is chosen.
                        this.state.charts.custom_range = None;
                        this.sync_chart_range_select(window, cx);
                        this.refresh_view(cx);
                        cx.notify();
                    }
                    _ => {}
                },
            )
            .detach();
        }

        app.spawn_event_loop(cx);
        // Install the accent before the first frame, so the very first paint is
        // already correct instead of flashing gpui_component's Light default.
        TokenMonitorApp::apply_theme(app.theme_color, cx);
        app.trigger_scan(cx); // initial auto-scan so data shows without manual action
        app.refresh_view(cx); // async: returns immediately, fills state in background
        app
    }

    /// Kick off a background scan of every provider.
    pub fn trigger_scan(&mut self, cx: &mut Context<Self>) {
        match self.collector.scan_async() {
            Ok(()) => {
                self.state.scan_status = ScanStatus::Scanning {
                    completed: 0,
                    total: self.collector.sources().len() as u32,
                };
                self.state.last_error = None;
            }
            Err(e) => self.state.last_error = Some(format!("failed to start scan: {e}")),
        }
        cx.notify();
    }

    /// Update the periodic rescan interval: persist it, wake the scheduler so
    /// the new value takes effect immediately, and kick off a scan now so the
    /// change shows without waiting for the next tick.
    pub fn select_scan_interval(&mut self, interval: ScanInterval, cx: &mut Context<Self>) {
        let secs = interval.seconds();
        let changed = self.scan_interval.load(Ordering::Relaxed) != secs;
        self.scan_interval.store(secs, Ordering::Relaxed);
        if let Err(e) = self.collector.set_scan_interval_seconds(secs) {
            self.state.last_error = Some(format!("save scan interval: {e}"));
        }
        if changed {
            let _ = self.scheduler_wake.send(());
            self.trigger_scan(cx);
        }
        cx.notify();
    }

    /// Update the app accent theme color: persist it, push it into the global
    /// theme, and repaint every window so the change is visible at once.
    pub fn select_theme_color(&mut self, color: ThemeColor, cx: &mut Context<Self>) {
        if self.theme_color == color {
            return;
        }
        self.theme_color = color;
        if let Err(e) = self.collector.set_theme_color(color) {
            self.state.last_error = Some(format!("save theme color: {e}"));
        }
        TokenMonitorApp::apply_theme(color, cx);
        // A global mutation repaints nothing on its own. Notifying this root only
        // dirties it and its ancestors, so the widget *views* nested inside it —
        // the settings Select and friends, each an entity with its own cached
        // prepaint — keep replaying their cached output in the old accent. That
        // is why the change used to appear only after navigating away and back,
        // which re-created them. Refreshing all windows forces one full
        // re-render of every view in every window; the ball is a window of its
        // own, so it is covered too.
        cx.refresh_windows();
        cx.notify();
    }

    /// Switch the dashboard time-range tab and re-query the window.
    pub fn select_time_tab(&mut self, tab: TimeTab, cx: &mut Context<Self>) {
        self.state.time_tab = tab;
        self.state.expanded_provider = None; // collapse expansion; data window changed
        self.refresh_view(cx);
        cx.notify();
    }

    /// Switch the active page; entering the charts page triggers a load.
    pub fn select_page(&mut self, page: ActivePage, cx: &mut Context<Self>) {
        self.state.active_page = page;
        self.state.report_hover = None;
        if page == ActivePage::Charts {
            self.refresh_view(cx);
        }
        cx.notify();
    }

    /// Charts control handlers. Range/provider changes re-query; metric/kind
    /// changes are pure render-time transforms.
    pub fn select_chart_range(
        &mut self,
        range: ChartRange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state.charts.range = range;
        self.state.charts.custom_range = None;
        if range != ChartRange::Custom {
            // Preset: clear the picker and re-query immediately. Selecting
            // "自定义" only reveals the picker; the chart keeps its current
            // data until a range is actually chosen.
            self.chart_range_picker.update(cx, |picker, cx| {
                picker.set_date(Date::Range(None, None), window, cx);
            });
            self.refresh_view(cx);
        }
        self.sync_chart_range_select(window, cx);
        cx.notify();
    }

    /// Set a custom (East-8, inclusive) date range, overriding the preset.
    pub fn select_chart_custom_range(
        &mut self,
        start: NaiveDate,
        end: NaiveDate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        self.state.charts.custom_range = Some((start, end));
        self.sync_chart_range_select(window, cx);
        self.refresh_view(cx);
        cx.notify();
    }

    pub fn select_chart_metric(&mut self, metric: ChartMetric, cx: &mut Context<Self>) {
        self.state.charts.metric = metric;
        cx.notify();
    }

    pub fn select_chart_app(&mut self, app: ChartApp, cx: &mut Context<Self>) {
        self.state.charts.app = app;
        self.refresh_view(cx);
        cx.notify();
    }

    /// Expand/collapse a provider card's per-model breakdown.
    pub fn toggle_provider_expanded(&mut self, provider: Provider, cx: &mut Context<Self>) {
        self.state.expanded_provider = if self.state.expanded_provider == Some(provider) {
            None
        } else {
            Some(provider)
        };
        cx.notify();
    }

    /// Rebuild the charts app dropdown ("全部" + every provider) and re-select
    /// the active app filter.
    fn sync_chart_app_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut options = vec![ChartApp::All];
        options.extend(Provider::ALL.into_iter().map(ChartApp::One));
        let selected = self.state.charts.app;
        self.chart_app_select.update(cx, |select, cx| {
            select.set_items(options, window, cx);
            select.set_selected_value(&selected, window, cx);
        });
    }

    /// Rebuild the charts time-range dropdown from the current selection,
    /// refreshing the "自定义" item's title to show the chosen dates.
    fn sync_chart_range_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let options = self.state.charts.range_options();
        let selected = self.state.charts.range;
        self.chart_range_select.update(cx, |select, cx| {
            select.set_items(options, window, cx);
            select.set_selected_value(&selected, window, cx);
        });
    }

    /// Forward collector events into app state, refreshing the view after each scan.
    fn spawn_event_loop(&self, cx: &mut Context<Self>) {
        let receiver = self.collector.events();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = receiver.recv().await {
                let _ = this.update(cx, |app, cx| app.handle_collector_event(event, cx));
            }
        })
        .detach();

        let view_rx = self.view_rx.clone();
        cx.spawn(async move |this, cx| {
            while let Ok(snapshot) = view_rx.recv().await {
                let _ = this.update(cx, |app, cx| app.apply_snapshot(snapshot, cx));
            }
        })
        .detach();
    }

    fn handle_collector_event(&mut self, event: CollectorEvent, cx: &mut Context<Self>) {
        match event {
            CollectorEvent::ScanStarted { .. } => {
                // Each provider reports its own ScanStarted; keep the running
                // completed count instead of resetting it on every provider.
                let total = self.collector.sources().len() as u32;
                let completed = match self.state.scan_status {
                    ScanStatus::Scanning { completed, .. } => completed,
                    _ => 0,
                };
                self.state.scan_status = ScanStatus::Scanning { completed, total };
            }
            CollectorEvent::ScanCompleted { summary } => {
                let total = self.collector.sources().len() as u32;
                let completed = match self.state.scan_status {
                    ScanStatus::Scanning { completed, .. } => completed + 1,
                    _ => 1,
                };
                self.state.scan_status = if completed >= total {
                    ScanStatus::Done {
                        records: summary.records,
                        at: Utc::now(),
                    }
                } else {
                    ScanStatus::Scanning { completed, total }
                };
                // Unchanged scans (fingerprint matched) have no new data; skip
                // the re-query + re-render so idle polls stay near-free.
                if !summary.unchanged {
                    self.refresh_view(cx);
                }
            }
            CollectorEvent::ScanFailed { provider, error } => {
                self.state.last_error = Some(format!("{}: {error}", provider.display_name()));
                self.state.scan_status = ScanStatus::Failed { error };
            }
            CollectorEvent::Watch(_) => {
                let _ = self.collector.scan_async();
            }
        }
        cx.notify();
    }

    /// Kick off a background aggregation. Runs on the **long-lived aggregate
    /// worker** (a single background thread owning one read-only SQLite
    /// connection opened at startup), then posts the result back through
    /// `view_tx` (bounded(1), latest-wins) for the event loop to apply.
    ///
    /// Multiple rapid triggers (e.g. one per provider during a full scan)
    /// collapse: the request channel is bounded to 1, so only the latest
    /// pending request survives — intermediate stale requests are
    /// overwritten before the worker picks them up.
    fn refresh_view(&mut self, _cx: &mut Context<Self>) {
        let seq = self.view_seq.wrapping_add(1);
        self.view_seq = seq;

        let db_path = self.collector.db_path().to_path_buf();
        let now = Utc::now();
        let time_tab = self.state.time_tab;
        let window = time_tab.window(now);
        let charts = if self.state.active_page == ActivePage::Charts {
            Some((self.state.charts.window(now), self.state.charts.app))
        } else {
            None
        };
        // The report section is embedded in the dashboard, so its 365-day
        // snapshot is always loaded alongside the window aggregates.
        let report = Some(TimeWindow::last_n_days(365, now));

        let req = AggRequest {
            seq,
            time_tab,
            db_path,
            window,
            charts,
            report,
        };
        // bounded(1): if a previous request is still queued, this `try_send`
        // fails and we silently drop the older one — the worker will pick up
        // this newer request next. If the worker is idle, this succeeds.
        let _ = self.agg_req_tx.try_send(req);
    }

    fn apply_snapshot(&mut self, snap: ViewSnapshot, cx: &mut Context<Self>) {
        // Apply monotonically: skip only snapshots older than one already
        // applied. This lets intermediate per-provider refreshes stream into the
        // UI during a scan, instead of every result being dropped in favor of
        // the single latest request.
        if snap.seq < self.applied_seq {
            return; // stale: superseded by a newer applied snapshot
        }
        self.applied_seq = snap.seq;
        self.state.summary = snap.summary;
        self.state.by_provider = snap.by_provider;
        self.state.by_provider_model = snap.by_provider_model;
        self.state.by_project = snap.by_project;
        self.state.by_day = snap.by_day;
        if let Some(charts) = snap.charts {
            self.state.charts.data = Some(charts);
        }
        if let Some(report) = snap.report {
            self.state.report.data = Some(report);
        }
        if let Some(error) = snap.error {
            self.state.last_error = Some(error);
        }
        // Keep the floating usage ball in sync with the dashboard summary.
        // The ball is labelled with the period the total was aggregated over —
        // taken from `snap.time_tab`, the tab this snapshot was *computed* for,
        // not the live `state.time_tab`, which may already have changed while
        // the request was in flight. Pairing them in one call is what keeps the
        // label from ever describing a different period than the number.
        #[cfg(target_os = "windows")]
        if let Some(f) = &self.floating {
            if let Some(f) = f.upgrade() {
                if let Some(s) = &snap.summary {
                    let total = s.input_tokens
                        + s.output_tokens
                        + s.cache_read_tokens
                        + s.cache_write_tokens;
                    let time_tab = snap.time_tab;
                    f.update(cx, |view, cx| {
                        view.set_usage(total, time_tab);
                        cx.notify();
                    });
                }
            } else {
                self.floating = None;
            }
        }
        cx.notify();
    }
}

/// Compute the aggregate view snapshot on the calling (background) thread.
/// Reuses the worker's long-lived read-only connection instead of opening a
/// new one per call.
fn compute_view_snapshot(
    conn: &Connection,
    seq: u64,
    time_tab: TimeTab,
    _db_path: &std::path::Path,
    window: TimeWindow,
    charts: Option<(TimeWindow, ChartApp)>,
    report: Option<TimeWindow>,
) -> ViewSnapshot {
    let mut snap = ViewSnapshot {
        seq,
        time_tab,
        ..ViewSnapshot::default()
    };
    let repo = UsageRepo::new(conn);

    // Keep the old partial-success semantics: a failing query sets the error
    // but does not discard the other aggregates.
    match repo.aggregate_window(&window) {
        Ok(s) => snap.summary = Some(s),
        Err(e) => snap.error = Some(format!("query failed: {e}")),
    }
    match repo.aggregate_by_provider(&window) {
        Ok(v) => snap.by_provider = v,
        Err(e) => snap.error = Some(format!("query failed: {e}")),
    }
    match repo.aggregate_by_provider_model(&window) {
        Ok(m) => snap.by_provider_model = m,
        Err(e) => snap.error = Some(format!("query failed: {e}")),
    }
    match repo.aggregate_by_project(&window) {
        Ok(v) => snap.by_project = v,
        Err(e) => snap.error = Some(format!("query failed: {e}")),
    }
    match repo.aggregate_by_day(&window) {
        Ok(v) => snap.by_day = v,
        Err(e) => snap.error = Some(format!("query failed: {e}")),
    }
    if let Some((chart_window, app)) = charts {
        snap.charts = Some(compute_chart_snapshot(&repo, chart_window, app));
    }
    if let Some(report_window) = report {
        snap.report = Some(compute_report_snapshot(&conn, report_window));
    }
    snap
}

/// Compute the report page's raw daily series (last 365 East-8 calendar days),
/// reusing the headless `report::data` loader shared with the TUI frontend.
fn compute_report_snapshot(conn: &Connection, window: TimeWindow) -> ReportSnapshot {
    let mut snap = ReportSnapshot::default();
    if let Ok(days) = crate::report::data::load_report_days(conn, &window) {
        snap.days = Arc::new(days);
    }
    snap
}
/// Compute the charts page's raw per-day series.
fn compute_chart_snapshot(
    repo: &UsageRepo<'_>,
    window: TimeWindow,
    app: ChartApp,
) -> ChartsSnapshot {
    let mut snap = ChartsSnapshot::default();
    for &p in &Provider::ALL {
        if let Ok(series) = repo.daily_series_by_provider(p, &window) {
            snap.provider_series.push((p, series));
        }
    }
    snap.model_series = match app {
        ChartApp::All => {
            let mut merged: BTreeMap<String, Vec<(String, SumStats)>> = BTreeMap::new();
            for &p in &Provider::ALL {
                if let Ok(models) = repo.daily_series_by_provider_model(p, &window) {
                    merge_model_series(&mut merged, models);
                }
            }
            merged
        }
        ChartApp::One(provider) => repo
            .daily_series_by_provider_model(provider, &window)
            .unwrap_or_default(),
    };
    snap
}

/// Merge per-model daily series from one provider into the cross-app map,
/// summing `SumStats` for matching (model, day) keys.
fn merge_model_series(
    dst: &mut BTreeMap<String, Vec<(String, SumStats)>>,
    src: BTreeMap<String, Vec<(String, SumStats)>>,
) {
    for (model, series) in src {
        let entry = dst.entry(model).or_default();
        for (day, stats) in series {
            match entry.iter_mut().find(|(d, _)| *d == day) {
                Some((_, acc)) => acc.add(&stats),
                None => entry.push((day, stats)),
            }
        }
    }
}

impl Focusable for TokenMonitorApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TokenMonitorApp {
    /// Push the app's accent `color` — and the dark surfaces it sits on — into
    /// gpui_component's global `Theme`.
    ///
    /// Called at bootstrap and again whenever the accent changes, but
    /// deliberately **not** as part of `render`:
    ///
    /// * `Theme::global_mut` queues a global-observer notification on every call,
    ///   so writing the theme each frame is a standing request to repaint.
    /// * Render is also too late for the views that own their own state. GPUI
    ///   replays a child view's *cached* prepaint unless that view itself was
    ///   notified or the whole window is refreshing (`ViewElement`'s cache in
    ///   `crates/gpui/src/view.rs`), and `Window::mark_view_dirty` only dirties
    ///   the ancestors of a notified view — never its descendants. Re-rendering
    ///   this root alone therefore leaves every embedded widget view (the
    ///   settings Select, the dropdowns) and the floating ball on the old hue.
    ///
    /// An associated function rather than a `&self` method so it can be exercised
    /// without building the whole app, which owns a live database connection.
    fn apply_theme(color: ThemeColor, cx: &mut App) {
        use gpui_component::{Theme, ThemeMode};
        if Theme::global(cx).mode != ThemeMode::Dark {
            Theme::change(ThemeMode::Dark, None, cx);
        }
        // The default dark theme's surfaces are near-black (#0a0a0a). Lift the
        // main panels to a lighter slate so the app reads as dark-gray rather
        // than pure black.
        let theme = Theme::global_mut(cx);
        theme.background = ui::hsla_from_hex(0x1b1e24);
        theme.secondary = ui::hsla_from_hex(0x262b33);
        theme.muted = ui::hsla_from_hex(0x2a2f38);
        theme.border = ui::hsla_from_hex(0x343a44);
        // Floating popovers (Select dropdown, DatePicker) default to near-black
        // too; lift them to the card surface so they sit above the panel without
        // reading as a black void.
        theme.tokens.popover = ui::hsla_from_hex(0x262b33).into();
        // Segmented tab bars (dashboard time range, report filter) render their
        // track and active pill from these tokens; both default to near-black,
        // so lift them to the card/main background.
        theme.tokens.tab_bar_segmented = ui::hsla_from_hex(0x262b33).into();
        theme.tokens.background = ui::hsla_from_hex(0x1b1e24).into();

        // Apply the user's accent theme color to the primary/button/chart
        // surfaces so the dashboard highlights, primary buttons, and chart
        // series follow the selection. This must come *after* `Theme::change`,
        // which re-applies the registry's default theme and would otherwise
        // overwrite every one of these.
        let accent = ui::accent_color(color);
        let [c1, c2, c3, c4, c5] = ui::accent_palette(color);
        theme.primary = accent;
        theme.primary_hover = accent.lighten(0.08);
        theme.primary_active = accent.darken(0.08);
        theme.button_primary = accent;
        theme.button_primary_hover = accent.lighten(0.08);
        theme.button_primary_active = accent.darken(0.08);
        theme.ring = accent;
        theme.blue = accent;
        theme.blue_light = accent.lighten(0.2);
        theme.chart_1 = c1;
        theme.chart_2 = c2;
        theme.chart_3 = c3;
        theme.chart_4 = c4;
        theme.chart_5 = c5;
    }
}

impl Render for TokenMonitorApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The theme is pushed by `apply_theme` — at bootstrap and on every accent
        // change — rather than here; see its doc comment for why.
        //
        // What remains is a read-only staleness guard. Something can still reset
        // the global behind our back: `gpui_component` boots at `ThemeMode::Light`
        // and rewrites the entire theme whenever its registry changes. Comparing
        // costs one global read, and the write only happens when the accent was
        // actually lost, so this cannot become a per-frame mutation.
        let theme_was_clobbered = {
            use gpui_component::{Theme, ThemeMode};
            let theme = Theme::global(cx);
            theme.mode != ThemeMode::Dark || theme.primary != ui::accent_color(self.theme_color)
        };
        if theme_was_clobbered {
            Self::apply_theme(self.theme_color, cx);
        }

        let p = crate::ui::palette(cx);
        v_flex()
            .id("tokenmonitor-root")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(p.background)
            .text_color(p.foreground)
            .child(ui::topbar::render_topbar(self, window, cx))
            .child(ui::router(self, window, cx))
    }
}

/// Edge length of the floating window (px), matching `WINDOW_SIZE` in
/// `ui::floating`. Must be ≥ the largest clip circle that module can ask for,
/// plus its margin — otherwise the clip would fill the window and `SetWindowRgn`
/// would degenerate into "no clip" (a square frame).
const FLOAT_WIN: f32 = 360.0;

/// Shared so the floating ball can persist its visibility preference on close
/// without holding its own `Collector` reference.
pub(crate) static COLLECTOR: OnceLock<Arc<Collector>> = OnceLock::new();

/// Weak handle to the main app, used to stash the floating window's entity.
pub(crate) static APP_WEAK: OnceLock<WeakEntity<TokenMonitorApp>> = OnceLock::new();

/// Show, hide, or create the floating usage ball. Once the window exists, the
/// toggle is a pure Win32 show/hide; the first call opens it through GPUI.
#[cfg(target_os = "windows")]
pub fn ensure_floating_window(cx: &mut App) {
    let hwnd = get_floating_hwnd();
    if hwnd != 0 && crate::platform::is_window_alive(hwnd) {
        let visible = !is_floating_visible();
        show_window(hwnd, visible);
        set_floating_visible(visible);
        return;
    }
    let bounds = Bounds::centered(None, size(px(FLOAT_WIN), px(FLOAT_WIN)), cx);
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            kind: WindowKind::PopUp,
            focus: false,
            show: true,
            window_background: WindowBackgroundAppearance::Transparent,
            window_decorations: None,
            ..Default::default()
        },
        |window, cx| {
            if let Ok(h) = HasWindowHandle::window_handle(&*window) {
                if let RawWindowHandle::Win32(win) = h.as_raw() {
                    let hwnd = win.hwnd.get();
                    register_floating_hwnd(hwnd);
                    set_always_on_top(hwnd, true);
                    // Round the window before the first paint. `render` also
                    // applies the region every frame, but on the very first
                    // frame the client rect may not be initialised yet — and
                    // then the popup would show its square frame (most visible
                    // at zero usage, where the ball is smallest).
                    crate::ui::floating::seed_window_region(hwnd);
                    // Drag the ball with our own Win32 subclass (not GPUI's
                    // `WindowControlArea::Drag`, which routes the move through
                    // the OS caption loop and flashes a black square).
                    install_ball_drag(hwnd);
                }
            }
            cx.new(|cx| FloatingView::new(window, cx))
        },
    );
    if let Ok(handle) = handle {
        set_floating_visible(true);
        if let Ok(entity) = handle.entity(cx) {
            if let Some(app) = APP_WEAK.get().and_then(|w| w.upgrade()) {
                app.update(cx, |app, _| app.floating = Some(entity.downgrade()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui_component::{Theme, ThemeMode};

    /// The accent has to reach the **global** theme, because that is what every
    /// widget paints from — setting `self.theme_color` alone changes nothing on
    /// screen.
    ///
    /// This also pins the ordering inside `apply_theme`: `Theme::change`
    /// re-applies the registry's default theme, which rewrites `primary`, the
    /// button colours and all five chart colours, so the app's overrides are
    /// only effective while they come *after* it. Moving that call down would
    /// silently hand the whole app gpui_component's default blue.
    #[gpui::test]
    fn apply_theme_installs_the_accent_into_the_global_theme(cx: &mut TestAppContext) {
        cx.update(|cx| gpui_component::init(cx));

        for color in ThemeColor::ALL {
            cx.update(|cx| TokenMonitorApp::apply_theme(color, cx));
            cx.update(|cx| {
                let theme = Theme::global(cx);
                let accent = ui::accent_color(color);
                assert_eq!(theme.mode, ThemeMode::Dark, "the app always forces dark");
                assert_eq!(theme.primary, accent, "{color:?}");
                assert_eq!(theme.button_primary, accent, "{color:?}");
                assert_eq!(theme.ring, accent, "{color:?}");
                assert_eq!(theme.blue, accent, "{color:?}");
                assert_eq!(
                    [
                        theme.chart_1,
                        theme.chart_2,
                        theme.chart_3,
                        theme.chart_4,
                        theme.chart_5
                    ],
                    ui::accent_palette(color),
                    "{color:?}"
                );
                // The dark surface overrides have to survive too — they are what
                // lifts the panels off gpui_component's near-black default.
                assert_eq!(theme.background, ui::hsla_from_hex(0x1b1e24));
                assert_eq!(theme.secondary, ui::hsla_from_hex(0x262b33));
            });
        }
    }
}

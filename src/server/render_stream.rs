//! Virtual rendering helpers for headless client frame streaming.

use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::layout::{Position, Rect, Size};

use crate::app::state::AppState;
use crate::app::Mode;
use crate::protocol::render_ansi::{BlitEncoder, EncodedBlit};
use crate::protocol::{CursorState, FrameData, RenderEncoding, ServerMessage, TerminalFrame};
use crate::terminal::TerminalRuntimeRegistry;

/// Per-client render baseline for the negotiated render encoding.
pub(crate) enum ClientRenderState {
    /// Semantic clients compare full frame data and skip identical frames.
    /// `force_send` makes the next frame ship even if it equals the baseline,
    /// without discarding the baseline — so the cheap retained path can still
    /// patch against it while honoring the post-input frame contract.
    Semantic {
        last_frame: Option<FrameData>,
        force_send: bool,
    },
    /// Terminal-ANSI clients keep a terminal diff encoder and sequence number.
    TerminalAnsi { blit_encoder: BlitEncoder, seq: u64 },
}

impl ClientRenderState {
    pub(crate) fn new(render_encoding: RenderEncoding) -> Self {
        match render_encoding {
            RenderEncoding::SemanticFrame => Self::Semantic {
                last_frame: None,
                force_send: false,
            },
            RenderEncoding::TerminalAnsi => Self::TerminalAnsi {
                blit_encoder: BlitEncoder::new(),
                seq: 0,
            },
        }
    }

    pub(crate) fn reset_baseline(&mut self) {
        match self {
            Self::Semantic { last_frame, .. } => *last_frame = None,
            Self::TerminalAnsi { blit_encoder, .. } => *blit_encoder = BlitEncoder::new(),
        }
    }

    /// Guarantees the next rendered frame is sent even if it equals the
    /// baseline, without dropping the baseline. Used after input so semantic /
    /// remote clients always receive a post-input frame.
    pub(crate) fn reset_semantic_input_baseline(&mut self) {
        if let Self::Semantic { force_send, .. } = self {
            *force_send = true;
        }
    }

    pub(crate) fn semantic_force_send_pending(&self) -> bool {
        matches!(
            self,
            Self::Semantic {
                force_send: true,
                ..
            }
        )
    }

    pub(crate) fn prepare_frame(&mut self, frame: FrameData) -> Option<PreparedRender> {
        match self {
            Self::Semantic {
                last_frame,
                force_send,
            } => {
                let force = std::mem::replace(force_send, false);
                let message = match last_frame.as_ref() {
                    Some(prev) if prev == &frame => {
                        if force {
                            // Post-input contract: deliver a frame even when the
                            // content is unchanged. An empty diff is the minimal
                            // frame the client can ack, and keeps the baseline so
                            // later renders stay on the cheap diff path.
                            crate::render_prof::event(
                                "prepare_frame.semantic.force_send_unchanged",
                            );
                            let diff = frame
                                .diff_from(prev)
                                .expect("dimensions equal, diff must be Some");
                            ServerMessage::FrameDiff(diff)
                        } else {
                            crate::render_prof::event("prepare_frame.semantic.skip_current");
                            return None;
                        }
                    }
                    // Same dimensions as the client's baseline: send only the
                    // rows that changed. The client patches its cached frame.
                    Some(prev) if prev.width == frame.width && prev.height == frame.height => {
                        crate::render_prof::event("prepare_frame.semantic.changed");
                        crate::render_prof::event("prepare_frame.semantic.diff");
                        let diff = frame
                            .diff_from(prev)
                            .expect("dimensions equal, diff must be Some");
                        crate::render_prof::counter(
                            "prepare_frame.semantic.diff_rows",
                            diff.rows.len() as u64,
                        );
                        ServerMessage::FrameDiff(diff)
                    }
                    // No baseline or a resize: send a full keyframe.
                    _ => {
                        crate::render_prof::event("prepare_frame.semantic.changed");
                        crate::render_prof::event("prepare_frame.semantic.keyframe");
                        ServerMessage::Frame(frame.clone())
                    }
                };
                Some(PreparedRender::Semantic { message, frame })
            }
            Self::TerminalAnsi { blit_encoder, seq } => {
                if blit_encoder.is_current(&frame) {
                    crate::render_prof::event("prepare_frame.ansi.skip_current");
                    return None;
                }
                let mut encoded = blit_encoder.encode(&frame, false);
                crate::render_prof::event("prepare_frame.ansi.changed");
                crate::render_prof::counter("prepare_frame.ansi.bytes", encoded.bytes.len() as u64);
                if encoded.full {
                    crate::render_prof::event("prepare_frame.ansi.full");
                } else {
                    crate::render_prof::event("prepare_frame.ansi.partial");
                }
                insert_graphics_before_sync_end(&mut encoded.bytes, &frame.graphics);
                crate::render_prof::counter(
                    "prepare_frame.graphics.bytes",
                    frame.graphics.len() as u64,
                );
                // Move the encoded bytes into the outgoing message rather than
                // cloning the whole frame buffer; `commit` only consumes the
                // cursor/shape/frame state from `encoded`, not its bytes.
                let full = encoded.full;
                let bytes = std::mem::take(&mut encoded.bytes);
                Some(PreparedRender::TerminalAnsi {
                    message: ServerMessage::Terminal(TerminalFrame {
                        seq: *seq + 1,
                        width: frame.width,
                        height: frame.height,
                        full,
                        bytes,
                    }),
                    frame,
                    encoded: Some(encoded),
                })
            }
        }
    }

    pub(crate) fn last_frame(&self) -> Option<&FrameData> {
        match self {
            Self::Semantic { last_frame, .. } => last_frame.as_ref(),
            Self::TerminalAnsi { blit_encoder, .. } => blit_encoder.last_frame(),
        }
    }

    pub(crate) fn commit_sent_frame(&mut self, prepared: PreparedRender) {
        match (self, prepared) {
            // The baseline is always the full new frame, regardless of whether a
            // full keyframe or a diff was sent on the wire.
            (Self::Semantic { last_frame, .. }, PreparedRender::Semantic { frame, .. }) => {
                *last_frame = Some(frame)
            }
            (
                Self::TerminalAnsi { blit_encoder, seq },
                PreparedRender::TerminalAnsi {
                    frame,
                    encoded: Some(encoded),
                    ..
                },
            ) => {
                blit_encoder.commit(frame, encoded);
                *seq += 1;
            }
            _ => {}
        }
    }

    #[cfg(test)]
    pub(crate) fn terminal_seq(&self) -> Option<u64> {
        match self {
            Self::Semantic { .. } => None,
            Self::TerminalAnsi { seq, .. } => Some(*seq),
        }
    }
}

const SYNC_OUTPUT_END: &[u8] = b"\x1b[?2026l";

fn insert_graphics_before_sync_end(encoded: &mut Vec<u8>, graphics: &[u8]) {
    if graphics.is_empty() {
        return;
    }

    if let Some(sync_end) = rfind_subslice(encoded, SYNC_OUTPUT_END) {
        encoded.splice(sync_end..sync_end, graphics.iter().copied());
    } else {
        encoded.extend_from_slice(graphics);
    }
}

fn rfind_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }

    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

/// A prepared client render message plus any baseline state needed after send.
pub(crate) enum PreparedRender {
    Semantic {
        /// Wire message: a full `Frame` keyframe or an incremental `FrameDiff`.
        message: ServerMessage,
        /// The full reconstructed frame, used as the next baseline on commit and
        /// for the oversized-graphics text-only fallback.
        frame: FrameData,
    },
    TerminalAnsi {
        message: ServerMessage,
        frame: FrameData,
        encoded: Option<EncodedBlit>,
    },
}

impl PreparedRender {
    pub(crate) fn message(&self) -> &ServerMessage {
        match self {
            Self::Semantic { message, .. } | Self::TerminalAnsi { message, .. } => message,
        }
    }

    pub(crate) fn into_frame(self) -> Option<FrameData> {
        match self {
            Self::Semantic { frame, .. } => Some(frame),
            Self::TerminalAnsi { frame, .. } => Some(frame),
        }
    }
}

struct CursorTrackingBackend {
    inner: TestBackend,
    rendered_cursor: Option<Position>,
}

impl CursorTrackingBackend {
    fn new(width: u16, height: u16) -> Self {
        Self {
            inner: TestBackend::new(width, height),
            rendered_cursor: None,
        }
    }

    fn buffer(&self) -> &ratatui::buffer::Buffer {
        self.inner.buffer()
    }

    /// Clears the captured cursor before a draw. Required when the backend is
    /// reused across renders so a frame that sets no cursor does not inherit the
    /// previous frame's cursor position.
    fn reset_cursor(&mut self) {
        self.rendered_cursor = None;
    }

    fn rendered_cursor(&self) -> Option<CursorState> {
        self.rendered_cursor.map(|pos| CursorState {
            x: pos.x,
            y: pos.y,
            visible: true,
            shape: 0,
        })
    }
}

impl Backend for CursorTrackingBackend {
    type Error = std::convert::Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()?;
        self.rendered_cursor = None;
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let position = position.into();
        self.inner.set_cursor_position(position)?;
        self.rendered_cursor = Some(position);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

/// Renders the AppState to an in-memory ratatui Buffer.
///
/// This produces the same output as the monolithic binary's terminal draw,
/// but writes to a `Buffer` instead of stdout. Cursor visibility is captured
/// from explicit frame cursor intent rather than incidental backend state.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn render_virtual(
    app_state: &mut AppState,
    area: Rect,
    resize_panes: bool,
) -> (ratatui::buffer::Buffer, Option<CursorState>) {
    let terminal_runtimes = TerminalRuntimeRegistry::new();
    render_virtual_with_runtime_registry(
        app_state,
        &terminal_runtimes,
        area,
        resize_panes,
        crate::kitty_graphics::HostCellSize::default(),
    )
}

pub(crate) fn render_virtual_with_runtime_registry(
    app_state: &mut AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    area: Rect,
    resize_panes: bool,
    cell_size: crate::kitty_graphics::HostCellSize,
) -> (ratatui::buffer::Buffer, Option<CursorState>) {
    let pre_compute_suppresses_focused_terminal_cursor =
        focused_terminal_suppresses_host_cursor(app_state, terminal_runtimes);
    if resize_panes {
        crate::ui::compute_view_with_cell_size(app_state, terminal_runtimes, area, cell_size);
    } else {
        crate::ui::compute_view_without_resizing_panes(app_state, terminal_runtimes, area);
    }
    let suppress_focused_terminal_cursor = pre_compute_suppresses_focused_terminal_cursor
        || focused_terminal_suppresses_host_cursor(app_state, terminal_runtimes);

    let backend = CursorTrackingBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend::new should never fail");

    terminal
        .draw(|frame| {
            crate::ui::render_with_runtime_registry(app_state, terminal_runtimes, frame);
        })
        .expect("render to TestBackend should never fail");

    let buffer = terminal.backend().buffer().clone();
    let cursor = if suppress_focused_terminal_cursor {
        None
    } else {
        focused_terminal_cursor(app_state, terminal_runtimes).or_else(|| {
            (!focused_terminal_owns_host_cursor(app_state, terminal_runtimes))
                .then(|| terminal.backend().rendered_cursor())
                .flatten()
        })
    };

    (buffer, cursor)
}

/// Reusable virtual renderer for the App-client frame path. Holds the ratatui
/// `Terminal` (and its double buffers) across renders so each full frame does
/// not allocate a fresh screen-sized backend, and builds [`FrameData`] directly
/// from the backend buffer without an intermediate clone.
///
/// Correctness: ratatui swaps and resets its previous buffer after each `draw`,
/// and the `TestBackend` retains the last rendered screen, so a reused terminal
/// applies only the inter-frame diff to the backend and the resulting buffer is
/// identical to a freshly allocated render (covered by `pooled_render_matches_
/// fresh_render`).
pub(crate) struct VirtualRenderer {
    terminal: Option<ratatui::Terminal<CursorTrackingBackend>>,
}

impl VirtualRenderer {
    pub(crate) fn new() -> Self {
        Self { terminal: None }
    }

    /// Returns the pooled terminal sized to `width`x`height`, allocating or
    /// resizing it only when the dimensions change.
    fn terminal(
        &mut self,
        width: u16,
        height: u16,
    ) -> &mut ratatui::Terminal<CursorTrackingBackend> {
        pooled_terminal(&mut self.terminal, width, height)
    }

    /// Renders the full App UI into the pooled terminal and returns the frame to
    /// stream to a semantic/ANSI client. Mirrors the standalone
    /// `render_virtual_with_runtime_registry` + `visible_hyperlinks` +
    /// `FrameData::from_ratatui_buffer_with_hyperlinks` sequence, minus the
    /// per-render buffer clone.
    pub(crate) fn render_app_frame(
        &mut self,
        app_state: &mut AppState,
        terminal_runtimes: &TerminalRuntimeRegistry,
        area: Rect,
        resize_panes: bool,
        cell_size: crate::kitty_graphics::HostCellSize,
    ) -> FrameData {
        let render_started = crate::render_prof::timer();
        let pre_compute_suppresses_focused_terminal_cursor =
            focused_terminal_suppresses_host_cursor(app_state, terminal_runtimes);
        if resize_panes {
            crate::ui::compute_view_with_cell_size(app_state, terminal_runtimes, area, cell_size);
        } else {
            crate::ui::compute_view_without_resizing_panes(app_state, terminal_runtimes, area);
        }
        let suppress_focused_terminal_cursor = pre_compute_suppresses_focused_terminal_cursor
            || focused_terminal_suppresses_host_cursor(app_state, terminal_runtimes);

        let terminal = self.terminal(area.width, area.height);
        terminal.backend_mut().reset_cursor();
        terminal
            .draw(|frame| {
                crate::ui::render_with_runtime_registry(app_state, terminal_runtimes, frame);
            })
            .expect("render to TestBackend should never fail");

        let cursor = if suppress_focused_terminal_cursor {
            None
        } else {
            focused_terminal_cursor(app_state, terminal_runtimes).or_else(|| {
                (!focused_terminal_owns_host_cursor(app_state, terminal_runtimes))
                    .then(|| terminal.backend().rendered_cursor())
                    .flatten()
            })
        };
        crate::render_prof::duration_since("full_render.render_virtual", render_started);

        let hyperlinks_started = crate::render_prof::timer();
        let hyperlinks = visible_hyperlinks(app_state, terminal_runtimes);
        crate::render_prof::duration_since("full_render.visible_hyperlinks", hyperlinks_started);

        let frame_started = crate::render_prof::timer();
        let frame = FrameData::from_ratatui_buffer_with_hyperlinks(
            self.terminal
                .as_ref()
                .expect("terminal initialized")
                .backend()
                .buffer(),
            cursor,
            &hyperlinks,
        );
        crate::render_prof::duration_since("full_render.frame_build", frame_started);
        frame
    }
}

/// Returns the pooled terminal in `slot` sized to `width`x`height`, allocating
/// or resizing it only when the dimensions change. Shared by `VirtualRenderer`
/// and `SidebarRenderer` so both reuse one backend across renders instead of
/// allocating a fresh full-screen buffer per frame.
fn pooled_terminal(
    slot: &mut Option<ratatui::Terminal<CursorTrackingBackend>>,
    width: u16,
    height: u16,
) -> &mut ratatui::Terminal<CursorTrackingBackend> {
    let fits = slot.as_ref().is_some_and(|terminal| {
        let area = terminal.backend().buffer().area;
        area.width == width && area.height == height
    });
    if !fits {
        let backend = CursorTrackingBackend::new(width, height);
        *slot = Some(ratatui::Terminal::new(backend).expect("TestBackend::new should never fail"));
    }
    slot.as_mut().expect("terminal initialized above")
}

/// Renders only the desktop sidebar into a full-size buffer, leaving pane and
/// other regions blank. The headless sidebar-only retained path copies the
/// `sidebar_rect` cells out of the rendered buffer into the cached frame,
/// avoiding the expensive full pane redraw when the spinner animation is the
/// only change.
///
/// The backend terminal is pooled across renders. Only the `sidebar_rect` cells
/// are ever read by callers, so stale content left in the pooled buffer outside
/// the sidebar columns is irrelevant — the sidebar columns are fully redrawn
/// every call (same reasoning as `VirtualRenderer`).
pub(crate) struct SidebarRenderer {
    terminal: Option<ratatui::Terminal<CursorTrackingBackend>>,
}

impl SidebarRenderer {
    pub(crate) fn new() -> Self {
        Self { terminal: None }
    }

    /// Renders the sidebar chrome into the pooled terminal and returns the
    /// borrowed backend buffer, avoiding the per-frame buffer clone.
    pub(crate) fn render(
        &mut self,
        app_state: &mut AppState,
        terminal_runtimes: &TerminalRuntimeRegistry,
        area: Rect,
    ) -> &ratatui::buffer::Buffer {
        // Refresh geometry (sidebar_rect, layout) without resizing panes,
        // matching the geometry pass of a full render so the patched cells are
        // identical.
        crate::ui::compute_view_without_resizing_panes(app_state, terminal_runtimes, area);

        let terminal = pooled_terminal(&mut self.terminal, area.width, area.height);
        terminal
            .draw(|frame| {
                crate::ui::render_sidebar_chrome_only(app_state, terminal_runtimes, frame);
            })
            .expect("render to TestBackend should never fail");

        terminal.backend().buffer()
    }
}

/// Renders one server-owned terminal directly for `terminal attach` clients.
pub(crate) fn render_terminal_virtual(
    runtime: &crate::terminal::TerminalRuntime,
    area: Rect,
) -> (ratatui::buffer::Buffer, Option<CursorState>) {
    let suppress_cursor = runtime.synchronized_output_active();
    let backend = CursorTrackingBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend::new should never fail");

    terminal
        .draw(|frame| {
            runtime.render(frame, area, true);
        })
        .expect("render to TestBackend should never fail");

    let buffer = terminal.backend().buffer().clone();
    let cursor = (!suppress_cursor)
        .then(|| runtime.cursor_state(area, true))
        .flatten()
        .map(|cursor| CursorState {
            x: cursor.x,
            y: cursor.y,
            visible: cursor.visible && !crate::ui::pane_is_scrolled_back(runtime),
            shape: cursor.shape,
        })
        .or_else(|| {
            (!suppress_cursor)
                .then(|| terminal.backend().rendered_cursor())
                .flatten()
        });

    (buffer, cursor)
}

pub(crate) fn visible_hyperlinks(
    app_state: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> Vec<((u16, u16), String, String)> {
    let Some(ws_idx) = app_state.active else {
        return Vec::new();
    };
    let Some(tab) = app_state
        .workspaces
        .get(ws_idx)
        .and_then(crate::workspace::Workspace::active_tab)
    else {
        return Vec::new();
    };

    let mut links = Vec::new();
    for info in &app_state.view.pane_infos {
        if let Some(runtime) = tab
            .terminal_id(info.id)
            .and_then(|terminal_id| terminal_runtimes.get(terminal_id))
        {
            links.extend(runtime.visible_hyperlinks(info.inner_rect));
        }
    }
    links
}

pub(crate) fn focused_terminal_cursor(
    app_state: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> Option<CursorState> {
    if app_state.mode != Mode::Terminal {
        return None;
    }

    let ws_idx = app_state.active?;
    let info = app_state
        .view
        .pane_infos
        .iter()
        .find(|info| info.is_focused)?;
    if !app_state.pane_exposes_host_cursor(ws_idx, info.id) {
        return None;
    }
    let rt = app_state.runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id)?;
    if rt.synchronized_output_active() {
        return None;
    }
    let scrolled_back = crate::ui::pane_is_scrolled_back(rt);
    // Determine whether the IME-anchor reveal applies to this focused pane.
    // The master switch must be on, and either no agent filter is configured
    // (apply to any pane) or the focused pane's detected agent matches the
    // allow-list. A configured list with no valid entries reveals nothing.
    let reveal = app_state.reveal_hidden_cursor_for_cjk_ime
        && (!app_state.cjk_ime_agent_filter_configured || {
            let detected = app_state
                .workspaces
                .get(ws_idx)
                .and_then(|ws| ws.terminal_id(info.id))
                .and_then(|tid| app_state.terminals.get(tid))
                .and_then(|t| t.detected_agent);
            detected.is_some_and(|agent| app_state.cjk_ime_agents.contains(&agent))
        });

    if let Some(cursor) = rt.cursor_state(info.inner_rect, true) {
        // When the reveal applies, expose the cursor anchor regardless of the
        // pane's `?25l` request so macOS IMEs keep tracking the candidate
        // window when TUIs paint their own cursor. Scrollback suppression
        // still applies.
        let visible = if reveal {
            !scrolled_back
        } else {
            cursor.visible && !scrolled_back
        };
        Some(CursorState {
            x: cursor.x,
            y: cursor.y,
            visible,
            shape: if reveal && visible {
                app_state.cjk_ime_cursor_shape
            } else {
                cursor.shape
            },
        })
    } else if reveal && !scrolled_back {
        // cursor_state() returned None — the viewport has no cursor position
        // (can happen with complex TUIs). Fall back to the pane's top-left so
        // the outer terminal still exposes a cursor anchor for IME tracking.
        Some(CursorState {
            x: info.inner_rect.x,
            y: info.inner_rect.y,
            visible: true,
            shape: app_state.cjk_ime_cursor_shape,
        })
    } else {
        None
    }
}

fn focused_terminal_owns_host_cursor(
    app_state: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> bool {
    if app_state.mode != Mode::Terminal {
        return false;
    }

    let Some(ws_idx) = app_state.active else {
        return false;
    };
    let Some(info) = app_state
        .view
        .pane_infos
        .iter()
        .find(|info| info.is_focused)
    else {
        return false;
    };
    if !app_state.pane_exposes_host_cursor(ws_idx, info.id) {
        return false;
    }

    app_state
        .runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id)
        .is_some()
}

fn focused_terminal_suppresses_host_cursor(
    app_state: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> bool {
    if app_state.mode != Mode::Terminal {
        return false;
    }

    let Some(ws_idx) = app_state.active else {
        return false;
    };
    let Some(info) = app_state
        .view
        .pane_infos
        .iter()
        .find(|info| info.is_focused)
    else {
        return false;
    };
    if !app_state.pane_exposes_host_cursor(ws_idx, info.id) {
        return false;
    }

    app_state
        .runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id)
        .is_some_and(crate::terminal::TerminalRuntime::synchronized_output_active)
}

#[cfg(test)]
mod sidebar_retained_tests {
    use super::*;
    use crate::detect::{Agent, AgentState};
    use crate::workspace::Workspace;

    /// Builds an app with two workspaces whose agents are in the Working state,
    /// so the sidebar renders their (static) agent-state dots.
    fn working_agent_app() -> AppState {
        let mut app = AppState::test_new();
        app.workspaces = vec![Workspace::test_new("one"), Workspace::test_new("two")];
        app.ensure_test_terminals();
        for ws_idx in 0..2 {
            let pane = app.workspaces[ws_idx].tabs[0].root_pane;
            let terminal_id = app.workspaces[ws_idx].tabs[0].panes[&pane]
                .attached_terminal_id
                .clone();
            let terminal = app.terminals.get_mut(&terminal_id).unwrap();
            terminal.detected_agent = Some(Agent::Claude);
            terminal.state = AgentState::Working;
        }
        app.active = Some(0);
        app.selected = 0;
        // Terminal mode: no navigate/overlay drawing, so an agent-state change
        // only affects the sidebar region — the precondition for the retained
        // sidebar path.
        app.mode = Mode::Terminal;
        app
    }

    fn full_frame(app: &mut AppState, area: Rect) -> FrameData {
        let registry = TerminalRuntimeRegistry::new();
        let (buffer, cursor) = render_virtual_with_runtime_registry(
            app,
            &registry,
            area,
            true,
            crate::kitty_graphics::HostCellSize::default(),
        );
        FrameData::from_ratatui_buffer(&buffer, cursor)
    }

    /// Produces what the sidebar-only retained path would: patch just the
    /// `sidebar_rect` cells of `base` from a freshly rendered sidebar buffer.
    fn patch_sidebar(base: &FrameData, app: &mut AppState, area: Rect) -> FrameData {
        let registry = TerminalRuntimeRegistry::new();
        let mut renderer = SidebarRenderer::new();
        let buffer = renderer.render(app, &registry, area);
        let sidebar_full = FrameData::from_ratatui_buffer(buffer, None);
        let sidebar = app.view.sidebar_rect;
        let mut patched = base.clone();
        let width = usize::from(patched.width);
        for local_y in 0..sidebar.height {
            let y = sidebar.y + local_y;
            let start = usize::from(y) * width + usize::from(sidebar.x);
            let end = start + usize::from(sidebar.width);
            patched.cells[start..end].clone_from_slice(&sidebar_full.cells[start..end]);
        }
        patched
    }

    #[test]
    fn sidebar_patch_reproduces_full_render_when_unchanged() {
        let area = Rect::new(0, 0, 100, 30);
        let mut app = working_agent_app();
        let frame = full_frame(&mut app, area);
        assert!(app.view.sidebar_rect.width > 0 && app.view.sidebar_rect.height > 0);

        let patched = patch_sidebar(&frame, &mut app, area);
        assert_eq!(
            patched.cells, frame.cells,
            "patching the sidebar region with the same widget must reproduce the full frame"
        );
    }

    #[test]
    fn sidebar_patch_matches_full_render_after_agent_state_change() {
        let area = Rect::new(0, 0, 100, 30);
        let mut app = working_agent_app();
        let frame0 = full_frame(&mut app, area);
        let sidebar = app.view.sidebar_rect;

        // Drive a sidebar-confined change the way the retained path is meant to
        // handle it: flip the non-focused workspace's agent state (its sidebar
        // dot color changes; the focused pane content does not).
        let pane = app.workspaces[1].tabs[0].root_pane;
        let terminal_id = app.workspaces[1].tabs[0].panes[&pane]
            .attached_terminal_id
            .clone();
        app.terminals.get_mut(&terminal_id).unwrap().state = AgentState::Idle;
        let frame1 = full_frame(&mut app, area);

        // The state change must actually change the rendered output, or the
        // test is vacuous.
        assert_ne!(
            frame0.cells, frame1.cells,
            "agent state change should change the rendered sidebar"
        );

        // And every change must be confined to the sidebar region.
        let width = usize::from(frame0.width);
        for y in 0..frame0.height {
            for x in 0..frame0.width {
                let in_sidebar = x >= sidebar.x
                    && x < sidebar.x + sidebar.width
                    && y >= sidebar.y
                    && y < sidebar.y + sidebar.height;
                if in_sidebar {
                    continue;
                }
                let idx = usize::from(y) * width + usize::from(x);
                assert_eq!(
                    frame0.cells[idx], frame1.cells[idx],
                    "agent state change altered a cell outside the sidebar at ({x}, {y})"
                );
            }
        }

        // The retained path patches the cached frame0; it must equal a full t1 render.
        let patched = patch_sidebar(&frame0, &mut app, area);
        assert_eq!(
            patched.cells, frame1.cells,
            "sidebar-only patch of the cached frame must equal a full render after the tick"
        );
    }
}

#[cfg(test)]
mod semantic_diff_tests {
    use super::*;
    use crate::protocol::{CellData, RenderEncoding};

    fn cell(sym: &str) -> CellData {
        CellData {
            symbol: sym.into(),
            fg: 0,
            bg: 0,
            modifier: 0,
            skip: false,
            hyperlink: None,
        }
    }

    fn frame(width: u16, height: u16, fill: &str) -> FrameData {
        FrameData {
            cells: vec![cell(fill); usize::from(width) * usize::from(height)],
            width,
            height,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        }
    }

    /// Mirrors the wire message into a simulated client baseline, the same way
    /// the real client reconstructs frames in `client::mod`.
    fn client_apply(baseline: &mut Option<FrameData>, msg: &ServerMessage) {
        match msg {
            ServerMessage::Frame(full) => *baseline = Some(full.clone()),
            ServerMessage::FrameDiff(diff) => {
                let base = baseline.as_mut().expect("diff requires a baseline");
                assert!(
                    base.apply_diff(diff),
                    "client must be able to apply the diff"
                );
            }
            other => panic!("unexpected message for a semantic client: {other:?}"),
        }
    }

    /// Runs one frame through the server and the simulated client, asserting they
    /// stay in sync. Returns the wire message kind label for further assertions.
    fn step(
        server: &mut ClientRenderState,
        client: &mut Option<FrameData>,
        new_frame: FrameData,
    ) -> &'static str {
        let expected = new_frame.clone();
        let Some(prepared) = server.prepare_frame(new_frame) else {
            return "skipped";
        };
        let message = prepared.message().clone();
        client_apply(client, &message);
        server.commit_sent_frame(prepared);
        assert_eq!(
            client.as_ref().expect("client baseline"),
            &expected,
            "client reconstruction must equal the server frame"
        );
        match message {
            ServerMessage::Frame(_) => "frame",
            ServerMessage::FrameDiff(_) => "diff",
            _ => "other",
        }
    }

    #[test]
    fn first_frame_is_keyframe_then_changes_are_diffs_and_stay_in_sync() {
        let mut server = ClientRenderState::new(RenderEncoding::SemanticFrame);
        let mut client: Option<FrameData> = None;

        // First frame: no baseline yet -> full keyframe.
        let f0 = frame(4, 3, "a");
        assert_eq!(step(&mut server, &mut client, f0), "frame");

        // Change one cell: same dimensions -> diff, client stays in sync.
        let mut f1 = frame(4, 3, "a");
        f1.cells[5] = cell("X"); // row 1
        assert_eq!(step(&mut server, &mut client, f1), "diff");

        // Identical frame -> skipped (no message).
        let f1_again = {
            let mut f = frame(4, 3, "a");
            f.cells[5] = cell("X");
            f
        };
        assert_eq!(step(&mut server, &mut client, f1_again), "skipped");

        // Multiple subsequent diffs compose correctly.
        let mut f2 = frame(4, 3, "a");
        f2.cells[5] = cell("X");
        f2.cells[11] = cell("Y"); // row 2
        assert_eq!(step(&mut server, &mut client, f2), "diff");
    }

    #[test]
    fn resize_sends_a_full_keyframe_not_a_diff() {
        let mut server = ClientRenderState::new(RenderEncoding::SemanticFrame);
        let mut client: Option<FrameData> = None;

        assert_eq!(step(&mut server, &mut client, frame(4, 3, "a")), "frame");
        // Different dimensions must not be diffed against the old baseline.
        assert_eq!(step(&mut server, &mut client, frame(6, 2, "b")), "frame");
        // Back to steady state, diffs resume at the new size.
        let mut changed = frame(6, 2, "b");
        changed.cells[0] = cell("Z");
        assert_eq!(step(&mut server, &mut client, changed), "diff");
    }

    #[test]
    fn diff_only_carries_changed_rows() {
        let prev = frame(5, 4, "a");
        let mut next = frame(5, 4, "a");
        next.cells[12] = cell("X"); // row 2 only
        let diff = next.diff_from(&prev).expect("same dims");
        assert_eq!(diff.rows.len(), 1, "only one row changed");
        assert_eq!(diff.rows[0].0, 2, "changed row index is 2");

        let mut reconstructed = prev.clone();
        assert!(reconstructed.apply_diff(&diff));
        assert_eq!(reconstructed, next);
    }

    #[test]
    fn apply_diff_rejects_dimension_mismatch() {
        let mut base = frame(4, 3, "a");
        let other = frame(5, 3, "a");
        let diff = other.diff_from(&base);
        // diff_from across mismatched dims yields None.
        assert!(frame(4, 3, "a").diff_from(&other).is_none());
        assert!(diff.is_none());

        // A hand-built mismatched diff is rejected, leaving base untouched.
        let bogus = crate::protocol::FrameDiffData {
            width: 5,
            height: 3,
            cursor: None,
            rows: Vec::new(),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        assert!(!base.apply_diff(&bogus));
    }
}

#[cfg(test)]
mod pooled_render_tests {
    use super::*;
    use crate::detect::{Agent, AgentState};
    use crate::workspace::Workspace;

    fn working_agent_app() -> AppState {
        let mut app = AppState::test_new();
        app.workspaces = vec![Workspace::test_new("one"), Workspace::test_new("two")];
        app.ensure_test_terminals();
        for ws_idx in 0..2 {
            let pane = app.workspaces[ws_idx].tabs[0].root_pane;
            let terminal_id = app.workspaces[ws_idx].tabs[0].panes[&pane]
                .attached_terminal_id
                .clone();
            let terminal = app.terminals.get_mut(&terminal_id).unwrap();
            terminal.detected_agent = Some(Agent::Claude);
            terminal.state = AgentState::Working;
        }
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;
        app
    }

    fn fresh_frame(app: &mut AppState, rt: &TerminalRuntimeRegistry, area: Rect) -> FrameData {
        let (buffer, cursor) = render_virtual_with_runtime_registry(
            app,
            rt,
            area,
            true,
            crate::kitty_graphics::HostCellSize::default(),
        );
        let hyperlinks = visible_hyperlinks(app, rt);
        FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, cursor, &hyperlinks)
    }

    #[test]
    fn pooled_render_matches_fresh_render() {
        let area = Rect::new(0, 0, 100, 30);
        let rt = TerminalRuntimeRegistry::new();
        let mut app = working_agent_app();
        let mut pooled = VirtualRenderer::new();
        let cell = crate::kitty_graphics::HostCellSize::default();

        // First render: pooled allocates its terminal; must match a fresh render.
        let fresh1 = fresh_frame(&mut app, &rt, area);
        let pooled1 = pooled.render_app_frame(&mut app, &rt, area, true, cell);
        assert_eq!(pooled1, fresh1, "first pooled render must match fresh");

        // Second render with no change: pooled reuses its terminal (diff path).
        let fresh2 = fresh_frame(&mut app, &rt, area);
        let pooled2 = pooled.render_app_frame(&mut app, &rt, area, true, cell);
        assert_eq!(pooled2, fresh2, "reused pooled render must match fresh");

        // Render after a change: pooled must reflect it and still match fresh.
        let pane = app.workspaces[1].tabs[0].root_pane;
        let terminal_id = app.workspaces[1].tabs[0].panes[&pane]
            .attached_terminal_id
            .clone();
        app.terminals.get_mut(&terminal_id).unwrap().state = AgentState::Idle;
        let fresh3 = fresh_frame(&mut app, &rt, area);
        let pooled3 = pooled.render_app_frame(&mut app, &rt, area, true, cell);
        assert_eq!(
            pooled3, fresh3,
            "pooled render after change must match fresh"
        );
        assert_ne!(pooled3, pooled2, "the change must actually alter the frame");
    }

    #[test]
    fn pooled_render_handles_resize() {
        let rt = TerminalRuntimeRegistry::new();
        let mut app = working_agent_app();
        let mut pooled = VirtualRenderer::new();
        let cell = crate::kitty_graphics::HostCellSize::default();

        let small = Rect::new(0, 0, 80, 24);
        let big = Rect::new(0, 0, 120, 40);

        let _ = pooled.render_app_frame(&mut app, &rt, small, true, cell);
        // Re-size up: pooled must reallocate its terminal and still match fresh.
        let fresh_big = fresh_frame(&mut app, &rt, big);
        let pooled_big = pooled.render_app_frame(&mut app, &rt, big, true, cell);
        assert_eq!(pooled_big.width, 120);
        assert_eq!(pooled_big.height, 40);
        assert_eq!(
            pooled_big, fresh_big,
            "pooled render after resize must match fresh"
        );
    }
}

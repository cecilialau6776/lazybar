use std::{
    fmt::Display,
    ops::BitAnd,
    pin::Pin,
    rc::Rc,
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, Result};
use csscolorparser::Color;
use lazy_static::lazy_static;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::{
    net::UnixStream,
    sync::{mpsc::UnboundedSender, OnceCell},
    task::JoinSet,
};
use tokio_stream::{Stream, StreamMap};
use x11rb::{
    connection::Connection,
    protocol::{
        self,
        xproto::{ConnectionExt, Visualtype, Window},
    },
    xcb_ffi::XCBConnection,
};

use crate::{
    create_surface, create_window,
    ipc::{self, ChannelEndpoint},
    set_wm_properties, Alignment, IpcStream, Margins, PanelDrawFn, PanelHideFn,
    PanelShowFn, PanelShutdownFn, PanelStream, Position,
};

lazy_static! {
    static ref REGEX: Regex =
        Regex::new(r"(?<region>[lcr])(?<idx>\d+).(?<message>.+)").unwrap();
    #[allow(missing_docs)]
    pub static ref BAR_INFO: OnceCell<BarInfo> = OnceCell::new();
}

/// Information about the bar, usually for use in building panels.
#[derive(Debug, Clone)]
pub struct BarInfo {
    /// The X resource id of the bar window
    pub window: Window,
    /// The X visual that the bar uses
    pub visual: Visualtype,
    /// The width of the bar in pixels
    pub width: u16,
    /// The height of the bar in pixels
    pub height: u16,
    /// Whether the bar supports transparency
    pub transparent: bool,
    /// The background color of the bar
    pub bg: Color,
}

#[derive(PartialEq, Eq, Debug)]
enum CenterState {
    Center,
    Left,
    Right,
    Unknown,
}

#[derive(Debug)]
enum Region {
    Left,
    CenterRight,
    Right,
    All,
    Custom { start_x: f64, end_x: f64 },
}

#[derive(Debug)]
struct Extents {
    left: f64,
    center: (f64, f64),
    right: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Which neighbor(s) a panel depends on to be shown
///
/// If a panel is dependent on another panel with non-None dependence, it will
/// not be shown.
pub enum Dependence {
    /// The panel will always be shown
    None,
    /// The panel will be shown if its left neighbor has a nonzero width
    Left,
    /// The panel will be shown if its right neighbor has a nonzero width
    Right,
    /// The panel will be shown if both of its neighbors have a nonzero width
    Both,
}

/// Information describing how to draw/redraw a [`Panel`].
pub struct PanelDrawInfo {
    /// The width in pixels of the panel.
    pub width: i32,
    /// The height in pixels of the panel.
    pub height: i32,
    /// When the panel should be hidden
    pub dependence: Dependence,
    /// A [`FnMut`] that draws the panel to the [`cairo::Context`], starting at
    /// (0, 0). Translating the Context is the responsibility of functions in
    /// this module.
    pub draw_fn: PanelDrawFn,
    /// The function to be run when the panel is shown.
    pub show_fn: Option<PanelShowFn>,
    /// The function to be run when the panel is hidden.
    pub hide_fn: Option<PanelHideFn>,
    /// The function to be run before the panel is destroyed. This function
    /// should run as quickly as possible because the shutdown functions
    /// for all panels are held to a time limit.
    pub shutdown: Option<PanelShutdownFn>,
}

impl PanelDrawInfo {
    /// Creates a new [`PanelDrawInfo`] from its components.
    #[must_use]
    pub const fn new(
        dims: (i32, i32),
        dependence: Dependence,
        draw_fn: PanelDrawFn,
        show_fn: Option<PanelShowFn>,
        hide_fn: Option<PanelHideFn>,
        shutdown: Option<PanelShutdownFn>,
    ) -> Self {
        Self {
            width: dims.0,
            height: dims.1,
            dependence,
            draw_fn,
            show_fn,
            hide_fn,
            shutdown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelStatus {
    Shown,
    ZeroWidth,
    Dependent(Dependence),
}

impl BitAnd for PanelStatus {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        if self == Self::Shown && rhs == Self::Shown {
            Self::Shown
        } else {
            Self::ZeroWidth
        }
    }
}

impl From<&Panel> for PanelStatus {
    fn from(value: &Panel) -> Self {
        if value.visible {
            value.draw_info.as_ref().map_or(Self::ZeroWidth, |d| {
                match (d.dependence, d.width) {
                    (Dependence::None, 0) => Self::ZeroWidth,
                    (Dependence::None, _) => Self::Shown,
                    (dep, _) => Self::Dependent(dep),
                }
            })
        } else {
            Self::ZeroWidth
        }
    }
}

/// A button that can be linked to an action for a panel
///
/// Note: scrolling direction may be incorrect depending on your configuration
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum MouseButton {
    /// The left mouse button
    Left,
    /// The middle mouse button
    Middle,
    /// The right mouse button
    Right,
    /// Scrolling up
    ScrollUp,
    /// Scrolling down
    ScrollDown,
}

impl MouseButton {
    fn try_parse(value: u8, reverse: bool) -> Result<Self> {
        match value {
            1 => Ok(Self::Left),
            2 => Ok(Self::Middle),
            3 => Ok(Self::Right),
            4 => {
                if reverse {
                    Ok(Self::ScrollUp)
                } else {
                    Ok(Self::ScrollDown)
                }
            }
            5 => {
                if reverse {
                    Ok(Self::ScrollDown)
                } else {
                    Ok(Self::ScrollUp)
                }
            }
            _ => Err(anyhow!("X server provided invalid button")),
        }
    }
}

/// A mouse event that can be passed to a panel
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct MouseEvent {
    /// The button that was pressed (or scrolled)
    pub button: MouseButton,
    /// The x coordinate of the press, relative to the panel
    pub x: i16,
    /// The y coordinate of the press, relative to the bar
    pub y: i16,
}

/// An event that can be passed to a panel
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Event {
    /// A mouse event
    Mouse(MouseEvent),
    /// A message (typically from another process)
    Action(String),
}

/// A response to an event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventResponse {
    /// The event executed normally
    Ok,
    /// An error occurred
    Err(String),
}

impl Display for EventResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok => write!(f, "SUCCESS"),
            Self::Err(e) => {
                write!(f, "FAILURE: {e}")
            }
        }
    }
}

/// A panel on the bar
pub struct Panel {
    /// How to draw the panel.
    pub draw_info: Option<PanelDrawInfo>,
    /// The current x-coordinate of the panel
    pub x: f64,
    /// The name of the panel (taken from the name of the toml table that
    /// defines it)
    pub name: &'static str,
    /// Whether the panel is visible. To set this value on startup, see
    /// [`PanelCommon`][crate::common::PanelCommon].
    pub visible: bool,
    // true if PanelStatus::Shown, false otherwise
    last_status: bool,
    endpoint: Option<Arc<Mutex<ChannelEndpoint<Event, EventResponse>>>>,
}

impl Panel {
    /// Create a new panel.
    #[must_use]
    pub fn new(
        draw_info: Option<PanelDrawInfo>,
        name: &'static str,
        endpoint: Option<ChannelEndpoint<Event, EventResponse>>,
        visible: bool,
    ) -> Self {
        Self {
            draw_info,
            x: 0.0,
            name,
            visible,
            last_status: false,
            endpoint: endpoint.map(|e| Arc::new(Mutex::new(e))),
        }
    }
}

#[allow(dead_code)]
/// The bar itself.
///
/// See [`parser::parse`][crate::parser::parse] for configuration details.
pub struct Bar {
    pub(crate) name: String,
    position: Position,
    pub(crate) conn: Arc<XCBConnection>,
    screen: usize,
    window: Window,
    surface: cairo::XCBSurface,
    pub(crate) cr: Rc<cairo::Context>,
    width: i32,
    height: u16,
    bg: Color,
    margins: Margins,
    extents: Extents,
    reverse_scroll: bool,
    pub(crate) left_panels: Vec<Panel>,
    pub(crate) center_panels: Vec<Panel>,
    pub(crate) right_panels: Vec<Panel>,
    pub(crate) streams: StreamMap<Alignment, StreamMap<usize, PanelStream>>,
    pub(crate) ipc: bool,
    mapped: bool,
    center_state: CenterState,
}

impl Bar {
    /// Create a new bar, typically from information held by a
    /// [`BarConfig`][crate::BarConfig].
    pub fn new(
        name: &str,
        position: Position,
        height: u16,
        transparent: bool,
        bg: Color,
        margins: Margins,
        reverse_scroll: bool,
        ipc: bool,
        monitor: Option<String>,
    ) -> Result<(Self, IpcStream)> {
        let (conn, screen, window, width, visual, mon) =
            create_window(position, height, transparent, &bg, monitor)?;

        BAR_INFO
            .set(BarInfo {
                window,
                visual,
                width,
                height,
                transparent,
                bg: bg.clone(),
            })
            .unwrap();

        let (result, name) = ipc::init(ipc, name);
        let ipc_stream: Pin<
            Box<
                dyn Stream<
                    Item = std::result::Result<UnixStream, std::io::Error>,
                >,
            >,
        > = match result {
            Ok(stream) => {
                log::info!("IPC initialized");
                stream
            }
            Err(e) => {
                log::info!("IPC disabled due to an error: {e}");
                Box::pin(tokio_stream::pending())
            }
        };

        set_wm_properties(
            &conn,
            window,
            position,
            width.into(),
            height.into(),
            name.as_str(),
            &mon,
        );
        conn.map_window(window)?;
        let surface =
            create_surface(window, visual, width.into(), height.into(), &conn)?;
        let cr = cairo::Context::new(&surface)?;
        surface.flush();
        conn.flush()?;

        Ok((
            Self {
                name,
                position,
                conn: Arc::new(conn),
                screen,
                window,
                surface,
                cr: Rc::new(cr),
                width: width.into(),
                height,
                bg,
                margins,
                extents: Extents {
                    left: 0.0,
                    center: ((width / 2).into(), (width / 2).into()),
                    right: width.into(),
                },
                reverse_scroll,
                left_panels: Vec::new(),
                center_panels: Vec::new(),
                right_panels: Vec::new(),
                streams: StreamMap::new(),
                ipc,
                mapped: true,
                center_state: CenterState::Center,
            },
            ipc_stream,
        ))
    }

    /// Calls each panel's shutdown function
    pub fn shutdown(self) {
        self.left_panels
            .into_iter()
            .chain(self.center_panels)
            .chain(self.right_panels)
            .filter_map(|panel| panel.draw_info)
            .filter_map(|draw_info| draw_info.shutdown)
            .for_each(|shutdown| shutdown());
    }

    fn apply_dependence(panels: &[Panel]) -> Vec<PanelStatus> {
        (0..panels.len())
            .map(|idx| match PanelStatus::from(&panels[idx]) {
                PanelStatus::Shown => PanelStatus::Shown,
                PanelStatus::ZeroWidth => PanelStatus::ZeroWidth,
                PanelStatus::Dependent(Dependence::Left) => panels
                    .get(idx - 1)
                    .map_or(PanelStatus::ZeroWidth, PanelStatus::from),
                PanelStatus::Dependent(Dependence::Right) => panels
                    .get(idx + 1)
                    .map_or(PanelStatus::ZeroWidth, PanelStatus::from),
                PanelStatus::Dependent(Dependence::Both) => {
                    panels
                        .get(idx - 1)
                        .map_or(PanelStatus::ZeroWidth, PanelStatus::from)
                        & panels
                            .get(idx + 1)
                            .map_or(PanelStatus::ZeroWidth, PanelStatus::from)
                }
                PanelStatus::Dependent(Dependence::None) => unreachable!(),
            })
            .collect()
    }

    fn show_panels(&self) {
        self.left_panels
            .iter()
            .chain(self.center_panels.iter())
            .chain(self.right_panels.iter())
            .filter_map(|p| p.draw_info.as_ref())
            .filter_map(|d| d.show_fn.as_ref())
            .for_each(|f| {
                if let Err(e) = f() {
                    log::warn!("showing panel produced an error: {e}");
                }
            });
    }

    fn hide_panels(&self) {
        self.left_panels
            .iter()
            .chain(self.center_panels.iter())
            .chain(self.right_panels.iter())
            .filter_map(|p| p.draw_info.as_ref())
            .filter_map(|d| d.hide_fn.as_ref())
            .for_each(|f| {
                if let Err(e) = f() {
                    log::warn!("hiding panel produced an error: {e}");
                }
            });
    }

    fn process_show_hide_events(
        panels: &mut [Panel],
        statuses: &[PanelStatus],
    ) {
        assert_eq!(panels.len(), statuses.len());
        let mut hidden = Vec::new();
        let mut shown = Vec::new();
        panels
            .iter_mut()
            .zip(statuses.iter())
            .for_each(|(panel, &status)| {
                if panel.draw_info.is_some()
                    && panel.last_status
                    && status != PanelStatus::Shown
                {
                    hidden.push(panel.draw_info.as_ref().unwrap());
                }
                if panel.draw_info.is_some()
                    && !panel.last_status
                    && status == PanelStatus::Shown
                {
                    shown.push(panel.draw_info.as_ref().unwrap());
                }
                panel.last_status = status == PanelStatus::Shown;
            });
        for draw_info in hidden {
            if let Some(ref hide) = draw_info.hide_fn {
                let _ = hide();
            }
        }
        for draw_info in shown {
            if let Some(ref show) = draw_info.show_fn {
                let _ = show();
            }
        }
    }

    /// Handle an event from the X server.
    pub fn process_event(&mut self, event: &protocol::Event) -> Result<()> {
        match event {
            protocol::Event::Expose(_) => {
                log::info!(
                    "Received expose event from X server; redrawing entire bar"
                );
                self.redraw_bar()
            }
            protocol::Event::ButtonPress(event) => match event.detail {
                button @ 1..=5 => {
                    let (x, y) = if event.same_screen {
                        (event.event_x, event.event_y)
                    } else {
                        // TODO: make sure this works/is relevant
                        (event.root_x, event.root_y)
                    };

                    let panel = self
                        .left_panels
                        .iter()
                        .chain(self.center_panels.iter())
                        .chain(self.right_panels.iter())
                        .filter(|p| p.draw_info.is_some())
                        .find(|p| {
                            p.x <= x as f64
                                && p.x
                                    + p.draw_info.as_ref().unwrap().width as f64
                                    >= x as f64
                        });
                    if let Some(p) = panel {
                        if let Some(e) = &p.endpoint {
                            let e = e.lock().unwrap();
                            e.send.send(Event::Mouse(MouseEvent {
                                button: MouseButton::try_parse(
                                    button,
                                    self.reverse_scroll,
                                )
                                // this can never fail due to match arm
                                .unwrap(),
                                x: x - p.x as i16,
                                y,
                            }))?;
                        }
                    }
                    Ok(())
                }
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }

    fn handle_ipc_event(&mut self, message: &str) -> Result<bool> {
        match message {
            "quit" => Ok(true),
            "show" => {
                self.mapped = true;
                self.conn.map_window(self.window)?;
                self.show_panels();
                Ok(false)
            }
            "hide" => {
                self.mapped = true;
                self.conn.unmap_window(self.window)?;
                self.hide_panels();
                Ok(false)
            }
            "toggle" => {
                if self.mapped {
                    self.handle_ipc_event("hide")
                } else {
                    self.handle_ipc_event("show")
                }
            }
            _ => Ok(false),
        }
    }

    fn handle_panel_event(&mut self, message: &str) -> Result<bool> {
        if let Some(caps) = REGEX.captures_iter(message).next() {
            let region = &caps["region"];
            let idx = caps["idx"].parse::<usize>()?;

            if let Some(target) = match region {
                "l" => self.left_panels.get_mut(idx),
                "c" => self.center_panels.get_mut(idx),
                "r" => self.right_panels.get_mut(idx),
                _ => unreachable!(),
            } {
                match &caps["message"] {
                    "show" => target.visible = true,
                    "hide" => target.visible = false,
                    "toggle" => target.visible = !target.visible,
                    message => {
                        return Err(anyhow!("Unknown message {message}"))
                    }
                }

                match region {
                    "l" => self.redraw_left(),
                    "c" => self.redraw_center_right(true),
                    "r" => self.redraw_right(true, None),
                    _ => unreachable!(),
                }?;
            }
        }
        Ok(false)
    }

    /// Sends a message to the appropriate panel.
    pub fn send_message(
        &mut self,
        message: &str,
        ipc_set: &mut JoinSet<Result<()>>,
        ipc_send: UnboundedSender<EventResponse>,
    ) -> Result<bool> {
        if let Some(stripped) = message.strip_prefix('#') {
            return self.handle_panel_event(stripped);
        }

        let (dest, message) = match message.split_once('.') {
            Some((panel, message)) => (Some(panel), message),
            None => (None, message),
        };

        if let Some(panel) = dest {
            let mut panels = self
                .left_panels
                .iter()
                .chain(self.center_panels.iter())
                .chain(self.right_panels.iter())
                .filter(|p| p.name == panel);

            let target = panels.next();
            let (endpoint, message) = match if target.is_none() {
                Err(anyhow!("No panel with name {panel} was found"))
            } else if panels.next().is_some() {
                Err(anyhow!(
                    "This panel has multiple instances and cannot be messaged"
                ))
            } else if let Some(ref endpoint) = target.unwrap().endpoint {
                Ok((endpoint.clone(), message.to_string()))
            } else {
                Err(anyhow!(
                    "The target panel has no associated sender and cannot be \
                     messaged"
                ))
            } {
                Ok(r) => r,
                Err(e) => {
                    let err = e.to_string();
                    ipc_set.spawn_blocking(move || {
                        Ok(ipc_send.send(EventResponse::Err(err))?)
                    });
                    return Err(e);
                }
            };

            ipc_set.spawn_blocking(move || {
                let send = endpoint.lock().unwrap().send.clone();
                let response = if let Err(e) = send.send(Event::Action(message))
                {
                    EventResponse::Err(e.to_string())
                } else {
                    endpoint
                        .lock()
                        .unwrap()
                        .recv
                        .blocking_recv()
                        .unwrap_or(EventResponse::Ok)
                };
                log::trace!("response received");

                ipc_send.send(response)?;
                log::trace!("response sent");

                Ok(())
            });

            log::trace!("task spawned");

            Ok(false)
        } else {
            self.handle_ipc_event(message)
        }
    }

    fn redraw_background(&self, scope: &Region) -> Result<()> {
        self.cr.save()?;
        self.cr.set_operator(cairo::Operator::Source);
        self.cr
            .set_source_rgba(self.bg.r, self.bg.g, self.bg.b, self.bg.a);
        match scope {
            Region::Left => self.cr.rectangle(
                0.0,
                0.0,
                self.extents.left + self.margins.internal,
                f64::from(self.height),
            ),
            Region::CenterRight => self.cr.rectangle(
                self.extents.center.0 - self.margins.internal,
                0.0,
                f64::from(self.width)
                    - (self.extents.center.0 - self.margins.internal),
                f64::from(self.height),
            ),
            Region::Right => self.cr.rectangle(
                self.extents.right - self.margins.internal,
                0.0,
                f64::from(self.width)
                    - (self.extents.right - self.margins.internal),
                f64::from(self.height),
            ),
            Region::All => {
                self.cr.rectangle(
                    0.0,
                    0.0,
                    f64::from(self.width),
                    f64::from(self.height),
                );
            }
            Region::Custom { start_x, end_x } => {
                self.cr.rectangle(
                    *start_x,
                    0.0,
                    end_x - start_x,
                    f64::from(self.height),
                );
            }
        }
        self.cr.fill()?;
        self.cr.restore()?;

        Ok(())
    }

    /// Handle a change in the content of a panel.
    pub fn update_panel(
        &mut self,
        alignment: Alignment,
        idx: usize,
        draw_info: PanelDrawInfo,
    ) -> Result<()> {
        let new_width = f64::from(draw_info.width);
        match alignment {
            Alignment::Left => {
                let cur_width = f64::from(
                    self.left_panels
                        .get(idx)
                        .expect("one or more panels have vanished")
                        .draw_info
                        .as_ref()
                        .map_or(0, |i| i.width),
                );

                self.left_panels
                    .get_mut(idx)
                    .expect("one or more panels have vanished")
                    .draw_info = Some(draw_info);

                if (new_width - cur_width).abs() < f64::EPSILON {
                    self.redraw_one(alignment, idx)?;
                } else if new_width - cur_width
                    + self.extents.left
                    + self.margins.internal
                    < self.extents.center.0
                    && (self.center_state == CenterState::Center
                        || self.center_state == CenterState::Left)
                {
                    self.redraw_left()?;
                } else {
                    self.redraw_bar()?;
                }

                Ok(())
            }
            Alignment::Center => {
                let cur_width = f64::from(
                    self.center_panels
                        .get(idx)
                        .expect("one or more panels have vanished")
                        .draw_info
                        .as_ref()
                        .map_or(0, |i| i.width),
                );

                self.center_panels
                    .get_mut(idx)
                    .expect("one or more panels have vanished")
                    .draw_info = Some(draw_info);

                if (new_width - cur_width).abs() < f64::EPSILON {
                    self.redraw_one(alignment, idx)?;
                } else {
                    self.redraw_bar()?;
                }

                Ok(())
            }
            Alignment::Right => {
                let cur_width = f64::from(
                    self.right_panels
                        .get(idx)
                        .expect("one or more panels have vanished")
                        .draw_info
                        .as_ref()
                        .map_or(0, |i| i.width),
                );

                self.right_panels
                    .get_mut(idx)
                    .expect("one or more panels have vanished")
                    .draw_info = Some(draw_info);

                if (new_width - cur_width).abs() < f64::EPSILON {
                    self.redraw_one(alignment, idx)?;
                } else if self.extents.right
                    - new_width
                    - cur_width
                    - self.margins.internal
                    > self.extents.center.1
                {
                    self.redraw_right(true, None)?;
                } else if (self.extents.right
                    - self.extents.center.1
                    - self.margins.internal)
                    + (self.extents.center.0
                        - self.extents.left
                        - self.margins.internal)
                    > new_width - cur_width
                {
                    self.extents.right += new_width - cur_width;
                    self.redraw_center_right(true)?;
                } else {
                    self.redraw_bar()?;
                }

                self.surface.flush();
                self.conn.flush()?;

                Ok(())
            }
        }
    }

    fn redraw_one(&self, alignment: Alignment, idx: usize) -> Result<()> {
        match alignment {
            Alignment::Left => {
                self.cr.save()?;

                let panel = self
                    .left_panels
                    .get(idx)
                    .expect("one or more panels have vanished");
                if let Some(draw_info) = &panel.draw_info {
                    self.redraw_background(&Region::Custom {
                        start_x: panel.x,
                        end_x: panel.x + f64::from(draw_info.width),
                    })?;
                    self.cr.translate(panel.x, 0.0);
                    (draw_info.draw_fn)(&self.cr, panel.x)?;
                }

                self.surface.flush();
                self.conn.flush()?;
                self.cr.restore()?;

                Ok(())
            }
            Alignment::Center => {
                self.cr.save()?;
                let panel = self
                    .center_panels
                    .get(idx)
                    .expect("one or more panels have vanished");

                if let Some(draw_info) = &self
                    .center_panels
                    .get(idx)
                    .expect("one or more panels have vanished")
                    .draw_info
                {
                    self.redraw_background(&Region::Custom {
                        start_x: panel.x,
                        end_x: panel.x + f64::from(draw_info.width),
                    })?;
                    self.cr.translate(panel.x, 0.0);
                    (draw_info.draw_fn)(&self.cr, panel.x)?;
                }

                self.surface.flush();
                self.conn.flush()?;
                self.cr.restore()?;

                Ok(())
            }
            Alignment::Right => {
                self.cr.save()?;
                let panel = self
                    .right_panels
                    .get(idx)
                    .expect("one or more panels have vanished");

                if let Some(draw_info) = &self
                    .right_panels
                    .get(idx)
                    .expect("one or more panels have vanished")
                    .draw_info
                {
                    self.redraw_background(&Region::Custom {
                        start_x: panel.x,
                        end_x: panel.x + f64::from(draw_info.width),
                    })?;
                    self.cr.translate(panel.x, 0.0);
                    (draw_info.draw_fn)(&self.cr, panel.x)?;
                }

                self.surface.flush();
                self.conn.flush()?;
                self.cr.restore()?;

                Ok(())
            }
        }
    }

    /// Redraw the entire bar, either as the result of an expose event or
    /// because the width of a panel changed.
    ///
    /// Note: this function is not called for every panel update. If the width
    /// doesn't change, only one panel is redrawn, and there are a number of
    /// other cases in which we can redraw only the left or right side. See
    /// [`Bar::update_panel`] for specifics.
    pub fn redraw_bar(&mut self) -> Result<()> {
        log::info!("Redrawing entire bar");

        self.redraw_background(&Region::All)?;

        self.redraw_left()?;
        self.redraw_center_right(false)?;

        Ok(())
    }

    fn redraw_left(&mut self) -> Result<()> {
        log::info!("Redrawing left");

        self.redraw_background(&Region::Left)?;

        self.extents.left = self.margins.left;

        let statuses = Self::apply_dependence(self.left_panels.as_slice());

        Self::process_show_hide_events(
            self.left_panels.as_mut_slice(),
            statuses.as_slice(),
        );

        for panel in self
            .left_panels
            .iter_mut()
            .enumerate()
            .filter(|(idx, _)| {
                statuses.get(*idx).unwrap() == &PanelStatus::Shown
            })
            .map(|(_, panel)| panel)
        {
            if let Some(draw_info) = &panel.draw_info {
                self.cr.save()?;
                let x = self.extents.left;
                panel.x = x;
                self.cr.translate(x, 0.0);
                (draw_info.draw_fn)(&self.cr, x)?;
                self.extents.left += f64::from(draw_info.width);
                self.cr.restore()?;
            }
        }

        self.surface.flush();
        self.conn.flush()?;

        Ok(())
    }

    fn redraw_center_right(&mut self, standalone: bool) -> Result<()> {
        log::info!("Redrawing center panels");
        if standalone {
            self.redraw_background(&Region::CenterRight)?;
        }

        let center_statuses =
            Self::apply_dependence(self.center_panels.as_slice());

        Self::process_show_hide_events(
            self.center_panels.as_mut_slice(),
            center_statuses.as_slice(),
        );

        let center_panels = self
            .center_panels
            .iter_mut()
            .enumerate()
            .filter(|(idx, _)| {
                center_statuses.get(*idx).unwrap() == &PanelStatus::Shown
            })
            .map(|(_, panel)| panel)
            .collect::<Vec<_>>();

        let right_statuses =
            Self::apply_dependence(self.right_panels.as_slice());

        Self::process_show_hide_events(
            self.right_panels.as_mut_slice(),
            right_statuses.as_slice(),
        );

        let right_panels = self
            .right_panels
            .iter()
            .enumerate()
            .filter(|(idx, _)| {
                right_statuses.get(*idx).unwrap() == &PanelStatus::Shown
            })
            .map(|(_, panel)| panel);

        let center_width = f64::from(
            center_panels
                .iter()
                .filter_map(|p| p.draw_info.as_ref().map(|i| i.width))
                .sum::<i32>(),
        );

        self.extents.right = f64::from(
            self.width
                - right_panels
                    .filter_map(|p| p.draw_info.as_ref().map(|i| i.width))
                    .sum::<i32>(),
        ) - self.margins.internal;

        if center_width
            > 2.0f64.mul_add(
                -self.margins.internal,
                self.extents.right - self.extents.left,
            )
        {
            self.extents.center.0 = self.margins.internal + self.extents.left;
            self.extents.center.1 = self.margins.internal + self.extents.left;
            self.center_state = CenterState::Unknown;
        } else if center_width / 2.0
            > self.extents.right
                - f64::from(self.width / 2)
                - self.margins.internal
        {
            self.extents.center.0 =
                self.extents.right - center_width - self.margins.internal;
            self.extents.center.1 =
                self.extents.right - center_width - self.margins.internal;
            self.center_state = CenterState::Left;
        } else if center_width / 2.0
            > f64::from(self.width / 2)
                - self.extents.left
                - self.margins.internal
        {
            self.extents.center.0 = self.extents.left + self.margins.internal;
            self.extents.center.1 = self.extents.left + self.margins.internal;
            self.center_state = CenterState::Right;
        } else {
            self.extents.center.0 =
                f64::from(self.width / 2) - center_width / 2.0;
            self.extents.center.1 =
                f64::from(self.width / 2) - center_width / 2.0;
            self.center_state = CenterState::Center;
        }

        for panel in center_panels {
            if let Some(draw_info) = &panel.draw_info {
                self.cr.save()?;
                let x = self.extents.center.1;
                panel.x = x;
                self.cr.translate(x, 0.0);
                (draw_info.draw_fn)(&self.cr, x)?;
                self.extents.center.1 += f64::from(draw_info.width);
                self.cr.restore()?;
            }
        }

        self.redraw_right(standalone, Some(right_statuses))?;

        self.surface.flush();
        self.conn.flush()?;

        Ok(())
    }

    fn redraw_right(
        &mut self,
        standalone: bool,
        statuses: Option<Vec<PanelStatus>>,
    ) -> Result<()> {
        log::info!("Redrawing right panels");

        if standalone {
            self.redraw_background(&Region::Right)?;
        }

        let statuses = statuses.unwrap_or_else(|| {
            Self::apply_dependence(self.right_panels.as_slice())
        });

        Self::process_show_hide_events(
            self.right_panels.as_mut_slice(),
            statuses.as_slice(),
        );

        let total_width = f64::from(
            self.right_panels
                .iter()
                .enumerate()
                .filter(|(idx, _)| {
                    statuses.get(*idx).unwrap() == &PanelStatus::Shown
                })
                .map(|(_, panel)| panel)
                .filter_map(|p| p.draw_info.as_ref().map(|i| i.width))
                .sum::<i32>(),
        ) + self.margins.right;

        if total_width > f64::from(self.width) - self.extents.center.1 {
            self.extents.right = self.extents.center.1 + self.margins.internal;
        } else {
            self.extents.right = f64::from(self.width) - total_width;
        }

        let mut temp = self.extents.right;

        for panel in self
            .right_panels
            .iter_mut()
            .enumerate()
            .filter(|(idx, _)| {
                statuses.get(*idx).unwrap() == &PanelStatus::Shown
            })
            .map(|(_, panel)| panel)
        {
            if let Some(draw_info) = &panel.draw_info {
                self.cr.save()?;
                let x = temp;
                panel.x = x;
                self.cr.translate(x, 0.0);
                (draw_info.draw_fn)(&self.cr, x)?;
                temp += f64::from(draw_info.width);
                self.cr.restore()?;
            }
        }

        self.surface.flush();
        self.conn.flush()?;

        Ok(())
    }
}

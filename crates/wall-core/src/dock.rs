//! Port of `wall/dockLayout.js`: decides where the wall's docked side panels
//! sit along the left and right edges of the viewport, how wide each docked
//! edge becomes, and how far the topbar insets from each edge.
//!
//! Panels stuck to the same edge stack side by side from the outer edge inward
//! in a fixed priority order: sidebar first, then the `termd` status window,
//! then settings, then help. Closed or floating panels consume no edge width.

/// Width of each panel when docked to a viewport edge, in CSS pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DockWidths {
    pub sidebar: f64,
    pub status: f64,
    pub settings: f64,
    pub help: f64,
}

/// Standard docked widths (`DOCK_WIDTHS` in `wall/dockLayout.js`), in CSS pixels.
pub const DOCK_WIDTHS: DockWidths = DockWidths {
    sidebar: 248.0,
    status: 440.0,
    settings: 380.0,
    help: 400.0,
};

/// Gap the topbar keeps from the canvas edge or from the innermost docked panel, in CSS pixels.
pub const TOPBAR_EDGE_GAP: f64 = 12.0;

/// Where a panel is placed: stuck to the left or right edge, or floating freely.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DockSide {
    Left,
    Right,
    #[default]
    Float,
}

/// Whether one chrome panel is open and which side it occupies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DockPanel {
    pub open: bool,
    pub side: DockSide,
}

impl DockPanel {
    /// Construct a panel state from its open flag and placement side.
    pub const fn new(open: bool, side: DockSide) -> Self {
        Self { open, side }
    }

    fn is_docked(self) -> bool {
        self.open && matches!(self.side, DockSide::Left | DockSide::Right)
    }
}

/// Open and side state for all four dockable panels (`settings_panel` and
/// `help_panel` default to `None`, treated as closed just as in JS).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DockState {
    pub sidebar: DockPanel,
    pub status_window: DockPanel,
    pub settings_panel: Option<DockPanel>,
    pub help_panel: Option<DockPanel>,
}

/// Horizontal measurements at the left and right viewport edges, in CSS pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EdgeWidths {
    pub left: f64,
    pub right: f64,
}

impl EdgeWidths {
    fn add_on(&mut self, side: DockSide, width: f64) -> f64 {
        match side {
            DockSide::Left => {
                let offset = self.left;
                self.left += width;
                offset
            }
            DockSide::Right => {
                let offset = self.right;
                self.right += width;
                offset
            }
            DockSide::Float => 0.0,
        }
    }
}

/// Computed offsets for each panel from its docked edge, total width consumed
/// per edge, and topbar insets from each edge (`dockLayout` in `wall/dockLayout.js`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DockLayout {
    pub sidebar: f64,
    pub status: f64,
    pub settings: f64,
    pub help: f64,
    pub edges: EdgeWidths,
    pub inset: EdgeWidths,
}

/// Compute where the docked panels sit, the total width docked on each edge,
/// and how far the topbar stays in from each edge.
pub fn dock_layout(state: &DockState) -> DockLayout {
    let mut used = EdgeWidths::default();
    let mut sidebar = 0.0;
    let mut status = 0.0;
    let mut settings = 0.0;
    let mut help = 0.0;

    if state.sidebar.is_docked() {
        sidebar = used.add_on(state.sidebar.side, DOCK_WIDTHS.sidebar);
    }
    if state.status_window.is_docked() {
        status = used.add_on(state.status_window.side, DOCK_WIDTHS.status);
    }
    if let Some(panel) = state.settings_panel {
        if panel.is_docked() {
            settings = used.add_on(panel.side, DOCK_WIDTHS.settings);
        }
    }
    if let Some(panel) = state.help_panel {
        if panel.is_docked() {
            help = used.add_on(panel.side, DOCK_WIDTHS.help);
        }
    }

    DockLayout {
        sidebar,
        status,
        settings,
        help,
        edges: used,
        inset: EdgeWidths {
            left: used.left + TOPBAR_EDGE_GAP,
            right: used.right + TOPBAR_EDGE_GAP,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDGE: f64 = 12.0;

    fn panel(open: bool, side: DockSide) -> DockPanel {
        DockPanel::new(open, side)
    }

    #[test]
    fn nothing_docked_keeps_edge_gap() {
        let layout = dock_layout(&DockState {
            sidebar: panel(false, DockSide::Right),
            status_window: panel(true, DockSide::Float),
            ..Default::default()
        });
        assert_eq!(layout.inset, EdgeWidths { left: EDGE, right: EDGE });
    }

    #[test]
    fn one_panel_per_edge_sits_at_its_edge_and_chrome_clears_it() {
        let layout = dock_layout(&DockState {
            sidebar: panel(true, DockSide::Left),
            status_window: panel(true, DockSide::Right),
            ..Default::default()
        });
        assert_eq!(layout.sidebar, 0.0);
        assert_eq!(layout.status, 0.0);
        assert_eq!(
            layout.inset,
            EdgeWidths {
                left: DOCK_WIDTHS.sidebar + EDGE,
                right: DOCK_WIDTHS.status + EDGE,
            }
        );
    }

    #[test]
    fn both_on_one_edge_stacks_status_beside_sidebar() {
        for side in [DockSide::Left, DockSide::Right] {
            let layout = dock_layout(&DockState {
                sidebar: panel(true, side),
                status_window: panel(true, side),
                ..Default::default()
            });
            assert_eq!(layout.sidebar, 0.0);
            assert_eq!(layout.status, DOCK_WIDTHS.sidebar);
            let (same_inset, other_inset) = match side {
                DockSide::Left => (layout.inset.left, layout.inset.right),
                DockSide::Right => (layout.inset.right, layout.inset.left),
                DockSide::Float => unreachable!(),
            };
            assert_eq!(same_inset, DOCK_WIDTHS.sidebar + DOCK_WIDTHS.status + EDGE);
            assert_eq!(other_inset, EDGE);
        }
    }

    #[test]
    fn closed_or_floating_panel_takes_no_edge() {
        let closed = dock_layout(&DockState {
            sidebar: panel(false, DockSide::Right),
            status_window: panel(true, DockSide::Right),
            ..Default::default()
        });
        assert_eq!(closed.status, 0.0);
        assert_eq!(closed.inset.right, DOCK_WIDTHS.status + EDGE);

        let floating = dock_layout(&DockState {
            sidebar: panel(true, DockSide::Float),
            status_window: panel(true, DockSide::Right),
            ..Default::default()
        });
        assert_eq!(floating.status, 0.0);
    }

    #[test]
    fn help_docks_after_settings_on_shared_edge_and_floating_takes_none() {
        let layout = dock_layout(&DockState {
            sidebar: panel(true, DockSide::Right),
            status_window: panel(false, DockSide::Right),
            settings_panel: Some(panel(true, DockSide::Right)),
            help_panel: Some(panel(true, DockSide::Right)),
        });
        assert_eq!(layout.settings, DOCK_WIDTHS.sidebar);
        assert_eq!(layout.help, DOCK_WIDTHS.sidebar + DOCK_WIDTHS.settings);
        assert_eq!(
            layout.inset.right,
            DOCK_WIDTHS.sidebar + DOCK_WIDTHS.settings + DOCK_WIDTHS.help + EDGE
        );

        let floating_help = dock_layout(&DockState {
            sidebar: panel(false, DockSide::Right),
            status_window: panel(false, DockSide::Right),
            settings_panel: None,
            help_panel: Some(panel(true, DockSide::Float)),
        });
        assert_eq!(floating_help.inset.right, EDGE);
    }
}

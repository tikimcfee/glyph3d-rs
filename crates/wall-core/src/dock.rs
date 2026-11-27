//! Port of `wall/dockLayout.js`: decides where the wall's docked side panels
//! sit along the left and right edges of the viewport, how wide each docked
//! edge becomes, and how far the topbar insets from each edge.
//!
//! Panels stuck to the same edge stack side by side from the outer edge inward
//! in a fixed priority order: sidebar first, then the `termd` status window,
//! then settings, then help. Closed or floating panels consume no edge width.
//!
//! A panel's size is the person's to set (2026-11-27; the JS panels were
//! fixed): [`PanelSize`] is what a drag on a panel's edge leaves in the
//! store, [`PanelLimits`] what a size may be, and [`DockState::widths`] the
//! docked widths the layout stacks.

use serde::{Deserialize, Serialize};

/// Width of each panel when docked to a viewport edge, in CSS pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DockWidths {
    pub sidebar: f64,
    pub status: f64,
    pub settings: f64,
    pub help: f64,
}

/// Standard docked widths (`DOCK_WIDTHS` in `wall/dockLayout.js`), in CSS pixels:
/// what each panel is until someone drags its edge.
pub const DOCK_WIDTHS: DockWidths = DockWidths {
    sidebar: 248.0,
    status: 440.0,
    settings: 380.0,
    help: 400.0,
};

impl Default for DockWidths {
    fn default() -> Self {
        DOCK_WIDTHS
    }
}

impl DockWidths {
    /// One panel's docked width.
    pub const fn get(&self, kind: PanelKind) -> f64 {
        match kind {
            PanelKind::Sidebar => self.sidebar,
            PanelKind::Status => self.status,
            PanelKind::Settings => self.settings,
            PanelKind::Help => self.help,
        }
    }

    /// Set one panel's docked width.
    pub fn set(&mut self, kind: PanelKind, width: f64) {
        match kind {
            PanelKind::Sidebar => self.sidebar = width,
            PanelKind::Status => self.status = width,
            PanelKind::Settings => self.settings = width,
            PanelKind::Help => self.help = width,
        }
    }
}

/// The four panels that dock to an edge or float.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PanelKind {
    Sidebar,
    Status,
    Settings,
    Help,
}

impl PanelKind {
    /// Every dockable panel, in the order they stack from an edge inward.
    pub const ALL: [PanelKind; 4] = [
        PanelKind::Sidebar,
        PanelKind::Status,
        PanelKind::Settings,
        PanelKind::Help,
    ];

    /// What a refusal calls the panel ("the sidebar docks ...").
    pub const fn name(self) -> &'static str {
        match self {
            PanelKind::Sidebar => "sidebar",
            PanelKind::Status => "status window",
            PanelKind::Settings => "settings panel",
            PanelKind::Help => "help",
        }
    }

    /// What its size may be, and what it is when none was set.
    pub const fn limits(self) -> PanelLimits {
        match self {
            PanelKind::Sidebar => PanelLimits {
                dock_default: DOCK_WIDTHS.sidebar,
                dock_min: 180.0,
                float_default_width: DOCK_WIDTHS.sidebar,
                float_min_width: 180.0,
                ..PanelLimits::COMMON
            },
            // Floating, the status window is as wide as its table until a
            // width is set: `float_default_width` is the width it starts
            // measuring from.
            PanelKind::Status => PanelLimits {
                dock_default: DOCK_WIDTHS.status,
                dock_min: 300.0,
                float_default_width: 580.0,
                float_min_width: 300.0,
                ..PanelLimits::COMMON
            },
            PanelKind::Settings => PanelLimits {
                dock_default: DOCK_WIDTHS.settings,
                dock_min: 300.0,
                float_default_width: DOCK_WIDTHS.settings,
                float_min_width: 300.0,
                ..PanelLimits::COMMON
            },
            PanelKind::Help => PanelLimits {
                dock_default: DOCK_WIDTHS.help,
                dock_min: 280.0,
                float_default_width: DOCK_WIDTHS.help,
                float_min_width: 280.0,
                ..PanelLimits::COMMON
            },
        }
    }
}

/// What the command palette's size may be: it has no docked form, and its
/// width is centred, so only the floating half is read.
pub const PALETTE_LIMITS: PanelLimits = PanelLimits {
    float_default_width: 560.0,
    float_min_width: 320.0,
    float_min_height: 160.0,
    ..PanelLimits::COMMON
};

/// What one panel's size may be, in CSS pixels. The window caps a size
/// further when the panel is drawn (a panel is never wider than the
/// viewport); these are what a stored size or a typed verb is held to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanelLimits {
    /// Docked width when none was set.
    pub dock_default: f64,
    pub dock_min: f64,
    pub dock_max: f64,
    /// Floating width when none was set.
    pub float_default_width: f64,
    pub float_min_width: f64,
    pub float_max_width: f64,
    pub float_min_height: f64,
    pub float_max_height: f64,
}

impl PanelLimits {
    /// The limits no panel overrides.
    const COMMON: PanelLimits = PanelLimits {
        dock_default: 320.0,
        dock_min: 180.0,
        dock_max: 1600.0,
        float_default_width: 320.0,
        float_min_width: 180.0,
        float_max_width: 4000.0,
        float_min_height: 120.0,
        float_max_height: 4000.0,
    };

    /// A docked width a verb asked for: a number within the limits, or a
    /// refusal naming the panel and what it takes.
    pub fn check_dock_width(&self, name: &str, width: f64) -> Result<f64, String> {
        if width.is_finite() && width >= self.dock_min && width <= self.dock_max {
            Ok(width)
        } else {
            Err(format!(
                "the {name} docks {} to {} px wide, not {width}",
                self.dock_min, self.dock_max
            ))
        }
    }

    /// A floating width a verb asked for.
    pub fn check_float_width(&self, name: &str, width: f64) -> Result<f64, String> {
        if width.is_finite() && width >= self.float_min_width && width <= self.float_max_width {
            Ok(width)
        } else {
            Err(format!(
                "the {name} floats {} to {} px wide, not {width}",
                self.float_min_width, self.float_max_width
            ))
        }
    }

    /// A floating height a verb asked for.
    pub fn check_float_height(&self, name: &str, height: f64) -> Result<f64, String> {
        if height.is_finite()
            && height >= self.float_min_height
            && height <= self.float_max_height
        {
            Ok(height)
        } else {
            Err(format!(
                "the {name} floats {} to {} px tall, not {height}",
                self.float_min_height, self.float_max_height
            ))
        }
    }
}

/// The sizes a person gave a panel by dragging its edges (or with
/// `wall.<panel> width|size`), kept in the store beside where the panel is.
/// `None` is the panel's own default. Docked and floating are sized apart: a
/// docked panel's width is taken from the canvas, a floating one's is not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PanelSize {
    /// Width while docked to an edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dock_width: Option<f64>,
    /// Width while floating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub float_width: Option<f64>,
    /// Height while floating; unset, a floating panel is as tall as what it holds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub float_height: Option<f64>,
}

impl PanelSize {
    /// Whether nothing was set: the panel is at its defaults.
    pub fn is_default(&self) -> bool {
        *self == PanelSize::default()
    }

    /// This size with every value outside `limits` dropped back to the
    /// default: what a size read from a store file is passed through, so a
    /// file edited by hand cannot make a panel no one can reach the edge of.
    pub fn within(self, limits: &PanelLimits) -> PanelSize {
        PanelSize {
            dock_width: self
                .dock_width
                .filter(|w| limits.check_dock_width("", *w).is_ok()),
            float_width: self
                .float_width
                .filter(|w| limits.check_float_width("", *w).is_ok()),
            float_height: self
                .float_height
                .filter(|h| limits.check_float_height("", *h).is_ok()),
        }
    }
}

/// Gap the topbar keeps from the canvas edge or from the innermost docked panel, in CSS pixels.
pub const TOPBAR_EDGE_GAP: f64 = 12.0;

/// Where a panel is placed: stuck to the left or right edge, or floating freely.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DockSide {
    Left,
    Right,
    #[default]
    Float,
}

impl DockSide {
    pub const fn as_str(self) -> &'static str {
        match self {
            DockSide::Left => "left",
            DockSide::Right => "right",
            DockSide::Float => "float",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "left" => Some(DockSide::Left),
            "right" => Some(DockSide::Right),
            "float" => Some(DockSide::Float),
            _ => None,
        }
    }
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
/// `help_panel` default to `None`, treated as closed just as in JS), and how
/// wide each is when docked.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DockState {
    pub sidebar: DockPanel,
    pub status_window: DockPanel,
    pub settings_panel: Option<DockPanel>,
    pub help_panel: Option<DockPanel>,
    /// Each panel's docked width ([`DOCK_WIDTHS`] until one is resized).
    pub widths: DockWidths,
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
    /// The docked width of each panel the offsets were stacked with.
    pub widths: DockWidths,
}

impl DockLayout {
    /// One panel's offset from the edge it is docked to.
    pub const fn offset(&self, kind: PanelKind) -> f64 {
        match kind {
            PanelKind::Sidebar => self.sidebar,
            PanelKind::Status => self.status,
            PanelKind::Settings => self.settings,
            PanelKind::Help => self.help,
        }
    }
}

/// Compute where the docked panels sit, the total width docked on each edge,
/// and how far the topbar stays in from each edge.
pub fn dock_layout(state: &DockState) -> DockLayout {
    let mut used = EdgeWidths::default();
    let mut sidebar = 0.0;
    let mut status = 0.0;
    let mut settings = 0.0;
    let mut help = 0.0;

    let widths = state.widths;
    if state.sidebar.is_docked() {
        sidebar = used.add_on(state.sidebar.side, widths.sidebar);
    }
    if state.status_window.is_docked() {
        status = used.add_on(state.status_window.side, widths.status);
    }
    if let Some(panel) = state.settings_panel {
        if panel.is_docked() {
            settings = used.add_on(panel.side, widths.settings);
        }
    }
    if let Some(panel) = state.help_panel {
        if panel.is_docked() {
            help = used.add_on(panel.side, widths.help);
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
        widths,
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
            ..Default::default()
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
            ..Default::default()
        });
        assert_eq!(floating_help.inset.right, EDGE);
    }

    #[test]
    fn a_resized_panel_moves_what_is_stacked_inside_it_and_the_inset() {
        // The sidebar at 320 and settings at 500, both on the right: settings
        // starts where the sidebar now ends, and the topbar clears both.
        let mut widths = DOCK_WIDTHS;
        widths.set(PanelKind::Sidebar, 320.0);
        widths.set(PanelKind::Settings, 500.0);
        let layout = dock_layout(&DockState {
            sidebar: panel(true, DockSide::Right),
            status_window: panel(true, DockSide::Left),
            settings_panel: Some(panel(true, DockSide::Right)),
            help_panel: None,
            widths,
        });
        assert_eq!(layout.offset(PanelKind::Sidebar), 0.0);
        assert_eq!(layout.offset(PanelKind::Settings), 320.0);
        assert_eq!(layout.edges.right, 820.0);
        assert_eq!(layout.inset.right, 820.0 + EDGE);
        // The other edge is untouched, and the layout says what it stacked with.
        assert_eq!(layout.edges.left, DOCK_WIDTHS.status);
        assert_eq!(layout.widths, widths);
        assert_eq!(layout.widths.get(PanelKind::Help), DOCK_WIDTHS.help);

        // Unset, every width is the standard one.
        assert_eq!(DockState::default().widths, DOCK_WIDTHS);
    }

    #[test]
    fn a_size_is_held_to_its_panels_limits_and_refused_by_name() {
        let sidebar = PanelKind::Sidebar.limits();
        assert_eq!(sidebar.dock_default, DOCK_WIDTHS.sidebar);
        assert_eq!(sidebar.check_dock_width("sidebar", 320.0), Ok(320.0));
        assert_eq!(sidebar.check_dock_width("sidebar", 180.0), Ok(180.0));
        assert_eq!(
            sidebar.check_dock_width("sidebar", 50.0).unwrap_err(),
            "the sidebar docks 180 to 1600 px wide, not 50"
        );
        assert!(sidebar.check_dock_width("sidebar", f64::NAN).is_err());
        assert!(sidebar.check_dock_width("sidebar", 1e9).is_err());
        assert_eq!(
            sidebar.check_float_width("sidebar", 10.0).unwrap_err(),
            "the sidebar floats 180 to 4000 px wide, not 10"
        );
        assert_eq!(
            sidebar.check_float_height("sidebar", 10.0).unwrap_err(),
            "the sidebar floats 120 to 4000 px tall, not 10"
        );
        assert_eq!(sidebar.check_float_height("sidebar", 400.0), Ok(400.0));

        // Every panel's defaults are inside its own limits.
        for kind in PanelKind::ALL {
            let l = kind.limits();
            assert!(l.check_dock_width(kind.name(), l.dock_default).is_ok(), "{kind:?}");
            assert!(
                l.check_float_width(kind.name(), l.float_default_width).is_ok(),
                "{kind:?}"
            );
        }
        assert!(PALETTE_LIMITS
            .check_float_width("palette", PALETTE_LIMITS.float_default_width)
            .is_ok());
    }

    #[test]
    fn a_stored_size_drops_what_is_out_of_range_and_writes_only_what_was_set() {
        let limits = PanelKind::Sidebar.limits();
        let size = PanelSize {
            dock_width: Some(320.0),
            float_width: Some(5.0),
            float_height: Some(f64::INFINITY),
        };
        assert_eq!(
            size.within(&limits),
            PanelSize { dock_width: Some(320.0), float_width: None, float_height: None }
        );
        assert!(PanelSize::default().is_default());
        assert!(!size.is_default());

        // On disk: camelCase, and nothing for what was never set.
        let json = serde_json::to_string(&PanelSize {
            dock_width: Some(320.0),
            ..PanelSize::default()
        })
        .unwrap();
        assert_eq!(json, r#"{"dockWidth":320.0}"#);
        let back: PanelSize = serde_json::from_str(r#"{"floatWidth":500,"floatHeight":400}"#).unwrap();
        assert_eq!(
            back,
            PanelSize { dock_width: None, float_width: Some(500.0), float_height: Some(400.0) }
        );
    }
}

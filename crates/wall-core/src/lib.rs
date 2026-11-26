//! The wall's model: terminals on an infinite canvas, and the rules that
//! place, group, split, snap, focus and name them, with no window and no
//! renderer (`out/WALL-PLAN-2026-11-26.md`, work package E).
//!
//! Each module is a port of one module of the JS wall (`wall/` in
//! glyph3d-js), named in its header, with that module's tests ported beside
//! it. Numbers are `f64` throughout, as they are in the JS, so a ported
//! test compares the same values.
//!
//! World space is the JS wall's: units are world units, **y is up**, and a
//! rectangle is its min and max corners ([`geom::Rect`]).

pub mod geom;
pub mod input;

pub mod camera;
pub mod chord;
pub mod config;
pub mod dock;
pub mod groups;
pub mod keymap;
pub mod labels;
pub mod layout;
pub mod menu;
pub mod mouse;
pub mod names;
pub mod palette;
pub mod scroll;
pub mod selection;
pub mod settings;
pub mod sidebar;
pub mod snap;
pub mod split;
pub mod status;
pub mod store;

//! egui chrome panels drawn over the terminal wall (`out/wall-port/briefs/P-panels.md`).
//!
//! Every panel is a pure view function (`show(ctx, state, input, out)`): plain data from
//! `wall-core` enters through `input`, widget-local view state lives in `state`, and user
//! actions leave solely as `wall.*` verb lines appended to `out`.

pub mod agent_view;

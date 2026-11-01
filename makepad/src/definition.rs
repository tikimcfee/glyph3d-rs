//! The definition engine's view (2026-10-31): one definition-run drawn as
//! the panels `definition_engine::view` lays out (the passage, the options
//! for the word in focus, the stack of diff cards), each a text the engine
//! places (`glyph3d_native::build_text_scene`), and a tap on any glyph
//! applied to the run through the same module.
//!
//! The run's file in the store is the only channel between this window and
//! the agent: a tap saves the run (an `expand`, a `substitute`, a
//! `checkout`), the agent's `defn pending --wait` wakes on it, its `defn
//! answer` saves the run back, and [`DefinitionView::poll`] sees the file's
//! stamp move and rebuilds the scene. A rebuild is a whole layout of a few
//! small texts (milliseconds), and the camera stays where it was.
//!
//! Run with `--definition-run <id or prefix>` (and `--definition-store
//! DIR`, else `$DEFN_STORE`, else the store's default), taken off the
//! command line before the engine's CLI reads the rest.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use definition_engine::view::{self, Metrics, View};
use definition_engine::{now_seconds, Run, Store};
use glyph3d_native::atlas::Atlas;
use glyph3d_native::glyph_scene::{CameraMode, UiProbe};
use glyph3d_native::repo::RepoParams;
use glyph3d_native::scene::SceneLike;
use glyph3d_native::{build_text_scene, PlacedText, SceneCullOptions, SceneLoad};

/// The flags that name the run, and the store holding it.
pub const RUN_FLAG: &str = "--definition-run";
pub const STORE_FLAG: &str = "--definition-store";

/// How often the run's file is checked for a change. A check is one stat.
const POLL: Duration = Duration::from_millis(100);

/// Take this module's flags (and their values) out of `args`; Some when a
/// run was named.
pub fn take_args(args: &mut Vec<String>) -> Result<Option<(String, Option<PathBuf>)>, String> {
    let mut take = |flag: &str| -> Result<Option<String>, String> {
        let Some(at) = args.iter().position(|a| a == flag) else { return Ok(None) };
        if at + 1 >= args.len() {
            return Err(format!("{flag} needs a value"));
        }
        let value = args.remove(at + 1);
        args.remove(at);
        Ok(Some(value))
    };
    let run = take(RUN_FLAG)?;
    let store = take(STORE_FLAG)?.map(PathBuf::from);
    match (run, store) {
        (Some(run), store) => Ok(Some((run, store))),
        (None, Some(_)) => Err(format!("{STORE_FLAG} without {RUN_FLAG}")),
        (None, None) => Ok(None),
    }
}

pub struct DefinitionView {
    store: Store,
    run: Run,
    /// The run file's stamp when `run` was read or written here.
    stamp: Option<(SystemTime, u64)>,
    last_poll: Instant,
    atlas: Arc<Atlas>,
    params: RepoParams,
    metrics: Metrics,
    view: View,
    /// The last refusal, shown at the foot of the options panel until the
    /// next change.
    status: Option<String>,
}

impl DefinitionView {
    /// Open the run (refused by name when the store or the run cannot be
    /// read) and the atlas its panels are drawn with.
    pub fn open(run: &str, store: Option<PathBuf>) -> Result<Self, String> {
        let store = Store::open(match store {
            Some(root) => root,
            None => Store::default_root()?,
        })?;
        let run = store.load(run)?;
        let stamp = store.stamp(&run.id).ok();
        let atlas = Arc::new(Atlas::load(&glyph3d_native::default_emoji_sheet()));
        // Rows wrap down, not back into depth: the passage is read as a page.
        let params = RepoParams { wrap_mode: glyph3d_native::fold::WrapMode::Down, ..Default::default() };
        let metrics = Metrics {
            cell_width: atlas.metrics.advance_fu as f32 * glyph3d_native::text::CELL_HEIGHT_WORLD / atlas.metrics.em_height_fu as f32,
            line_height: params.line_height as f32,
            wrap_columns: params.wrap_cols as usize,
        };
        let view = view::compose(&run, None, metrics);
        log::info!("definition view: run {} at v{} of {} in {}", run.id, run.head, run.tip(), store.root().display());
        Ok(Self { store, run, stamp, last_poll: Instant::now(), atlas, params, metrics, view, status: None })
    }

    /// The run laid out as a scene, with its probe.
    pub fn build(&mut self, load: &SceneLoad, cull_opts: SceneCullOptions) -> (Box<dyn SceneLike>, Option<UiProbe>) {
        let started = Instant::now();
        // Every panel on a face of its own: the stack is cards, a newer one
        // hiding the older ones' bodies behind it.
        let cull_opts = SceneCullOptions { file_backgrounds: true, ..cull_opts };
        self.view = view::compose(&self.run, self.status.as_deref(), self.metrics);
        let texts = self
            .view
            .panels
            .iter()
            .map(|panel| PlacedText { rel_path: panel.name.clone(), bytes: panel.text.clone().into_bytes(), offset: panel.offset })
            .collect();
        let built = build_text_scene(load, self.atlas.clone(), texts, &self.params, CameraMode::Fly, cull_opts, true)
            .expect("the view's panels are UTF-8, small and named apart");
        log::info!("definition view: {} panels built in {:.1} ms", self.view.panels.len(), started.elapsed().as_secs_f64() * 1e3);
        built
    }

    /// Whether the run changed on disk since it was last read or written
    /// here (the agent answered; the CLI took, checked out); it is read again
    /// then. At most one stat per [`POLL`].
    pub fn poll(&mut self) -> bool {
        if self.last_poll.elapsed() < POLL {
            return false;
        }
        self.last_poll = Instant::now();
        let stamp = self.store.stamp(&self.run.id).ok();
        if stamp == self.stamp {
            return false;
        }
        self.stamp = stamp;
        match self.store.load(&self.run.id) {
            Ok(run) => {
                self.run = run;
                self.status = None;
            }
            Err(error) => self.status = Some(error),
        }
        true
    }

    /// A tap on the glyph the scene picked, applied to the run and saved.
    /// True when the view changed (rebuild). A run that changed on disk
    /// since the view was built is read again instead, and the tap dropped:
    /// it was aimed at panels that are no longer there.
    pub fn tap(&mut self, picked: Option<(String, usize)>) -> bool {
        let Some((panel, byte)) = picked else { return false };
        if self.store.stamp(&self.run.id).ok() != self.stamp {
            self.last_poll = Instant::now() - POLL;
            return self.poll();
        }
        let Some(action) = self.view.action(&self.run, &panel, byte) else { return false };
        log::info!("definition view: tap {panel} byte {byte}: {action:?}");
        let mut run = self.run.clone();
        match view::apply(&mut run, action, now_seconds()).and_then(|()| self.store.save(&run)) {
            Ok(()) => {
                self.run = run;
                self.stamp = self.store.stamp(&self.run.id).ok();
                self.status = None;
            }
            Err(error) => self.status = Some(error),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::take_args;

    fn args(line: &[&str]) -> Vec<String> {
        line.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_flags_and_their_values_leave_the_engines_line() {
        let mut line = args(&["app", "--definition-run", "letter", "--field-mode", "derived", "--definition-store", "/s"]);
        let taken = take_args(&mut line).unwrap();
        assert_eq!(taken, Some(("letter".to_string(), Some("/s".into()))));
        assert_eq!(line, ["app", "--field-mode", "derived"]);
        assert_eq!(take_args(&mut args(&["app"])).unwrap(), None);
        assert!(take_args(&mut args(&["app", "--definition-run"])).is_err());
        assert!(take_args(&mut args(&["app", "--definition-store", "/s"])).is_err());
    }
}

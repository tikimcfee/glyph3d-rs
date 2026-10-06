//! Agent Carrel: Unified 3D workstation combining Turn Cards and Workdesk.
//!
//! Orchestrates the side-by-side pair:
//! - Left: [`Deck`] of 2-page [`super::turn_card::AgentTurnCard`]s (Rolodex paging or splay).
//! - Right: [`Workdesk`] of [`FileRevisionStack`]s cascading along -Z.
//! - Bidirectional linkage: selecting a turn focuses its touched file revisions.

use bevy_ecs::prelude::*;
use bevy_transform::prelude::*;
use glam::Vec3;
use crate::agent_transcript::AgentSession;
use crate::revision::RevisionEngine;
use super::{
    deck::{Deck, DeckItem, DeckMode},
    workdesk::{FileRevisionStack, Workdesk},
    ChildOf, SpatialScene, Visible,
};

use serde::{Deserialize, Serialize};

/// Layout configuration and sliding window limits for an Agent Carrel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarrelLayoutOptions {
    /// Maximum number of turn / beat cards visible in the sliding window (default 20).
    pub deck_window_limit: usize,
    /// Number of turns/beats scrolled backward in time from the latest ($K$, default 0).
    pub deck_scroll_offset: usize,
    /// Maximum number of file revisions visible in each file stack sliding window (default 20).
    pub desk_revision_limit: usize,
    /// Number of revisions scrolled backward in time for file stacks (default 0).
    pub desk_scroll_offset: usize,
    /// Maximum number of file stacks placed on the workdesk (default 20).
    pub max_file_stacks: usize,
    /// Optional active beat index to focus immediately on layout (defaults to newest slot 0).
    pub active_beat: Option<usize>,
}

impl Default for CarrelLayoutOptions {
    fn default() -> Self {
        Self {
            deck_window_limit: 20,
            deck_scroll_offset: 0,
            desk_revision_limit: 20,
            desk_scroll_offset: 0,
            max_file_stacks: 20,
            active_beat: None,
        }
    }
}

impl CarrelLayoutOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_deck_limit(mut self, limit: usize) -> Self {
        self.deck_window_limit = limit.max(1);
        self
    }

    pub fn with_deck_scroll(mut self, scroll: usize) -> Self {
        self.deck_scroll_offset = scroll;
        self
    }

    pub fn with_desk_limit(mut self, limit: usize) -> Self {
        self.desk_revision_limit = limit.max(1);
        self
    }

    pub fn with_desk_scroll(mut self, scroll: usize) -> Self {
        self.desk_scroll_offset = scroll;
        self
    }

    pub fn with_max_file_stacks(mut self, max: usize) -> Self {
        self.max_file_stacks = max.max(1);
        self
    }

    pub fn with_active_beat(mut self, beat: Option<usize>) -> Self {
        self.active_beat = beat;
        self
    }
}

/// Component marking an Agent Carrel workstation container.
#[derive(Component, Debug, Clone)]
pub struct AgentCarrel {
    pub session_id: String,
    pub deck_entity: Entity,
    pub workdesk_entity: Entity,
    pub inactive_pool: Entity,
    pub all_card_entities: Vec<Entity>,
    pub active_turn: usize,
    pub turn_count: usize,
    pub active_beat: usize,
    pub beat_count: usize,
    pub layout_options: CarrelLayoutOptions,
    pub slot_to_beat: Vec<usize>,
}

impl SpatialScene {
    /// Spawn an Agent Carrel containing an AgentTurnCard Deck and a Workdesk with custom layout options.
    pub fn spawn_agent_carrel_with_options(
        &mut self,
        parent: Entity,
        session: &AgentSession,
        revision_engine: &RevisionEngine,
        options: CarrelLayoutOptions,
    ) -> Entity {
        let events = session.linearize_events(Some(revision_engine));
        let total_events = events.len();
        let turn_count = session.turn_count();

        // 1. Root Carrel container
        let carrel_entity = self
            .world
            .spawn((
                Transform::IDENTITY,
                ChildOf(parent),
                Name::new(format!("agent_carrel:{}", session.session_id)),
            ))
            .id();

        // Container for cards outside the visible sliding window
        let inactive_pool = self
            .world
            .spawn((
                Transform::from_scale(Vec3::ZERO),
                ChildOf(carrel_entity),
                Name::new("inactive_card_pool"),
                Visible(false),
            ))
            .id();

        // 2. Deck container on the left side
        let deck_parent = self
            .world
            .spawn((
                Transform::from_translation(Vec3::new(-75.0, 0.0, 0.0)),
                ChildOf(carrel_entity),
                Name::new("deck_anchor"),
            ))
            .id();

        let deck = Deck::default()
            .with_mode(DeckMode::Deck)
            .with_window_limit(options.deck_window_limit)
            .with_scroll_offset(options.deck_scroll_offset);
        let deck_entity = self.spawn_deck(deck_parent, "turn_deck", deck);

        // Spawn 2-page Agent Turn Cards using sliding window
        // The newest item is always first in the stack (position 0 / slot 0)
        let total_items = if total_events > 0 { total_events } else { turn_count };
        let mut slot_to_beat = Vec::new();

        let limit = options.deck_window_limit.max(1);
        let max_scroll = total_items.saturating_sub(limit);
        let scroll_k = options.deck_scroll_offset.min(max_scroll);
        let window_size = limit.min(total_items.saturating_sub(scroll_k));

        if total_items > 0 {
            for slot in 0..window_size {
                let chrono_idx = (total_items - 1 - scroll_k) - slot;
                slot_to_beat.push(chrono_idx);
            }
        }

        // Spawn ALL card entities: active window as children of deck_entity, inactive in inactive_pool
        let mut all_card_entities = Vec::with_capacity(total_items);
        for chrono_idx in 0..total_items {
            let in_window_slot = slot_to_beat.iter().position(|&b| b == chrono_idx);
            let parent_e = if in_window_slot.is_some() { deck_entity } else { inactive_pool };
            let slot = in_window_slot.unwrap_or(0);

            let card_e = if !events.is_empty() {
                let event = &events[chrono_idx];
                self.spawn_agent_turn_card(
                    parent_e,
                    slot,
                    event.index,
                    event.turn_index,
                    [55.0, 40.0],
                    4.0,
                    event.summary(),
                    Some(event.banner_colors()),
                )
            } else {
                let turn = &session.turns[chrono_idx];
                self.spawn_agent_turn_card(
                    parent_e,
                    slot,
                    turn.turn_index,
                    turn.turn_index,
                    [55.0, 40.0],
                    4.0,
                    turn.summary(),
                    None,
                )
            };

            if in_window_slot.is_none() {
                self.world
                    .entity_mut(card_e)
                    .remove::<DeckItem>()
                    .insert((
                        Visible(false),
                        Transform::from_scale(Vec3::ZERO),
                    ));
            } else {
                self.world.entity_mut(card_e).insert(Visible(true));
            }

            all_card_entities.push(card_e);
        }

        // 3. Workdesk container on the right side
        let workdesk_parent = self
            .world
            .spawn((
                Transform::from_translation(Vec3::new(75.0, 0.0, 0.0)),
                ChildOf(carrel_entity),
                Name::new("workdesk_anchor"),
            ))
            .id();

        let workdesk_entity = self.spawn_workdesk(
            workdesk_parent,
            format!("desk:{}", session.session_id),
            [60.0, 45.0],
            15.0,
        );

        // Populate Workdesk with FileRevisionStacks from RevisionEngine using sliding window
        let mut all_files = revision_engine.file_paths();
        all_files.sort_by_key(|path| {
            let last_ev = revision_engine
                .history(path)
                .and_then(|h| h.revisions.last())
                .and_then(|r| r.event_index)
                .unwrap_or(0);
            std::cmp::Reverse(last_ev)
        });

        let file_limit = options.max_file_stacks.max(1);
        let desk_rev_limit = options.desk_revision_limit.max(1);

        for path in all_files.into_iter().take(file_limit) {
            if let Some(history) = revision_engine.history(&path) {
                let total_revs = history.revisions.len();
                if total_revs == 0 {
                    continue;
                }
                let max_rev_scroll = total_revs.saturating_sub(desk_rev_limit);
                let rev_scroll = options.desk_scroll_offset.min(max_rev_scroll);
                let rev_window_size = desk_rev_limit.min(total_revs.saturating_sub(rev_scroll));

                // Spawn cards in descending chronological order: latest revision in window pushed FIRST (at z=0)
                for s_rev in 0..rev_window_size {
                    let rev_idx = (total_revs - 1 - rev_scroll) - s_rev;
                    let rev = &history.revisions[rev_idx];
                    let card_size = [55.0, 38.0];
                    self.workdesk_push_revision_card(
                        workdesk_entity,
                        &path,
                        rev.revision_index,
                        rev.action,
                        card_size,
                        &rev.summary,
                    );
                }
            }
        }

        // 4. Attach AgentCarrel component to root
        let initial_beat = options
            .active_beat
            .filter(|&b| slot_to_beat.contains(&b))
            .unwrap_or_else(|| slot_to_beat.first().copied().unwrap_or(0));
        let initial_turn = if !events.is_empty() {
            events.get(initial_beat).map(|e| e.turn_index).unwrap_or(0)
        } else {
            initial_beat
        };

        self.world.entity_mut(carrel_entity).insert(AgentCarrel {
            session_id: session.session_id.clone(),
            deck_entity,
            workdesk_entity,
            inactive_pool,
            all_card_entities,
            active_turn: initial_turn,
            turn_count,
            active_beat: initial_beat,
            beat_count: total_events,
            layout_options: options,
            slot_to_beat: slot_to_beat.clone(),
        });

        // Initialize active beat/turn state
        if total_events > 0 {
            self.carrel_set_beat(carrel_entity, initial_beat, session, revision_engine);
        } else {
            self.carrel_set_turn(carrel_entity, initial_turn, session, revision_engine);
        }

        carrel_entity
    }

    /// Spawn an Agent Carrel containing an AgentTurnCard Deck and a Workdesk with default options.
    pub fn spawn_agent_carrel(
        &mut self,
        parent: Entity,
        session: &AgentSession,
        revision_engine: &RevisionEngine,
    ) -> Entity {
        self.spawn_agent_carrel_with_options(
            parent,
            session,
            revision_engine,
            CarrelLayoutOptions::default(),
        )
    }

    /// Advance the carrel to the next atomic beat, wrapping around.
    pub fn carrel_next_beat(
        &mut self,
        carrel_entity: Entity,
        session: &AgentSession,
        revision_engine: &RevisionEngine,
    ) -> usize {
        self.carrel_step_next(carrel_entity, session, revision_engine)
    }

    /// Go back to the previous atomic beat in the carrel, wrapping around.
    pub fn carrel_prev_beat(
        &mut self,
        carrel_entity: Entity,
        session: &AgentSession,
        revision_engine: &RevisionEngine,
    ) -> usize {
        self.carrel_step_prev(carrel_entity, session, revision_engine)
    }

    /// Step back one beat in history (older beat), sliding the window in O(1) if at the end of the window.
    pub fn carrel_step_prev(
        &mut self,
        carrel_entity: Entity,
        session: &AgentSession,
        revision_engine: &RevisionEngine,
    ) -> usize {
        let (deck_e, inactive_pool, total_items, current_beat, slot_to_beat, all_cards) = {
            let Some(carrel) = self.world.get::<AgentCarrel>(carrel_entity) else {
                return 0;
            };
            let total = carrel.beat_count.max(carrel.turn_count);
            (
                carrel.deck_entity,
                carrel.inactive_pool,
                total,
                carrel.active_beat,
                carrel.slot_to_beat.clone(),
                carrel.all_card_entities.clone(),
            )
        };

        if total_items == 0 || slot_to_beat.is_empty() {
            return 0;
        }

        let current_slot = slot_to_beat.iter().position(|&b| b == current_beat);
        let s = current_slot.unwrap_or(0);

        let target_beat = if s + 1 < slot_to_beat.len() {
            // Already inside the window: advance to next older slot
            let next_slot = s + 1;
            let target = slot_to_beat[next_slot];
            if let Some(mut deck) = self.world.get_mut::<Deck>(deck_e) {
                deck.set_active_page(next_slot, slot_to_beat.len());
            }
            target
        } else {
            // At the oldest edge of the window: slide backward in history in O(1)
            let oldest_in_window = slot_to_beat[s];
            if oldest_in_window > 0 {
                let next_older = oldest_in_window - 1;

                // 1. Pop slot 0 (newest in window) and move to inactive pool
                let popped_beat = slot_to_beat[0];
                if popped_beat < all_cards.len() {
                    let popped_e = all_cards[popped_beat];
                    self.world
                        .entity_mut(popped_e)
                        .remove::<DeckItem>()
                        .insert((
                            ChildOf(inactive_pool),
                            Visible(false),
                            Transform::from_scale(Vec3::ZERO),
                        ));
                }

                // 2. Shift remaining cards in ECS: slot j -> j - 1
                let mut new_slot_to_beat = slot_to_beat[1..].to_vec();
                for (j, &beat) in new_slot_to_beat.iter().enumerate() {
                    if beat < all_cards.len() {
                        let card_e = all_cards[beat];
                        if let Some(mut di) = self.world.get_mut::<DeckItem>(card_e) {
                            di.index = j;
                        }
                    }
                }

                // 3. Append next_older at slot W - 1
                new_slot_to_beat.push(next_older);
                let new_slot = new_slot_to_beat.len() - 1;
                if next_older < all_cards.len() {
                    let new_e = all_cards[next_older];
                    self.world
                        .entity_mut(new_e)
                        .insert((
                            ChildOf(deck_e),
                            DeckItem { index: new_slot },
                            Visible(true),
                            Transform::IDENTITY,
                        ));
                }

                if let Some(mut deck) = self.world.get_mut::<Deck>(deck_e) {
                    deck.set_active_page(new_slot, new_slot_to_beat.len());
                }

                if let Some(mut carrel) = self.world.get_mut::<AgentCarrel>(carrel_entity) {
                    carrel.slot_to_beat = new_slot_to_beat;
                    carrel.layout_options.deck_scroll_offset =
                        (total_items - 1).saturating_sub(carrel.slot_to_beat[0]);
                }

                next_older
            } else {
                // At the beginning of history: wrap to newest turn (total_items - 1)
                self.carrel_set_beat_sliding(
                    carrel_entity,
                    total_items.saturating_sub(1),
                    session,
                    revision_engine,
                );
                return total_items.saturating_sub(1);
            }
        };

        self.carrel_set_beat_internal(carrel_entity, target_beat, session, revision_engine);
        target_beat
    }

    /// Step forward one beat in history (newer beat), sliding the window in O(1) if at the newest slot.
    pub fn carrel_step_next(
        &mut self,
        carrel_entity: Entity,
        session: &AgentSession,
        revision_engine: &RevisionEngine,
    ) -> usize {
        let (deck_e, inactive_pool, total_items, current_beat, slot_to_beat, all_cards) = {
            let Some(carrel) = self.world.get::<AgentCarrel>(carrel_entity) else {
                return 0;
            };
            let total = carrel.beat_count.max(carrel.turn_count);
            (
                carrel.deck_entity,
                carrel.inactive_pool,
                total,
                carrel.active_beat,
                carrel.slot_to_beat.clone(),
                carrel.all_card_entities.clone(),
            )
        };

        if total_items == 0 || slot_to_beat.is_empty() {
            return 0;
        }

        let current_slot = slot_to_beat.iter().position(|&b| b == current_beat);
        let s = current_slot.unwrap_or(0);

        let target_beat = if s > 0 {
            // Already inside the window: retreat to newer slot
            let next_slot = s - 1;
            let target = slot_to_beat[next_slot];
            if let Some(mut deck) = self.world.get_mut::<Deck>(deck_e) {
                deck.set_active_page(next_slot, slot_to_beat.len());
            }
            target
        } else {
            // At the newest edge of the window (slot 0): slide forward in history in O(1)
            let newest_in_window = slot_to_beat[0];
            if newest_in_window + 1 < total_items {
                let next_newer = newest_in_window + 1;

                // 1. Pop oldest slot at end of window and move to inactive pool
                let popped_beat = slot_to_beat[slot_to_beat.len() - 1];
                if popped_beat < all_cards.len() {
                    let popped_e = all_cards[popped_beat];
                    self.world
                        .entity_mut(popped_e)
                        .remove::<DeckItem>()
                        .insert((
                            ChildOf(inactive_pool),
                            Visible(false),
                            Transform::from_scale(Vec3::ZERO),
                        ));
                }

                // 2. Shift remaining cards in ECS: slot j -> j + 1
                let mut new_slot_to_beat = Vec::with_capacity(slot_to_beat.len());
                new_slot_to_beat.push(next_newer);
                for &beat in &slot_to_beat[..slot_to_beat.len() - 1] {
                    new_slot_to_beat.push(beat);
                }

                for (j, &beat) in new_slot_to_beat.iter().enumerate().skip(1) {
                    if beat < all_cards.len() {
                        let card_e = all_cards[beat];
                        if let Some(mut di) = self.world.get_mut::<DeckItem>(card_e) {
                            di.index = j;
                        }
                    }
                }

                // 3. Prepend next_newer at slot 0
                if next_newer < all_cards.len() {
                    let new_e = all_cards[next_newer];
                    self.world
                        .entity_mut(new_e)
                        .insert((
                            ChildOf(deck_e),
                            DeckItem { index: 0 },
                            Visible(true),
                            Transform::IDENTITY,
                        ));
                }

                if let Some(mut deck) = self.world.get_mut::<Deck>(deck_e) {
                    deck.set_active_page(0, new_slot_to_beat.len());
                }

                if let Some(mut carrel) = self.world.get_mut::<AgentCarrel>(carrel_entity) {
                    carrel.slot_to_beat = new_slot_to_beat;
                    carrel.layout_options.deck_scroll_offset =
                        (total_items - 1).saturating_sub(carrel.slot_to_beat[0]);
                }

                next_newer
            } else {
                // At newest turn: wrap to oldest (beat 0)
                self.carrel_set_beat_sliding(carrel_entity, 0, session, revision_engine);
                return 0;
            }
        };

        self.carrel_set_beat_internal(carrel_entity, target_beat, session, revision_engine);
        target_beat
    }

    /// Set the active atomic beat index for the carrel, re-anchoring sliding window if necessary.
    pub fn carrel_set_beat(
        &mut self,
        carrel_entity: Entity,
        beat_index: usize,
        session: &AgentSession,
        revision_engine: &RevisionEngine,
    ) {
        self.carrel_set_beat_sliding(carrel_entity, beat_index, session, revision_engine);
    }

    /// Jump to a specific beat, re-anchoring the sliding window if the target is outside.
    pub fn carrel_set_beat_sliding(
        &mut self,
        carrel_entity: Entity,
        target_beat: usize,
        session: &AgentSession,
        revision_engine: &RevisionEngine,
    ) {
        let (deck_e, inactive_pool, total_items, old_slot_to_beat, all_cards, limit) = {
            let Some(carrel) = self.world.get::<AgentCarrel>(carrel_entity) else {
                return;
            };
            let total = carrel.beat_count.max(carrel.turn_count);
            (
                carrel.deck_entity,
                carrel.inactive_pool,
                total,
                carrel.slot_to_beat.clone(),
                carrel.all_card_entities.clone(),
                carrel.layout_options.deck_window_limit.max(1),
            )
        };

        if total_items == 0 {
            return;
        }
        let target_beat = target_beat.min(total_items - 1);

        if old_slot_to_beat.contains(&target_beat) {
            let slot = old_slot_to_beat.iter().position(|&b| b == target_beat).unwrap();
            if let Some(mut deck) = self.world.get_mut::<Deck>(deck_e) {
                deck.set_active_page(slot, old_slot_to_beat.len());
            }
            self.carrel_set_beat_internal(carrel_entity, target_beat, session, revision_engine);
            return;
        }

        // Re-anchor window to contain target_beat
        let window_size = limit.min(total_items);
        let max_k = total_items.saturating_sub(window_size);
        let target_k = (total_items.saturating_sub(1).saturating_sub(target_beat)).min(max_k);

        let mut new_slot_to_beat = Vec::with_capacity(window_size);
        for slot in 0..window_size {
            let chrono_idx = (total_items - 1 - target_k) - slot;
            new_slot_to_beat.push(chrono_idx);
        }

        // Move cards leaving the window to inactive_pool
        for &beat in &old_slot_to_beat {
            if !new_slot_to_beat.contains(&beat) && beat < all_cards.len() {
                let e = all_cards[beat];
                self.world
                    .entity_mut(e)
                    .remove::<DeckItem>()
                    .insert((
                        ChildOf(inactive_pool),
                        Visible(false),
                        Transform::from_scale(Vec3::ZERO),
                    ));
            }
        }

        // Move cards entering the window to deck_entity
        for (slot, &beat) in new_slot_to_beat.iter().enumerate() {
            if beat < all_cards.len() {
                let e = all_cards[beat];
                self.world
                    .entity_mut(e)
                    .insert((
                        ChildOf(deck_e),
                        DeckItem { index: slot },
                        Visible(true),
                        Transform::IDENTITY,
                    ));
            }
        }

        let slot = new_slot_to_beat.iter().position(|&b| b == target_beat).unwrap_or(0);
        if let Some(mut deck) = self.world.get_mut::<Deck>(deck_e) {
            deck.set_active_page(slot, new_slot_to_beat.len());
        }

        if let Some(mut carrel) = self.world.get_mut::<AgentCarrel>(carrel_entity) {
            carrel.slot_to_beat = new_slot_to_beat;
            carrel.layout_options.deck_scroll_offset = target_k;
        }

        self.carrel_set_beat_internal(carrel_entity, target_beat, session, revision_engine);
    }

    /// Internal synchronization of active beat, turn index, and workdesk file revisions.
    fn carrel_set_beat_internal(
        &mut self,
        carrel_entity: Entity,
        beat_index: usize,
        session: &AgentSession,
        revision_engine: &RevisionEngine,
    ) {
        let workdesk_e = if let Some(mut carrel) = self.world.get_mut::<AgentCarrel>(carrel_entity) {
            carrel.active_beat = beat_index;
            carrel.workdesk_entity
        } else {
            return;
        };

        let events = session.linearize_events(Some(revision_engine));
        if let Some(event) = events.get(beat_index) {
            if let Some(mut carrel) = self.world.get_mut::<AgentCarrel>(carrel_entity) {
                carrel.active_turn = event.turn_index;
            }

            let desk_comp = match self.world.get::<Workdesk>(workdesk_e) {
                Some(d) => d.clone(),
                None => return,
            };

            // If the event targets a file, focus that file's exact revision at this beat
            if let Some(target_file) = event.file_path() {
                if let Some(&stack_e) = desk_comp.file_stacks.get(target_file) {
                    if let Some(history) = revision_engine.history(target_file) {
                        if let Some(rev) = history.revision_for_event(beat_index).or_else(|| history.revision_for_turn(event.turn_index)) {
                            if let Some(mut stack) = self.world.get_mut::<FileRevisionStack>(stack_e) {
                                stack.active_revision = rev.revision_index;
                            }
                        }
                    }
                }
            } else {
                // Otherwise synchronize all files to their state at this beat
                for (file_path, &stack_e) in &desk_comp.file_stacks {
                    if let Some(history) = revision_engine.history(file_path) {
                        if let Some(rev) = history.revision_for_event(beat_index).or_else(|| history.revision_for_turn(event.turn_index)) {
                            if let Some(mut stack) = self.world.get_mut::<FileRevisionStack>(stack_e) {
                                stack.active_revision = rev.revision_index;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Advance the carrel to the next turn, wrapping around.
    pub fn carrel_next_turn(
        &mut self,
        carrel_entity: Entity,
        session: &AgentSession,
        revision_engine: &RevisionEngine,
    ) -> usize {
        let (cur_turn, turn_cnt) = if let Some(carrel) = self.world.get::<AgentCarrel>(carrel_entity) {
            (carrel.active_turn, carrel.turn_count)
        } else {
            return 0;
        };
        let next_turn = if turn_cnt == 0 { 0 } else { (cur_turn + 1) % turn_cnt };
        self.carrel_set_turn(carrel_entity, next_turn, session, revision_engine);
        next_turn
    }

    /// Go back to the previous turn in the carrel, wrapping around.
    pub fn carrel_prev_turn(
        &mut self,
        carrel_entity: Entity,
        session: &AgentSession,
        revision_engine: &RevisionEngine,
    ) -> usize {
        let (cur_turn, turn_cnt) = if let Some(carrel) = self.world.get::<AgentCarrel>(carrel_entity) {
            (carrel.active_turn, carrel.turn_count)
        } else {
            return 0;
        };
        let prev_turn = if turn_cnt == 0 { 0 } else { (cur_turn + turn_cnt - 1) % turn_cnt };
        self.carrel_set_turn(carrel_entity, prev_turn, session, revision_engine);
        prev_turn
    }

    /// Set the active turn index for the carrel and synchronize the workdesk.
    pub fn carrel_set_turn(
        &mut self,
        carrel_entity: Entity,
        turn_index: usize,
        session: &AgentSession,
        revision_engine: &RevisionEngine,
    ) {
        let events = session.linearize_events(Some(revision_engine));
        if !events.is_empty() {
            if let Some(beat_idx) = events
                .iter()
                .rposition(|e| e.turn_index == turn_index && e.file_path().is_some())
                .or_else(|| events.iter().position(|e| e.turn_index == turn_index))
            {
                self.carrel_set_beat_sliding(carrel_entity, beat_idx, session, revision_engine);
                return;
            }
        }

        self.carrel_set_beat_sliding(carrel_entity, turn_index, session, revision_engine);

        // Find file revisions produced or observed in this turn
        let workdesk_e = if let Some(mut carrel) = self.world.get_mut::<AgentCarrel>(carrel_entity) {
            carrel.active_turn = turn_index;
            carrel.workdesk_entity
        } else {
            return;
        };

        if let Some(turn) = session.turns.get(turn_index) {
            let desk_comp = match self.world.get::<Workdesk>(workdesk_e) {
                Some(d) => d.clone(),
                None => return,
            };

            for action in &turn.file_actions {
                if let Some(&stack_e) = desk_comp.file_stacks.get(&action.file_path) {
                    if let Some(history) = revision_engine.history(&action.file_path) {
                        if let Some(rev) = history.revision_for_turn(turn_index) {
                            if let Some(mut stack) = self.world.get_mut::<FileRevisionStack>(stack_e) {
                                stack.active_revision = rev.revision_index;
                            }
                        }
                    }
                }
            }
        }
    }
}

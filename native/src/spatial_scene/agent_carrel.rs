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
    deck::{Deck, DeckMode},
    workdesk::{FileRevisionStack, Workdesk},
    ChildOf, SpatialScene,
};

/// Component marking an Agent Carrel workstation container.
#[derive(Component, Debug, Clone)]
pub struct AgentCarrel {
    pub session_id: String,
    pub deck_entity: Entity,
    pub workdesk_entity: Entity,
    pub active_turn: usize,
    pub turn_count: usize,
    pub active_beat: usize,
    pub beat_count: usize,
}

impl SpatialScene {
    /// Spawn an Agent Carrel containing an AgentTurnCard Deck and a Workdesk.
    pub fn spawn_agent_carrel(
        &mut self,
        parent: Entity,
        session: &AgentSession,
        revision_engine: &RevisionEngine,
    ) -> Entity {
        let events = session.linearize_events(Some(revision_engine));
        let beat_count = events.len();
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

        // 2. Deck container on the left side
        let deck_parent = self
            .world
            .spawn((
                Transform::from_translation(Vec3::new(-75.0, 0.0, 0.0)),
                ChildOf(carrel_entity),
                Name::new("deck_anchor"),
            ))
            .id();

        let deck = Deck::default().with_mode(DeckMode::Deck);
        let deck_entity = self.spawn_deck(deck_parent, "turn_deck", deck);

        // Spawn 2-page Agent Turn Cards for each atomic narrative beat
        if events.is_empty() {
            for turn in &session.turns {
                self.spawn_agent_turn_card(
                    deck_entity,
                    turn.turn_index,
                    turn.turn_index,
                    [55.0, 40.0],
                    4.0,
                    turn.summary(),
                    None,
                );
            }
        } else {
            for event in &events {
                self.spawn_agent_turn_card(
                    deck_entity,
                    event.index,
                    event.turn_index,
                    [55.0, 40.0],
                    4.0,
                    event.summary(),
                    Some(event.banner_colors()),
                );
            }
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

        // Populate Workdesk with FileRevisionStacks from RevisionEngine
        for path in revision_engine.file_paths() {
            if let Some(history) = revision_engine.history(&path) {
                for rev in &history.revisions {
                    let card_size = [55.0, 38.0];
                    self.workdesk_push_revision(
                        workdesk_entity,
                        &path,
                        rev.action,
                        card_size,
                        &rev.summary,
                    );
                }
            }
        }

        // 4. Attach AgentCarrel component to root
        self.world.entity_mut(carrel_entity).insert(AgentCarrel {
            session_id: session.session_id.clone(),
            deck_entity,
            workdesk_entity,
            active_turn: 0,
            turn_count,
            active_beat: 0,
            beat_count,
        });

        // Initialize active beat/turn state
        if beat_count > 0 {
            self.carrel_set_beat(carrel_entity, 0, session, revision_engine);
        } else {
            self.carrel_set_turn(carrel_entity, 0, session, revision_engine);
        }

        carrel_entity
    }

    /// Advance the carrel to the next atomic beat, wrapping around.
    pub fn carrel_next_beat(
        &mut self,
        carrel_entity: Entity,
        session: &AgentSession,
        revision_engine: &RevisionEngine,
    ) -> usize {
        let (next_idx, deck_e) = if let Some(carrel) = self.world.get::<AgentCarrel>(carrel_entity) {
            let next = if carrel.beat_count == 0 {
                0
            } else {
                (carrel.active_beat + 1) % carrel.beat_count
            };
            (next, carrel.deck_entity)
        } else {
            return 0;
        };

        self.deck_set_active(deck_e, next_idx);
        self.carrel_set_beat(carrel_entity, next_idx, session, revision_engine);
        next_idx
    }

    /// Go back to the previous atomic beat in the carrel, wrapping around.
    pub fn carrel_prev_beat(
        &mut self,
        carrel_entity: Entity,
        session: &AgentSession,
        revision_engine: &RevisionEngine,
    ) -> usize {
        let (prev_idx, deck_e) = if let Some(carrel) = self.world.get::<AgentCarrel>(carrel_entity) {
            let prev = if carrel.beat_count == 0 {
                0
            } else {
                (carrel.active_beat + carrel.beat_count - 1) % carrel.beat_count
            };
            (prev, carrel.deck_entity)
        } else {
            return 0;
        };

        self.deck_set_active(deck_e, prev_idx);
        self.carrel_set_beat(carrel_entity, prev_idx, session, revision_engine);
        prev_idx
    }

    /// Set the active atomic beat index for the carrel and synchronize the workdesk.
    pub fn carrel_set_beat(
        &mut self,
        carrel_entity: Entity,
        beat_index: usize,
        session: &AgentSession,
        revision_engine: &RevisionEngine,
    ) {
        let (deck_e, workdesk_e) = if let Some(mut carrel) = self.world.get_mut::<AgentCarrel>(carrel_entity) {
            carrel.active_beat = beat_index;
            (carrel.deck_entity, carrel.workdesk_entity)
        } else {
            return;
        };

        self.deck_set_active(deck_e, beat_index);

        let events = session.linearize_events(Some(revision_engine));
        if let Some(event) = events.get(beat_index) {
            if let Some(mut carrel) = self.world.get_mut::<AgentCarrel>(carrel_entity) {
                carrel.active_turn = event.turn_index;
            }

            let desk_comp = match self.world.get::<Workdesk>(workdesk_e) {
                Some(d) => d.clone(),
                None => return,
            };

            // If the event targets a file, focus that file's revision
            if let Some(target_file) = event.file_path() {
                if let Some(&stack_e) = desk_comp.file_stacks.get(target_file) {
                    if let Some(history) = revision_engine.history(target_file) {
                        if let Some(rev) = history.revision_for_turn(event.turn_index) {
                            if let Some(mut stack) = self.world.get_mut::<FileRevisionStack>(stack_e) {
                                stack.active_revision = rev.revision_index;
                            }
                        }
                    }
                }
            } else if let Some(turn) = session.turns.get(event.turn_index) {
                // Otherwise synchronize all files in this turn
                for action in &turn.file_actions {
                    if let Some(&stack_e) = desk_comp.file_stacks.get(&action.file_path) {
                        if let Some(history) = revision_engine.history(&action.file_path) {
                            if let Some(rev) = history.revision_for_turn(event.turn_index) {
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
        if let Some(beat_idx) = events.iter().position(|e| e.turn_index == turn_index) {
            self.carrel_set_beat(carrel_entity, beat_idx, session, revision_engine);
            return;
        }

        let (deck_e, workdesk_e) = if let Some(mut carrel) = self.world.get_mut::<AgentCarrel>(carrel_entity) {
            carrel.active_turn = turn_index;
            (carrel.deck_entity, carrel.workdesk_entity)
        } else {
            return;
        };

        self.deck_set_active(deck_e, turn_index);

        // Find file revisions produced or observed in this turn
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

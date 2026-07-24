//! Durable interactive input delivery for a non-terminal turn.

use super::support::*;
use super::*;

/// One accepted user input whose content is owned by the referenced `Item`.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingTurnInput {
    pub input_id: String,
    pub item_id: String,
    pub delivery: TurnInputDelivery,
    pub input: Value,
}

/// Controls and ordered inputs visible at one AgentLoop safe boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnBoundaryState {
    pub pause_requested: bool,
    pub inputs: Vec<PendingTurnInput>,
}

impl SessionStore {
    /// Persist a real user message without acquiring workspace execution ownership.
    ///
    /// `input_id` is the idempotency key. Repeating the same request returns the
    /// existing turn; reusing the key for different content fails closed.
    pub fn append_turn_input(
        &self,
        turn_id: &str,
        input_id: &str,
        delivery: TurnInputDelivery,
        input: &Value,
    ) -> StoreResult<Turn> {
        if turn_id.trim().is_empty()
            || input_id.trim().is_empty()
            || !input.is_array()
            || input.as_array().is_none_or(Vec::is_empty)
        {
            return Err(StoreError::InvalidState(
                "turn input identity or payload is invalid".to_string(),
            ));
        }
        let (input, redacted) = sanitize_item_payload(&ItemKind::UserMessage, input.clone())?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let turn = self.turn_in_transaction(&transaction, turn_id)?;
        let existing = transaction
            .query_row(
                "select ti.turn_id, ti.delivery, i.payload
                 from turn_inputs ti join items i on i.item_id = ti.item_id
                 where ti.input_id = ?1",
                params![input_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((existing_turn, existing_delivery, existing_payload)) = existing {
            if existing_turn != turn_id
                || existing_delivery != delivery.as_storage_text()
                || existing_payload != serde_json::to_string(&input)?
            {
                return Err(StoreError::InvalidState(
                    "turn input idempotency key was reused with different content".to_string(),
                ));
            }
            transaction.commit()?;
            return Ok(turn);
        }
        if is_terminal_turn_status(&turn.status) {
            return Err(StoreError::InvalidState(
                "terminal turn cannot accept interactive input".to_string(),
            ));
        }

        let item = Self::new_item(turn_id, ItemKind::UserMessage, input);
        let item_sequence = Self::next_item_sequence(&transaction, turn_id)?;
        Self::insert_item(&transaction, &item, item_sequence, redacted)?;
        transaction.execute(
            "insert into turn_inputs(input_id, turn_id, item_id, delivery, delivery_state)
             values(?1, ?2, ?3, ?4, 'pending')",
            params![input_id, turn_id, item.item_id, delivery.as_storage_text()],
        )?;
        transaction.commit()?;
        Ok(turn)
    }

    /// Read controls and eligible inputs in stable item order without consuming them.
    pub fn turn_boundary_state(
        &self,
        turn_id: &str,
        include_follow_up: bool,
    ) -> StoreResult<TurnBoundaryState> {
        let pause_requested: bool = self
            .connection
            .query_row(
                "select pause_requested from turns where turn_id = ?1",
                params![turn_id],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("turn {turn_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        let mut statement = self.connection.prepare(
            "select ti.input_id, ti.item_id, ti.delivery, i.payload
             from turn_inputs ti join items i on i.item_id = ti.item_id
             where ti.turn_id = ?1 and ti.delivery_state = 'pending'
               and (ti.delivery = 'steer' or ?2)
             order by i.item_sequence, ti.input_id",
        )?;
        let rows = statement
            .query_map(params![turn_id, include_follow_up], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut inputs = Vec::with_capacity(rows.len());
        for (input_id, item_id, delivery, payload) in rows {
            let delivery = TurnInputDelivery::from_storage_text(&delivery).ok_or_else(|| {
                StoreError::InvalidState(format!(
                    "turn input {input_id} has unknown delivery state"
                ))
            })?;
            inputs.push(PendingTurnInput {
                input_id,
                item_id,
                delivery,
                input: serde_json::from_str(&payload)?,
            });
        }
        Ok(TurnBoundaryState {
            pause_requested,
            inputs,
        })
    }

    /// Atomically consume inputs and publish the checkpoint that already contains them.
    pub fn consume_turn_inputs_with_checkpoint(
        &self,
        turn_id: &str,
        thread_id: &str,
        input_ids: &[String],
        checkpoint: &Value,
        checkpoint_version: u32,
        pause: bool,
    ) -> StoreResult<()> {
        if input_ids.is_empty() || !checkpoint.is_object() {
            return Err(StoreError::InvalidState(
                "turn input checkpoint commit is invalid".to_string(),
            ));
        }
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let turn = self.turn_in_transaction(&transaction, turn_id)?;
        if turn.thread_id != thread_id || is_terminal_turn_status(&turn.status) {
            return Err(StoreError::InvalidState(
                "turn input checkpoint binding is invalid".to_string(),
            ));
        }
        for input_id in input_ids {
            let changed = transaction.execute(
                "update turn_inputs set delivery_state = 'consumed', consumed_at = current_timestamp
                 where input_id = ?1 and turn_id = ?2 and delivery_state = 'pending'",
                params![input_id, turn_id],
            )?;
            if changed != 1 {
                return Err(StoreError::InvalidState(format!(
                    "turn input {input_id} is not pending"
                )));
            }
        }
        Self::upsert_turn_checkpoint(
            &transaction,
            turn_id,
            thread_id,
            checkpoint,
            checkpoint_version,
        )?;
        if pause {
            transaction.execute(
                "update turns set status = 'paused', agent_loop_status = 'paused',
                                  pause_requested = 0 where turn_id = ?1",
                params![turn_id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Request a pause at the next safe boundary. Suspended turns can pause immediately.
    pub fn request_turn_pause(&self, turn_id: &str) -> StoreResult<Turn> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let mut turn = self.turn_in_transaction(&transaction, turn_id)?;
        if is_terminal_turn_status(&turn.status) {
            return Err(StoreError::InvalidState(
                "terminal turn cannot be paused".to_string(),
            ));
        }
        match turn.status {
            TurnStatus::Paused => {}
            TurnStatus::Suspended => {
                transaction.execute(
                    "update turns set status = 'paused', agent_loop_status = 'paused',
                                      pause_requested = 0 where turn_id = ?1",
                    params![turn_id],
                )?;
                turn.status = TurnStatus::Paused;
                turn.agent_loop_status = "paused".to_string();
            }
            _ => {
                transaction.execute(
                    "update turns set pause_requested = 1 where turn_id = ?1",
                    params![turn_id],
                )?;
            }
        }
        transaction.commit()?;
        Ok(turn)
    }

    /// Publish a pause checkpoint and release the turn from running state in one transaction.
    pub fn pause_turn_with_checkpoint(
        &self,
        turn_id: &str,
        thread_id: &str,
        checkpoint: &Value,
        checkpoint_version: u32,
    ) -> StoreResult<Turn> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let mut turn = self.turn_in_transaction(&transaction, turn_id)?;
        if turn.thread_id != thread_id || is_terminal_turn_status(&turn.status) {
            return Err(StoreError::InvalidState(
                "pause checkpoint binding is invalid".to_string(),
            ));
        }
        Self::upsert_turn_checkpoint(
            &transaction,
            turn_id,
            thread_id,
            checkpoint,
            checkpoint_version,
        )?;
        transaction.execute(
            "update turns set status = 'paused', agent_loop_status = 'paused',
                              pause_requested = 0 where turn_id = ?1",
            params![turn_id],
        )?;
        transaction.commit()?;
        turn.status = TurnStatus::Paused;
        turn.agent_loop_status = "paused".to_string();
        Ok(turn)
    }
}

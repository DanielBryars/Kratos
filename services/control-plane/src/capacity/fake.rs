//! A provider that makes nothing.
//!
//! It exists so the boundary can be settled before anything costs money, and so the properties
//! that matter — one machine per attempt however many times it is asked for, a release that
//! converges — can be proved without a cloud account.
//!
//! It is deliberately strict where a real provider would be forgiving. Asking for a second
//! machine under one idempotency key is recorded rather than quietly satisfied, so a test can
//! assert it never happened; and releasing something already gone is success, because a provider
//! that errored there would leave a request that could never converge.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use uuid::Uuid;

use super::{CapacityError, CapacityProvider, CapacitySpec, ProvisionedCapacity};

/// What the fake was asked to do, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    Provision(String),
    Release(String),
}

#[derive(Default)]
struct State {
    /// Idempotency key to the machine it was answered with.
    machines: HashMap<String, String>,
    released: Vec<String>,
    calls: Vec<Call>,
    fail_next_provision: Option<String>,
    fail_next_release: Option<String>,
    /// When set, the next provision waits here after announcing it has started. That is what lets
    /// a test cancel a request while the provider is mid-answer, which is the race the
    /// reconciliation exists for and cannot otherwise be reached.
    hold_provision: Option<Arc<tokio::sync::Notify>>,
}

/// A provider that records what it was asked and hands back stable identifiers.
#[derive(Default)]
pub struct FakeProvider {
    state: Mutex<State>,
    started: Arc<tokio::sync::Notify>,
}

impl FakeProvider {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every call the provider received, in order.
    ///
    /// # Panics
    /// Panics if the lock is poisoned, which can only happen if a test has already failed.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }

    /// How many distinct machines exist. The number that must not grow on a replay.
    ///
    /// # Panics
    /// Panics if the lock is poisoned, which can only happen if a test has already failed.
    #[must_use]
    pub fn machine_count(&self) -> usize {
        self.state.lock().unwrap().machines.len()
    }

    /// Whether this machine has been released.
    ///
    /// # Panics
    /// Panics if the lock is poisoned, which can only happen if a test has already failed.
    #[must_use]
    pub fn is_released(&self, external_id: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .released
            .iter()
            .any(|id| id == external_id)
    }

    /// Make the next provision fail, as an unreachable provider would.
    ///
    /// # Panics
    /// Panics if the lock is poisoned, which can only happen if a test has already failed.
    pub fn fail_next_provision(&self, reason: &str) {
        self.state.lock().unwrap().fail_next_provision = Some(reason.to_owned());
    }

    /// Make the next release fail.
    ///
    /// # Panics
    /// Panics if the lock is poisoned, which can only happen if a test has already failed.
    pub fn fail_next_release(&self, reason: &str) {
        self.state.lock().unwrap().fail_next_release = Some(reason.to_owned());
    }

    /// Hold the next provision open until the returned gate is notified, announcing on `started`
    /// when the provider has been entered.
    ///
    /// # Panics
    /// Panics if the lock is poisoned, which can only happen if a test has already failed.
    pub fn hold_next_provision(&self, gate: Arc<tokio::sync::Notify>) {
        self.state.lock().unwrap().hold_provision = Some(gate);
    }

    /// Notified when a held provision has begun.
    #[must_use]
    pub fn provision_started(&self) -> Arc<tokio::sync::Notify> {
        self.started.clone()
    }
}

#[async_trait]
impl CapacityProvider for FakeProvider {
    fn name(&self) -> &'static str {
        "fake"
    }

    async fn provision(&self, spec: &CapacitySpec) -> Result<ProvisionedCapacity, CapacityError> {
        let gate = {
            let mut state = self.state.lock().unwrap();
            state
                .calls
                .push(Call::Provision(spec.idempotency_key.clone()));
            if let Some(reason) = state.fail_next_provision.take() {
                return Err(CapacityError::Refused(reason));
            }
            state.hold_provision.take()
        };
        if let Some(gate) = gate {
            self.started.notify_waiters();
            gate.notified().await;
        }
        let mut state = self.state.lock().unwrap();
        // The contract a real provider has to honour: one key is one machine, forever. Returning
        // the existing id rather than making another is what makes a replay safe.
        let external_id = state
            .machines
            .entry(spec.idempotency_key.clone())
            .or_insert_with(|| format!("fake-{}", Uuid::new_v4()))
            .clone();
        Ok(ProvisionedCapacity { external_id })
    }

    async fn release(&self, external_id: &str) -> Result<(), CapacityError> {
        let mut state = self.state.lock().unwrap();
        state.calls.push(Call::Release(external_id.to_owned()));
        if let Some(reason) = state.fail_next_release.take() {
            return Err(CapacityError::Refused(reason));
        }
        // Releasing something already gone is success. A provider that errored here would leave a
        // request that could never converge to released.
        if !state.released.iter().any(|id| id == external_id) {
            state.released.push(external_id.to_owned());
        }
        Ok(())
    }
}

//! What the lease keeps loaded for the sessions: every model of the run,
//! or, when the run swaps them (ADR 0015), one at a time, the resident
//! one. Freeing and restoring the engine depend on it; with one resident
//! model they go through `serve_alone`, so an engine error or a job never
//! loads the others back next to it.

use std::iter;

use super::{Holder, LeaseError, LeaseManager, LeaseState, Step};
use crate::ports::{EngineError, ModelState, serves};

/// Every model of the run loaded, or one at a time.
#[derive(Debug)]
pub(super) enum Residency {
    All,
    One(Resident),
}

/// Which model should be loaded, and which one the lease last left
/// serving: one the engine no longer lists was evicted by the engine.
#[derive(Debug)]
pub(super) struct Resident {
    target: String,
    left: Option<String>,
}

/// Whether `listed` has `id` loaded, ready or not.
fn loaded(listed: &[ModelState], id: &str) -> bool {
    listed.iter().any(|m| m.id == id && m.loaded)
}

impl LeaseManager {
    /// Keeps one of the run's models loaded at a time, the run's own first
    /// (ADR 0015). Errs with why it cannot: the engine cannot unload one
    /// model and keep answering, or the run has one model.
    pub fn with_swap(mut self) -> Result<Self, &'static str> {
        if !self.llm.unloads_each_model() {
            return Err("the engine cannot unload one model and keep answering");
        }
        let [run_model, _, ..] = self.llm_models.as_slice() else {
            return Err("a run with one model has nothing to swap");
        };
        let target = run_model.clone();
        self.state.get_mut().residency = Residency::One(Resident { target, left: None });
        self.swap = true;
        Ok(self)
    }

    /// Whether the lease keeps one model loaded at a time.
    pub fn swaps(&self) -> bool {
        self.swap
    }

    /// The models the engine should have loaded: every model of the run,
    /// or the resident one when swapping.
    pub async fn resident_models(&self) -> Vec<String> {
        match &self.state.lock().await.residency {
            Residency::All => self.llm_models.clone(),
            Residency::One(resident) => vec![resident.target.clone()],
        }
    }

    /// Before a session of `model`: when swapping, makes it the resident
    /// model and the only one of the run's models the engine has loaded,
    /// and gives the lease to the LLMs. Nothing happens when it is already
    /// so, or when every model stays loaded: then `ensure_llm` is what
    /// brings the LLMs back. A failure leaves the gate closed, the holder
    /// as it was, and `model` resident, so `ensure_llm` tries the whole
    /// swap again.
    pub async fn serve_only(&self, model: &str) -> Result<(), LeaseError> {
        if !self.llm_models.iter().any(|id| id == model) {
            return Err(LeaseError::UnknownLlm(model.to_owned()));
        }
        let mut state = self.state.lock().await;
        let LeaseState { holder, residency } = &mut *state;
        let Residency::One(resident) = residency else {
            return Ok(());
        };
        model.clone_into(&mut resident.target);
        let served = self.serve_alone(resident, Some(model)).await;
        self.settle(holder, served)
    }

    /// Before the first session: loads the LLMs the engine does not list
    /// as ready, or, when swapping, frees the run's models and loads each
    /// alone, in turn, ending with the run's own, which stays resident. A
    /// model that cannot load then stops the run before any session,
    /// rather than at the first session that needs it. A failure closes
    /// the gate.
    pub async fn prepare_llms(&self) -> Result<(), LeaseError> {
        let mut state = self.state.lock().await;
        let LeaseState { holder, residency } = &mut *state;
        let prepared = match residency {
            Residency::One(resident) => self.load_each_in_turn(resident).await,
            Residency::All => self.recover(residency).await,
        };
        self.settle(holder, prepared)
    }

    /// The LLMs hold the lease with the gate open after `outcome` worked;
    /// otherwise the gate stays closed.
    fn settle(
        &self,
        holder: &mut Holder,
        outcome: Result<(), EngineError>,
    ) -> Result<(), LeaseError> {
        if let Err(error) = outcome {
            self.gate.close();
            return Err(error.into());
        }
        *holder = Holder::Llm;
        self.gate.open();
        Ok(())
    }

    /// Frees the memory the LLMs hold: every model of the run, or, when
    /// swapping, those of them the engine lists as loaded.
    pub(super) async fn free(&self, residency: &mut Residency) -> Result<(), EngineError> {
        match residency {
            Residency::All => {
                let unload = self.llm.unload_all(&self.llm_models);
                self.timed(Step::llm("unload", None), unload).await
            }
            Residency::One(resident) => self.serve_alone(resident, None).await,
        }
    }

    /// Loads back what `free` freed, and journals `ready`. Every model of
    /// the run goes through the engine's `load_all`, which may start an
    /// engine that `free` stopped.
    pub(super) async fn restore(&self, residency: &mut Residency) -> Result<(), EngineError> {
        match residency {
            Residency::All => {
                let load = self.llm.load_all(&self.llm_models);
                self.timed(Step::llm("load", None), load).await?;
                self.observe(Holder::Llm, "ready", 0, None);
                Ok(())
            }
            Residency::One(resident) => {
                let target = resident.target.clone();
                self.serve_alone(resident, Some(&target)).await
            }
        }
    }

    /// As `restore`, but only for an engine that answers: one that does not
    /// is not started here, where it may only be slow.
    pub(super) async fn recover(&self, residency: &mut Residency) -> Result<(), EngineError> {
        match residency {
            Residency::All => {
                let listed = self.llm.models().await?;
                if self.llm_models.iter().all(|id| serves(&listed, id)) {
                    return Ok(());
                }
            }
            // `serve_alone` asks the engine first and starts nothing.
            Residency::One(_) => {}
        }
        self.restore(residency).await
    }

    /// After a `free` that failed: some of every model may be freed
    /// already, so they are loaded back while the engine answers; requests
    /// meet the engine retry like any engine failure instead of waiting
    /// here. One resident model is left as it is: the session's swap or
    /// `ensure_llm` loads it, and loading next to a model that would not
    /// unload is what swapping avoids.
    pub(super) async fn undo_free(&self, residency: &mut Residency) {
        match residency {
            Residency::All => {
                let _ = self.recover(residency).await;
            }
            Residency::One(_) => {}
        }
    }

    async fn load_each_in_turn(&self, resident: &mut Resident) -> Result<(), EngineError> {
        let Some((run_model, others)) = self.llm_models.split_first() else {
            return Ok(());
        };
        run_model.clone_into(&mut resident.target);
        self.serve_alone(resident, None).await?;
        for id in others.iter().chain(iter::once(run_model)) {
            self.serve_alone(resident, Some(id)).await?;
        }
        Ok(())
    }

    /// Unloads the run's models the engine lists as loaded, but `model`,
    /// then loads `model` unless it serves; `None` frees them all. First
    /// journals the model left serving before when the engine no longer
    /// lists it. Closes the gate while it changes models and leaves it
    /// to the caller to open.
    pub(super) async fn serve_alone(
        &self,
        resident: &mut Resident,
        model: Option<&str>,
    ) -> Result<(), EngineError> {
        let listed = self.llm.models().await?;
        let plan = self.plan(&listed, resident, model);
        if plan.evicted.is_some() {
            resident.left = None;
        }
        let changes = plan.changes_models();
        if changes {
            self.gate.close();
        }
        self.carry_out(plan, &mut resident.left).await?;
        resident.left = model.map(str::to_owned);
        if let Some(id) = model.filter(|_| changes) {
            self.observe(Holder::Llm, "ready", 0, Some(id.to_owned()));
        }
        Ok(())
    }

    /// What serving `model` alone takes, from the engine's list of models
    /// and the `resident` one. The resident model is unloaded last, so an
    /// unload that fails on another model leaves it serving.
    fn plan<'a>(
        &'a self,
        listed: &[ModelState],
        resident: &Resident,
        model: Option<&'a str>,
    ) -> Plan<'a> {
        let mut unload: Vec<&String> = self
            .llm_models
            .iter()
            .filter(|id| model != Some(id.as_str()) && loaded(listed, id))
            .collect();
        unload.sort_by_key(|id| **id == resident.target);
        Plan {
            evicted: resident.left.clone().filter(|left| !loaded(listed, left)),
            unload,
            load: model.filter(|id| !serves(listed, id)),
        }
    }

    /// Does `plan`. A model the lease unloads is no longer `left` serving.
    async fn carry_out(
        &self,
        plan: Plan<'_>,
        left: &mut Option<String>,
    ) -> Result<(), EngineError> {
        if let Some(id) = plan.evicted {
            self.observe(Holder::Llm, "evicted", 0, Some(id));
        }
        for id in plan.unload {
            let unload = async { self.llm.unload(id).await.map_err(|e| e.about(id)) };
            self.timed(Step::llm("unload", Some(id)), unload).await?;
            if left.as_ref() == Some(id) {
                *left = None;
            }
        }
        if let Some(id) = plan.load {
            let load = async { self.llm.load(id).await.map_err(|e| e.about(id)) };
            self.timed(Step::llm("load", Some(id)), load).await?;
        }
        Ok(())
    }
}

/// What `serve_alone` does: journal an eviction, unload the run's other
/// models, load the one to serve.
struct Plan<'a> {
    evicted: Option<String>,
    unload: Vec<&'a String>,
    load: Option<&'a str>,
}

impl Plan<'_> {
    /// Whether the engine's models change: an eviction alone does not.
    fn changes_models(&self) -> bool {
        !self.unload.is_empty() || self.load.is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::fakes::{
        FakeLlm, Setup, Unload, job, noting_job, records, setup_engine, setup_run, two,
    };
    use super::*;
    use crate::lease::Holder;

    /// A swapping lease over `llm`, whose run's models are all it lists.
    fn swapping(llm: FakeLlm) -> (Setup, Arc<FakeLlm>) {
        let llm = Arc::new(llm);
        let (manager, log, transitions) = setup_engine(llm.clone(), false, false);
        ((manager.with_swap().unwrap(), log, transitions), llm)
    }

    /// A swapping lease over `m` (the run's) and `r`, of which `loaded`
    /// are loaded, with `r` resident.
    async fn on_r(loaded: &[&str]) -> (Setup, Arc<FakeLlm>) {
        let ((manager, log, transitions), llm) = swapping(two(loaded, Unload::Works, None));
        manager.serve_only("r").await.unwrap();
        log.clear();
        transitions.lock().unwrap().clear();
        ((manager, log, transitions), llm)
    }

    /// Requests wait at the gate while the engine changes models.
    #[tokio::test]
    async fn serving_a_model_alone_unloads_the_others_first() {
        let ((manager, log, transitions), llm) = swapping(two(&["m"], Unload::Works, None));
        llm.watch(manager.gate());
        manager.serve_only("r").await.unwrap();
        assert_eq!(log.lines(), ["llm unload m", "llm load r (gate closed)"]);
        assert_eq!(
            records(&transitions),
            ["llm unload m", "llm load r", "llm ready r"]
        );
        let after = (manager.resident_models().await, manager.gate().is_open());
        assert_eq!(after, (vec!["r".to_owned()], true));
        assert!(manager.swaps());
    }

    /// As `on_r`, after a job whose reload of `r` failed: the job holds
    /// the lease, and `r` does not load until `refuse_load(None)`.
    async fn after_a_failed_reload() -> (Setup, Arc<FakeLlm>) {
        let ((manager, log, transitions), llm) = on_r(&["m"]).await;
        llm.refuse_load(Some("r"));
        let ran = manager.run_job(noting_job(&manager, &log)).await;
        assert!(ran.lease.is_err(), "{:?}", ran.lease);
        ((manager, log, transitions), llm)
    }

    /// A swap after a job whose reload failed gives the lease back to the LLMs.
    #[tokio::test]
    async fn a_swap_gives_the_lease_back_to_the_llms() {
        let ((manager, _, _), llm) = after_a_failed_reload().await;
        assert_eq!(manager.holder().await, Holder::Job);
        llm.refuse_load(None);
        manager.serve_only("m").await.unwrap();
        assert_eq!(manager.holder().await, Holder::Llm);
        assert!(manager.gate().is_open());
    }

    /// A swap that fails leaves the lease with whoever held it.
    #[tokio::test]
    async fn a_failed_swap_leaves_the_holder_as_it_was() {
        let ((manager, _, _), llm) = after_a_failed_reload().await;
        llm.refuse_load(Some("m"));
        assert!(manager.serve_only("m").await.is_err());
        assert_eq!(manager.holder().await, Holder::Job);
    }

    /// A model the lease left serving stays known through a swap that
    /// failed on another model: its later eviction is still journaled.
    #[tokio::test]
    async fn the_model_left_serving_survives_a_failed_swap() {
        let ((manager, _, transitions), llm) = swapping(two(&[], Unload::FailsFor("m"), None));
        manager.serve_only("r").await.unwrap();
        llm.take_up("m");
        manager.serve_only("r").await.unwrap_err();
        llm.drop_all();
        manager.serve_only("r").await.unwrap();
        assert!(records(&transitions).contains(&"llm evicted r".to_owned()));
    }

    /// The resident model, unloaded last, stays left serving when a free
    /// fails on another model first: its later eviction is journaled.
    #[tokio::test]
    async fn a_free_that_fails_first_keeps_the_model_left_serving() {
        let ((manager, log, transitions), llm) = swapping(two(&[], Unload::FailsFor("m"), None));
        manager.serve_only("r").await.unwrap();
        llm.take_up("m");
        let ran = manager.run_job(noting_job(&manager, &log)).await;
        assert!(ran.lease.is_err(), "{:?}", ran.lease);
        llm.drop_all();
        manager.ensure_llm().await.unwrap();
        assert!(records(&transitions).contains(&"llm evicted r".to_owned()));
    }

    /// The check before the first session that fails leaves the lease with
    /// whoever held it, and the gate closed.
    #[tokio::test]
    async fn a_failed_check_leaves_the_holder_as_it_was() {
        let ((manager, _, _), _) = after_a_failed_reload().await;
        assert!(manager.prepare_llms().await.is_err());
        let after = (manager.holder().await, manager.gate().is_open());
        assert_eq!(after, (Holder::Job, false));
    }

    /// A model the lease unloaded itself, in a swap that then failed, is
    /// not journaled as evicted.
    #[tokio::test]
    async fn a_model_the_lease_unloaded_is_not_evicted() {
        let ((manager, _, transitions), llm) = on_r(&["m"]).await;
        llm.refuse_load(Some("m"));
        manager.serve_only("m").await.unwrap_err();
        llm.refuse_load(None);
        manager.serve_only("m").await.unwrap();
        assert!(!records(&transitions).iter().any(|r| r.contains("evicted")));
    }

    /// A builder after a builder: nothing to unload or load, nothing journaled.
    #[tokio::test]
    async fn serving_the_model_already_alone_does_nothing() {
        let ((manager, log, transitions), _) = swapping(two(&["m"], Unload::Works, None));
        manager.serve_only("m").await.unwrap();
        manager.serve_only("m").await.unwrap();
        assert!(log.lines().is_empty(), "{:?}", log.lines());
        assert!(transitions.lock().unwrap().is_empty());
    }

    /// The models the engine holds for something else stay loaded.
    #[tokio::test]
    async fn a_model_that_is_not_the_run_s_is_left_alone() {
        let llm = Arc::new(FakeLlm::new(
            &["m", "r", "other"],
            &["m", "other"],
            Unload::Works,
        ));
        let models = vec!["m".to_owned(), "r".to_owned()];
        let (manager, log, _) = setup_run(llm, models, false, false);
        manager.with_swap().unwrap().serve_only("r").await.unwrap();
        assert_eq!(log.lines(), ["llm unload m", "llm load r"]);
    }

    /// A model still loading takes memory: it is unloaded like a ready one.
    #[tokio::test]
    async fn a_model_still_loading_is_unloaded_before_the_next_loads() {
        let ((manager, log, _), llm) = swapping(two(&[], Unload::Works, None));
        llm.warm("m");
        manager.serve_only("r").await.unwrap();
        assert_eq!(log.lines(), ["llm unload m", "llm load r"]);
    }

    /// Without swapping every model stays loaded, so a session has nothing
    /// to swap, and the check before the first session loads what is not.
    #[tokio::test]
    async fn without_swapping_every_model_stays_loaded() {
        let llm = Arc::new(two(&["m"], Unload::Works, None));
        let (manager, log, _) = setup_engine(llm, false, false);
        manager.serve_only("r").await.unwrap();
        manager.prepare_llms().await.unwrap();
        assert_eq!(log.lines(), ["llm load r"]);
        assert_eq!(manager.resident_models().await, ["m", "r"]);
        assert!(!manager.swaps());
    }

    /// A run with one model has nothing to swap: swapping is refused.
    #[tokio::test]
    async fn one_model_is_not_swapped() {
        let llm = Arc::new(FakeLlm::new(&["m"], &["m"], Unload::Works));
        let (manager, _, _) = setup_engine(llm, false, false);
        let refused = manager.with_swap().err();
        assert_eq!(refused, Some("a run with one model has nothing to swap"));
    }

    /// An engine that cannot unload, or whose unload stops it as a whole,
    /// cannot swap: swapping is refused.
    #[tokio::test]
    async fn an_engine_that_cannot_unload_one_model_is_not_swapped() {
        let unsupported = two(&["m", "r"], Unload::Unsupported, None);
        let stops = FakeLlm {
            stops: true,
            ..two(&["m", "r"], Unload::Works, None)
        };
        for llm in [unsupported, stops] {
            let (manager, _, _) = setup_engine(Arc::new(llm), false, false);
            let refused = manager.with_swap().err();
            assert_eq!(
                refused,
                Some("the engine cannot unload one model and keep answering")
            );
        }
    }

    /// A model outside the run's is refused, not served.
    #[tokio::test]
    async fn a_model_that_is_not_the_run_s_cannot_be_served() {
        let ((manager, log, _), _) = swapping(two(&["m"], Unload::Works, None));
        let error = manager.serve_only("x").await.unwrap_err();
        assert!(
            matches!(&error, LeaseError::UnknownLlm(id) if id == "x"),
            "{error}"
        );
        assert!(log.lines().is_empty(), "{:?}", log.lines());
    }

    /// A model the lease left serving and the engine dropped by itself is
    /// journaled before the swap goes on.
    #[tokio::test]
    async fn a_model_the_engine_dropped_is_journaled_as_evicted() {
        let ((manager, _, transitions), llm) = on_r(&["m"]).await;
        llm.drop_all();
        manager.serve_only("m").await.unwrap();
        assert_eq!(
            records(&transitions),
            ["llm evicted r", "llm load m", "llm ready m"]
        );
    }

    /// An eviction alone changes no model: it is journaled, with no `ready`.
    #[tokio::test]
    async fn an_eviction_alone_is_journaled_without_ready() {
        let ((manager, _, transitions), llm) = swapping(two(&["m"], Unload::Works, None));
        manager.serve_only("m").await.unwrap();
        llm.drop_all();
        llm.take_up("r");
        manager.serve_only("r").await.unwrap();
        assert_eq!(records(&transitions), ["llm evicted m"]);
    }

    /// A model still loading is not evicted.
    #[tokio::test]
    async fn a_model_still_loading_is_not_journaled_as_evicted() {
        let ((manager, _, transitions), llm) = on_r(&["m"]).await;
        llm.drop_all();
        llm.warm("r");
        manager.serve_only("m").await.unwrap();
        assert_eq!(
            records(&transitions),
            ["llm unload r", "llm load m", "llm ready m"]
        );
    }

    /// The load that fails leaves the gate closed with the model still the
    /// resident one: `ensure_llm` tries the whole swap again, unloading
    /// what the engine took up meanwhile.
    #[tokio::test]
    async fn a_failed_swap_is_tried_again_by_ensure_llm() {
        let ((manager, log, transitions), llm) = swapping(two(&["m"], Unload::Works, Some("r")));
        let error = manager.serve_only("r").await.unwrap_err();
        assert!(
            matches!(&error, LeaseError::Engine(EngineError::Refused(m)) if m == "r: no memory"),
            "{error}"
        );
        assert!(!manager.gate().is_open());
        llm.refuse_load(None);
        llm.take_up("m");
        manager.ensure_llm().await.unwrap();
        assert_eq!(log.lines(), ["llm unload m", "llm unload m", "llm load r"]);
        assert_eq!(
            records(&transitions),
            [
                "llm unload m",
                "llm failed load: engine refused: r: no memory",
                "llm unload m",
                "llm load r",
                "llm ready r"
            ]
        );
        assert!(manager.gate().is_open());
    }

    /// An unload that fails stops the swap: loading next to the model that
    /// stayed is the out-of-memory a swap avoids.
    #[tokio::test]
    async fn a_failed_unload_does_not_load_the_next_model() {
        let ((manager, log, transitions), _) = swapping(two(&["m"], Unload::FailsFor("m"), None));
        let error = manager.serve_only("r").await.unwrap_err();
        assert!(
            matches!(&error, LeaseError::Engine(EngineError::Refused(m)) if m == "m: busy"),
            "{error}"
        );
        assert_eq!(
            (log.lines(), records(&transitions), manager.gate().is_open()),
            (
                vec![],
                vec!["llm failed unload: engine refused: m: busy".to_owned()],
                false
            )
        );
    }

    #[tokio::test]
    async fn an_engine_that_does_not_answer_fails_the_swap_with_the_gate_closed() {
        let llm = FakeLlm {
            down: true,
            ..two(&["m"], Unload::Works, None)
        };
        let ((manager, log, _), _) = swapping(llm);
        let error = manager.serve_only("r").await.unwrap_err();
        assert!(
            matches!(error, LeaseError::Engine(EngineError::Unreachable(_))),
            "{error}"
        );
        assert_eq!((log.lines(), manager.gate().is_open()), (vec![], false));
    }

    /// The check before the first session frees the run's models, the
    /// resident one last, then loads each alone, with requests held at
    /// the gate, and ends with the run's own.
    #[tokio::test]
    async fn each_model_is_loaded_in_turn_ending_with_the_run_s() {
        let ((manager, log, transitions), llm) = swapping(two(&["m", "r"], Unload::Works, None));
        llm.watch(manager.gate());
        manager.prepare_llms().await.unwrap();
        assert_eq!(
            log.lines(),
            [
                "llm unload r",
                "llm unload m",
                "llm load r (gate closed)",
                "llm unload r",
                "llm load m (gate closed)"
            ]
        );
        assert_eq!(
            records(&transitions),
            [
                "llm unload r",
                "llm unload m",
                "llm load r",
                "llm ready r",
                "llm unload r",
                "llm load m",
                "llm ready m"
            ]
        );
        let after = (manager.resident_models().await, manager.gate().is_open());
        assert_eq!(after, (vec!["m".to_owned()], true));
    }

    /// The run's own model is checked too, last.
    #[tokio::test]
    async fn the_run_s_own_model_that_does_not_load_stops_the_check() {
        let ((manager, log, _), _) = swapping(two(&[], Unload::Works, Some("m")));
        let error = manager.prepare_llms().await.unwrap_err();
        assert!(
            matches!(&error, LeaseError::Engine(EngineError::Refused(m)) if m == "m: no memory"),
            "{error}"
        );
        assert_eq!(log.lines(), ["llm load r", "llm unload r"]);
    }

    /// The run's model stays the resident one, so a later `ensure_llm`
    /// does not serve the model that failed.
    #[tokio::test]
    async fn a_model_that_does_not_load_stops_the_check() {
        let ((manager, log, _), _) = swapping(two(&[], Unload::Works, Some("r")));
        let error = manager.prepare_llms().await.unwrap_err();
        assert!(
            matches!(&error, LeaseError::Engine(EngineError::Refused(m)) if m == "r: no memory"),
            "{error}"
        );
        let after = (manager.resident_models().await, manager.gate().is_open());
        assert_eq!(
            (log.lines(), after),
            (vec![], (vec!["m".to_owned()], false))
        );
    }

    /// Around a heavy job a swapping lease frees and reloads the resident
    /// model only, and names it.
    #[tokio::test]
    async fn a_heavy_job_frees_and_reloads_only_the_resident_model() {
        let ((manager, log, transitions), _) = on_r(&["m"]).await;
        let ran = manager.run_job(noting_job(&manager, &log)).await;
        assert_eq!((ran.output, ran.lease.is_ok()), (Some(7), true));
        assert_eq!(
            log.lines(),
            ["llm unload r", "job (gate open: false)", "llm load r"]
        );
        assert_eq!(
            records(&transitions),
            ["llm unload r", "job job -", "llm load r", "llm ready r"]
        );
    }

    #[tokio::test]
    async fn a_media_job_frees_and_reloads_only_the_resident_model() {
        let ((manager, _, transitions), _) = on_r(&["m"]).await;
        manager.run_media("comfyui", job()).await.unwrap();
        assert_eq!(
            records(&transitions),
            [
                "llm unload r",
                "media:comfyui load -",
                "media:comfyui job -",
                "media:comfyui unload -",
                "llm load r",
                "llm ready r"
            ]
        );
    }

    /// A model the engine took up in place of the resident one is freed
    /// before a job, and the eviction is journaled.
    #[tokio::test]
    async fn a_job_frees_every_run_model_the_engine_holds() {
        let ((manager, log, transitions), llm) = on_r(&["m"]).await;
        llm.drop_all();
        llm.take_up("m");
        let ran = manager.run_job(noting_job(&manager, &log)).await;
        assert_eq!((ran.output, ran.lease.is_ok()), (Some(7), true));
        assert_eq!(
            log.lines(),
            ["llm unload m", "job (gate open: false)", "llm load r"]
        );
        assert_eq!(records(&transitions)[0], "llm evicted r");
    }

    /// After a job whose reload failed, `ensure_llm` serves the resident
    /// model alone.
    #[tokio::test]
    async fn a_failed_reload_after_a_job_is_finished_by_ensure_llm() {
        let ((manager, log, _), llm) = on_r(&["m"]).await;
        llm.refuse_load(Some("r"));
        let ran = manager.run_job(noting_job(&manager, &log)).await;
        assert_eq!(ran.output, Some(7));
        assert!(
            matches!(ran.lease, Err(LeaseError::ReloadFailed(_))),
            "{:?}",
            ran.lease
        );
        assert!(!manager.gate().is_open());
        llm.refuse_load(None);
        llm.take_up("m");
        manager.ensure_llm().await.unwrap();
        assert_eq!(
            log.lines(),
            [
                "llm unload r",
                "job (gate open: false)",
                "llm unload m",
                "llm load r"
            ]
        );
        assert!(manager.gate().is_open());
    }

    /// A free that fails on a model the engine took up leaves the
    /// resident model serving, since it is unloaded last, and loads
    /// nothing: the job does not run.
    #[tokio::test]
    async fn a_free_that_fails_on_another_model_leaves_the_resident_one() {
        let ((manager, log, transitions), llm) = swapping(two(&["m"], Unload::FailsFor("r"), None));
        llm.take_up("r");
        let ran = manager.run_job(noting_job(&manager, &log)).await;
        assert!(
            matches!(ran.lease, Err(LeaseError::Engine(_))),
            "{:?}",
            ran.lease
        );
        assert_eq!((log.lines(), manager.gate().is_open()), (vec![], true));
        assert_eq!(
            records(&transitions),
            ["llm failed unload: engine refused: r: busy"]
        );
    }

    /// A job whose unload fails does not run, and the resident model
    /// stays: nothing is loaded next to the model that would not unload.
    #[tokio::test]
    async fn a_job_whose_unload_fails_does_not_run() {
        let ((manager, log, _), _) = swapping(two(&["m"], Unload::FailsFor("r"), None));
        manager.serve_only("r").await.unwrap();
        log.clear();
        let ran = manager.run_job(noting_job(&manager, &log)).await;
        assert_eq!(ran.output, None);
        assert!(
            matches!(ran.lease, Err(LeaseError::Engine(_))),
            "{:?}",
            ran.lease
        );
        assert_eq!((log.lines(), manager.gate().is_open()), (vec![], true));
    }
}

//! `droidtop_agent.pair`: pairing a handheld from this computer
//! (droidtop-agent-pair), the way `droidtop-agent`'s window does.

use droidtop_agent_core::sync::LockExt;
use std::sync::{Arc, Mutex};
use std::thread;

use pyo3::prelude::*;
use serde_json::json;

use crate::{agent, run};

pub fn fill(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(pair, m)?)?;
    m.add_function(wrap_pyfunction!(show, m)?)?;
    m.add_function(wrap_pyfunction!(status, m)?)?;
    m.add_function(wrap_pyfunction!(cancel, m)?)?;
    m.add_function(wrap_pyfunction!(places, m)?)?;
    m.add_function(wrap_pyfunction!(offer, m)?)?;
    m.add_function(wrap_pyfunction!(find, m)?)?;
    m.add_function(wrap_pyfunction!(pick, m)?)?;
    m.add_function(wrap_pyfunction!(compare_status, m)?)?;
    m.add_function(wrap_pyfunction!(confirm, m)?)?;
    m.add_function(wrap_pyfunction!(prepare_confirm, m)?)?;
    m.add_function(wrap_pyfunction!(prepare_pick, m)?)?;
    m.add_function(wrap_pyfunction!(stop, m)?)?;
    m.add_class::<Confirmation>()?;
    m.add_class::<Pick>()?;
    Ok(())
}

/// This computer showing a pairing code: the outcome once there is one.
struct Showing {
    cancel: Arc<droidtop_agent_core::accept::Cancel>,
    outcome: Arc<Mutex<Option<Result<String, String>>>>,
}

static SHOWING: Mutex<Option<Showing>> = Mutex::new(None);

/// Pairs with a handheld that shows `code` (droidtop: Settings > Computers >
/// Pair a computer): the 6 digits, or the text of its QR code. Waits for the
/// exchange (seconds); returns the handheld's name.
#[pyfunction]
fn pair(py: Python<'_>, code: String) -> PyResult<PyObject> {
    run(py, move || {
        let a = agent()?;
        Ok(json!(droidtop_agent_pair::pair(&a.store, &code)?))
    })
}

/// This computer shows a code for a handheld to type (droidtop: Pair a
/// computer > Use a code from the computer), for 10 minutes: `code`, `port`,
/// `places` (every address a handheld can use, each with its `label`),
/// `invite` (the text a QR code of the invitation carries) and `seconds`
/// left. Ask `status()` for the outcome; a code shown before is stopped.
#[pyfunction]
fn show(py: Python<'_>) -> PyResult<PyObject> {
    run(py, || {
        let a = agent()?;
        if let Some(s) = &*SHOWING.locked() {
            s.cancel.cancel();
        }
        let showing = droidtop_agent_pair::listen()?;
        let reply = json!({
            "code": showing.code,
            "port": showing.port,
            "places": showing.places.iter().map(|p| json!({ "label": p.label, "address": p.address })).collect::<Vec<_>>(),
            "invite": showing.invite(&a.store),
            "seconds": showing.until.saturating_duration_since(std::time::Instant::now()).as_secs(),
        });
        let cancel = Arc::new(droidtop_agent_core::accept::Cancel::new());
        let outcome = Arc::new(Mutex::new(None));
        {
            let (cancel, outcome) = (cancel.clone(), outcome.clone());
            thread::spawn(move || {
                let result = droidtop_agent_pair::wait(&a.store, showing, &cancel);
                *outcome.locked() = Some(result);
            });
        }
        *SHOWING.locked() = Some(Showing { cancel, outcome });
        Ok(reply)
    })
}

/// How the code `show()` shows is doing: `state` is `idle` (none shown),
/// `waiting`, `paired` (with the handheld's name as `paired`) or `failed`
/// (with `error`).
#[pyfunction]
fn status(py: Python<'_>) -> PyResult<PyObject> {
    run(py, || {
        Ok(match &*SHOWING.locked() {
            None => json!({ "state": "idle" }),
            Some(s) => match &*s.outcome.locked() {
                None => json!({ "state": "waiting" }),
                Some(Ok(name)) => json!({ "state": "paired", "paired": name }),
                Some(Err(e)) => json!({ "state": "failed", "error": e }),
            },
        })
    })
}

/// Stops showing a code.
#[pyfunction]
fn cancel(py: Python<'_>) -> PyResult<PyObject> {
    run(py, || {
        if let Some(s) = &*SHOWING.locked() {
            s.cancel.cancel();
        }
        Ok(json!(null))
    })
}

/// Every address of this computer a handheld can use with the agent's port,
/// each with its `label` ("Local network", "Tailscale", "IPv6"...).
#[pyfunction]
fn places(py: Python<'_>) -> PyResult<PyObject> {
    run(py, || {
        let list: Vec<_> = droidtop_agent_pair::places(droidtop_agent_core::PORT)
            .into_iter()
            .map(|p| json!({ "label": p.label, "address": p.address }))
            .collect();
        Ok(json!(list))
    })
}

// ---- Pairing by comparing one code on both screens (docs/SPEC.md 7.1, 7.2) ----

/// Where a comparison is: what [`compare_status`] reports.
#[derive(Clone)]
enum Phase {
    Idle,
    /// Offering to be picked, since `since`.
    Offering(std::time::Instant),
    /// Both screens show `code`; waiting for the person's answer.
    Comparing {
        code: String,
        name: String,
    },
    /// The person answered; waiting for the other device's.
    Finishing,
    Paired(String),
    Failed(String),
}

struct Compare {
    phase: Phase,
    comparison: Option<droidtop_agent_pair::Comparison>,
    generation: Option<Generation>,
}

#[derive(Clone)]
struct Generation {
    offering: Arc<droidtop_agent_core::accept::Cancel>,
    comparison: Arc<droidtop_agent_pair::ComparisonCancel>,
}

impl Generation {
    fn new() -> Self {
        Self {
            offering: Arc::new(droidtop_agent_core::accept::Cancel::new()),
            comparison: Arc::new(droidtop_agent_pair::ComparisonCancel::new()),
        }
    }

    fn cancel(&self) {
        self.offering.cancel();
        self.comparison.cancel();
    }

    fn is_cancelled(&self) -> bool {
        self.offering.is_cancelled() || self.comparison.is_cancelled()
    }

    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.offering, &other.offering)
    }
}

static COMPARE: Mutex<Compare> = Mutex::new(Compare { phase: Phase::Idle, comparison: None, generation: None });

fn is_current(c: &Compare, generation: &Generation) -> bool {
    c.generation.as_ref().is_some_and(|current| current.same(generation)) && !generation.is_cancelled()
}

fn set_phase(generation: &Generation, phase: Phase) -> bool {
    let mut c = COMPARE.locked();
    if !is_current(&c, generation) {
        return false;
    }
    c.phase = phase;
    true
}

/// Stops the current generation, including a comparison already moved into a waiting confirmation.
fn reset() -> Generation {
    let mut c = COMPARE.locked();
    if let Some(generation) = &c.generation {
        generation.cancel();
    }
    let generation = Generation::new();
    c.comparison = None;
    c.phase = Phase::Idle;
    c.generation = Some(generation.clone());
    generation
}

fn cancel_generation(generation: &Generation) {
    let mut c = COMPARE.locked();
    generation.cancel();
    if c.generation.as_ref().is_some_and(|current| current.same(generation)) {
        c.comparison = None;
        c.phase = Phase::Idle;
    }
}

fn check_expected_code(expected: Option<&str>, actual: &str) -> Result<(), String> {
    if expected.is_some_and(|expected| expected != actual) {
        return Err("The comparison code changed. Check both screens before answering again.".into());
    }
    Ok(())
}

/// This computer offers to be picked by a handheld or another computer
/// (droidtop's Pair a computer; a desktop manager's Pair with another
/// computer): it answers `DTCQ1` as the embedding program ([`crate::configure`])
/// and waits for the device that picks it, until
/// [`stop`]. The page calls it while it shows pairing (SPEC 7.1: at no other
/// time does it answer). `compare_status` then says `offering`, and
/// `comparing` with the `code` once a device picked it.
#[pyfunction]
fn offer(py: Python<'_>) -> PyResult<PyObject> {
    run(py, || {
        let a = agent()?;
        let generation = reset();
        let offering = droidtop_agent_pair::offer(&a.store, crate::program())?;
        let reply = json!({ "name": offering.offer.name, "port": offering.offer.port });
        if !set_phase(&generation, Phase::Offering(std::time::Instant::now())) {
            return Err("Pairing stopped.".into());
        }
        thread::spawn(move || match offering.accept_with_cancel(&a.store, &generation.offering, generation.comparison.clone()) {
            Ok(comparison) => {
                let mut c = COMPARE.locked();
                if is_current(&c, &generation) {
                    c.phase = Phase::Comparing { code: comparison.code.clone(), name: comparison.name.clone() };
                    c.comparison = Some(comparison);
                }
            }
            Err(e) => {
                set_phase(&generation, Phase::Failed(e));
            }
        });
        Ok(reply)
    })
}

/// Other computers on this network offering to pair (a desktop manager's
/// Devices page open, or `droidtop-agent pair compare`), looking for `seconds`: each with
/// `id`, `name`, `program` and the `address` to pick it at.
#[pyfunction]
#[pyo3(signature = (seconds = 3.0))]
fn find(py: Python<'_>, seconds: f64) -> PyResult<PyObject> {
    run(py, move || {
        let a = agent()?;
        let found = droidtop_agent_pair::computers(&a.store, std::time::Duration::from_secs_f64(seconds.clamp(0.5, 15.0)))?;
        Ok(json!(found
            .into_iter()
            .map(|(o, at)| json!({ "id": o.id, "name": o.name, "program": o.program, "address": at.to_string(), "port": o.port }))
            .collect::<Vec<_>>()))
    })
}

/// Picks a computer [`find`] listed and starts comparing: returns the
/// `code` this screen shows (the other shows the same one).
#[pyfunction]
fn pick(py: Python<'_>, id: String, name: String, program: String, address: String) -> PyResult<PyObject> {
    prepare_pick(py, id, name, program, address)?.finish(py)
}

struct PickWork {
    offer: droidtop_agent_core::discovery::Offer,
    at: std::net::SocketAddr,
}

/// One reserved outgoing comparison. Its network work can run after the caller releases its ownership lock.
#[pyclass]
struct Pick {
    work: Mutex<Option<PickWork>>,
    generation: Generation,
}

#[pymethods]
impl Pick {
    fn finish(&self, py: Python<'_>) -> PyResult<PyObject> {
        let work = self.work.locked().take().ok_or_else(|| crate::AgentError::new_err("This computer was already picked."))?;
        let generation = self.generation.clone();
        run(py, move || {
            if generation.is_cancelled() {
                return Err("Pairing stopped.".into());
            }
            let a = agent()?;
            let comparison = match droidtop_agent_pair::pick_with_cancel(&a.store, &work.offer, work.at, generation.comparison.clone()) {
                Ok(comparison) => comparison,
                Err(error) => {
                    set_phase(&generation, Phase::Failed(error.clone()));
                    return Err(error);
                }
            };
            let reply = json!({ "code": comparison.code, "name": comparison.name });
            let mut c = COMPARE.locked();
            if !is_current(&c, &generation) {
                return Err("Pairing stopped.".into());
            }
            c.phase = Phase::Comparing { code: comparison.code.clone(), name: comparison.name.clone() };
            c.comparison = Some(comparison);
            Ok(reply)
        })
    }

    fn cancel(&self, py: Python<'_>) {
        py.allow_threads(|| cancel_generation(&self.generation));
    }
}

/// Reserves the generation synchronously, without starting network work. Call `finish` outside the ownership lock.
#[pyfunction]
fn prepare_pick(py: Python<'_>, id: String, name: String, program: String, address: String) -> PyResult<Pick> {
    let at: std::net::SocketAddr =
        address.parse().map_err(|_| crate::AgentError::new_err(format!("{address} is not an address and port")))?;
    let offer = droidtop_agent_core::discovery::Offer { id, name, port: at.port(), program };
    let generation = py.allow_threads(reset);
    Ok(Pick { work: Mutex::new(Some(PickWork { offer, at })), generation })
}

/// Where pairing by comparison is: `state` is `idle`, `offering` (with
/// `seconds` since it started), `comparing` (with the `code` and the other
/// device's `name` when known), `finishing`, `paired` (with `paired`, its
/// name) or `failed` (with `error`).
#[pyfunction]
fn compare_status(py: Python<'_>) -> PyResult<PyObject> {
    run(py, || {
        Ok(match COMPARE.lock().unwrap().phase.clone() {
            Phase::Idle => json!({ "state": "idle" }),
            Phase::Offering(since) => json!({ "state": "offering", "seconds": since.elapsed().as_secs() }),
            Phase::Comparing { code, name } => json!({ "state": "comparing", "code": code, "name": name }),
            Phase::Finishing => json!({ "state": "finishing" }),
            Phase::Paired(name) => json!({ "state": "paired", "paired": name }),
            Phase::Failed(error) => json!({ "state": "failed", "error": error }),
        })
    })
}

/// The person's answer: `same` when both screens show the same code. Waits
/// for the other device's answer; pins it when both said yes, and returns
/// its name. "They don't match" on either side pairs nothing.
#[pyfunction]
fn confirm(py: Python<'_>, same: bool) -> PyResult<PyObject> {
    prepare_confirm(py, None)?.finish(py, same)
}

/// An exact comparison captured before an asynchronous worker can be delayed behind a newer offer.
#[pyclass]
struct Confirmation {
    comparison: Mutex<Option<droidtop_agent_pair::Comparison>>,
    generation: Generation,
}

#[pymethods]
impl Confirmation {
    fn finish(&self, py: Python<'_>, same: bool) -> PyResult<PyObject> {
        let comparison =
            self.comparison.locked().take().ok_or_else(|| crate::AgentError::new_err("This comparison was already answered."))?;
        let generation = self.generation.clone();
        run(py, move || {
            let a = agent()?;
            match comparison.finish(&a.store, same) {
                Ok(name) => {
                    set_phase(&generation, Phase::Paired(name.clone()));
                    Ok(json!(name))
                }
                Err(e) => {
                    set_phase(&generation, Phase::Failed(e.clone()));
                    Err(e)
                }
            }
        })
    }

    fn cancel(&self, py: Python<'_>) {
        py.allow_threads(|| cancel_generation(&self.generation));
    }
}

/// Takes the currently displayed comparison synchronously; `finish(same)` does the waiting outside caller locks.
#[pyfunction]
#[pyo3(signature = (expected_code = None))]
fn prepare_confirm(py: Python<'_>, expected_code: Option<String>) -> PyResult<Confirmation> {
    py.allow_threads(|| {
        let mut c = COMPARE.locked();
        let generation = c.generation.clone().ok_or_else(|| crate::AgentError::new_err("No comparison is under way."))?;
        if generation.is_cancelled() {
            return Err(crate::AgentError::new_err("Pairing stopped."));
        }
        let comparison = c.comparison.as_ref().ok_or_else(|| crate::AgentError::new_err("No comparison is under way."))?;
        check_expected_code(expected_code.as_deref(), &comparison.code).map_err(crate::AgentError::new_err)?;
        let comparison = c.comparison.take().ok_or_else(|| crate::AgentError::new_err("No comparison is under way."))?;
        c.phase = Phase::Finishing;
        Ok(Confirmation { comparison: Mutex::new(Some(comparison)), generation })
    })
}

/// Stops offering and any comparison under way (the page closed).
#[pyfunction]
fn stop(py: Python<'_>) -> PyResult<PyObject> {
    run(py, || {
        reset();
        Ok(json!(null))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn an_old_worker_cannot_publish_into_or_cancel_a_newer_comparison() {
        let _serial = SERIAL.locked();
        let old = reset();
        let current = reset();
        assert!(set_phase(&current, Phase::Comparing { code: "654321".into(), name: "new".into() }));
        assert!(!set_phase(&old, Phase::Paired("old".into())));
        cancel_generation(&old);
        assert!(!current.is_cancelled());
        assert!(matches!(&COMPARE.locked().phase, Phase::Comparing { code, .. } if code == "654321"));
        reset();
    }

    #[test]
    fn stopping_finishing_keeps_and_cancels_its_ownership_handle() {
        let _serial = SERIAL.locked();
        let current = reset();
        assert!(set_phase(&current, Phase::Finishing));
        cancel_generation(&current);
        assert!(current.comparison.is_cancelled());
        assert!(matches!(COMPARE.locked().phase, Phase::Idle));
        assert!(!set_phase(&current, Phase::Paired("late result".into())));
    }

    #[test]
    fn an_answer_to_an_older_displayed_code_is_rejected() {
        assert!(check_expected_code(Some("123456"), "654321").is_err());
        assert!(check_expected_code(Some("654321"), "654321").is_ok());
        // Existing callers keep their original signature, while owned UI operations supply the observed code.
        assert!(check_expected_code(None, "654321").is_ok());
    }
}

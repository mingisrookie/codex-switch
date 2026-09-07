use std::{cell::Cell, path::Path};

use crate::{
    operation_log::timestamp_millis,
    process_control::{
        cache_chatgpt_launch_target, close_codex_processes, list_codex_process_inventory,
        CodexProcess,
    },
    request_route_switcher::{
        preflight_request_route_switch, switch_request_route_preflighted_with_progress,
    },
    runtime_store::{RuntimeStore, PLUS_RUNTIME_ID},
    runtime_switcher::{
        RuntimeSwitchFailureReason, RuntimeSwitchOutcome, RuntimeSwitchPhase, RuntimeSwitchResult,
    },
    session_storage::provenance::{
        record_or_verify_route_epoch, RouteEpochInput, RouteProvenanceReceipt,
    },
};

#[derive(Debug)]
pub(crate) struct RuntimeSwitchExecution {
    pub result: Result<RuntimeSwitchResult, String>,
    pub failure_outcome: RuntimeSwitchOutcome,
    pub failure_operation_id: Option<String>,
    pub failure_reason: RuntimeSwitchFailureReason,
    pub launch_target_captured: bool,
    pub provenance_error: Option<String>,
}

pub(crate) fn execute_runtime_switch_route<Progress>(
    store: &RuntimeStore,
    current_home: &Path,
    runtime_id: &str,
    mut progress: Progress,
) -> RuntimeSwitchExecution
where
    Progress: FnMut(RuntimeSwitchPhase, Option<String>),
{
    let mut failure_outcome = RuntimeSwitchOutcome::FailedBeforeWrite;
    let mut failure_operation_id = None;
    let failure_reason = Cell::new(RuntimeSwitchFailureReason::Unknown);
    let target_is_account = runtime_id == PLUS_RUNTIME_ID;
    let mut launch_target_captured = false;
    let mut provenance_error = None;

    let result = (|| {
        let plan =
            preflight_request_route_switch(store, runtime_id, current_home).map_err(|failure| {
                failure_reason.set(failure.reason);
                failure_operation_id = failure.operation_id;
                failure_outcome = failure.outcome;
                failure.message
            })?;
        if plan.requires_change() || target_is_account {
            let close_result = close_runtime_processes_with_progress(
                || {
                    let (processes, standalone) = list_codex_process_inventory()?;
                    if !standalone.is_empty() {
                        failure_reason.set(RuntimeSwitchFailureReason::StandaloneWriterActive);
                        return Err(
                            "a standalone Codex CLI is still running; close it before switching request routes"
                                .to_string(),
                        );
                    }
                    capture_launch_target_once(&mut launch_target_captured, || {
                        cache_chatgpt_launch_target().is_ok()
                    });
                    Ok(processes)
                },
                || {
                    close_codex_processes().map(|_| ()).inspect_err(|_| {
                        failure_reason.set(RuntimeSwitchFailureReason::ProcessCloseFailed);
                    })
                },
                &mut progress,
            );
            if close_result.is_err() && failure_reason.get() == RuntimeSwitchFailureReason::Unknown
            {
                failure_reason.set(RuntimeSwitchFailureReason::ProcessCloseFailed);
            }
            close_result?;
        } else {
            capture_launch_target_once(&mut launch_target_captured, || {
                cache_chatgpt_launch_target().is_ok()
            });
        }

        match switch_request_route_preflighted_with_progress(
            store,
            current_home,
            plan,
            &mut || {
                let (managed, standalone) = list_codex_process_inventory()?;
                if !standalone.is_empty() {
                    failure_reason.set(RuntimeSwitchFailureReason::StandaloneWriterActive);
                } else if !managed.is_empty() {
                    failure_reason.set(RuntimeSwitchFailureReason::ProcessCloseFailed);
                }
                ensure_codex_closed_from_processes("switching request routes", managed, standalone)
            },
            &mut |phase| progress(phase, None),
        ) {
            Ok(mut receipt) => {
                let provider = if runtime_id == PLUS_RUNTIME_ID {
                    "openai"
                } else {
                    "openai_custom"
                };
                let account_slot =
                    format!("{}:{}", receipt.runtime.id, receipt.runtime.created_at_ms);
                let provenance = timestamp_millis()
                    .map_err(|_| "route epoch cutover timestamp is unavailable".to_string())
                    .and_then(|effective_at_ms| {
                        record_or_verify_route_epoch(
                            &store.data_root()?,
                            RouteEpochInput::new(
                                &receipt.operation_id,
                                effective_at_ms,
                                runtime_id,
                                provider,
                                &account_slot,
                                receipt.runtime.model.as_deref(),
                            ),
                            true,
                        )
                    });
                receipt.route_provenance = match provenance {
                    Ok(provenance) => provenance,
                    Err(error) => {
                        provenance_error = Some(error);
                        let message =
                            "会话来源账本未写入；为避免产生无来源回合，ChatGPT 已保持关闭"
                                .to_string();
                        receipt.warnings.push(message.clone());
                        RouteProvenanceReceipt::failed(message)
                    }
                };
                Ok(receipt)
            }
            Err(failure) => {
                failure_operation_id = failure.operation_id;
                failure_outcome = failure.outcome;
                if failure_reason.get() != RuntimeSwitchFailureReason::StandaloneWriterActive {
                    failure_reason.set(failure.reason);
                }
                Err(failure.message)
            }
        }
    })();

    RuntimeSwitchExecution {
        result,
        failure_outcome,
        failure_operation_id,
        failure_reason: failure_reason.get(),
        launch_target_captured,
        provenance_error,
    }
}

pub(crate) fn capture_launch_target_once<Capture>(captured: &mut bool, capture: Capture)
where
    Capture: FnOnce() -> bool,
{
    if !*captured {
        *captured = capture();
    }
}

pub(crate) fn close_runtime_processes_with_progress<List, Close, Progress>(
    mut list_managed_processes: List,
    mut close_managed_processes: Close,
    mut progress: Progress,
) -> Result<bool, String>
where
    List: FnMut() -> Result<Vec<CodexProcess>, String>,
    Close: FnMut() -> Result<(), String>,
    Progress: FnMut(RuntimeSwitchPhase, Option<String>),
{
    progress(RuntimeSwitchPhase::DetectingApp, None);
    let processes = list_managed_processes()?;
    let closed_running_processes = !processes.is_empty();
    if !processes.is_empty() {
        progress(
            RuntimeSwitchPhase::ClosingApp,
            Some(format!("Closing {} ChatGPT process(es)", processes.len())),
        );
        close_managed_processes()?;
    }
    if list_managed_processes()?.is_empty() {
        Ok(closed_running_processes)
    } else {
        Err("ChatGPT is still running; close it before switching request routes".to_string())
    }
}

fn ensure_codex_closed_from_processes(
    action: &str,
    managed: Vec<CodexProcess>,
    standalone: Vec<CodexProcess>,
) -> Result<(), String> {
    match (managed.is_empty(), standalone.is_empty()) {
        (true, true) => Ok(()),
        (false, true) => Err(format!(
            "ChatGPT is still running; close it before {action}"
        )),
        (true, false) => Err(format!(
            "a standalone Codex CLI is still running; close it before {action}"
        )),
        (false, false) => Err(format!(
            "ChatGPT and a standalone Codex CLI are still running; close them before {action}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, collections::VecDeque};

    use proptest::prelude::*;

    use super::{capture_launch_target_once, close_runtime_processes_with_progress};
    use crate::{process_control::CodexProcess, runtime_switcher::RuntimeSwitchPhase};

    fn process(pid: u32) -> CodexProcess {
        CodexProcess {
            image_name: "ChatGPT.exe".to_string(),
            pid,
            parent_pid: 0,
            creation_time_100ns: None,
        }
    }

    #[test]
    fn close_sequence_reports_real_process_phases() {
        let mut listings = VecDeque::from([Ok(vec![process(1)]), Ok(Vec::new())]);
        let closed = Cell::new(false);
        let mut phases = Vec::new();

        let did_close = close_runtime_processes_with_progress(
            || listings.pop_front().expect("unexpected process listing"),
            || {
                closed.set(true);
                Ok(())
            },
            |phase, _| phases.push(phase),
        )
        .unwrap();

        assert!(did_close);
        assert!(closed.get());
        assert_eq!(
            phases,
            vec![
                RuntimeSwitchPhase::DetectingApp,
                RuntimeSwitchPhase::ClosingApp
            ]
        );
    }

    #[test]
    fn launch_target_capture_is_idempotent_after_success() {
        let calls = Cell::new(0);
        let mut captured = false;
        capture_launch_target_once(&mut captured, || {
            calls.set(calls.get() + 1);
            true
        });
        capture_launch_target_once(&mut captured, || {
            calls.set(calls.get() + 1);
            true
        });
        assert!(captured);
        assert_eq!(calls.get(), 1);
    }
    proptest! {
        #[test]
        fn launch_target_capture_stops_after_the_first_success(results in prop::collection::vec(any::<bool>(), 0..64)) {
            let calls = Cell::new(0usize);
            let mut captured = false;
            for result in results.iter().copied() {
                capture_launch_target_once(&mut captured, || {
                    calls.set(calls.get() + 1);
                    result
                });
            }
            let first_success = results.iter().position(|value| *value);
            let expected_calls = first_success.map_or(results.len(), |index| index + 1);
            prop_assert_eq!(captured, first_success.is_some());
            prop_assert_eq!(calls.get(), expected_calls);
        }

        #[test]
        fn process_close_state_machine_never_closes_an_empty_inventory(initial_count in 0usize..32) {
            let initial = (0..initial_count)
                .map(|index| process(u32::try_from(index + 1).unwrap()))
                .collect::<Vec<_>>();
            let after = Vec::new();
            let mut listings = VecDeque::from([Ok(initial), Ok(after)]);
            let close_calls = Cell::new(0usize);
            let mut phases = Vec::new();

            let did_close = close_runtime_processes_with_progress(
                || listings.pop_front().expect("unexpected process listing"),
                || {
                    close_calls.set(close_calls.get() + 1);
                    Ok(())
                },
                |phase, _| phases.push(phase),
            ).unwrap();

            prop_assert_eq!(did_close, initial_count > 0);
            prop_assert_eq!(close_calls.get(), usize::from(initial_count > 0));
            prop_assert_eq!(phases.first(), Some(&RuntimeSwitchPhase::DetectingApp));
            prop_assert_eq!(phases.contains(&RuntimeSwitchPhase::ClosingApp), initial_count > 0);
        }
    }
}

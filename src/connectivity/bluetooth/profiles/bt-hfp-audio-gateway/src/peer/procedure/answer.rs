// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{Procedure, ProcedureError, ProcedureMarker, ProcedureRequest};

use crate::peer::service_level_connection::SlcState;
use crate::peer::slc_request::SlcRequest;
use crate::peer::update::AgUpdate;
use at_commands as at;

#[derive(Debug, PartialEq, Clone, Copy)]
enum State {
    /// Initial state of the Answer Procedure.
    Start,
    /// A request has been received from the HF to answer an incoming call.
    SetRequest,
    /// The AG has responded to the HF's request to set the state.
    Terminated,
}

impl State {
    /// Transition to the next state in the Answer procedure.
    fn transition(&mut self) {
        match *self {
            Self::Start => *self = Self::SetRequest,
            Self::SetRequest => *self = Self::Terminated,
            Self::Terminated => *self = Self::Terminated,
        }
    }
}

/// The HF Answers an incoming call via this procedure. See HFP v1.8, Section 4.13.
///
/// This procedure is implemented from the perspective of the AG. Namely, outgoing `requests`
/// typically request information about the current state of the AG, to be sent to the remote
/// peer acting as the HF.
#[derive(Debug)]
pub struct AnswerProcedure {
    /// The current state of the procedure
    state: State,
}

impl AnswerProcedure {
    /// Create a new Answer procedure in the Start state.
    pub fn new() -> Self {
        Self { state: State::Start }
    }
}

impl Procedure for AnswerProcedure {
    fn marker(&self) -> ProcedureMarker {
        ProcedureMarker::Answer
    }

    fn hf_update(&mut self, update: at::Command, _state: &mut SlcState) -> ProcedureRequest {
        match (self.state, update) {
            (State::Start, at::Command::Answer {}) => {
                self.state.transition();
                let response = Box::new(Into::into);
                SlcRequest::Answer { response }.into()
            }
            (_, update) => ProcedureError::UnexpectedHf(update).into(),
        }
    }

    fn ag_update(&mut self, update: AgUpdate, _state: &mut SlcState) -> ProcedureRequest {
        match (self.state, update) {
            (State::SetRequest, update @ AgUpdate::Ok)
            | (State::SetRequest, update @ AgUpdate::Error) => {
                self.state.transition();
                update.into()
            }
            (_, update) => ProcedureError::UnexpectedAg(update).into(),
        }
    }

    fn is_terminated(&self) -> bool {
        self.state == State::Terminated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;

    #[test]
    fn state_transitions() {
        let mut state = State::Start;
        state.transition();
        assert_eq!(state, State::SetRequest);
        state.transition();
        assert_eq!(state, State::Terminated);
        state.transition();
        assert_eq!(state, State::Terminated);
    }

    #[test]
    fn correct_marker() {
        let marker = AnswerProcedure::new().marker();
        assert_eq!(marker, ProcedureMarker::Answer);
    }

    #[test]
    fn is_terminated_in_terminated_state() {
        let mut proc = AnswerProcedure::new();
        assert!(!proc.is_terminated());
        proc.state = State::SetRequest;
        assert!(!proc.is_terminated());
        proc.state = State::Terminated;
        assert!(proc.is_terminated());
    }

    #[test]
    fn unexpected_hf_update_returns_error() {
        let mut proc = AnswerProcedure::new();
        let mut state = SlcState::default();

        // SLCI AT command.
        let random_hf = at::Command::CindRead {};
        assert_matches!(
            proc.hf_update(random_hf, &mut state),
            ProcedureRequest::Error(err) if matches!(*err, ProcedureError::UnexpectedHf(_))
        );
    }

    #[test]
    fn unexpected_ag_update_returns_error() {
        let mut proc = AnswerProcedure::new();
        let mut state = SlcState::default();
        // SLCI AT command.
        let random_ag = AgUpdate::ThreeWaySupport;
        assert_matches!(
            proc.ag_update(random_ag, &mut state),
            ProcedureRequest::Error(err) if matches!(*err, ProcedureError::UnexpectedAg(_))
        );
    }

    #[test]
    fn answer_incoming_call_produces_ok_result() {
        let mut proc = AnswerProcedure::new();
        let mut state = SlcState::default();

        let req = proc.hf_update(at::Command::Answer {}, &mut state);
        let update = match req {
            ProcedureRequest::Request(SlcRequest::Answer { response }) => response(Ok(())),
            x => panic!("Unexpected message: {:?}", x),
        };
        let req = proc.ag_update(update, &mut state);
        assert_matches!(
            req,
            ProcedureRequest::SendMessages(resp) if resp == vec![at::Response::Ok]
        );

        // Check that the procedure is terminated and any new messages produce an error.
        assert!(proc.is_terminated());
        assert_matches!(
            proc.hf_update(at::Command::Answer {}, &mut state),
            ProcedureRequest::Error(err) if matches!(*err, ProcedureError::UnexpectedHf(_))
        );
        assert_matches!(
            proc.ag_update(AgUpdate::Ok, &mut state),
            ProcedureRequest::Error(err) if matches!(*err, ProcedureError::UnexpectedAg(_))
        );
    }

    #[test]
    fn answer_no_incoming_call_produces_error_result() {
        let mut proc = AnswerProcedure::new();
        let mut state = SlcState::default();

        let req = proc.hf_update(at::Command::Answer {}, &mut state);
        let update = match req {
            ProcedureRequest::Request(SlcRequest::Answer { response }) => response(Err(())),
            x => panic!("Unexpected message: {:?}", x),
        };
        let req = proc.ag_update(update, &mut state);
        assert_matches!(
            req,
            ProcedureRequest::SendMessages(resp) if resp == vec![at::Response::Error]
        );
    }
}

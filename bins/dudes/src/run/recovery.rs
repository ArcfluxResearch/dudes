use std::io::{Read, Write};
use std::path::Path;

use dudes_proto::record::Hex32;
use dudes_proto::rpc::CurrentMethod;
use dudes_proto::rpc::recovery::{
    RecoveryAllowedChoices, RecoveryChoice, RecoveryInspectParams, RecoveryInspectResult,
    RecoveryReceiptParams, RecoveryReceiptResult, RecoverySubmitParams, RecoverySubmitResult,
};

use super::{Failure, Message, encode, local, operator_current_call, typed};
use crate::args::RecoveryCommand;
use crate::env::Environment;
use crate::{exit, render};

const MAX_INSPECTION_BYTES: u64 = 16 * 1024;

pub(super) fn command(
    command: &RecoveryCommand,
    env: &Environment,
    machine: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    match command {
        RecoveryCommand::Inspect(agent) => {
            let result: RecoveryInspectResult = typed(operator_current_call(
                env,
                CurrentMethod::RecoveryInspect,
                Some(encode(&RecoveryInspectParams { agent: *agent })?),
            )?)?;
            render::recovery_inspection(out, machine, &result).map_err(local)?;
            Ok(
                if matches!(result, RecoveryInspectResult::Available { .. }) {
                    exit::SUCCESS
                } else {
                    exit::REFUSED
                },
            )
        }
        RecoveryCommand::Receipt(request_id) => {
            let result: RecoveryReceiptResult = typed(operator_current_call(
                env,
                CurrentMethod::RecoveryReceipt,
                Some(encode(&RecoveryReceiptParams {
                    request_id: *request_id,
                })?),
            )?)?;
            render::recovery_receipt(out, machine, &result).map_err(local)?;
            Ok(
                if matches!(result, RecoveryReceiptResult::Unavailable { .. }) {
                    exit::OTHER
                } else {
                    exit::SUCCESS
                },
            )
        }
        RecoveryCommand::Submit { inspection, choice } => {
            submit(inspection, *choice, env, machine, out, err)
        }
    }
}

fn read_inspection(
    path: &Path,
    choice: RecoveryChoice,
) -> Result<dudes_proto::rpc::recovery::ExpectedRecovery, Failure> {
    let file =
        std::fs::File::open(path).map_err(|_| local("cannot read recovery inspection file"))?;
    let mut bytes = Vec::new();
    file.take(MAX_INSPECTION_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| local("cannot read recovery inspection file"))?;
    if bytes.len() as u64 > MAX_INSPECTION_BYTES {
        return Err(local("recovery inspection file exceeds 16 KiB"));
    }
    let inspection: RecoveryInspectResult =
        serde_json::from_slice(&bytes).map_err(|_| local("invalid recovery inspection file"))?;
    let RecoveryInspectResult::Available {
        expected, choices, ..
    } = inspection
    else {
        return Err(local(
            "recovery inspection is unavailable; inspect and review a current opportunity",
        ));
    };
    let offered = matches!(
        (choices, choice),
        (
            RecoveryAllowedChoices::FreshOnly | RecoveryAllowedChoices::RecordedOrFresh,
            RecoveryChoice::FreshWithRecap
        ) | (
            RecoveryAllowedChoices::RecordedOrFresh,
            RecoveryChoice::TryRecorded
        ) | (
            RecoveryAllowedChoices::RepeatIssuedFresh,
            RecoveryChoice::RepeatIssuedFresh
        )
    );
    if !offered {
        return Err(local(
            "the selected recovery choice was not offered by this inspection",
        ));
    }
    Ok(*expected)
}

fn submit(
    path: &Path,
    choice: RecoveryChoice,
    env: &Environment,
    machine: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let expected = read_inspection(path, choice)?;
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| local("cannot generate a recovery request ID"))?;
    let request = RecoverySubmitParams {
        request_id: Hex32::new(bytes),
        expected,
        choice,
    };
    let params = encode(&request)?;
    render::recovery_choice_warning(err, choice).map_err(local)?;
    let request_id = serde_json::to_value(request.request_id).map_err(local)?;
    writeln!(
        err,
        "Recovery request ID: {}",
        request_id
            .as_str()
            .ok_or_else(|| local("invalid recovery request ID"))?
    )
    .map_err(local)?;
    err.flush().map_err(local)?;
    let result: RecoverySubmitResult =
        match operator_current_call(env, CurrentMethod::RecoverySubmit, Some(params)) {
            Ok(value) => match typed(value) {
                Ok(result) => result,
                Err(_) => return reply_unavailable(request.request_id, machine, out, err),
            },
            Err(failure @ (Failure::Refused(_) | Failure::Other(Message::Rpc(_)))) => {
                return Err(failure);
            }
            Err(_) => return reply_unavailable(request.request_id, machine, out, err),
        };
    render::recovery_submitted(out, machine, &result).map_err(local)?;
    if matches!(result, RecoverySubmitResult::Unknown { .. }) {
        receipt_guidance(err, request.request_id).map_err(local)?;
    }
    Ok(match result {
        RecoverySubmitResult::Accepted { .. } => exit::SUCCESS,
        RecoverySubmitResult::Refused { .. } => exit::REFUSED,
        RecoverySubmitResult::Unknown { .. } => exit::OTHER,
    })
}

fn reply_unavailable(
    request_id: Hex32,
    machine: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    render::recovery_reply_unavailable(out, machine, request_id).map_err(local)?;
    receipt_guidance(err, request_id).map_err(local)?;
    Ok(exit::OTHER)
}

fn receipt_guidance(err: &mut dyn Write, request_id: Hex32) -> std::io::Result<()> {
    let value = serde_json::to_value(request_id).map_err(std::io::Error::other)?;
    let text = value
        .as_str()
        .ok_or_else(|| std::io::Error::other("invalid recovery request ID"))?;
    writeln!(
        err,
        "Authorization is not confirmed. Keep the request ID and run `dudes recovery receipt {text}`. Do not resend the submission."
    )
}

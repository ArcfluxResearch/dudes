//! Ticket RPC registration over the daemon's single writer loop. Every method is operator-only:
//! the caller is resolved structurally and no request names an author.

use dudes_proto::rpc::ticket::{
    TicketAcceptParams, TicketAssignParams, TicketCheckoutParams, TicketCreateParams,
    TicketEditParams, TicketReadParams, TicketRemoveParams, TicketStopParams,
};
use dudes_proto::rpc::{CurrentMethod, ErrorCode, Method};
use dudes_rpc::Dispatcher;
use dudes_rpc::auth::Requirement;
use dudes_rpc::dispatch::error;
use dudes_rpc::methods::{encode, encode_current, params};

use crate::board::actor;
use crate::ports::{Answered, Ask, Handle, TicketAsk};

fn wrong_answer(method: Method) -> dudes_proto::rpc::RpcError {
    error(
        ErrorCode::InternalError,
        format!("{} returned the wrong result", method.name()),
    )
}

pub(crate) fn register(dispatcher: &mut Dispatcher, handle: &Handle) {
    let checkout = handle.clone();
    dispatcher.register_current_audited(
        CurrentMethod::TicketCheckout,
        Requirement::Operator,
        move |call, pid| {
            let request: TicketCheckoutParams = params(&call)?;
            let by = actor(call.role, pid)?;
            match checkout.ask(Ask::Ticket(TicketAsk::Checkout { request, by }))? {
                Answered::TicketCheckout(result) => {
                    encode_current(CurrentMethod::TicketCheckout, &result)
                }
                _ => Err(error(
                    ErrorCode::InternalError,
                    "ticket.checkout returned the wrong result",
                )),
            }
        },
    );
    let create = handle.clone();
    dispatcher.register_audited(
        Method::TicketCreate,
        Requirement::Operator,
        move |call, pid| {
            let request: TicketCreateParams = params(&call)?;
            let by = actor(call.role, pid)?;
            match create.ask(Ask::Ticket(TicketAsk::Create { request, by }))? {
                Answered::TicketCreated(result) => encode(Method::TicketCreate, &result),
                _ => Err(wrong_answer(Method::TicketCreate)),
            }
        },
    );

    let edit = handle.clone();
    dispatcher.register_audited(
        Method::TicketEdit,
        Requirement::Operator,
        move |call, pid| {
            let request: TicketEditParams = params(&call)?;
            let by = actor(call.role, pid)?;
            edit.ask(Ask::Ticket(TicketAsk::Edit { request, by }))?;
            encode(Method::TicketEdit, &dudes_proto::rpc::Empty {})
        },
    );

    let assign = handle.clone();
    dispatcher.register_audited(
        Method::TicketAssign,
        Requirement::Operator,
        move |call, pid| {
            let request: TicketAssignParams = params(&call)?;
            let by = actor(call.role, pid)?;
            match assign.ask(Ask::Ticket(TicketAsk::Assign { request, by }))? {
                Answered::TicketAssigned(result) => encode(Method::TicketAssign, &result),
                _ => Err(wrong_answer(Method::TicketAssign)),
            }
        },
    );

    let remove = handle.clone();
    dispatcher.register_audited(
        Method::TicketRemove,
        Requirement::Operator,
        move |call, pid| {
            let request: TicketRemoveParams = params(&call)?;
            let by = actor(call.role, pid)?;
            remove.ask(Ask::Ticket(TicketAsk::Remove { request, by }))?;
            encode(Method::TicketRemove, &dudes_proto::rpc::Empty {})
        },
    );

    let accept = handle.clone();
    dispatcher.register_audited(
        Method::TicketAccept,
        Requirement::Operator,
        move |call, pid| {
            let request: TicketAcceptParams = params(&call)?;
            let by = actor(call.role, pid)?;
            accept.ask(Ask::Ticket(TicketAsk::Accept { request, by }))?;
            encode(Method::TicketAccept, &dudes_proto::rpc::Empty {})
        },
    );

    let stop = handle.clone();
    dispatcher.register_audited(
        Method::TicketStop,
        Requirement::Operator,
        move |call, pid| {
            let request: TicketStopParams = params(&call)?;
            let by = actor(call.role, pid)?;
            stop.ask(Ask::Ticket(TicketAsk::Stop { request, by }))?;
            encode(Method::TicketStop, &dudes_proto::rpc::Empty {})
        },
    );

    let read = handle.clone();
    dispatcher.register(Method::TicketRead, Requirement::Operator, move |call| {
        let request: TicketReadParams = params(&call)?;
        match read.ask(Ask::Ticket(TicketAsk::Read(request)))? {
            Answered::TicketRead(result) => encode(Method::TicketRead, &*result),
            _ => Err(wrong_answer(Method::TicketRead)),
        }
    });
}

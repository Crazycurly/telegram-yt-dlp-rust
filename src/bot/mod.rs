pub mod handlers;
pub mod menu;

use teloxide::dispatching::UpdateHandler;
use teloxide::prelude::*;

/// The dptree handler tree: messages go to the URL/menu handler, button taps to
/// the callback handler. Authorization is enforced inside each handler.
pub fn schema() -> UpdateHandler<anyhow::Error> {
    dptree::entry()
        .branch(Update::filter_message().endpoint(handlers::message_handler))
        .branch(Update::filter_callback_query().endpoint(handlers::callback_handler))
}

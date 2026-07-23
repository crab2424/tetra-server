mod common;
mod reliable;
mod unreliable;

pub use common::{
    RELIABLE_CHANNEL_LABEL, UNRELIABLE_CHANNEL_LABEL, disconnect_player, spawn_close_channels,
};
pub use reliable::handle_reliable_connection;
pub use unreliable::handle_unreliable_connection;

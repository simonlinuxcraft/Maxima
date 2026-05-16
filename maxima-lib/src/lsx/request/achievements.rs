// MAXIMA-LINUX-PORT-MOD: New file. Stub handler for QueryAchievements
// LSX request. See Kyber/ThirdParty/Maxima/CHANGES.md (2026-05-05) and
// upstream Issue #2 (https://github.com/ArmchairDevelopers/Maxima/issues/2).

use log::debug;

use crate::{
    lsx::{
        connection::LockedConnectionState,
        request::LSXRequestError,
        types::{LSXQueryAchievements, LSXQueryAchievementsResponse, LSXResponseType},
    },
    make_lsx_handler_response,
};

pub async fn handle_query_achievements_request(
    _: LockedConnectionState,
    request: LSXQueryAchievements,
) -> Result<Option<LSXResponseType>, LSXRequestError> {
    debug!(
        "QueryAchievements stub: user={:?} offer={:?}",
        request.attr_UserId, request.attr_OfferId
    );

    make_lsx_handler_response!(Response, QueryAchievementsResponse, { achievement: Vec::new() })
}

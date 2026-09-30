//! Separate prepared predicates keep delta reads indexable under generic plans.

pub(super) fn selector_predicate(has_cursor: bool, contrast: bool) -> &'static str {
    if contrast {
        "AND ge.treatment = 'control' AND ea.experiment_uuid = ANY($3) \
         AND $2::timestamptz IS NULL"
    } else if has_cursor {
        "AND GREATEST(ge.resolved_at, ge.replayed_3d_at, ge.replayed_14d_at, \
             ge.replayed_30d_at, ge.last_partial_resolution_at) > $2::timestamptz \
         AND $3::uuid[] IS NULL"
    } else {
        "AND $2::timestamptz IS NULL AND $3::uuid[] IS NULL"
    }
}

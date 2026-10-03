-- Play outcome series are correlational, not causal.
--
-- Before this migration the play completion path folded the provider-series
-- verdict ("the metric moved during the play window") into play_learning.
-- That can reward or retire a play because of unrelated organic growth, press,
-- another release, or any concurrent intervention. The runtime no longer feeds
-- correlational claims into autonomous standing. Reset that contaminated
-- machine-learned state so old coincidence does not keep steering new runs.
--
-- A human's explicit operator retirement is authority, not model inference,
-- and is preserved exactly.

UPDATE play_learning
SET improved_count = 0,
    neutral_count = 0,
    worsened_count = 0,
    insufficient_count = 0,
    consecutive_worsened = 0,
    weight_basis_points = 10000,
    retired_at = NULL,
    retired_reason = NULL
WHERE retired_reason IS DISTINCT FROM 'operator_retired';

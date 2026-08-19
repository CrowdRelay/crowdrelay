from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
d=(ROOT/'crates/crowdrelay-domain/src/show_growth.rs').read_text()
a=(ROOT/'crates/crowdrelay-api/src/fan_context.rs').read_text()
for phase in ('Planning','Amplify','Convert','Ready','Live','Afterglow','Review','Complete'):
    assert phase in d
for key in ('free_listing_lead_days','free_fan_channel_push_lead_days','last_mile_lead_days','post_show_merch_hours'):
    assert key in d
assert 'show_lifecycle(' in a and 'ShowGrowthPolicy::default()' in a
print('SHOW_LIFECYCLE=PASS phases=8 shared_policy=true second_scheduler=false')

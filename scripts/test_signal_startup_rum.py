from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
s=(ROOT/'crates/crowdrelay-api/src/autopilot/runtime.rs').read_text()
for key in ('cached_content_ready_ms','network_content_ready_ms'):
    assert key in s, key
assert 'virya_signal' in s
print('SIGNAL_STARTUP_RUM=PASS metrics=2')

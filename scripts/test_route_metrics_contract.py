#!/usr/bin/env python3
from pathlib import Path
R=Path(__file__).resolve().parents[1]
lib=(R/'crates/crowdrelay-api/src/lib.rs').read_text(); hm=(R/'crates/crowdrelay-api/src/http_metrics.rs').read_text()
assert 'MatchedPath' in lib and 'record_route(&method, &route' in lib
assert 'MAX_ROUTE_SERIES: usize = 1024' in hm and 'route_series_dropped' in hm
start=lib.index('async fn measure_request')
segment=lib[start:lib.index('let mut response = next.run(request).await;', start)]
assert 'request.uri().path()' not in segment
print('ROUTE_METRICS_CONTRACT=PASS bounded=1024 matched_path=true')

http=(R/'crates/crowdrelay-api/src/http_metrics.rs').read_text()
assert 'route_series.try_lock()' in http
assert 'route_series_dropped.fetch_add(1, Ordering::Relaxed)' in http
print('ROUTE_METRICS_NONBLOCKING=PASS')

#!/usr/bin/env python3
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def toml_section(path, name):
    text = path.read_text()
    match = re.search(
        rf"(?ms)^\[{re.escape(name)}\]\s*$\n(?P<body>.*?)(?=^\[|\Z)",
        text,
    )
    if not match:
        raise AssertionError(f"missing TOML section [{name}] in {path}")
    return match.group("body")


def toml_value(path, section, key):
    body = toml_section(path, section)
    match = re.search(rf"(?m)^{re.escape(key)}\s*=\s*(?P<value>.+?)\s*$", body)
    if not match:
        raise AssertionError(f"missing TOML key [{section}] {key} in {path}")
    raw = match.group("value").split("#", 1)[0].strip()
    if raw == "true":
        return True
    if raw == "false":
        return False
    if re.fullmatch(r"-?\d+", raw):
        return int(raw)
    quoted = re.fullmatch(r'"(.*)"', raw)
    return quoted.group(1) if quoted else raw


def cargo_lock_packages(path):
    packages = []
    for block in re.split(r"(?m)^\[\[package\]\]\s*$", path.read_text())[1:]:
        name = re.search(r'(?m)^name = "([^"]+)"\s*$', block)
        version = re.search(r'(?m)^version = "([^"]+)"\s*$', block)
        if not name or not version:
            continue
        dependencies = []
        dependency_block = re.search(
            r"(?ms)^dependencies = \[\s*(.*?)^\]\s*$",
            block,
        )
        if dependency_block:
            dependencies = re.findall(r'"([^"]+)"', dependency_block.group(1))
        packages.append(
            {
                "name": name.group(1),
                "version": version.group(1),
                "dependencies": dependencies,
            }
        )
    return packages


class RuntimePerformanceContract(unittest.TestCase):
    def test_release_profile_stays_small_without_changing_panic_semantics(self):
        manifest = ROOT / "Cargo.toml"
        self.assertIs(toml_value(manifest, "profile.release", "lto"), True)
        self.assertEqual(toml_value(manifest, "profile.release", "codegen-units"), 1)
        self.assertEqual(toml_value(manifest, "profile.release", "strip"), "symbols")
        self.assertEqual(toml_value(manifest, "profile.release", "panic"), "unwind")

    def test_os_rng_uses_the_current_direct_dependency(self):
        manifest = ROOT / "Cargo.toml"
        self.assertEqual(
            toml_value(manifest, "workspace.dependencies", "getrandom"),
            "0.4",
        )
        lock_packages = cargo_lock_packages(ROOT / "Cargo.lock")
        local_packages = {
            package["name"]: package
            for package in lock_packages
            if package["name"].startswith("crowdrelay-")
        }
        for name in ("crowdrelay-infra", "crowdrelay-worker"):
            dependencies = local_packages[name]["dependencies"]
            self.assertTrue(any(value.startswith("getrandom 0.4.") for value in dependencies))
            self.assertFalse(any(value.startswith("getrandom 0.3.") for value in dependencies))
        self.assertFalse(
            any(
                package["name"] == "getrandom"
                and package["version"].startswith("0.3.")
                for package in lock_packages
            )
        )

    def test_feature_flag_cache_is_bounded(self):
        source = (ROOT / "crates/crowdrelay-api/src/ecosystem.rs").read_text()
        self.assertIn("const MAX_FLAG_CACHE_ENTRIES: usize = 256;", source)
        self.assertIn("cache.retain(|_, entry| entry.expires_at > now);", source)
        self.assertIn("feature_flag_cache_is_strictly_bounded", source)

    def test_periodic_tasks_skip_missed_ticks_instead_of_bursting(self):
        roots = [
            ROOT / "crates/crowdrelay-api/src",
            ROOT / "crates/crowdrelay-worker/src",
            ROOT / "crates/crowdrelay-infra/src",
        ]
        interval_sources = []
        for source_root in roots:
            for path in source_root.rglob("*.rs"):
                text = path.read_text()
                if re.search(r"\binterval\(", text):
                    interval_sources.append(path)
                    self.assertIn(
                        "set_missed_tick_behavior(MissedTickBehavior::Skip)",
                        text,
                        f"{path.relative_to(ROOT)} may burst after a stall",
                    )
        self.assertGreaterEqual(len(interval_sources), 8)

    def test_outbound_http_clients_bound_idle_connections(self):
        for relative in (
            "crates/crowdrelay-worker/src/outbox/transport.rs",
            "crates/crowdrelay-worker/src/event_sync.rs",
        ):
            source = (ROOT / relative).read_text()
            self.assertIn(".pool_idle_timeout(Duration::from_secs(30))", source)
            self.assertIn(".pool_max_idle_per_host(2)", source)
            self.assertIn(".tcp_keepalive(Duration::from_secs(30))", source)
        event_sync = (ROOT / "crates/crowdrelay-worker/src/event_sync.rs").read_text()
        self.assertIn(
            ".connect_timeout(config.http_timeout.min(Duration::from_secs(5)))",
            event_sync,
        )


    def test_event_repository_caches_stable_workspace_identity(self):
        source = (ROOT / "crates/crowdrelay-infra/src/events.rs").read_text()
        self.assertIn("workspace_id: Arc<OnceCell<WorkspaceId>>", source)
        trusted = source[source.index("async fn trusted_workspace_id") :]
        trusted = trusted[: trusted.index("async fn load_published_events_inner")]
        self.assertIn(".get_or_try_init(|| async {", trusted)
        self.assertIn("SELECT id FROM workspaces WHERE slug = $1", trusted)

    def test_production_runtime_threads_are_explicitly_bounded(self):
        compose = (ROOT / "compose.production.yaml").read_text()
        for fragment in (
            "CROWDRELAY_SETUP_TOKIO_THREADS:-1",
            "CROWDRELAY_API_TOKIO_THREADS:-2",
            "CROWDRELAY_WORKER_TOKIO_THREADS:-1",
        ):
            self.assertIn(fragment, compose)
        env_example = (ROOT / "deploy/env.production.example").read_text()
        for fragment in (
            "CROWDRELAY_SETUP_TOKIO_THREADS=1",
            "CROWDRELAY_API_TOKIO_THREADS=2",
            "CROWDRELAY_WORKER_TOKIO_THREADS=1",
        ):
            self.assertIn(fragment, env_example)

    def test_worker_image_does_not_install_api_healthcheck_tooling(self):
        dockerfile = (ROOT / "Dockerfile").read_text()
        runtime, api = dockerfile.split("FROM runtime AS api", 1)
        self.assertNotRegex(runtime, r"apt-get install[^\n]*\bcurl\b")
        self.assertRegex(api, r"apt-get install[^\n]*\bcurl\b")


if __name__ == "__main__":
    unittest.main()

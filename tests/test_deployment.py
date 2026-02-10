import sys
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import pytest

# Ensure src is importable when running pytest from repo root
ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

from handlers.deployment import Deployment, _parse_int_or_string  # noqa: E402
from handlers.postgres_clusters import PostgresCluster  # noqa: E402


def _make_handler(spec=None, defaults=None, name="test"):
    return SimpleNamespace(
        name=name,
        namespace="default",
        spec=spec or {},
        defaults=defaults or {},
        owner_reference={"fake": "owner"},
        odoo_user_secret=None,
    )


@pytest.fixture(autouse=True)
def mock_postgres_cluster():
    """Mock the postgres cluster for all deployment tests."""
    mock_cluster = PostgresCluster(
        name="test-cluster",
        host="postgres.example",
        port=5432,
        admin_user="postgres",
        admin_password="secret",
        is_default=True,
    )
    with patch(
        "handlers.deployment.get_cluster_for_instance", return_value=mock_cluster
    ):
        yield mock_cluster


def test_deployment_defaults_and_ports():
    handler = _make_handler(defaults={"odooImage": "odoo:18.0"})
    dep_handler = Deployment(handler)
    dep_handler._resource = SimpleNamespace(
        spec=None
    )  # avoid live API call for replicas
    dep = dep_handler._get_resource_body()  # noqa: SLF001 private use OK in tests
    tpl = dep.spec.template
    container = tpl.spec.containers[0]

    assert container.image == "odoo:18.0"
    assert container.command == ["/entrypoint.sh", "odoo"]

    ports = {p.name: p.container_port for p in container.ports}
    assert ports == {"http": 8069, "websocket": 8072}

    assert tpl.spec.security_context.run_as_user == 100
    assert tpl.spec.security_context.run_as_group == 101
    assert tpl.spec.security_context.fs_group == 101

    mounts = {m.name: m.mount_path for m in container.volume_mounts}
    assert mounts == {"filestore": "/var/lib/odoo", "odoo-conf": "/etc/odoo"}

    volumes = {v.name for v in tpl.spec.volumes}
    assert volumes == {"filestore", "odoo-conf"}

    # Probes target the health endpoint on 8069 with default paths
    assert container.startup_probe.http_get.port == 8069
    assert container.startup_probe.http_get.path == "/web/health"
    assert container.liveness_probe.http_get.port == 8069
    assert container.liveness_probe.http_get.path == "/web/health"
    assert container.readiness_probe.http_get.port == 8069
    assert container.readiness_probe.http_get.path == "/web/health"


def test_deployment_image_override_and_pull_secret():
    spec = {"image": "registry/odoo:custom", "imagePullSecret": "pull-me"}
    handler = _make_handler(spec=spec, defaults={"odooImage": "odoo:18.0"})
    dep_handler = Deployment(handler)
    dep_handler._resource = SimpleNamespace(spec=None)
    dep = dep_handler._get_resource_body()
    container = dep.spec.template.spec.containers[0]

    assert container.image == "registry/odoo:custom"
    assert dep.spec.template.spec.image_pull_secrets[0].name == "pull-me"


def test_deployment_env_vars_from_secrets_and_env(monkeypatch):
    handler = _make_handler(defaults={"odooImage": "odoo:18.0"}, name="demo")
    dep_handler = Deployment(handler)
    dep_handler._resource = SimpleNamespace(spec=None)
    dep = dep_handler._get_resource_body()
    envs = {e.name: e for e in dep.spec.template.spec.containers[0].env}

    assert envs["HOST"].value == "postgres.example"
    assert envs["PORT"].value == "5432"
    assert envs["USER"].value_from.secret_key_ref.name == "demo-odoo-user"
    assert envs["USER"].value_from.secret_key_ref.key == "username"
    assert envs["PASSWORD"].value_from.secret_key_ref.name == "demo-odoo-user"
    assert envs["PASSWORD"].value_from.secret_key_ref.key == "password"


def test_deployment_strategy_recreate_default():
    """Test default deployment strategy is Recreate"""
    handler = _make_handler(defaults={"odooImage": "odoo:18.0"})
    dep_handler = Deployment(handler)
    dep_handler._resource = SimpleNamespace(spec=None)
    dep = dep_handler._get_resource_body()

    assert dep.spec.strategy.type == "Recreate"
    assert dep.spec.strategy.rolling_update is None


def test_deployment_strategy_recreate_spec():
    """Test Recreate strategy when specified in spec"""
    spec = {"strategy": {"type": "Recreate"}}
    handler = _make_handler(spec=spec, defaults={"odooImage": "odoo:18.0"})
    dep_handler = Deployment(handler)
    dep_handler._resource = SimpleNamespace(spec=None)
    dep = dep_handler._get_resource_body()

    assert dep.spec.strategy.type == "Recreate"
    assert dep.spec.strategy.rolling_update is None


def test_deployment_strategy_rolling_update_default_params():
    """Test RollingUpdate strategy with default parameters"""
    spec = {"strategy": {"type": "RollingUpdate"}}
    handler = _make_handler(spec=spec, defaults={"odooImage": "odoo:18.0"})
    dep_handler = Deployment(handler)
    dep_handler._resource = SimpleNamespace(spec=None)
    dep = dep_handler._get_resource_body()

    assert dep.spec.strategy.type == "RollingUpdate"
    assert dep.spec.strategy.rolling_update.max_unavailable == "25%"
    assert dep.spec.strategy.rolling_update.max_surge == "25%"


def test_deployment_strategy_rolling_update_custom_params():
    """Test RollingUpdate strategy with custom parameters (numeric strings become ints)"""
    spec = {
        "strategy": {
            "type": "RollingUpdate",
            "rollingUpdate": {"maxUnavailable": "1", "maxSurge": "2"},
        }
    }
    handler = _make_handler(spec=spec, defaults={"odooImage": "odoo:18.0"})
    dep_handler = Deployment(handler)
    dep_handler._resource = SimpleNamespace(spec=None)
    dep = dep_handler._get_resource_body()

    assert dep.spec.strategy.type == "RollingUpdate"
    assert dep.spec.strategy.rolling_update.max_unavailable == 1  # converted to int
    assert dep.spec.strategy.rolling_update.max_surge == 2  # converted to int


def test_deployment_strategy_rolling_update_from_defaults():
    """Test RollingUpdate strategy defaults from helm values"""
    defaults = {
        "odooImage": "odoo:18.0",
        "deploymentStrategy": {
            "type": "RollingUpdate",
            "rollingUpdate": {"maxUnavailable": "10%", "maxSurge": "20%"},
        },
    }
    handler = _make_handler(defaults=defaults)
    dep_handler = Deployment(handler)
    dep_handler._resource = SimpleNamespace(spec=None)
    dep = dep_handler._get_resource_body()

    assert dep.spec.strategy.type == "RollingUpdate"
    assert dep.spec.strategy.rolling_update.max_unavailable == "10%"
    assert dep.spec.strategy.rolling_update.max_surge == "20%"


def test_deployment_strategy_spec_overrides_defaults():
    """Test that spec strategy overrides defaults"""
    spec = {
        "strategy": {
            "type": "RollingUpdate",
            "rollingUpdate": {"maxUnavailable": "0", "maxSurge": "1"},
        }
    }
    defaults = {
        "odooImage": "odoo:18.0",
        "deploymentStrategy": {
            "type": "Recreate",
            "rollingUpdate": {"maxUnavailable": "25%", "maxSurge": "25%"},
        },
    }
    handler = _make_handler(spec=spec, defaults=defaults)
    dep_handler = Deployment(handler)
    dep_handler._resource = SimpleNamespace(spec=None)
    dep = dep_handler._get_resource_body()

    assert dep.spec.strategy.type == "RollingUpdate"
    assert dep.spec.strategy.rolling_update.max_unavailable == 0  # converted to int
    assert dep.spec.strategy.rolling_update.max_surge == 1  # converted to int


def test_deployment_strategy_partial_rolling_update_params():
    """Test RollingUpdate with only some parameters specified"""
    spec = {
        "strategy": {
            "type": "RollingUpdate",
            "rollingUpdate": {
                "maxUnavailable": "1"
                # maxSurge not specified, should use default
            },
        }
    }
    handler = _make_handler(spec=spec, defaults={"odooImage": "odoo:18.0"})
    dep_handler = Deployment(handler)
    dep_handler._resource = SimpleNamespace(spec=None)
    dep = dep_handler._get_resource_body()

    assert dep.spec.strategy.type == "RollingUpdate"
    assert dep.spec.strategy.rolling_update.max_unavailable == 1  # converted to int
    assert (
        dep.spec.strategy.rolling_update.max_surge == "25%"
    )  # default value (percentage)


# Tests for _parse_int_or_string helper function


def test_parse_int_or_string_percentage():
    """Percentage strings should be kept as-is"""
    assert _parse_int_or_string("25%") == "25%"
    assert _parse_int_or_string("0%") == "0%"
    assert _parse_int_or_string("100%") == "100%"


def test_parse_int_or_string_numeric():
    """Numeric strings should be converted to integers"""
    assert _parse_int_or_string("0") == 0
    assert _parse_int_or_string("1") == 1
    assert _parse_int_or_string("25") == 25
    assert _parse_int_or_string("100") == 100


def test_parse_int_or_string_already_int():
    """Integers should pass through unchanged"""
    assert _parse_int_or_string(0) == 0
    assert _parse_int_or_string(1) == 1
    assert _parse_int_or_string(25) == 25


def test_parse_int_or_string_invalid():
    """Invalid strings should be kept as-is"""
    assert _parse_int_or_string("abc") == "abc"
    assert _parse_int_or_string("1.5") == "1.5"


# Tests for probe configuration


def test_deployment_probe_paths_default():
    """Test that all probes default to /web/health"""
    handler = _make_handler(defaults={"odooImage": "odoo:18.0"})
    dep_handler = Deployment(handler)
    dep_handler._resource = SimpleNamespace(spec=None)
    dep = dep_handler._get_resource_body()
    container = dep.spec.template.spec.containers[0]

    assert container.startup_probe.http_get.path == "/web/health"
    assert container.liveness_probe.http_get.path == "/web/health"
    assert container.readiness_probe.http_get.path == "/web/health"


def test_deployment_probe_paths_custom_readiness():
    """Test custom readiness path for health_check_k8s module"""
    spec = {
        "probes": {
            "readinessPath": "/health/ready",
        }
    }
    handler = _make_handler(spec=spec, defaults={"odooImage": "odoo:18.0"})
    dep_handler = Deployment(handler)
    dep_handler._resource = SimpleNamespace(spec=None)
    dep = dep_handler._get_resource_body()
    container = dep.spec.template.spec.containers[0]

    # Startup and liveness should still use default
    assert container.startup_probe.http_get.path == "/web/health"
    assert container.liveness_probe.http_get.path == "/web/health"
    # Readiness uses custom path
    assert container.readiness_probe.http_get.path == "/health/ready"


def test_deployment_probe_paths_all_custom():
    """Test all custom probe paths"""
    spec = {
        "probes": {
            "startupPath": "/custom/startup",
            "livenessPath": "/custom/liveness",
            "readinessPath": "/custom/readiness",
        }
    }
    handler = _make_handler(spec=spec, defaults={"odooImage": "odoo:18.0"})
    dep_handler = Deployment(handler)
    dep_handler._resource = SimpleNamespace(spec=None)
    dep = dep_handler._get_resource_body()
    container = dep.spec.template.spec.containers[0]

    assert container.startup_probe.http_get.path == "/custom/startup"
    assert container.liveness_probe.http_get.path == "/custom/liveness"
    assert container.readiness_probe.http_get.path == "/custom/readiness"


def test_deployment_startup_probe_exists():
    """Test that startup probe is configured with appropriate settings"""
    handler = _make_handler(defaults={"odooImage": "odoo:18.0"})
    dep_handler = Deployment(handler)
    dep_handler._resource = SimpleNamespace(spec=None)
    dep = dep_handler._get_resource_body()
    container = dep.spec.template.spec.containers[0]

    # Startup probe should exist and have reasonable settings for slow-starting Odoo
    assert container.startup_probe is not None
    assert container.startup_probe.http_get.port == 8069
    assert container.startup_probe.failure_threshold >= 20  # Allow time for slow starts
    assert container.startup_probe.period_seconds >= 5

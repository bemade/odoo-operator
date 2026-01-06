import os
import sys
import base64
from datetime import datetime, timezone
from pathlib import Path
from types import SimpleNamespace

import pytest
from kubernetes import client
from kubernetes.client.rest import ApiException

# Ensure src is importable when running pytest from repo root
ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

from handlers.upgrade_job_handler import OdooUpgradeJobHandler  # noqa: E402


@pytest.fixture(autouse=True)
def db_env(monkeypatch):
    monkeypatch.setenv("DB_HOST", "postgres.example")
    monkeypatch.setenv("DB_PORT", "5432")
    monkeypatch.setenv("DEFAULT_ODOO_IMAGE", "odoo:18.0")


def _make_body(status=None, modules=None):
    return {
        "metadata": {"name": "upgrade1", "namespace": "default", "uid": "u1"},
        "spec": {
            "odooInstanceRef": {"name": "demo", "namespace": "default"},
            **({"modules": modules} if modules is not None else {}),
        },
        "status": status or {},
    }


def _make_instance(
    image="registry/odoo:custom", pull_secret=None, phase=None, replicas=1
):
    spec = {"image": image, "replicas": replicas}
    if pull_secret:
        spec["imagePullSecret"] = pull_secret
    return {
        "metadata": {"name": "demo", "uid": "1234-5678"},
        "spec": spec,
        "status": {"phase": phase} if phase else {},
    }


def test_create_upgrade_job_builds_spec(monkeypatch):
    body = _make_body(modules=["base", "sale"])
    handler = OdooUpgradeJobHandler(body)

    # Instance fetch
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.client.CustomObjectsApi.get_namespaced_custom_object",
        lambda *args, **kwargs: _make_instance(pull_secret="pullme"),
    )
    scale_calls = []
    phase_calls = []
    status_calls = []
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.OdooUpgradeJobHandler._scale_deployment",
        lambda self, name, ns, replicas: scale_calls.append((name, ns, replicas)),
    )
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.OdooUpgradeJobHandler._update_instance_phase",
        lambda self, phase: phase_calls.append(phase),
    )
    creation_time = datetime(2025, 1, 1, tzinfo=timezone.utc)

    def fake_create(self, namespace, body):
        body.metadata = body.metadata or client.V1ObjectMeta()
        body.metadata.name = "job-upgrade"
        body.metadata.creation_timestamp = creation_time
        return body

    monkeypatch.setattr(
        "handlers.upgrade_job_handler.client.BatchV1Api.create_namespaced_job",
        fake_create,
    )
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.OdooUpgradeJobHandler._update_status",
        lambda self, phase, **kwargs: status_calls.append((phase, kwargs)),
    )

    handler.on_create()

    # Scaled down and phase set to Upgrading
    assert scale_calls == [("demo", "default", 0)]
    assert "Upgrading" in phase_calls
    assert status_calls and status_calls[-1][0] == "Running"

    # Inspect job spec
    job = handler._create_upgrade_job(_make_instance(pull_secret="pullme"))
    container = job.spec.template.spec.containers[0]
    env = {e.name: e for e in container.env}
    assert container.image == "registry/odoo:custom"
    assert "-u" in container.args and "base,sale" in ",".join(container.args)
    assert env["HOST"].value == "postgres.example"
    assert env["PORT"].value == "5432"
    assert env["USER"].value_from.secret_key_ref.name == "demo-odoo-user"
    pull = job.spec.template.spec.image_pull_secrets
    assert pull[0].name == "pullme"


def test_upgrade_job_has_required_volumes(monkeypatch):
    """Verify upgrade job mounts filestore PVC and odoo-conf ConfigMap for addons."""
    body = _make_body(modules=["base"])
    handler = OdooUpgradeJobHandler(body)

    monkeypatch.setattr(
        "handlers.upgrade_job_handler.client.BatchV1Api.create_namespaced_job",
        lambda self, namespace, body: body,
    )

    job = handler._create_upgrade_job(_make_instance())
    pod_spec = job.spec.template.spec

    # Check volumes
    volume_names = {v.name for v in pod_spec.volumes}
    assert "filestore" in volume_names, "Missing filestore volume"
    assert "odoo-conf" in volume_names, "Missing odoo-conf volume for addons"

    # Verify volume sources
    volumes_by_name = {v.name: v for v in pod_spec.volumes}
    assert (
        volumes_by_name["filestore"].persistent_volume_claim.claim_name
        == "demo-filestore-pvc"
    )
    assert volumes_by_name["odoo-conf"].config_map.name == "demo-odoo-conf"

    # Check volume mounts on the upgrade container
    container = pod_spec.containers[0]
    mount_names = {m.name for m in container.volume_mounts}
    assert "filestore" in mount_names, "Container missing filestore mount"
    assert "odoo-conf" in mount_names, "Container missing odoo-conf mount for addons"

    # Verify mount paths
    mounts_by_name = {m.name: m for m in container.volume_mounts}
    assert mounts_by_name["filestore"].mount_path == "/var/lib/odoo"
    assert mounts_by_name["odoo-conf"].mount_path == "/etc/odoo"


def test_on_create_missing_instance(monkeypatch):
    body = _make_body()
    handler = OdooUpgradeJobHandler(body)

    monkeypatch.setattr(
        "handlers.upgrade_job_handler.client.CustomObjectsApi.get_namespaced_custom_object",
        lambda *args, **kwargs: (_ for _ in ()).throw(ApiException(status=404)),
    )
    status_calls = []
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.OdooUpgradeJobHandler._update_status",
        lambda self, phase, **kwargs: status_calls.append((phase, kwargs)),
    )
    handler.on_create()
    assert status_calls and status_calls[-1][0] == "Failed"


def test_on_create_busy_instance(monkeypatch):
    body = _make_body()
    handler = OdooUpgradeJobHandler(body)
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.client.CustomObjectsApi.get_namespaced_custom_object",
        lambda *args, **kwargs: _make_instance(phase="Upgrading"),
    )
    status_calls = []
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.OdooUpgradeJobHandler._update_status",
        lambda self, phase, **kwargs: status_calls.append((phase, kwargs)),
    )
    handler.on_create()
    assert status_calls and status_calls[-1][0] == "Failed"


def test_check_job_status_completed(monkeypatch):
    body = _make_body(status={"jobName": "job-1"})
    handler = OdooUpgradeJobHandler(body)
    job_obj = SimpleNamespace(
        status=SimpleNamespace(
            succeeded=1,
            failed=None,
            completion_time=datetime(2025, 1, 2, tzinfo=timezone.utc),
        )
    )
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.client.BatchV1Api.read_namespaced_job",
        lambda self, name, namespace: job_obj,
    )
    status_calls = []
    phase_calls = []
    notify_calls = []
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.OdooUpgradeJobHandler._update_status",
        lambda self, phase, **kwargs: status_calls.append((phase, kwargs)),
    )
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.OdooUpgradeJobHandler._update_instance_phase",
        lambda self, phase: phase_calls.append(phase),
    )
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.OdooUpgradeJobHandler._restore_deployment_scale",
        lambda self: phase_calls.append("scaled"),
    )
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.OdooUpgradeJobHandler._notify_webhook",
        lambda self, phase: notify_calls.append(phase),
    )
    handler.check_job_status()
    assert status_calls and status_calls[-1][0] == "Completed"
    assert "Running" in phase_calls  # phase reset
    assert "scaled" in phase_calls
    assert notify_calls == ["Completed"]


def test_check_job_status_failed(monkeypatch):
    body = _make_body(status={"jobName": "job-1"})
    handler = OdooUpgradeJobHandler(body)
    job_obj = SimpleNamespace(
        status=SimpleNamespace(
            succeeded=None,
            failed=1,
            completion_time=datetime(2025, 1, 2, tzinfo=timezone.utc),
        )
    )
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.client.BatchV1Api.read_namespaced_job",
        lambda self, name, namespace: job_obj,
    )
    status_calls = []
    phase_calls = []
    notify_calls = []
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.OdooUpgradeJobHandler._update_status",
        lambda self, phase, **kwargs: status_calls.append((phase, kwargs)),
    )
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.OdooUpgradeJobHandler._update_instance_phase",
        lambda self, phase: phase_calls.append(phase),
    )
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.OdooUpgradeJobHandler._restore_deployment_scale",
        lambda self: phase_calls.append("scaled"),
    )
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.OdooUpgradeJobHandler._notify_webhook",
        lambda self, phase: notify_calls.append(phase),
    )
    handler.check_job_status()
    assert status_calls and status_calls[-1][0] == "Failed"
    assert "Running" in phase_calls
    assert "scaled" in phase_calls
    assert notify_calls == ["Failed"]


def test_check_job_status_skips_terminal(monkeypatch):
    body = _make_body(status={"phase": "Completed", "jobName": "job-1"})
    handler = OdooUpgradeJobHandler(body)
    calls = []
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.client.BatchV1Api.read_namespaced_job",
        lambda *args, **kwargs: calls.append(True),
    )
    handler.check_job_status()
    assert calls == []


def test_check_job_status_no_jobname(monkeypatch):
    body = _make_body(status={"phase": "Running"})
    handler = OdooUpgradeJobHandler(body)
    calls = []
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.client.BatchV1Api.read_namespaced_job",
        lambda *args, **kwargs: calls.append(True),
    )
    handler.check_job_status()
    assert calls == []


def test_restore_deployment_scale_default(monkeypatch):
    body = _make_body()
    handler = OdooUpgradeJobHandler(body)
    scale_calls = []
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.client.CustomObjectsApi.get_namespaced_custom_object",
        lambda *args, **kwargs: {"spec": {"replicas": 5}},
    )
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.OdooUpgradeJobHandler._scale_deployment",
        lambda self, name, ns, replicas: scale_calls.append((name, ns, replicas)),
    )
    handler._restore_deployment_scale()
    assert scale_calls == [("demo", "default", 5)]


def test_restore_deployment_scale_missing_instance(monkeypatch, caplog):
    body = _make_body()
    handler = OdooUpgradeJobHandler(body)
    scale_calls = []

    # Instance lookup fails, should default to 1 and still attempt scale
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.client.CustomObjectsApi.get_namespaced_custom_object",
        lambda *args, **kwargs: (_ for _ in ()).throw(ApiException(status=404)),
    )
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.OdooUpgradeJobHandler._scale_deployment",
        lambda self, name, ns, replicas: scale_calls.append((name, ns, replicas)),
    )
    handler._restore_deployment_scale()
    assert scale_calls == [("demo", "default", 1)]


def test_scale_deployment_404(monkeypatch, caplog):
    body = _make_body()
    handler = OdooUpgradeJobHandler(body)

    def fake_patch(*args, **kwargs):
        raise ApiException(status=404)

    monkeypatch.setattr(
        "handlers.upgrade_job_handler.client.AppsV1Api.patch_namespaced_deployment_scale",
        fake_patch,
    )
    with caplog.at_level("WARNING"):
        handler._scale_deployment("demo", "default", 0)
    assert any("not found" in msg for msg in caplog.messages)


def test_update_instance_phase_missing_name(monkeypatch, caplog):
    body = _make_body()
    handler = OdooUpgradeJobHandler(body)
    handler.odoo_instance_ref = {}  # remove name
    with caplog.at_level("WARNING"):
        handler._update_instance_phase("Running")
    assert any("cannot update phase" in msg for msg in caplog.messages)


def test_notify_webhook_secret_token(monkeypatch):
    body = _make_body()
    handler = OdooUpgradeJobHandler(body)
    handler.webhook = {
        "url": "https://example.com/hook",
        "secretTokenSecretRef": {"name": "hooksec", "key": "token"},
    }

    def fake_read_secret(self, name, namespace):
        assert name == "hooksec"
        return SimpleNamespace(data={"token": base64.b64encode(b"sek").decode()})

    posted = {}
    monkeypatch.setattr(
        "handlers.upgrade_job_handler.client.CoreV1Api.read_namespaced_secret",
        fake_read_secret,
    )
    monkeypatch.setattr(
        "requests.post",
        lambda url, json, headers, timeout: posted.update(
            {"url": url, "json": json, "headers": headers}
        )
        or SimpleNamespace(status_code=200),
    )

    handler._notify_webhook("Completed")
    assert posted["headers"].get("Authorization") == "Bearer sek"
    assert posted["json"]["phase"] == "Completed"

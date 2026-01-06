import base64
import os
import sys
from datetime import datetime, timezone
from pathlib import Path
from types import SimpleNamespace

import pytest
from kubernetes.client.rest import ApiException

# Ensure src is importable when running pytest from repo root
ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

from handlers.restore_job_handler import OdooRestoreJobHandler  # noqa: E402


@pytest.fixture(autouse=True)
def db_env(monkeypatch):
    monkeypatch.setenv("DB_HOST", "postgres.example")
    monkeypatch.setenv("DB_PORT", "5432")
    monkeypatch.setenv("DEFAULT_ODOO_IMAGE", "odoo:18.0")


def _make_body(status=None, source=None, fmt="zip", neutralize=True):
    source = source or {
        "type": "s3",
        "s3": {
            "bucket": "b",
            "objectKey": "key",
            "s3CredentialsSecretRef": {"name": "s3-creds", "namespace": "default"},
        },
    }
    return {
        "metadata": {"name": "restore1", "namespace": "default", "uid": "u1"},
        "spec": {
            "odooInstanceRef": {"name": "demo", "namespace": "default"},
            "source": source,
            "format": fmt,
            "neutralize": neutralize,
        },
        "status": status or {},
    }


def _make_instance(image="registry/odoo:custom", pull_secret=None, phase=None):
    spec = {"image": image}
    if pull_secret:
        spec["imagePullSecret"] = pull_secret
    return {
        "metadata": {"name": "demo", "uid": "1234-5678"},
        "spec": spec,
        "status": {"phase": phase} if phase else {},
    }


def test_create_restore_job_s3_env(monkeypatch):
    body = _make_body()
    handler = OdooRestoreJobHandler(body)

    # Fake S3 secret
    access = base64.b64encode(b"AK").decode()
    secret = base64.b64encode(b"SK").decode()
    monkeypatch.setattr(
        "handlers.restore_job_handler.client.CoreV1Api.read_namespaced_secret",
        lambda self, name, namespace: SimpleNamespace(
            data={"accessKey": access, "secretKey": secret}
        ),
    )
    # Stub job creation to return body
    monkeypatch.setattr(
        "handlers.restore_job_handler.client.BatchV1Api.create_namespaced_job",
        lambda self, namespace, body: body,
    )

    job = handler._create_restore_job(_make_instance(pull_secret="pullme"))
    init = job.spec.template.spec.init_containers[0]
    restore = job.spec.template.spec.containers[0]
    init_env = {e.name: e for e in init.env}
    restore_env = {e.name: e for e in restore.env}

    # S3 downloader uses mc image
    assert init.image == "quay.io/minio/mc:latest"
    assert init_env["AWS_ACCESS_KEY_ID"].value == "AK"
    assert init_env["AWS_SECRET_ACCESS_KEY"].value == "SK"
    assert init_env["OUTPUT_FILE"].value.endswith(".zip")

    # Restore container uses instance image and carries DB/env flags
    assert restore.image == "registry/odoo:custom"
    assert restore_env["RESTORE_FORMAT"].value == "zip"
    assert restore_env["NEUTRALIZE"].value == "True"
    assert restore_env["DB_NAME"].value == "odoo_1234_5678"

    pull = job.spec.template.spec.image_pull_secrets
    assert pull[0].name == "pullme"


def test_on_create_missing_instance(monkeypatch):
    body = _make_body()
    handler = OdooRestoreJobHandler(body)

    monkeypatch.setattr(
        "handlers.restore_job_handler.client.CustomObjectsApi.get_namespaced_custom_object",
        lambda *args, **kwargs: (_ for _ in ()).throw(ApiException(status=404)),
    )
    status_calls = []
    monkeypatch.setattr(
        "handlers.restore_job_handler.OdooRestoreJobHandler._update_status",
        lambda self, phase, **kwargs: status_calls.append((phase, kwargs)),
    )
    handler.on_create()
    assert status_calls and status_calls[-1][0] == "Failed"


def test_on_create_busy_instance(monkeypatch):
    body = _make_body()
    handler = OdooRestoreJobHandler(body)
    monkeypatch.setattr(
        "handlers.restore_job_handler.client.CustomObjectsApi.get_namespaced_custom_object",
        lambda *args, **kwargs: _make_instance(phase="Upgrading"),
    )
    status_calls = []
    monkeypatch.setattr(
        "handlers.restore_job_handler.OdooRestoreJobHandler._update_status",
        lambda self, phase, **kwargs: status_calls.append((phase, kwargs)),
    )
    handler.on_create()
    assert status_calls and status_calls[-1][0] == "Failed"


def test_check_job_status_success(monkeypatch):
    body = _make_body(status={"jobName": "job1"})
    handler = OdooRestoreJobHandler(body)
    job_obj = SimpleNamespace(
        status=SimpleNamespace(
            succeeded=1,
            failed=None,
            completion_time=datetime(2025, 1, 2, tzinfo=timezone.utc),
        )
    )
    monkeypatch.setattr(
        "handlers.restore_job_handler.client.BatchV1Api.read_namespaced_job",
        lambda self, name, namespace: job_obj,
    )
    status_calls = []
    notify_calls = []
    monkeypatch.setattr(
        "handlers.restore_job_handler.OdooRestoreJobHandler._update_status",
        lambda self, phase, **kwargs: status_calls.append((phase, kwargs)),
    )
    monkeypatch.setattr(
        "handlers.restore_job_handler.OdooRestoreJobHandler._notify_webhook",
        lambda self, phase: notify_calls.append(phase),
    )
    monkeypatch.setattr(
        "handlers.restore_job_handler.OdooRestoreJobHandler._scale_instance_back_up",
        lambda self: None,
    )
    handler.check_job_status()
    assert status_calls and status_calls[-1][0] == "Completed"
    assert notify_calls == ["Completed"]


def test_check_job_status_failed(monkeypatch):
    body = _make_body(status={"jobName": "job1"})
    handler = OdooRestoreJobHandler(body)
    job_obj = SimpleNamespace(
        status=SimpleNamespace(
            succeeded=None,
            failed=1,
            completion_time=datetime(2025, 1, 2, tzinfo=timezone.utc),
        )
    )
    monkeypatch.setattr(
        "handlers.restore_job_handler.client.BatchV1Api.read_namespaced_job",
        lambda self, name, namespace: job_obj,
    )
    status_calls = []
    notify_calls = []
    monkeypatch.setattr(
        "handlers.restore_job_handler.OdooRestoreJobHandler._update_status",
        lambda self, phase, **kwargs: status_calls.append((phase, kwargs)),
    )
    monkeypatch.setattr(
        "handlers.restore_job_handler.OdooRestoreJobHandler._notify_webhook",
        lambda self, phase: notify_calls.append(phase),
    )
    monkeypatch.setattr(
        "handlers.restore_job_handler.OdooRestoreJobHandler._scale_instance_back_up",
        lambda self: None,
    )
    handler.check_job_status()
    assert status_calls and status_calls[-1][0] == "Failed"
    assert notify_calls == ["Failed"]


def test_check_job_status_skips_terminal(monkeypatch):
    body = _make_body(status={"phase": "Completed", "jobName": "job1"})
    handler = OdooRestoreJobHandler(body)
    calls = []
    monkeypatch.setattr(
        "handlers.restore_job_handler.client.BatchV1Api.read_namespaced_job",
        lambda *args, **kwargs: calls.append(True),
    )
    handler.check_job_status()
    assert calls == []


def test_check_job_status_no_jobname(monkeypatch):
    body = _make_body(status={"phase": "Running"})
    handler = OdooRestoreJobHandler(body)
    calls = []
    monkeypatch.setattr(
        "handlers.restore_job_handler.client.BatchV1Api.read_namespaced_job",
        lambda *args, **kwargs: calls.append(True),
    )
    handler.check_job_status()
    assert calls == []


def test_notify_webhook_secret_token(monkeypatch):
    body = _make_body()
    handler = OdooRestoreJobHandler(body)
    handler.webhook = {
        "url": "https://example.com/hook",
        "secretTokenSecretRef": {"name": "hooksec", "key": "token"},
    }

    def fake_read_secret(self, name, namespace):
        return SimpleNamespace(data={"token": base64.b64encode(b"sek").decode()})

    posted = {}
    monkeypatch.setattr(
        "handlers.restore_job_handler.client.CoreV1Api.read_namespaced_secret",
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


def test_notify_webhook_direct_token(monkeypatch):
    body = _make_body()
    handler = OdooRestoreJobHandler(body)
    handler.webhook = {"url": "https://example.com/hook", "token": "abc"}
    posted = {}
    monkeypatch.setattr(
        "requests.post",
        lambda url, json, headers, timeout: posted.update(
            {"url": url, "json": json, "headers": headers}
        )
        or SimpleNamespace(status_code=200),
    )
    handler._notify_webhook("Completed")
    assert posted["headers"]["Authorization"] == "Bearer abc"
    assert posted["json"]["phase"] == "Completed"


def test_create_restore_job_odoo_source(monkeypatch):
    source = {
        "type": "odoo",
        "odoo": {
            "url": "https://odoo.example",
            "sourceDatabase": "src",
            "masterPassword": "pwd",
        },
    }
    body = _make_body(source=source, fmt="dump")
    handler = OdooRestoreJobHandler(body)

    monkeypatch.setattr(
        "handlers.restore_job_handler.client.BatchV1Api.create_namespaced_job",
        lambda self, namespace, body: body,
    )
    job = handler._create_restore_job(_make_instance())
    init = job.spec.template.spec.init_containers[0]
    # Should use odoo download container path (curl image)
    assert init.name == "download"
    assert "curl" in init.image


def test_restore_job_has_required_volumes(monkeypatch):
    """Verify restore job mounts filestore PVC, odoo-conf ConfigMap, and backup scratch volume."""
    body = _make_body()
    handler = OdooRestoreJobHandler(body)

    # Fake S3 secret
    access = base64.b64encode(b"AK").decode()
    secret = base64.b64encode(b"SK").decode()
    monkeypatch.setattr(
        "handlers.restore_job_handler.client.CoreV1Api.read_namespaced_secret",
        lambda self, name, namespace: SimpleNamespace(
            data={"accessKey": access, "secretKey": secret}
        ),
    )
    monkeypatch.setattr(
        "handlers.restore_job_handler.client.BatchV1Api.create_namespaced_job",
        lambda self, namespace, body: body,
    )

    job = handler._create_restore_job(_make_instance())
    pod_spec = job.spec.template.spec

    # Check volumes
    volume_names = {v.name for v in pod_spec.volumes}
    assert "filestore" in volume_names, "Missing filestore volume"
    assert "odoo-conf" in volume_names, "Missing odoo-conf volume for addons"
    assert "backup" in volume_names, "Missing backup scratch volume"

    # Verify volume sources
    volumes_by_name = {v.name: v for v in pod_spec.volumes}
    assert (
        volumes_by_name["filestore"].persistent_volume_claim.claim_name
        == "demo-filestore-pvc"
    )
    assert volumes_by_name["odoo-conf"].config_map.name == "demo-odoo-conf"
    assert volumes_by_name["backup"].empty_dir is not None

    # Check volume mounts on the restore container
    restore_container = pod_spec.containers[0]
    mount_names = {m.name for m in restore_container.volume_mounts}
    assert "filestore" in mount_names, "Restore container missing filestore mount"
    assert (
        "odoo-conf" in mount_names
    ), "Restore container missing odoo-conf mount for addons"
    assert "backup" in mount_names, "Restore container missing backup mount"

    # Verify mount paths
    mounts_by_name = {m.name: m for m in restore_container.volume_mounts}
    assert mounts_by_name["filestore"].mount_path == "/var/lib/odoo"
    assert mounts_by_name["odoo-conf"].mount_path == "/etc/odoo"
    assert mounts_by_name["backup"].mount_path == "/mnt/backup"

    # Check init container also has the mounts
    init_container = pod_spec.init_containers[0]
    init_mount_names = {m.name for m in init_container.volume_mounts}
    assert "backup" in init_mount_names, "Init container missing backup mount"

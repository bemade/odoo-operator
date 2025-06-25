"""
Restore a database from a running OdooInstance.

This can be executed either by adding a `restore` field
to an OdooInstance spec:

```yaml
spec:
  restore:
    enabled: true
    url: "https://my.odoo.url"
    sourceDatabase: "mydatabase"
    targetDatabase: "mydatabase"
    masterPassword: "my-master-password"
    neutralize: true
    withFilestore: true
```

This requires a running Odoo instance, usually the current
production database if launching a staging.
"""

from __future__ import annotations
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .odoo_handler import OdooHandler

from .job_handler import JobHandler
from kubernetes import client


class RestoreJob(JobHandler):
    def __init__(self, handler: OdooHandler):
        self.restore_spec = handler.spec.get("restore", {})
        self.enabled = self.restore_spec.get("enabled", False)
        self.url = self.restore_spec.get("url")
        self.source_database = self.restore_spec.get("sourceDatabase")
        self.target_database = self.restore_spec.get("targetDatabase")
        self.master_password = self.restore_spec.get("masterPassword")
        self.neutralize = self.restore_spec.get("neutralize", True)
        super().__init__(
            handler=handler,
            status_key="restoreJob",
            status_phase="Restoring",
            completion_patch={"spec": {"restore": {"enabled": False}}},
        )
        self.defaults = handler.defaults

    def _get_resource_body(self):
        """Create the job resource definition."""
        curl_image = "curlimages/curl"

        metadata = client.V1ObjectMeta(
            generate_name=f"{self.name}-restore-",
            namespace=self.namespace,
            owner_references=[self.owner_reference],
        )

        volumes, volume_mounts = self.handler.deployment.get_volumes_and_mounts()
        volumes.append(
            client.V1Volume(name="backup", empty_dir=client.V1EmptyDirVolumeSource())
        )
        volume_mounts.append(
            client.V1VolumeMount(name="backup", mount_path="/mnt/backup")
        )
        odoo_container = (
            self.handler.deployment._get_resource_body().spec.template.spec.containers[
                0
            ]
        )
        odoo_container.command = ["/bin/sh", "-c", self._get_restore_script()]
        odoo_container.volume_mounts = volume_mounts

        job_spec = client.V1JobSpec(
            template=client.V1PodTemplateSpec(
                spec=client.V1PodSpec(
                    restart_policy="Never",
                    volumes=volumes,
                    security_context=client.V1PodSecurityContext(
                        run_as_user=100,
                        run_as_group=101,
                        fs_group=101,
                    ),
                    init_containers=[
                        client.V1Container(
                            name="curl",
                            image=curl_image,
                            command=["/bin/sh", "-c", self._get_curl_script()],
                            volume_mounts=volume_mounts,
                        ),
                    ],
                    containers=[
                        odoo_container,
                    ],
                    affinity=self.spec.get(
                        "affinity", self.defaults.get("affinity", {})
                    ),
                    tolerations=self.spec.get(
                        "tolerations", self.defaults.get("tolerations", [])
                    ),
                ),
            ),
            backoff_limit=0,
            ttl_seconds_after_finished=3600,
        )
        return client.V1Job(
            api_version="batch/v1",
            kind="Job",
            metadata=metadata,
            spec=job_spec,
        )

    def _get_curl_script(self) -> str:
        """Script to download the backup file from the OdooInstance."""
        url = self.url.rstrip("/") + "/web/database/backup"
        db_name = self.source_database
        password = self.master_password
        backup_format = (
            "zip" if self.restore_spec.get("withFilestore", False) else "sql"
        )
        if backup_format == "zip":
            destination = "/mnt/backup/backup.zip"
        else:
            destination = "/mnt/backup/dump.sql"
        script = f"""
        #!/bin/bash
        curl -X POST \
             -F "master_pwd={password}" \
             -F "name={db_name}" \
             -F "backup_format={backup_format}" \
             -o {destination} \
             {url}
        """
        return script

    def _get_restore_script(self) -> str:
        """
        Script to run the odoo db load command, resotring the database from the backup file.
        This is intended to run on an odoo container.
        """
        target_db = self.target_database

        opts = """--db_host "$HOST" --db_port "$PORT" -r "$USER" -w "$PASSWORD" -c /etc/odoo/odoo.conf"""
        script = f"""
        #!/bin/bash

        export PGPASSWORD=$PASSWORD
        if [ -f /mnt/backup/dump.sql ]; then
            # Restore using pg_restore
            createdb -h "$HOST" -p "$PORT" -U "$USER" "{target_db}"
            # pg_restore sometimes "fails" with a warning about recreating sequences
            # but it's actually successful, so we ignore the error and make sure to neutralize
            pg_restore -h "$HOST" -p "$PORT" -U "$USER" -d "{target_db}" --no-owner /mnt/backup/dump.sql || true
        elif [ -f /mnt/backup/backup.zip ]; then
            odoo db {opts} load "{target_db}" /mnt/backup/backup.zip || true
        else
            echo "Backup file not found"
            exit 1
        fi
        """
        if self.neutralize:
            script += f"""
            odoo neutralize {opts} -d {target_db}
            """
        return script

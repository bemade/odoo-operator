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
        # Disable liveness and readiness probes as these jobs can run long
        odoo_container.liveness_probe = None
        odoo_container.readiness_probe = None

        # Add image pull secret if specified
        pull_secret = (
            {
                "image_pull_secrets": [
                    client.V1LocalObjectReference(
                        name=f"{self.spec.get('imagePullSecret')}"
                    )
                ]
            }
            if self.spec.get("imagePullSecret")
            else {}
        )

        job_spec = client.V1JobSpec(
            template=client.V1PodTemplateSpec(
                spec=client.V1PodSpec(
                    **pull_secret,
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
        set -e
        echo "Downloading backup from {url}..."
        curl -X POST \
             -F "master_pwd={password}" \
             -F "name={db_name}" \
             -F "backup_format={backup_format}" \
             -o {destination} \
             -w "HTTP Status: %{{http_code}}\\n" \
             --fail-with-body \
             {url}
        
        echo "Download complete. Checking file..."
        ls -lh {destination}
        
        echo "Init container complete - backup downloaded successfully"
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
        set -x  # Enable command tracing
        
        echo "=== Starting restore process ==="
        echo "Target database: {target_db}"
        echo "Backup directory contents:"
        ls -lh /mnt/backup/
        echo ""

        export PGPASSWORD=$PASSWORD
        if [ -f /mnt/backup/dump.sql ]; then
            echo "Found dump.sql - using pg_restore method"
            # Drop existing database if it exists, then create fresh
            dropdb -h "$HOST" -p "$PORT" -U "$USER" --if-exists "{target_db}"
            createdb -h "$HOST" -p "$PORT" -U "$USER" "{target_db}"
            # pg_restore sometimes "fails" with a warning about recreating sequences
            # but it's actually successful, so we ignore the error and make sure to neutralize
            pg_restore -h "$HOST" -p "$PORT" -U "$USER" -d "{target_db}" --no-owner /mnt/backup/dump.sql || true
            
            echo "Running database parameter re-initialization..."
            # Re-initialize database parameters after pg_restore using direct SQL
            psql -h "$HOST" -p "$PORT" -U "$USER" -d "{target_db}" << 'EOF'
-- Generate new UUIDs and parameters
DO $$
DECLARE
    new_secret TEXT := gen_random_uuid()::text;
    new_uuid TEXT := gen_random_uuid()::text;
BEGIN
    -- Delete existing parameters that need to be refreshed
    DELETE FROM ir_config_parameter WHERE key IN (
        'database.secret',
        'database.uuid', 
        'database.create_date',
        'web.base.url',
        'base.login_cooldown_after',
        'base.login_cooldown_duration'
    );
    
    -- Insert fresh parameters
    INSERT INTO ir_config_parameter (key, value, create_uid, create_date, write_uid, write_date) VALUES
        ('database.secret', new_secret, 1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP),
        ('database.uuid', new_uuid, 1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP),
        ('database.create_date', LOCALTIMESTAMP::text, 1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP),
        ('web.base.url', 'http://localhost:8069', 1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP),
        ('base.login_cooldown_after', '10', 1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP),
        ('base.login_cooldown_duration', '60', 1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP);
        
    RAISE NOTICE 'Database parameters re-initialized successfully';
EXCEPTION
    WHEN OTHERS THEN
        RAISE WARNING 'Could not re-initialize database parameters: %', SQLERRM;
END $$;
EOF
            echo "Database parameter re-initialization complete"
        elif [ -f /mnt/backup/backup.zip ]; then
            echo "Found backup.zip - using odoo db load method"
            echo "File info:"
            file /mnt/backup/backup.zip
            echo "File size:"
            ls -lh /mnt/backup/backup.zip
            echo "First 100 bytes (hex):"
            head -c 100 /mnt/backup/backup.zip | od -A x -t x1z -v
            echo ""
            echo "Running: odoo {opts} db load {target_db} /mnt/backup/backup.zip"
            odoo {opts} db load "{target_db}" /mnt/backup/backup.zip || true
        else
            echo "ERROR: Backup file not found in /mnt/backup/"
            ls -la /mnt/backup/
            exit 1
        fi
        
        echo "=== Restore process complete ==="
        """
        if self.neutralize:
            script += f"""
        echo "=== Starting neutralization ==="
        odoo {opts} neutralize -d {target_db}
        echo "=== Neutralization complete ==="
        """
        return script

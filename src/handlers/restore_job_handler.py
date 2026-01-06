"""
Handler for OdooRestoreJob CRD.

This handler manages the lifecycle of restore jobs:
1. On create: look up the referenced OdooInstance, create a Kubernetes Job to run the restore
2. On update/timer: check Job status and update the OdooRestoreJob status accordingly
3. On completion: optionally call webhook

Supports two source types:
- s3: Download backup from S3/MinIO
- odoo: Download backup from a running Odoo instance
"""

from __future__ import annotations
from typing import Any, Optional
from kubernetes import client
from kubernetes.client.rest import ApiException
import base64
import logging
import os
from typing import cast

from .deployment import get_odoo_volumes_and_mounts

logger = logging.getLogger(__name__)

# Default namespace for S3 credentials secret
OPERATOR_NAMESPACE = os.environ.get("OPERATOR_NAMESPACE", "odoo-operator")


class OdooRestoreJobHandler:
    """Handles OdooRestoreJob CR lifecycle."""

    def __init__(self, body: dict, **kwargs):
        self.body = body
        self.spec = body.get("spec", {})
        self.meta = body.get("metadata", {})
        self.namespace = self.meta.get("namespace")
        self.name = self.meta.get("name")
        self.uid = self.meta.get("uid")
        self.status = body.get("status", {})

        # Extract spec fields
        self.odoo_instance_ref = self.spec.get("odooInstanceRef", {})
        self.source = self.spec.get("source", {})
        self.source_type = self.source.get("type", "s3")
        self.webhook = self.spec.get("webhook", {})
        self.format = self.spec.get("format", "zip")
        self.neutralize = self.spec.get("neutralize", True)

    @property
    def owner_reference(self):
        return client.V1OwnerReference(
            api_version="bemade.org/v1",
            kind="OdooRestoreJob",
            name=self.name,
            uid=self.uid,
            block_owner_deletion=True,
        )

    def _get_s3_credentials(self, s3_config: dict) -> tuple[str, str]:
        """Fetch S3 credentials from the referenced secret.

        Args:
            s3_config: The s3 source configuration dict

        Returns:
            Tuple of (access_key, secret_key)
        """
        secret_ref = s3_config.get("s3CredentialsSecretRef", {})
        secret_name = secret_ref.get("name")
        secret_namespace = secret_ref.get("namespace", OPERATOR_NAMESPACE)

        if not secret_name:
            raise ValueError("s3CredentialsSecretRef.name is required")

        try:
            secret = cast(
                client.V1Secret,
                client.CoreV1Api().read_namespaced_secret(
                    name=secret_name,
                    namespace=secret_namespace,
                ),
            )
            if not secret.data:
                raise ValueError(
                    f"Secret {secret_namespace}/{secret_name} missing data"
                )
            access_key = base64.b64decode(secret.data.get("accessKey", "")).decode(
                "utf-8"
            )
            secret_key = base64.b64decode(secret.data.get("secretKey", "")).decode(
                "utf-8"
            )

            if not access_key or not secret_key:
                raise ValueError(
                    f"Secret {secret_namespace}/{secret_name} missing accessKey or secretKey"
                )

            return access_key, secret_key
        except ApiException as e:
            raise ValueError(
                f"Failed to read S3 credentials secret {secret_namespace}/{secret_name}: {e}"
            )

    def on_create(self):
        """Handle creation of OdooRestoreJob - create the restore Job."""
        logger.info(f"Creating restore job for OdooRestoreJob {self.name}")

        # Skip if already running or completed
        phase = self.status.get("phase")
        if phase in ("Running", "Completed", "Failed"):
            logger.info(f"Restore job {self.name} already in phase {phase}, skipping")
            return

        # Get the referenced OdooInstance
        instance_name = self.odoo_instance_ref.get("name")
        instance_ns = self.odoo_instance_ref.get("namespace", self.namespace)

        try:
            odoo_instance = client.CustomObjectsApi().get_namespaced_custom_object(
                group="bemade.org",
                version="v1",
                namespace=instance_ns,
                plural="odooinstances",
                name=instance_name,
            )
        except ApiException as e:
            if e.status == 404:
                self._update_status(
                    "Failed", message=f"OdooInstance {instance_name} not found"
                )
                return
            raise

        # Create the restore Job
        if not isinstance(odoo_instance, dict):
            self._update_status("Failed", message="Invalid OdooInstance response")
            return

        # Check if instance is already being upgraded or restored
        instance_phase = odoo_instance.get("status", {}).get("phase")  # pyright: ignore
        if instance_phase in ("Upgrading", "Restoring"):
            self._update_status(
                "Failed",
                message=f"OdooInstance {instance_name} is already {instance_phase}",
            )
            return

        # Scale down the deployment before restore
        self._scale_deployment(instance_name, instance_ns, 0)

        job = self._create_restore_job(odoo_instance)
        if job is None or job.metadata is None:
            self._update_status("Failed", message="Failed to create restore job")
            return

        job_name = job.metadata.name or ""
        start_time: Optional[str] = None
        if job.metadata.creation_timestamp is not None:
            start_time = job.metadata.creation_timestamp.isoformat()

        logger.info(f"Created restore job {job_name} for {self.name}")

        # Update status to Running
        self._update_status(
            "Running",
            job_name=job_name,
            start_time=start_time,
        )

    def on_update(self):
        """Handle update of OdooRestoreJob."""
        self.check_job_status()

    def check_job_status(self):
        """Check the underlying Job status and update OdooRestoreJob accordingly."""
        # Skip if already in a terminal state
        current_phase = self.status.get("phase")
        if current_phase in ("Completed", "Failed"):
            return

        job_name = self.status.get("jobName")
        if not job_name:
            return

        try:
            job = client.BatchV1Api().read_namespaced_job(
                name=job_name, namespace=self.namespace
            )
        except ApiException as e:
            if e.status == 404:
                logger.warning(f"Job {job_name} not found for restore {self.name}")
                return
            raise

        job_status = job.status
        if job_status is None:
            return

        if job_status.succeeded:
            completion_time: Optional[str] = None
            if job_status.completion_time is not None:
                completion_time = job_status.completion_time.isoformat()
            self._update_status("Completed", completion_time=completion_time)
            self._scale_instance_back_up()
            self._notify_webhook("Completed")
        elif job_status.failed:
            completion_time = None
            if job_status.completion_time is not None:
                completion_time = job_status.completion_time.isoformat()
            self._update_status(
                "Failed",
                completion_time=completion_time,
                message="Restore job failed",
            )
            self._scale_instance_back_up()
            self._notify_webhook("Failed")

    def _create_restore_job(self, odoo_instance: dict[str, Any]) -> client.V1Job:
        """Create a Kubernetes Job to perform the restore."""
        instance_spec = odoo_instance.get("spec", {})
        instance_meta = odoo_instance.get("metadata", {})
        instance_name = instance_meta.get("name")
        instance_uid = instance_meta.get("uid", "")

        # Use the same image as the OdooInstance
        image = instance_spec.get(
            "image", os.environ.get("DEFAULT_ODOO_IMAGE", "odoo:18.0")
        )

        # Get image pull secret if specified
        image_pull_secrets = None
        if instance_spec.get("imagePullSecret"):
            image_pull_secrets = [
                client.V1LocalObjectReference(name=instance_spec["imagePullSecret"])
            ]

        # Get database credentials from the instance's secret
        db_secret_name = f"{instance_name}-odoo-user"
        db_name = f"odoo_{instance_uid.replace('-', '_')}"

        # Build the job
        metadata = client.V1ObjectMeta(
            generate_name=f"{self.name}-",
            namespace=self.namespace,
            owner_references=[self.owner_reference],
        )

        # Get standard volumes and mounts (includes filestore and odoo-conf for addons)
        volumes, volume_mounts = get_odoo_volumes_and_mounts(instance_name)

        # Add backup scratch volume for restore operations
        volumes.append(
            client.V1Volume(name="backup", empty_dir=client.V1EmptyDirVolumeSource())
        )
        volume_mounts.append(
            client.V1VolumeMount(name="backup", mount_path="/mnt/backup")
        )

        # Build init container based on source type
        if self.source_type == "s3":
            init_container = self._build_s3_download_container(volume_mounts)
        else:
            init_container = self._build_odoo_download_container(volume_mounts)

        # Environment variables for database connection
        db_host = os.environ.get("DB_HOST", "postgres")
        db_port = os.environ.get("DB_PORT", "5432")

        db_env = [
            client.V1EnvVar(name="HOST", value=db_host),
            client.V1EnvVar(name="PORT", value=db_port),
            client.V1EnvVar(
                name="USER",
                value_from=client.V1EnvVarSource(
                    secret_key_ref=client.V1SecretKeySelector(
                        name=db_secret_name, key="username"
                    )
                ),
            ),
            client.V1EnvVar(
                name="PASSWORD",
                value_from=client.V1EnvVarSource(
                    secret_key_ref=client.V1SecretKeySelector(
                        name=db_secret_name, key="password"
                    )
                ),
            ),
            client.V1EnvVar(name="DB_NAME", value=db_name),
            client.V1EnvVar(name="RESTORE_FORMAT", value=self.format),
            client.V1EnvVar(name="NEUTRALIZE", value=str(self.neutralize)),
        ]

        # Main restore container
        restore_container = client.V1Container(
            name="restore",
            image=image,
            command=["/bin/sh", "-c", self._get_restore_script(db_name)],
            env=db_env,
            volume_mounts=volume_mounts,
        )

        job_spec = client.V1JobSpec(
            template=client.V1PodTemplateSpec(
                spec=client.V1PodSpec(
                    restart_policy="Never",
                    image_pull_secrets=image_pull_secrets,
                    security_context=client.V1PodSecurityContext(
                        run_as_user=100,
                        run_as_group=101,
                        fs_group=101,
                    ),
                    volumes=volumes,
                    init_containers=[init_container],
                    containers=[restore_container],
                ),
            ),
            backoff_limit=0,
            ttl_seconds_after_finished=900,  # Clean up 15 min after completion
        )

        job = client.V1Job(
            api_version="batch/v1",
            kind="Job",
            metadata=metadata,
            spec=job_spec,
        )

        return client.BatchV1Api().create_namespaced_job(
            namespace=self.namespace, body=job
        )

    def _build_s3_download_container(self, volume_mounts: list) -> client.V1Container:
        """Build init container to download backup from S3/MinIO."""
        s3_config = self.source.get("s3", {})
        bucket = s3_config.get("bucket", "")
        object_key = s3_config.get("objectKey", "")
        endpoint = s3_config.get("endpoint", "")
        insecure = s3_config.get("insecure", False)

        # Determine output filename based on format
        if self.format == "zip":
            output_file = "/mnt/backup/backup.zip"
        elif self.format == "dump":
            output_file = "/mnt/backup/dump.dump"
        else:
            output_file = "/mnt/backup/dump.sql"

        # Fetch S3 credentials from centralized secret
        access_key, secret_key = self._get_s3_credentials(s3_config)

        env = [
            client.V1EnvVar(name="S3_BUCKET", value=bucket),
            client.V1EnvVar(name="S3_KEY", value=object_key),
            client.V1EnvVar(name="S3_ENDPOINT", value=endpoint),
            client.V1EnvVar(name="S3_INSECURE", value="true" if insecure else "false"),
            client.V1EnvVar(name="OUTPUT_FILE", value=output_file),
            client.V1EnvVar(name="MC_CONFIG_DIR", value="/tmp/.mc"),
            client.V1EnvVar(name="AWS_ACCESS_KEY_ID", value=access_key),
            client.V1EnvVar(name="AWS_SECRET_ACCESS_KEY", value=secret_key),
        ]

        script = """
set -ex
echo "=== Downloading backup from S3 ==="
echo "Endpoint: $S3_ENDPOINT"
echo "Bucket: $S3_BUCKET"
echo "Key: $S3_KEY"

MC_CONFIG_DIR="${MC_CONFIG_DIR:-/tmp/.mc}"
mkdir -p "$MC_CONFIG_DIR"

MC_ALIAS_ARGS=""
if [ "${S3_INSECURE}" = "true" ]; then
    MC_ALIAS_ARGS="--insecure"
fi

mc $MC_ALIAS_ARGS alias set source "$S3_ENDPOINT" "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY"
mc $MC_ALIAS_ARGS cp "source/$S3_BUCKET/$S3_KEY" "$OUTPUT_FILE"

echo "Download complete:"
ls -lh "$OUTPUT_FILE"
echo "=== S3 download complete ==="
"""

        return client.V1Container(
            name="download",
            image="quay.io/minio/mc:latest",
            command=["/bin/sh", "-c", script],
            env=env,
            volume_mounts=volume_mounts,
        )

    def _build_odoo_download_container(self, volume_mounts: list) -> client.V1Container:
        """Build init container to download backup from a running Odoo instance."""
        odoo_config = self.source.get("odoo", {})
        url = odoo_config.get("url", "").rstrip("/")
        source_db = odoo_config.get("sourceDatabase", "")
        master_password = odoo_config.get("masterPassword", "")

        # Determine output filename based on format
        if self.format == "zip":
            output_file = "/mnt/backup/backup.zip"
            backup_format = "zip"
        else:
            output_file = "/mnt/backup/dump.sql"
            backup_format = "dump"

        script = f"""
set -ex
echo "=== Downloading backup from Odoo instance ==="
echo "URL: {url}"
echo "Database: {source_db}"

curl -X POST \\
     -F "master_pwd={master_password}" \\
     -F "name={source_db}" \\
     -F "backup_format={backup_format}" \\
     -o {output_file} \\
     -w "HTTP Status: %{{http_code}}\\n" \\
     --fail-with-body \\
     {url}/web/database/backup

echo "Download complete:"
ls -lh {output_file}

# Validate the downloaded file
echo "Validating downloaded file..."
FILE_SIZE=$(stat -c%s {output_file} 2>/dev/null || stat -f%z {output_file})
echo "File size: $FILE_SIZE bytes"

# Check minimum file size (100KB) - a real backup should be larger
if [ "$FILE_SIZE" -lt 102400 ]; then
    echo "ERROR: Downloaded file is too small ($FILE_SIZE bytes) - likely an error response!"
    echo "File contents:"
    cat {output_file}
    exit 1
fi

if [ "{backup_format}" = "zip" ]; then
    # Check if it starts with PK (zip magic bytes)
    MAGIC=$(head -c 2 {output_file} | od -An -tx1 | tr -d ' ')
    if [ "$MAGIC" != "504b" ]; then
        echo "ERROR: Downloaded file is not a valid zip file (magic: $MAGIC)!"
        echo "File contents (first 500 bytes):"
        head -c 500 {output_file}
        exit 1
    fi
    echo "Valid zip file confirmed (PK header found)"
else
    # For dump format, check it starts with SQL or pg_dump header
    HEADER=$(head -c 5 {output_file})
    if echo "$HEADER" | grep -qE '^(--|PGDMP|CREAT|SET )'; then
        echo "Valid SQL dump confirmed"
    else
        echo "ERROR: Downloaded file does not appear to be a valid SQL dump!"
        echo "File contents (first 500 bytes):"
        head -c 500 {output_file}
        exit 1
    fi
fi

echo "=== Odoo download complete ==="
"""

        return client.V1Container(
            name="download",
            image="curlimages/curl:latest",
            command=["/bin/sh", "-c", script],
            volume_mounts=volume_mounts,
        )

    def _get_restore_script(self, db_name: str) -> str:
        """Generate the restore script for the main container."""
        neutralize_flag = "-n" if self.neutralize else ""

        script = f"""
set -x
echo "=== Starting restore process ==="
echo "Target database: {db_name}"
echo "Format: $RESTORE_FORMAT"
echo "Neutralize: $NEUTRALIZE"

echo "Backup directory contents:"
ls -lh /mnt/backup/

export PGPASSWORD=$PASSWORD

# Function to re-initialize database parameters after restore
# This resets secrets/UUIDs to prevent session hijacking and ensure unique identity
reinit_db_params() {{
    echo "=== Re-initializing database parameters ==="
    psql -h "$HOST" -p "$PORT" -U "$USER" -d "{db_name}" << 'EOSQL'
DO $$
DECLARE
    new_secret TEXT := gen_random_uuid()::text;
    new_uuid TEXT := gen_random_uuid()::text;
BEGIN
    DELETE FROM ir_config_parameter WHERE key IN (
        'database.secret',
        'database.uuid', 
        'database.create_date',
        'web.base.url',
        'base.login_cooldown_after',
        'base.login_cooldown_duration'
    );
    
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
EOSQL
    echo "Database parameter re-initialization complete"
}}

# Function to run full Odoo neutralization using the CLI
# This disables mail servers, crons, payment providers, webhooks, etc.
run_odoo_neutralize() {{
    echo "=== Running Odoo neutralization ==="
    
    # Run odoo neutralize command - this is the official way to neutralize
    # It handles all module-specific neutralization scripts
    odoo neutralize --db_host "$HOST" --db_port "$PORT" --db_user "$USER" --db_password "$PASSWORD" -d "{db_name}"
    
    echo "Odoo neutralize command completed"
}}

# Function to verify neutralization succeeded
# CRITICAL: If neutralization was requested but failed, we must fail the job
# AND DROP THE DATABASE to prevent un-neutralized databases from running
# (which could steal emails, trigger bank syncs, send erroneous emails, etc.)
verify_neutralization() {{
    echo "=== Verifying neutralization ==="
    
    # Check if database.is_neutralized flag is set to 'true'
    NEUTRALIZED=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "{db_name}" -t -A -c \
        "SELECT value FROM ir_config_parameter WHERE key = 'database.is_neutralized';" 2>/dev/null || echo "")
    
    if [ "$NEUTRALIZED" = "true" ] || [ "$NEUTRALIZED" = "True" ]; then
        echo "✓ Neutralization verified: database.is_neutralized = $NEUTRALIZED"
        
        # Additional verification: check mail servers are disabled
        ACTIVE_MAIL_SERVERS=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "{db_name}" -t -A -c \
            "SELECT COUNT(*) FROM ir_mail_server WHERE active = true AND name != 'neutralization - disable emails';" 2>/dev/null || echo "0")
        echo "Active mail servers (excluding neutralization dummy): $ACTIVE_MAIL_SERVERS"
        
        # Check active crons (should only be autovacuum after neutralization)
        ACTIVE_CRONS=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "{db_name}" -t -A -c \
            "SELECT COUNT(*) FROM ir_cron WHERE active = true;" 2>/dev/null || echo "0")
        echo "Active crons: $ACTIVE_CRONS"
        
        return 0
    else
        echo "✗ CRITICAL ERROR: Neutralization verification FAILED!"
        echo "  database.is_neutralized = '$NEUTRALIZED' (expected 'true')"
        echo ""
        echo "This database may have active mail servers, crons, payment providers,"
        echo "and other integrations that could cause serious issues in production."
        echo ""
        echo "DROPPING DATABASE to prevent un-neutralized database from being used!"
        dropdb -h "$HOST" -p "$PORT" -U "$USER" --if-exists "{db_name}"
        echo "Database {db_name} has been dropped."
        return 1
    fi
}}

# Drop existing database and filestore using odoo CLI
echo "Dropping existing database {db_name}..."
odoo db --db_host "$HOST" --db_port "$PORT" --db_user "$USER" --db_password "$PASSWORD" drop "{db_name}" || true

if [ -f /mnt/backup/dump.dump ]; then
    echo "Found dump.dump - using pg_restore method"
    
    # Create fresh database
    createdb -h "$HOST" -p "$PORT" -U "$USER" "{db_name}"
    
    # Restore using pg_restore for custom format dumps
    # NOTE: || true because restore tools often exit non-zero for warnings (e.g., sequences,
    # ownership) even when the restore succeeds. verify_neutralization will catch real failures.
    pg_restore -h "$HOST" -p "$PORT" -U "$USER" -d "{db_name}" --no-owner /mnt/backup/dump.dump || true
    
elif [ -f /mnt/backup/dump.sql ]; then
    echo "Found dump.sql - using psql method"
    
    # Create fresh database
    createdb -h "$HOST" -p "$PORT" -U "$USER" "{db_name}"
    
    # Restore using psql for plain SQL dumps
    # NOTE: || true - see pg_restore comment above
    psql -h "$HOST" -p "$PORT" -U "$USER" -d "{db_name}" -f /mnt/backup/dump.sql || true
    
elif [ -f /mnt/backup/backup.zip ]; then
    echo "Found backup.zip - using odoo db load method"
    echo "File info:"
    file /mnt/backup/backup.zip || true
    ls -lh /mnt/backup/backup.zip
    
    echo "Running: odoo db load {neutralize_flag} -f ..."
    # NOTE: || true - see pg_restore comment above
    odoo db --db_host "$HOST" --db_port "$PORT" --db_user "$USER" --db_password "$PASSWORD" load {neutralize_flag} -f "{db_name}" /mnt/backup/backup.zip || true
else
    echo "ERROR: Backup file not found in /mnt/backup/"
    ls -la /mnt/backup/
    exit 1
fi

# Neutralization (runs after any restore type)
if [ "$NEUTRALIZE" = "True" ]; then
    reinit_db_params
    run_odoo_neutralize
    verify_neutralization || exit 1
else
    echo "Skipping neutralization (neutralize=False)"
fi

echo "=== Restore process complete ==="
"""
        return script

    def _update_status(
        self,
        phase: str,
        job_name: Optional[str] = None,
        start_time: Optional[str] = None,
        completion_time: Optional[str] = None,
        message: Optional[str] = None,
    ) -> None:
        """Update the OdooRestoreJob status."""
        status = {"phase": phase}
        if job_name is not None:
            status["jobName"] = job_name
        if start_time:
            status["startTime"] = start_time
        if completion_time:
            status["completionTime"] = completion_time
        if message:
            status["message"] = message

        try:
            client.CustomObjectsApi().patch_namespaced_custom_object_status(
                group="bemade.org",
                version="v1",
                namespace=self.namespace,
                plural="odoorestorejobs",
                name=self.name,
                body={"status": status},
            )
        except ApiException as e:
            logger.error(f"Failed to update status for {self.name}: {e}")

    def _notify_webhook(self, phase: str):
        """Send webhook notification if configured."""
        url = self.webhook.get("url")
        if not url:
            return

        import requests
        import base64

        headers = {"Content-Type": "application/json"}

        # Get token - prefer direct token, fall back to secret reference
        token = self.webhook.get("token")
        if not token:
            secret_ref = self.webhook.get("secretTokenSecretRef")
            if secret_ref:
                try:
                    secret = client.CoreV1Api().read_namespaced_secret(
                        name=secret_ref["name"], namespace=self.namespace
                    )
                    # V1Secret has a .data attribute but type stubs are incomplete
                    if (
                        secret is not None
                        and hasattr(secret, "data")
                        and secret.data is not None
                    ):
                        secret_key = secret_ref.get("key", "token")
                        token_b64 = (
                            secret.data.get(secret_key)
                            if isinstance(secret.data, dict)
                            else None
                        )
                        if token_b64 and isinstance(token_b64, str):
                            token = base64.b64decode(token_b64).decode("utf-8")
                except Exception as e:
                    logger.warning(f"Failed to read webhook secret: {e}")

        if token:
            headers["Authorization"] = f"Bearer {token}"

        payload = {
            "restoreJob": self.name,
            "namespace": self.namespace,
            "phase": phase,
            "targetInstance": self.odoo_instance_ref.get("name"),
        }

        try:
            resp = requests.post(url, json=payload, headers=headers, timeout=10)
            logger.info(f"Webhook notification sent: {resp.status_code}")
        except Exception as e:
            logger.warning(f"Webhook notification failed: {e}")

    def _scale_deployment(self, instance_name: str, instance_ns: str, replicas: int):
        """Scale the OdooInstance deployment to the specified number of replicas."""
        try:
            apps_api = client.AppsV1Api()
            apps_api.patch_namespaced_deployment_scale(
                name=instance_name,
                namespace=instance_ns,
                body={"spec": {"replicas": replicas}},
            )
            logger.info(f"Scaled deployment {instance_name} to {replicas} replicas")
        except ApiException as e:
            if e.status == 404:
                logger.warning(f"Deployment {instance_name} not found, cannot scale")
            else:
                logger.error(f"Failed to scale deployment {instance_name}: {e}")

    def _scale_instance_back_up(self):
        """Scale the OdooInstance deployment back up after restore completes."""
        instance_name = self.odoo_instance_ref.get("name")
        instance_ns = self.odoo_instance_ref.get("namespace", self.namespace)

        if not instance_name:
            logger.warning("No instance name found, cannot scale up")
            return

        # Get the desired replicas from the OdooInstance spec
        try:
            odoo_instance = client.CustomObjectsApi().get_namespaced_custom_object(
                group="bemade.org",
                version="v1",
                namespace=instance_ns,
                plural="odooinstances",
                name=instance_name,
            )
            replicas = odoo_instance.get("spec", {}).get("replicas", 1)
        except ApiException as e:
            logger.warning(
                f"Could not get OdooInstance {instance_name}, defaulting to 1 replica: {e}"
            )
            replicas = 1

        self._scale_deployment(instance_name, instance_ns, replicas)

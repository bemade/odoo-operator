"""
Handler for OdooBackupJob CRD.

This handler manages the lifecycle of backup jobs:
1. On create: look up the referenced OdooInstance, create a Kubernetes Job to run the backup
2. On update/timer: check Job status and update the OdooBackupJob status accordingly
3. On completion: optionally call webhook
"""

from __future__ import annotations
from kubernetes import client
from kubernetes.client.rest import ApiException
import base64
import logging
import os

from .deployment import get_odoo_volumes_and_mounts

logger = logging.getLogger(__name__)

# Default namespace for S3 credentials secret
OPERATOR_NAMESPACE = os.environ.get("OPERATOR_NAMESPACE", "odoo-operator")


class OdooBackupJobHandler:
    """Handles OdooBackupJob CR lifecycle."""

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
        self.destination = self.spec.get("destination", {})
        self.webhook = self.spec.get("webhook", {})
        self.format = self.spec.get("format", "zip")
        self.with_filestore = self.spec.get("withFilestore", True)

    @property
    def owner_reference(self):
        return client.V1OwnerReference(
            api_version="bemade.org/v1",
            kind="OdooBackupJob",
            name=self.name,
            uid=self.uid,
            block_owner_deletion=True,
        )

    def _get_s3_credentials(self) -> tuple[str, str]:
        """Fetch S3 credentials from the referenced secret.

        Returns:
            Tuple of (access_key, secret_key)
        """
        secret_ref = self.destination.get("s3CredentialsSecretRef", {})
        secret_name = secret_ref.get("name")
        secret_namespace = secret_ref.get("namespace", OPERATOR_NAMESPACE)

        if not secret_name:
            raise ValueError("s3CredentialsSecretRef.name is required")

        try:
            secret = client.CoreV1Api().read_namespaced_secret(
                name=secret_name,
                namespace=secret_namespace,
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
        """Handle creation of OdooBackupJob - create the backup Job."""
        logger.info(f"Creating backup job for OdooBackupJob {self.name}")

        # Skip if already running or completed
        phase = self.status.get("phase")
        if phase in ("Running", "Completed", "Failed"):
            logger.info(f"Backup job {self.name} already in phase {phase}, skipping")
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

        # Create the backup Job
        job = self._create_backup_job(odoo_instance)
        logger.info(f"Created backup job {job.metadata.name} for {self.name}")

        # Update status to Running
        self._update_status(
            "Running",
            job_name=job.metadata.name,
            start_time=(
                job.metadata.creation_timestamp.isoformat()
                if job.metadata.creation_timestamp
                else None
            ),
        )

    def on_update(self):
        """Handle update of OdooBackupJob."""
        self.check_job_status()

    def check_job_status(self):
        """Check the underlying Job status and update OdooBackupJob accordingly."""
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
                logger.warning(f"Job {job_name} not found for backup {self.name}")
                return
            raise

        job_status = job.status
        if job_status.succeeded:
            self._update_status(
                "Completed",
                completion_time=(
                    job_status.completion_time.isoformat()
                    if job_status.completion_time
                    else None
                ),
                job_name=None,
            )
            self._notify_webhook("Completed")
        elif job_status.failed:
            self._update_status(
                "Failed",
                completion_time=(
                    job_status.completion_time.isoformat()
                    if job_status.completion_time
                    else None
                ),
                message="Backup job failed",
                job_name=None,
            )
            self._notify_webhook("Failed")

    def _create_backup_job(self, odoo_instance: dict) -> client.V1Job:
        """Create a Kubernetes Job to perform the backup."""
        instance_spec = odoo_instance.get("spec", {})
        instance_meta = odoo_instance.get("metadata", {})
        instance_uid = instance_meta.get("uid", "")

        # Use the same image as the OdooInstance
        image = instance_spec.get(
            "image", os.environ.get("DEFAULT_ODOO_IMAGE", "odoo:18.0")
        )

        # Reuse imagePullSecret from the OdooInstance spec if defined
        image_pull_secrets = None
        image_pull_secret_name = instance_spec.get("imagePullSecret")
        if image_pull_secret_name:
            image_pull_secrets = [
                client.V1LocalObjectReference(name=str(image_pull_secret_name))
            ]

        # Database name follows the operator convention
        db_name = f"odoo_{instance_uid.replace('-', '_')}"

        object_key = self.destination.get("objectKey", "")
        local_filename = (
            os.path.basename(object_key)
            if object_key
            else f"{instance_meta.get('name', 'odoo')}-backup"
        )

        # Build environment variables
        db_host = os.environ.get("DB_HOST", "postgres")
        db_port = os.environ.get("DB_PORT", "5432")

        common_env = [
            client.V1EnvVar(name="HOST", value=db_host),
            client.V1EnvVar(name="PORT", value=db_port),
            client.V1EnvVar(
                name="USER",
                value_from=client.V1EnvVarSource(
                    secret_key_ref=client.V1SecretKeySelector(
                        name=f"{instance_meta.get('name')}-odoo-user",
                        key="username",
                    )
                ),
            ),
            client.V1EnvVar(
                name="PASSWORD",
                value_from=client.V1EnvVarSource(
                    secret_key_ref=client.V1SecretKeySelector(
                        name=f"{instance_meta.get('name')}-odoo-user",
                        key="password",
                    )
                ),
            ),
            client.V1EnvVar(name="DB_NAME", value=db_name),
            client.V1EnvVar(name="BACKUP_FORMAT", value=self.format),
            client.V1EnvVar(
                name="BACKUP_WITH_FILESTORE", value=str(self.with_filestore)
            ),
            client.V1EnvVar(name="LOCAL_FILENAME", value=local_filename),
        ]

        # Fetch S3 credentials from centralized secret and inject directly
        if self.destination.get("s3CredentialsSecretRef"):
            access_key, secret_key = self._get_s3_credentials()
            common_env.extend(
                [
                    client.V1EnvVar(name="AWS_ACCESS_KEY_ID", value=access_key),
                    client.V1EnvVar(name="AWS_SECRET_ACCESS_KEY", value=secret_key),
                ]
            )

        # Get standard volumes and mounts (includes filestore and odoo-conf for addons)
        instance_name = instance_meta.get("name")
        volumes, volume_mounts = get_odoo_volumes_and_mounts(instance_name)

        # Add backup scratch volume for backup operations
        volumes.append(
            client.V1Volume(name="backup", empty_dir=client.V1EmptyDirVolumeSource())
        )
        volume_mounts.append(
            client.V1VolumeMount(name="backup", mount_path="/mnt/backup")
        )

        # Build the backup script
        script = self._backup_script(db_name)

        backup_container = client.V1Container(
            name="backup",
            image=image,
            command=["/bin/sh", "-c", script],
            env=common_env,
            volume_mounts=volume_mounts,
        )

        upload_script = self._upload_script()
        upload_env = [
            client.V1EnvVar(name="S3_BUCKET", value=self.destination.get("bucket", "")),
            client.V1EnvVar(name="S3_KEY", value=self.destination.get("objectKey", "")),
            client.V1EnvVar(
                name="S3_ENDPOINT", value=self.destination.get("endpoint", "")
            ),
            client.V1EnvVar(name="LOCAL_FILENAME", value=local_filename),
            client.V1EnvVar(name="MC_CONFIG_DIR", value="/tmp/.mc"),
            client.V1EnvVar(
                name="S3_INSECURE",
                value="true" if self.destination.get("insecure") else "false",
            ),
        ]
        # Add S3 credentials (already fetched above)
        if self.destination.get("s3CredentialsSecretRef"):
            access_key, secret_key = self._get_s3_credentials()
            upload_env.extend(
                [
                    client.V1EnvVar(name="AWS_ACCESS_KEY_ID", value=access_key),
                    client.V1EnvVar(name="AWS_SECRET_ACCESS_KEY", value=secret_key),
                ]
            )

        upload_container = client.V1Container(
            name="uploader",
            image=os.environ.get("BACKUP_UPLOAD_IMAGE", "quay.io/minio/mc:latest"),
            command=["/bin/sh", "-c", upload_script],
            env=upload_env,
            volume_mounts=[
                client.V1VolumeMount(name="backup", mount_path="/mnt/backup")
            ],
        )

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
                    image_pull_secrets=image_pull_secrets,
                    init_containers=[backup_container],
                    containers=[upload_container],
                ),
            ),
            backoff_limit=0,
            ttl_seconds_after_finished=900,  # Clean up 15 min after completion
        )

        job = client.V1Job(
            api_version="batch/v1",
            kind="Job",
            metadata=client.V1ObjectMeta(
                generate_name=f"{self.name}-",
                namespace=self.namespace,
                owner_references=[self.owner_reference],
            ),
            spec=job_spec,
        )

        return client.BatchV1Api().create_namespaced_job(
            namespace=self.namespace, body=job
        )

    def _backup_script(self, db_name: str) -> str:
        """Generate the shell script to perform backup artifact creation."""
        return f"""
set -ex
echo "=== Starting backup for $DB_NAME ==="
export PGPASSWORD=$PASSWORD

FILENAME="{db_name}-$(date +%Y%m%d-%H%M%S)"

TARGET_NAME="${{LOCAL_FILENAME:-$FILENAME}}"
OUTPUT_NAME="$TARGET_NAME"

if [ "$BACKUP_FORMAT" = "zip" ] && [ "$BACKUP_WITH_FILESTORE" = "True" ]; then
    echo "Creating zip backup with filestore using odoo db dump..."
    case "$OUTPUT_NAME" in
        *.zip) : ;;
        *) OUTPUT_NAME="$OUTPUT_NAME.zip" ;;
    esac
    # odoo connection options must come before the 'db dump' subcommand
    odoo db \
        --db_host "$HOST" \
        --db_port "$PORT" \
        --db_user "$USER" \
        --db_password "$PASSWORD" \
        dump \
        "$DB_NAME" \
        "/mnt/backup/$OUTPUT_NAME"
elif [ "$BACKUP_FORMAT" = "dump" ]; then
    echo "Creating PostgreSQL custom format dump with pg_dump..."
    case "$OUTPUT_NAME" in
        *.dump) : ;;
        *) OUTPUT_NAME="$OUTPUT_NAME.dump" ;;
    esac
    pg_dump -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" --format=custom -f /mnt/backup/$OUTPUT_NAME
else
    echo "Creating SQL dump with pg_dump (no filestore)..."
    case "$OUTPUT_NAME" in
        *.sql) : ;;
        *) OUTPUT_NAME="$OUTPUT_NAME.sql" ;;
    esac
    pg_dump -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" > /mnt/backup/$OUTPUT_NAME
fi

echo "$OUTPUT_NAME" > /mnt/backup/.artifact-name

echo "Backup created: /mnt/backup/$OUTPUT_NAME"
ls -lh /mnt/backup/$OUTPUT_NAME

echo "=== Backup complete ==="
"""

    def _upload_script(self) -> str:
        """Generate script executed by the uploader container."""
        return """
set -ex

MC_CONFIG_DIR="${MC_CONFIG_DIR:-/tmp/.mc}"
mkdir -p "$MC_CONFIG_DIR"

if [ -f /mnt/backup/.artifact-name ]; then
    LOCAL_FILENAME="$(cat /mnt/backup/.artifact-name)"
fi

FILE_PATH="/mnt/backup/${LOCAL_FILENAME}"
DEST_KEY="${S3_KEY:-$LOCAL_FILENAME}"

if [ ! -f "$FILE_PATH" ]; then
    echo "Backup artifact $FILE_PATH not found" >&2
    exit 1
fi

if [ -z "$S3_BUCKET" ] || [ -z "$S3_ENDPOINT" ]; then
    echo "S3 bucket or endpoint missing" >&2
    exit 1
fi

if [ -z "$AWS_ACCESS_KEY_ID" ] || [ -z "$AWS_SECRET_ACCESS_KEY" ]; then
    echo "AWS credentials missing" >&2
    exit 1
fi

MC_ALIAS_ARGS=""
if [ "${S3_INSECURE}" = "true" ]; then
    MC_ALIAS_ARGS="--insecure"
fi

mc $MC_ALIAS_ARGS alias set dest "$S3_ENDPOINT" "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY"
mc $MC_ALIAS_ARGS cp "$FILE_PATH" "dest/$S3_BUCKET/$DEST_KEY"
echo "Upload complete"
"""

    def _update_status(self, phase: str, **kwargs):
        """Update the OdooBackupJob status."""
        status_body = {"phase": phase}
        for key, value in kwargs.items():
            if value is not None:
                # Convert snake_case to camelCase for K8s
                camel_key = "".join(
                    word.capitalize() if i > 0 else word
                    for i, word in enumerate(key.split("_"))
                )
                status_body[camel_key] = value

        client.CustomObjectsApi().patch_namespaced_custom_object_status(
            group="bemade.org",
            version="v1",
            namespace=self.namespace,
            plural="odoobackupjobs",
            name=self.name,
            body={"status": status_body},
        )

    def _notify_webhook(self, phase: str):
        """Send webhook notification if configured."""
        url = self.webhook.get("url")
        if not url:
            return

        import requests
        import base64

        headers = {"Content-Type": "application/json"}

        # Get token - either directly from spec or from secret reference
        token = self.webhook.get("token")
        if not token:
            # Fall back to secret reference
            secret_ref = self.webhook.get("secretTokenSecretRef")
            if secret_ref:
                try:
                    secret = client.CoreV1Api().read_namespaced_secret(
                        name=secret_ref["name"], namespace=self.namespace
                    )
                    token_b64 = secret.data.get(secret_ref["key"])
                    if token_b64:
                        token = base64.b64decode(token_b64).decode("utf-8")
                except Exception as e:
                    logger.warning(f"Failed to read webhook secret: {e}")

        if token:
            headers["Authorization"] = f"Bearer {token}"

        payload = {
            "backupJob": self.name,
            "namespace": self.namespace,
            "phase": phase,
            "objectKey": self.destination.get("objectKey"),
            "bucket": self.destination.get("bucket"),
        }

        try:
            resp = requests.post(url, json=payload, headers=headers, timeout=10)
            logger.info(f"Webhook notification sent: {resp.status_code}")
        except Exception as e:
            logger.warning(f"Webhook notification failed: {e}")

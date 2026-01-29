"""
Handler for OdooInitJob CRD.

This handler manages the lifecycle of database initialization jobs:
1. On create: look up the referenced OdooInstance, create a Kubernetes Job to initialize the database
2. On update/timer: check Job status and update the OdooInitJob status accordingly
3. On completion: optionally call webhook, scale up the deployment
"""

from __future__ import annotations
from typing import Any, Optional
from kubernetes import client
from kubernetes.client.rest import ApiException
import base64
import logging
import os

from .postgres_clusters import get_cluster_for_instance

logger = logging.getLogger(__name__)


class OdooInitJobHandler:
    """Handles OdooInitJob CR lifecycle."""

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
        self.modules = self.spec.get("modules", ["base"])
        self.webhook = self.spec.get("webhook", {})

    @property
    def owner_reference(self):
        return client.V1OwnerReference(
            api_version="bemade.org/v1",
            kind="OdooInitJob",
            name=self.name,
            uid=self.uid,
            block_owner_deletion=True,
        )

    def on_create(self):
        """Handle creation of OdooInitJob - create the init Job."""
        logger.info(f"Creating init job for OdooInitJob {self.name}")

        # Skip if already running or completed
        phase = self.status.get("phase")
        if phase in ("Running", "Completed", "Failed"):
            logger.info(f"Init job {self.name} already in phase {phase}, skipping")
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

        if not isinstance(odoo_instance, dict):
            self._update_status("Failed", message="Invalid OdooInstance response")
            return

        # Check if instance is already being initialized, upgraded or restored
        instance_phase = odoo_instance.get("status", {}).get("phase")
        if instance_phase in ("Initializing", "Upgrading", "Restoring"):
            self._update_status(
                "Failed",
                message=f"OdooInstance {instance_name} is already {instance_phase}",
            )
            return

        # Scale down the deployment before init (in case it's running)
        self._scale_deployment(instance_name, instance_ns, 0)

        job = self._create_init_job(odoo_instance)
        if job is None or job.metadata is None:
            self._update_status("Failed", message="Failed to create init job")
            return

        job_name = job.metadata.name or ""
        start_time: Optional[str] = None
        if job.metadata.creation_timestamp is not None:
            start_time = job.metadata.creation_timestamp.isoformat()

        logger.info(f"Created init job {job_name} for {self.name}")

        # Update status to Running
        self._update_status(
            "Running",
            job_name=job_name,
            start_time=start_time,
        )

    def on_update(self):
        """Handle update of OdooInitJob."""
        self.check_job_status()

    def check_job_status(self):
        """Check the underlying Job status and update OdooInitJob accordingly."""
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
                logger.warning(f"Job {job_name} not found for init {self.name}")
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
                message="Init job failed",
            )
            self._scale_instance_back_up()
            self._notify_webhook("Failed")

    def _create_init_job(self, odoo_instance: dict[str, Any]) -> client.V1Job:
        """Create a Kubernetes Job to perform the database initialization."""
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

        # Get the PostgreSQL cluster configuration for this instance
        pg_cluster = get_cluster_for_instance(instance_spec)

        # Environment variables for database connection
        db_env = [
            client.V1EnvVar(name="HOST", value=pg_cluster.host),
            client.V1EnvVar(name="PORT", value=str(pg_cluster.port)),
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
        ]

        # Volumes - need filestore and odoo.conf
        volumes = [
            client.V1Volume(
                name="filestore",
                persistent_volume_claim=client.V1PersistentVolumeClaimVolumeSource(
                    claim_name=f"{instance_name}-filestore-pvc"
                ),
            ),
            client.V1Volume(
                name="odoo-conf",
                config_map=client.V1ConfigMapVolumeSource(
                    name=f"{instance_name}-odoo-conf"
                ),
            ),
        ]

        volume_mounts = [
            client.V1VolumeMount(name="filestore", mount_path="/var/lib/odoo"),
            client.V1VolumeMount(name="odoo-conf", mount_path="/etc/odoo"),
        ]

        # Build modules string
        modules_str = ",".join(self.modules) if self.modules else "base"

        # Main init container - runs odoo with -i to initialize
        # Use entrypoint.sh like the deployment does - it handles wait-for-psql
        # and reads db config from odoo.conf
        init_container = client.V1Container(
            name="init",
            image=image,
            command=["/entrypoint.sh", "odoo"],
            args=[
                "-i",
                modules_str,
                "-d",
                db_name,
                "--no-http",
                "--stop-after-init",
            ],
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
                    containers=[init_container],
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

    def _update_status(
        self,
        phase: str,
        job_name: Optional[str] = None,
        start_time: Optional[str] = None,
        completion_time: Optional[str] = None,
        message: Optional[str] = None,
    ) -> None:
        """Update the OdooInitJob status."""
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
                plural="odooinitjobs",
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
            "initJob": self.name,
            "namespace": self.namespace,
            "phase": phase,
            "targetInstance": self.odoo_instance_ref.get("name"),
            "modules": self.modules,
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
        """Scale the OdooInstance deployment back to its desired replicas."""
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
            if isinstance(odoo_instance, dict):
                replicas = odoo_instance.get("spec", {}).get("replicas", 1)
                self._scale_deployment(instance_name, instance_ns, replicas)
        except ApiException as e:
            logger.error(
                f"Failed to get OdooInstance {instance_name} for scale-up: {e}"
            )

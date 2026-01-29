"""
Handler for OdooUpgradeJob CRD.

This handler manages the lifecycle of upgrade jobs:
1. On create: look up the referenced OdooInstance, create a Kubernetes Job to run the upgrade
2. On update/timer: check Job status and update the OdooUpgradeJob status accordingly
3. On completion: optionally call webhook
"""

from __future__ import annotations
from kubernetes import client
from kubernetes.client.rest import ApiException
import logging
import os

from .deployment import get_odoo_volumes_and_mounts
from .postgres_clusters import get_cluster_for_instance

logger = logging.getLogger(__name__)


class OdooUpgradeJobHandler:
    """Handles OdooUpgradeJob CR lifecycle."""

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
        self.modules = self.spec.get("modules", [])
        self.modules_install = self.spec.get("modulesInstall", [])
        self.scheduled_time = self.spec.get("scheduledTime")
        self.webhook = self.spec.get("webhook", {})

    @property
    def owner_reference(self):
        return client.V1OwnerReference(
            api_version="bemade.org/v1",
            kind="OdooUpgradeJob",
            name=self.name,
            uid=self.uid,
            block_owner_deletion=True,
        )

    def on_create(self):
        """Handle creation of OdooUpgradeJob - create the upgrade Job."""
        logger.info(f"Creating upgrade job for OdooUpgradeJob {self.name}")

        # Skip if already running or completed
        phase = self.status.get("phase")
        if phase in ("Running", "Completed", "Failed"):
            logger.info(f"Upgrade job {self.name} already in phase {phase}, skipping")
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

        # Check if instance is already being upgraded or restored
        instance_phase = odoo_instance.get("status", {}).get("phase")
        if instance_phase in ("Upgrading", "Restoring"):
            self._update_status(
                "Failed",
                message=f"OdooInstance {instance_name} is already {instance_phase}",
            )
            return

        # Scale down the deployment before upgrading
        self._scale_deployment(instance_name, instance_ns, 0)

        # Update OdooInstance phase to Upgrading
        self._update_instance_phase("Upgrading")

        # Create the upgrade Job
        job = self._create_upgrade_job(odoo_instance)
        logger.info(f"Created upgrade job {job.metadata.name} for {self.name}")

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
        """Handle update of OdooUpgradeJob."""
        self.check_job_status()

    def check_job_status(self):
        """Check the underlying Job status and update OdooUpgradeJob accordingly."""
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
                logger.warning(f"Job {job_name} not found for upgrade {self.name}")
                return
            raise

        job_status = job.status
        if job_status.succeeded:
            # Scale deployment back up
            self._restore_deployment_scale()

            self._update_status(
                "Completed",
                completion_time=(
                    job_status.completion_time.isoformat()
                    if job_status.completion_time
                    else None
                ),
            )
            # Reset OdooInstance phase back to Running
            self._update_instance_phase("Running")
            self._notify_webhook("Completed")
        elif job_status.failed:
            # Scale deployment back up even on failure
            self._restore_deployment_scale()

            self._update_status(
                "Failed",
                completion_time=(
                    job_status.completion_time.isoformat()
                    if job_status.completion_time
                    else None
                ),
                message="Upgrade job failed",
            )
            # Reset OdooInstance phase back to Running even on failure
            self._update_instance_phase("Running")
            self._notify_webhook("Failed")

    def _create_upgrade_job(self, odoo_instance: dict) -> client.V1Job:
        """Create a Kubernetes Job to perform the upgrade."""
        instance_spec = odoo_instance.get("spec", {})
        instance_meta = odoo_instance.get("metadata", {})
        instance_name = instance_meta.get("name")
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

        # Format modules list as comma-separated string
        modules_str = ",".join(self.modules)
        modules_install_str = ",".join(self.modules_install)

        # Get the PostgreSQL cluster configuration for this instance
        pg_cluster = get_cluster_for_instance(instance_spec)

        env_vars = [
            client.V1EnvVar(name="HOST", value=pg_cluster.host),
            client.V1EnvVar(name="PORT", value=str(pg_cluster.port)),
            client.V1EnvVar(
                name="USER",
                value_from=client.V1EnvVarSource(
                    secret_key_ref=client.V1SecretKeySelector(
                        name=f"{instance_name}-odoo-user",
                        key="username",
                    )
                ),
            ),
            client.V1EnvVar(
                name="PASSWORD",
                value_from=client.V1EnvVarSource(
                    secret_key_ref=client.V1SecretKeySelector(
                        name=f"{instance_name}-odoo-user",
                        key="password",
                    )
                ),
            ),
        ]

        # Get volumes and mounts from the shared utility (includes filestore and odoo-conf)
        volumes, volume_mounts = get_odoo_volumes_and_mounts(instance_name)

        modules_args = []
        if modules_str:
            modules_args.append("-u")
            modules_args.append(modules_str)
        if modules_install_str:
            modules_args.append("-i")
            modules_args.append(modules_install_str)
        upgrade_container = client.V1Container(
            name="odoo-upgrade",
            image=image,
            command=["odoo"],
            args=[
                f"--db_host=$(HOST)",
                f"--db_user=$(USER)",
                f"--db_port=$(PORT)",
                f"--db_password=$(PASSWORD)",
                *modules_args,
                "-d",
                db_name,
                "--no-http",
                "--stop-after-init",
            ],
            env=env_vars,
            volume_mounts=volume_mounts,
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
                    containers=[upgrade_container],
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

    def _update_status(self, phase: str, **kwargs):
        """Update the OdooUpgradeJob status."""
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
            plural="odooupgradejobs",
            name=self.name,
            body={"status": status_body},
        )

    def _scale_deployment(self, instance_name: str, namespace: str, replicas: int):
        """Scale the OdooInstance deployment to the specified number of replicas."""
        try:
            apps_api = client.AppsV1Api()
            apps_api.patch_namespaced_deployment_scale(
                name=instance_name,
                namespace=namespace,
                body={"spec": {"replicas": replicas}},
            )
            logger.info(f"Scaled deployment {instance_name} to {replicas} replicas")
        except ApiException as e:
            if e.status == 404:
                logger.warning(f"Deployment {instance_name} not found, cannot scale")
            else:
                logger.error(f"Failed to scale deployment {instance_name}: {e}")

    def _restore_deployment_scale(self):
        """Scale the OdooInstance deployment back up after upgrade completes."""
        instance_name = self.odoo_instance_ref.get("name")
        instance_ns = self.odoo_instance_ref.get("namespace", self.namespace)

        if not instance_name:
            logger.warning("No instance name, cannot restore deployment scale")
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

    def _update_instance_phase(self, phase: str):
        """Update the OdooInstance status phase."""
        instance_name = self.odoo_instance_ref.get("name")
        instance_ns = self.odoo_instance_ref.get("namespace", self.namespace)

        if not instance_name:
            logger.warning("No instance name in odooInstanceRef, cannot update phase")
            return

        try:
            client.CustomObjectsApi().patch_namespaced_custom_object_status(
                group="bemade.org",
                version="v1",
                namespace=instance_ns,
                plural="odooinstances",
                name=instance_name,
                body={"status": {"phase": phase}},
            )
            logger.info(f"Updated OdooInstance {instance_name} phase to {phase}")
        except ApiException as e:
            logger.warning(f"Failed to update OdooInstance phase: {e}")

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
            "upgradeJob": self.name,
            "namespace": self.namespace,
            "phase": phase,
            "modules": self.modules,
        }

        try:
            resp = requests.post(url, json=payload, headers=headers, timeout=10)
            logger.info(f"Webhook notification sent: {resp.status_code}")
        except Exception as e:
            logger.warning(f"Webhook notification failed: {e}")

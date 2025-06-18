from __future__ import annotations
from .job_handler import JobHandler
from kubernetes import client
import os
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .odoo_handler import OdooHandler


class UpgradeJob(JobHandler):
    """Manages the Odoo Upgrade Job."""

    def __init__(self, handler: OdooHandler):
        super().__init__(
            handler=handler,
            status_key="upgradeJob",
            status_phase="Upgrading",
            completion_patch={"spec": {"upgrade": None}},
        )
        self.defaults = handler.defaults
        self.upgrade_spec = handler.spec.get("upgrade", {})
        self.modules = self.upgrade_spec.get("modules", [])
        self.database = self.upgrade_spec.get("database", "")

    def handle_create(self):
        if not self.handler.git_sync_job.is_running:
            super().handle_create()

    def handle_update(self):
        if not self.handler.git_sync_job.is_running:
            super().handle_update()

    def _get_resource_body(self):
        """Create the job resource definition."""
        image = self.spec.get("image", self.defaults.get("odooImage", "odoo:18.0"))

        # Add labels to make it easier to find this job later
        labels = {
            "app.kubernetes.io/name": "odoo",
            "app.kubernetes.io/instance": self.name,
            "app.kubernetes.io/component": "upgrade",
            "app.kubernetes.io/managed-by": "odoo-operator",
        }

        # Format modules list as comma-separated string
        modules_str = ",".join(self.modules)

        metadata = client.V1ObjectMeta(
            generate_name=f"{self.name}-upgrade-",  # Kubernetes will append a unique suffix
            namespace=self.namespace,
            owner_references=[self.owner_reference],
            labels=labels,  # Use our standardized labels
        )

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

        db_host = os.environ["DB_HOST"]
        db_port = os.environ["DB_PORT"]
        volumes, volume_mounts = self.handler.deployment.get_volumes_and_mounts()

        # Create the job spec
        job_spec = client.V1JobSpec(
            template=client.V1PodTemplateSpec(
                metadata=client.V1ObjectMeta(
                    labels=labels,  # Use our standardized labels
                ),
                spec=client.V1PodSpec(
                    **pull_secret,
                    restart_policy="Never",
                    volumes=volumes,
                    security_context=client.V1PodSecurityContext(
                        run_as_user=100,
                        run_as_group=101,
                        fs_group=101,
                    ),
                    affinity=self.spec.get(
                        "affinity", self.defaults.get("affinity", {})
                    ),
                    tolerations=self.spec.get(
                        "tolerations", self.defaults.get("tolerations", [])
                    ),
                    init_containers=[
                        self.handler.deployment._get_init_container_spec()
                    ],
                    containers=[
                        client.V1Container(
                            name=f"odoo-upgrade-{self.name}",
                            image=image,
                            command=["odoo"],
                            args=[
                                f"--db_host=$(HOST)",
                                f"--db_user=$(USER)",
                                f"--db_port=$(PORT)",
                                f"--db_password=$(PASSWORD)",
                                f"-u",
                                f"{modules_str}",
                                f"-d",
                                f"{self.database}",
                                "--no-http",
                                "--stop-after-init",
                            ],
                            volume_mounts=volume_mounts,
                            env=self.handler.deployment.get_environment_variables(),
                            resources=self.spec.get(
                                "resources",
                                self.defaults.get("resources", {}),
                            ),
                        )
                    ],
                ),
            ),
            backoff_limit=0,  # Retry at most 2 times
            ttl_seconds_after_finished=3600,  # Delete job 1 hour after completion
        )

        return client.V1Job(
            api_version="batch/v1",
            kind="Job",
            metadata=metadata,
            spec=job_spec,
        )

from kubernetes import client
import logging
from .resource_handler import ResourceHandler, update_if_exists, create_if_missing
import os


class UpgradeJob(ResourceHandler):
    """Manages the Odoo Upgrade Job."""

    def __init__(self, handler):
        super().__init__(handler)
        self.defaults = handler.defaults
        self.odoo_user_secret = handler.odoo_user_secret
        self.deployment = handler.deployment
        self.upgrade_spec = self.spec.get("upgrade", {})
        self.database = self.upgrade_spec.get("database", "")
        self.modules = self.upgrade_spec.get("modules", [])

    def _read_resource(self):
        try:
            return client.BatchV1Api().read_namespaced_job(
                name=f"{self.name}-upgrade",
                namespace=self.namespace,
            )
        except client.exceptions.ApiException as e:
            if e.status == 404:
                return None
            raise

    @update_if_exists
    def handle_create(self):
        # Only create the job if there's an upgrade request
        if not self.should_upgrade():
            return None

        # Scale down the deployment
        self.deployment.scale(0)

        # Create the upgrade job
        job = self._get_resource_body()
        self._resource = client.BatchV1Api().create_namespaced_job(
            namespace=self.namespace,
            body=job,
        )

        logging.info(
            f"Created upgrade job for {self.name} to upgrade modules: {self.modules}"
        )
        return self._resource

    @create_if_missing
    def handle_update(self):
        # Only create/update the job if there's an upgrade request
        if not self.should_upgrade():
            return None

        # Scale down the deployment
        self.deployment.scale(0)

        # Create or update the upgrade job
        job = self._get_resource_body()
        self._resource = client.BatchV1Api().patch_namespaced_job(
            name=f"{self.name}-upgrade",
            namespace=self.namespace,
            body=job,
        )

        logging.info(
            f"Updated upgrade job for {self.name} to upgrade modules: {self.modules}"
        )
        return self._resource

    def handle_delete(self):
        # Delete the job if it exists
        if self.resource:
            try:
                client.BatchV1Api().delete_namespaced_job(
                    name=f"{self.name}-upgrade",
                    namespace=self.namespace,
                    body=client.V1DeleteOptions(
                        propagation_policy="Foreground",
                    ),
                )
                logging.info(f"Deleted upgrade job for {self.name}")
            except client.exceptions.ApiException as e:
                if e.status != 404:
                    raise

    def should_upgrade(self):
        """Check if an upgrade should be performed."""
        return (
            self.upgrade_spec
            and self.database
            and isinstance(self.modules, list)
            and len(self.modules) > 0
        )

    def check_job_completed(self):
        """Check if the upgrade job has completed successfully."""
        if not self.resource:
            return False

        # Check if the job has completed successfully
        job_status = self.resource.status
        if job_status and job_status.succeeded:
            logging.info(f"Upgrade job for {self.name} completed successfully")
            return True

        return False

    def handle_completion(self):
        """Handle the completion of the upgrade job.
        This includes scaling the deployment back up and updating other resources.

        Returns:
            bool: True if the job was completed and handled, False otherwise
        """
        # Skip if there's no upgrade job or if it's not completed
        if not self.resource or not self.check_job_completed():
            return False

        logging.info(f"Handling completion of upgrade job for {self.name}")

        # Scale the deployment back up
        self._scale_deployment_up()

        # Remove the upgrade section from the spec to prevent re-triggering
        self._remove_upgrade_from_spec()

        logging.info(f"Upgrade process completed for {self.name}")
        return True

    def _scale_deployment_up(self):
        """Scale the deployment back up after upgrade."""
        # Get the desired replicas from the spec or default to 1
        replicas = self.spec.get("replicas", 1)

        # Scale the deployment
        success = self.deployment.scale(replicas)

        if success:
            logging.info(
                f"Scaled deployment {self.name} back up to {replicas} replicas"
            )
        else:
            logging.warning(
                f"Failed to scale deployment {self.name} back up to {replicas} replicas"
            )

    def _remove_upgrade_from_spec(self):
        """Remove the upgrade section from the spec to prevent re-triggering."""
        try:
            # Get the current custom resource
            api = client.CustomObjectsApi()

            # Apply the patch to remove the upgrade field
            patch = {"spec": {"upgrade": None}}  # This will remove the upgrade field

            api.patch_namespaced_custom_object(
                group="bemade.org",
                version="v1",
                namespace=self.namespace,
                plural="odooinstances",
                name=self.name,
                body=patch,
            )

            logging.info(f"Removed upgrade field from {self.name} spec")
        except Exception as e:
            logging.error(f"Error removing upgrade from spec: {e}")

    def _get_resource_body(self):
        """Create the job resource definition."""
        image = self.spec.get("image", self.defaults.get("odooImage", "odoo:18.0"))

        # Format modules list as comma-separated string
        modules_str = ",".join(self.modules)

        metadata = client.V1ObjectMeta(
            name=f"{self.name}-upgrade",
            namespace=self.namespace,
            owner_references=[self.owner_reference],
            labels={"app-instance": self.name, "app": f"{self.name}-upgrade", "type": "upgrade-job"},
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
        # Create the job spec
        job_spec = client.V1JobSpec(
            template=client.V1PodTemplateSpec(
                metadata=client.V1ObjectMeta(
                    labels={"app-instance": self.name, "app": f"{self.name}-upgrade", "type": "upgrade-job"},
                ),
                spec=client.V1PodSpec(
                    **pull_secret,
                    restart_policy="Never",
                    volumes=[
                        client.V1Volume(
                            name=f"filestore",
                            persistent_volume_claim=client.V1PersistentVolumeClaimVolumeSource(
                                claim_name=f"{self.name}-filestore-pvc"
                            ),
                        ),
                        client.V1Volume(
                            name="odoo-conf",
                            config_map=client.V1ConfigMapVolumeSource(
                                name=f"{self.name}-odoo-conf"
                            ),
                        ),
                    ],
                    security_context=client.V1PodSecurityContext(
                        run_as_user=1001,
                        run_as_group=1001,
                        fs_group=1001,
                    ),
                    affinity=self.spec.get(
                        "affinity", self.defaults.get("affinity", {})
                    ),
                    tolerations=self.spec.get(
                        "tolerations", self.defaults.get("tolerations", [])
                    ),
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
                            volume_mounts=[
                                client.V1VolumeMount(
                                    name="filestore",
                                    mount_path="/var/lib/odoo",
                                ),
                                client.V1VolumeMount(
                                    name="odoo-conf",
                                    mount_path="/etc/odoo",
                                ),
                            ],
                            env=[
                                client.V1EnvVar(
                                    name="HOST",
                                    value=db_host,
                                ),
                                client.V1EnvVar(
                                    name="PORT",
                                    value=db_port,
                                ),
                                client.V1EnvVar(
                                    name="USER",
                                    value_from=client.V1EnvVarSource(
                                        secret_key_ref=client.V1SecretKeySelector(
                                            name=f"{self.name}-odoo-user",
                                            key="username",
                                        )
                                    ),
                                ),
                                client.V1EnvVar(
                                    name="PASSWORD",
                                    value_from=client.V1EnvVarSource(
                                        secret_key_ref=client.V1SecretKeySelector(
                                            name=f"{self.name}-odoo-user",
                                            key="password",
                                        )
                                    ),
                                ),
                            ],
                            resources=self.spec.get(
                                "resources",
                                self.defaults.get("resources", {}),
                            ),
                        )
                    ],
                ),
            ),
            backoff_limit=2,  # Retry at most 2 times
            ttl_seconds_after_finished=3600,  # Delete job 1 hour after completion
        )

        return client.V1Job(
            api_version="batch/v1",
            kind="Job",
            metadata=metadata,
            spec=job_spec,
        )

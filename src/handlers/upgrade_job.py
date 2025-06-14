from kubernetes import client
import logging
from .resource_handler import ResourceHandler, update_if_exists, create_if_missing
import os
from datetime import datetime, timezone


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
        """Read the most recent upgrade job for this OdooInstance.

        This method finds all jobs with matching labels and owner references,
        then returns the most recent active job, or the most recent completed job
        if no active jobs are found.
        """
        try:
            # Get all jobs with matching labels
            jobs = client.BatchV1Api().list_namespaced_job(
                namespace=self.namespace,
                label_selector=f"app.kubernetes.io/instance={self.name},app.kubernetes.io/component=upgrade",
            )

            # Filter for jobs owned by this OdooInstance
            owned_jobs = [j for j in jobs.items if self._verify_owner_reference(j)]
            if not owned_jobs:
                logging.debug(f"No upgrade jobs found for {self.name}")
                return None

            # Filter for active jobs (not completed or failed)
            active_jobs = [
                j
                for j in owned_jobs
                if j.status and not (j.status.succeeded or j.status.failed)
            ]

            if active_jobs:
                # Sort by creation timestamp, newest first
                active_jobs.sort(
                    key=lambda j: j.metadata.creation_timestamp, reverse=True
                )
                return active_jobs[0]

            # If no active jobs, return the most recent completed job
            owned_jobs.sort(key=lambda j: j.metadata.creation_timestamp, reverse=True)
            return owned_jobs[0]

        except client.exceptions.ApiException as e:
            logging.warning(f"Error listing upgrade jobs for {self.name}: {e}")
            return None

    def _verify_owner_reference(self, resource):
        """Verify that the resource is owned by this OdooInstance.

        Args:
            resource: The Kubernetes resource to check

        Returns:
            bool: True if the resource is owned by this OdooInstance, False otherwise
        """
        if (
            not resource
            or not resource.metadata
            or not resource.metadata.owner_references
        ):
            return False

        # Check if any of the owner references match this OdooInstance
        for owner_ref in resource.metadata.owner_references:
            if (
                owner_ref.uid == self.owner_reference.uid
                and owner_ref.kind == self.owner_reference.kind
                and owner_ref.name == self.owner_reference.name
            ):
                return True

        return False

    @property
    def is_completed(self):
        return self._check_job_completed()

    @update_if_exists
    def handle_create(self):
        # Only create the job if there's an upgrade request
        if not self.should_upgrade():
            return None

        logging.debug(f"Creating a new upgrade job for {self.name} instead of updating")
        # Scale down the deployment
        self.deployment.scale(0)

        # Create the upgrade job
        job = self._get_resource_body()
        self._resource = client.BatchV1Api().create_namespaced_job(
            namespace=self.namespace,
            body=job,
        )

        logging.debug(
            f"Created upgrade job for {self.name} to upgrade modules: {self.modules}"
        )
        return self._resource

    @create_if_missing
    def handle_update(self):
        # For upgrade jobs, we don't update - we create a new one
        # This ensures we get a clean state for each upgrade
        return self.handle_create()

    def handle_delete(self):
        # Delete the job if it exists
        if self.resource and self.resource.metadata and self.resource.metadata.name:
            try:
                client.BatchV1Api().delete_namespaced_job(
                    name=self.resource.metadata.name,
                    namespace=self.namespace,
                    body=client.V1DeleteOptions(
                        propagation_policy="Foreground",
                    ),
                )
                logging.debug(
                    f"Deleted upgrade job {self.resource.metadata.name} for {self.name}"
                )
            except client.exceptions.ApiException as e:
                if e.status != 404:
                    raise

    def should_upgrade(self):
        """Check if an upgrade should be performed."""
        # Basic validation that this is a valid upgrade request
        if not (
            self.upgrade_spec
            and self.database
            and isinstance(self.modules, list)
            and len(self.modules) > 0
            and (
                not self.upgrade_spec.get("time")
                or datetime.fromisoformat(self.upgrade_spec.get("time"))
                < datetime.now(tz=timezone.utc)
            )
        ):
            return False

        if self.handler.git_sync_job_handler.is_running:
            return False

        # If we have a resource, check if it's still running
        if self.resource:
            # If the job is completed, we can proceed with cleanup
            # but we shouldn't create a new job yet - that will happen
            # after the upgrade spec is removed and then added again
            if self.is_completed:
                return False

            # If the job is still running, don't create a new one
            return False

        # No existing job, so we can create one
        return True

    def _check_job_completed(self):
        """Check if the upgrade job has completed successfully."""
        # If there's no job resource, it can't be completed
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
        # Skip if the job isn't completed
        if not self.is_completed:
            return False

        logging.debug(f"Handling completion of upgrade job for {self.name}")

        # Scale the deployment back up
        self._scale_deployment_up()

        # Remove the upgrade section from the spec to prevent re-triggering
        # Only if it still exists
        if self.spec.get("upgrade"):
            self._remove_upgrade_from_spec()

        # Delete the completed job
        self._cleanup_completed_job()

        # Clear the resource reference to prevent re-processing
        self._resource = None

        logging.info(f"Upgrade process completed for {self.name}")
        return True

    def _scale_deployment_up(self):
        """Scale the deployment back up after upgrade."""
        # Get the desired replicas from the spec or default to 1
        replicas = self.spec.get("replicas", 1)

        # Scale the deployment
        success = self.deployment.scale(replicas)

        if success:
            logging.debug(
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

            logging.debug(f"Removed upgrade field from {self.name} spec")
        except Exception as e:
            logging.error(f"Error removing upgrade from spec: {e}")

    def _cleanup_completed_job(self):
        """Delete the completed upgrade job to clean up resources."""
        if (
            not self.resource
            or not self.resource.metadata
            or not self.resource.metadata.name
        ):
            logging.debug(f"No job resource to delete for {self.name}")
            return

        try:
            # Delete the job using its actual name from the resource
            client.BatchV1Api().delete_namespaced_job(
                name=self.resource.metadata.name,
                namespace=self.namespace,
                body=client.V1DeleteOptions(
                    propagation_policy="Background",
                ),
            )
            logging.debug(
                f"Deleted completed upgrade job {self.resource.metadata.name} for {self.name}"
            )
            # Clear the resource reference
            self._resource = None
        except client.exceptions.ApiException as e:
            if e.status != 404:  # Ignore if job is already gone
                logging.error(f"Error deleting completed upgrade job: {e}")

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
        # Create the job spec
        job_spec = client.V1JobSpec(
            template=client.V1PodTemplateSpec(
                metadata=client.V1ObjectMeta(
                    labels=labels,  # Use our standardized labels
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

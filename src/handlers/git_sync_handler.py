import logging
from typing import Any
from kubernetes import client
from .resource_handler import ResourceHandler, update_if_exists, create_if_missing
from datetime import datetime, timezone
from .odoo_handler import OdooHandler
import os
import yaml

logger = logging.getLogger(__name__)


class GitSyncHandler(ResourceHandler):
    """Simple handler for GitSync resources that launches a sync job.

    This is a minimal handler that creates a job to sync a Git repository.
    The job's status is tracked by Kubernetes, so we don't need to manage status here.
    """

    def __init__(self, body: Any = None, **kwargs):
        if body:
            self.body = body
            self.spec = body.get("spec", {})
            self.meta = body.get("meta", body.get("metadata"))
            self.namespace = self.meta.get("namespace")
            self.name = self.meta.get("name")
            self.uid = self.meta.get("uid")
            self.owner_reference = self._get_owner_reference()
        else:
            self.body = {}
            self.spec = {}
            self.meta = {}
            self.namespace = None
            self.name = None
            self.uid = None
        self._resource = None
        self.operator_ns = os.environ.get("OPERATOR_NAMESPACE")
        # Load defaults if available
        try:
            with open("/etc/odoo/instance-defaults.yaml") as f:
                self.defaults = yaml.safe_load(f)
        except (FileNotFoundError, PermissionError):
            self.defaults = {}

    def _get_owner_reference(self):
        # First check if we're dealing with a Job object
        # If so, extract the OdooInstance name from the ownerReferences
        odoo_instance_name = None
        owner_refs = self.meta.get("ownerReferences", [])

        for ref in owner_refs:
            if (
                ref.get("kind") == "OdooInstance"
                and ref.get("apiVersion") == "bemade.org/v1"
            ):
                odoo_instance_name = ref.get("name")
                # Return owner reference directly if we already have all needed info
                if ref.get("uid"):
                    return client.V1OwnerReference(
                        api_version="bemade.org/v1",
                        kind="OdooInstance",
                        name=odoo_instance_name,
                        uid=ref.get("uid"),
                        block_owner_deletion=True,
                    )

        # If no name found in ownerReferences, try the spec
        if not odoo_instance_name:
            odoo_instance_name = self.spec.get("odooInstance")

        if not odoo_instance_name:
            logger.warning(
                f"Unable to determine OdooInstance name for {self.namespace}/{self.name}"
            )
            return None

        odoo_instance = client.CustomObjectsApi().get_namespaced_custom_object(
            group="bemade.org",
            version="v1",
            namespace=self.namespace,
            plural="odooinstances",
            name=odoo_instance_name,
        )
        return client.V1OwnerReference(
            api_version="bemade.org/v1",
            kind="OdooInstance",
            name=odoo_instance_name,
            uid=odoo_instance.get("metadata").get("uid"),
            block_owner_deletion=True,
        )

    def _read_resource(self):
        return client.BatchV1Api().read_namespaced_job(
            name=self.name,
            namespace=self.namespace,
        )

    @property
    def odoo_handler(self):
        name = self.spec.get("odooInstance") or self.meta.get("ownerReferences")[0].get(
            "name"
        )
        body = client.CustomObjectsApi().get_namespaced_custom_object(
            group="bemade.org",
            version="v1",
            namespace=self.namespace,
            plural="odooinstances",
            name=name,
        )
        return OdooHandler(body=body)

    @update_if_exists
    def handle_create(self):
        """Create a job to sync the Git repository."""
        self._resource = self._create_sync_job()

    @create_if_missing
    def handle_update(self):
        """Update a job to sync the Git repository."""
        logger.debug(f"In handle_update for GitSync Job")
        if self.resource.status.succeeded or self.resource.status.failed:
            logger.debug(f"GitSync Job completed with status: {self.resource.status}")
            self.handle_completion()

    def _create_sync_job(self) -> client.V1Job:
        """Create a Kubernetes Job to sync the Git repository."""
        name = self.spec.get("odooInstance") or self.meta.get("ownerReferences")[0].get(
            "name"
        )

        # Prepare volumes for the pod
        volumes = [
            client.V1Volume(
                name="repo-volume",
                persistent_volume_claim=client.V1PersistentVolumeClaimVolumeSource(
                    claim_name=self.spec.get("pvcName", f"{name}-repo-pvc")
                ),
            )
        ]

        # Add SSH secret volume if specified
        ssh_secret_name = self.spec.get("sshSecret")
        if ssh_secret_name:
            volumes.append(
                client.V1Volume(
                    name="git-secret",
                    secret=client.V1SecretVolumeSource(secret_name=ssh_secret_name),
                )
            )

        # Define the job
        job = client.V1Job(
            metadata=client.V1ObjectMeta(
                name=self.name,
                namespace=self.namespace,
                owner_references=[self.owner_reference],
            ),
            spec=client.V1JobSpec(
                template=client.V1PodTemplateSpec(
                    spec=client.V1PodSpec(
                        containers=[self._get_git_sync_container()],
                        restart_policy="Never",
                        volumes=volumes,
                    )
                ),
                backoff_limit=2,
            ),
        )

        # Create the job
        job = client.BatchV1Api().create_namespaced_job(
            namespace=self.namespace, body=job
        )
        logger.info(f"Created Git sync job: {self.name}")

        return job

    def _get_git_sync_container(self) -> client.V1Container:
        """Create the container spec for the Git sync job."""
        branch = self.spec.get("branch", "main")
        repository = self.spec.get("repository", "")
        ssh_secret_name = self.spec.get("sshSecret")

        # Create a shell script that will handle both initial clone and updates with submodules
        # Include SSH setup if an SSH secret is provided
        ssh_setup = ""
        if ssh_secret_name:
            ssh_setup = """
# Set up SSH configuration
mkdir -p ~/.ssh
cp /etc/git-secret/ssh-privatekey ~/.ssh/id_rsa
chmod 600 ~/.ssh/id_rsa
ssh-keyscan -t rsa github.com gitlab.com bitbucket.org >> ~/.ssh/known_hosts

# Set git to use the SSH key
git config --global core.sshCommand 'ssh -i ~/.ssh/id_rsa -o StrictHostKeyChecking=accept-new'
"""

        git_script = f"""#!/bin/sh
set -e

REPO_DIR="/repo"
BRANCH="{branch}"
REPOSITORY="{repository}"

{ssh_setup}

# Check if the directory is a git repository
if [ -d "$REPO_DIR/.git" ]; then
    echo "Updating existing repository..."
    cd "$REPO_DIR"

    # Reset any local changes in main repo and submodules
    git submodule foreach --recursive 'git reset --hard && git clean -fd'
    git reset --hard
    git clean -fd

    # Fetch and reset to the remote branch
    git fetch --depth=1 origin "$BRANCH"
    git reset --hard "origin/$BRANCH"

    # Update submodules to their recorded commits
    git submodule update --init --recursive --depth=1 --force
else
    echo "Cloning repository..."
    # Clone with depth=1 for minimal download
    git clone --depth=1 --branch "$BRANCH" --recurse-submodules --shallow-submodules "$REPOSITORY" "$REPO_DIR"

    # Ensure submodules are at the correct commits
    cd "$REPO_DIR"
    git submodule update --init --recursive --depth=1
fi

echo "Git sync completed successfully"
"""

        # Prepare volume mounts
        volume_mounts = [client.V1VolumeMount(name="repo-volume", mount_path="/repo")]

        # Add SSH secret volume mount if provided
        if ssh_secret_name:
            volume_mounts.append(
                client.V1VolumeMount(name="git-secret", mount_path="/etc/git-secret")
            )

        return client.V1Container(
            name="git-sync",
            image="alpine/git:latest",
            command=["/bin/sh", "-c", git_script],
            volume_mounts=volume_mounts,
        )

    def handle_completion(self):
        """Handle the completion of the git sync job.

        This includes redeploying the OdooInstance to pick up the new code.
        """
        try:
            logger.info("Handling completion for Git sync job")
            # Find the deployment through OdooHandler
            try:
                deployment = self.odoo_handler.deployment

                if not deployment or not hasattr(deployment, "resource"):
                    logger.error("Deployment not found or deployment.resource is None")
                    return

                # Ensure labels dict exists
                if (
                    not hasattr(deployment.resource.metadata, "labels")
                    or deployment.resource.metadata.labels is None
                ):
                    deployment.resource.metadata.labels = {}

                # Add sync timestamp
                deployment.resource.metadata.labels["bemade.org/last_sync"] = (
                    datetime.now(tz=timezone.utc).isoformat()
                )

                # Determine job success status
                job_succeeded = False
                if hasattr(self._resource, "status") and hasattr(
                    self._resource.status, "succeeded"
                ):
                    job_succeeded = self._resource.status.succeeded > 0

                deployment.resource.metadata.labels["bemade.org/last_sync_status"] = (
                    "succeeded" if job_succeeded else "failed"
                )

                logger.info(
                    f"Patching deployment {deployment.name} after Git sync completion (status: {job_succeeded})"
                )

                # Patch the deployment to trigger a restart
                client.AppsV1Api().patch_namespaced_deployment(
                    name=deployment.name,
                    namespace=deployment.namespace,
                    body=deployment.resource,
                )

                logger.info(
                    f"Successfully patched deployment {deployment.name} after Git sync"
                )
            except Exception as e:
                logger.error(f"Error getting deployment: {str(e)}")
        except Exception as e:
            logger.error(f"Error in handle_completion: {str(e)}")
            raise e

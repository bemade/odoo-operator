from .resource_handler import ResourceHandler
from datetime import datetime
from kubernetes import client
import kopf
import logging
import os

logger = logging.getLogger(__name__)


class GitSyncJobHandler(ResourceHandler):
    """Handler for Git sync jobs directly owned by OdooInstance resources.

    This handler creates and manages jobs to sync Git repositories specified in OdooInstance specs.
    It works directly with OdooInstance resources rather than with a separate GitSync CRD.
    """

    def _read_resource(self):
        """Read the job resource from the Kubernetes API."""
        try:
            # For an existing running job, use the name pattern with timestamp
            # Query by label selector to find all jobs related to this OdooInstance
            jobs = client.BatchV1Api().list_namespaced_job(
                namespace=self.namespace, label_selector=f"app={self.name}"
            )

            # If any active jobs exist, return the most recent one
            if jobs and jobs.items:
                # Sort by creation timestamp, newest first
                sorted_jobs = sorted(
                    jobs.items,
                    key=lambda job: job.metadata.creation_timestamp,
                    reverse=True,
                )
                # Return the most recent job
                return sorted_jobs[0]
            # If no jobs found, return None
            return None
        except client.exceptions.ApiException as e:
            logger.error(f"Error reading job for {self.name}: {e}")
            return None

    def handle_create(self):
        """Create a job to sync the Git repository.

        This uses information from the OdooInstance's gitProject field to determine what to sync.
        """
        logger.debug(
            f"Creating git sync job for {self.name} in namespace {self.namespace}"
        )

        # Get git project information from the OdooInstance spec
        git_project = self.spec.get("gitProject", {})
        repository = git_project.get("repository")
        branch = git_project.get("branch", "main")
        ssh_secret_name = git_project.get("sshSecret")

        if not repository:
            logger.error(f"No repository specified in gitProject for {self.name}")
            return

        # Generate a timestamp for the job name to ensure uniqueness
        timestamp = datetime.now().strftime("%Y%m%d-%H%M%S")
        job_name = f"{self.name}-{timestamp}"

        # Prepare volumes for the pod
        volumes = [
            client.V1Volume(
                name="repo-volume",
                persistent_volume_claim=client.V1PersistentVolumeClaimVolumeSource(
                    claim_name=f"{self.spec.get('name') or self.name.replace('-gitsync', '')}-repo-pvc"
                ),
            )
        ]

        # Add SSH secret volume if specified
        if ssh_secret_name:
            volumes.append(
                client.V1Volume(
                    name="git-secret",
                    secret=client.V1SecretVolumeSource(secret_name=ssh_secret_name),
                )
            )

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

        # Create the container
        container = client.V1Container(
            name="git-sync",
            image="alpine/git:latest",
            command=["/bin/sh", "-c", git_script],
            volume_mounts=volume_mounts,
        )

        # Define the job
        job = client.V1Job(
            metadata=client.V1ObjectMeta(
                name=job_name,
                namespace=self.namespace,
                owner_references=[self.owner_reference],
                labels={"app": self.name, "type": "git-sync-job"},
            ),
            spec=client.V1JobSpec(
                template=client.V1PodTemplateSpec(
                    spec=client.V1PodSpec(
                        containers=[container],
                        restart_policy="Never",
                        volumes=volumes,
                    )
                ),
                backoff_limit=2,
            ),
        )
        # First, scale down the deployment to avoid conflicts
        self.handler.deployment.scale(0)

        # Create the job
        job = client.BatchV1Api().create_namespaced_job(
            namespace=self.namespace, body=job
        )
        logger.info(f"Created Git sync job: {job_name} in namespace {self.namespace}")

        # Store the reference to the created job
        self._resource = job
        return job

    def handle_update(self):
        """Handle job updates, including completion processing.

        This checks if the job has completed (success or failure) and updates the
        OdooInstance status accordingly. It also restarts the deployment if needed.
        """
        logger.debug(f"Handling Git Sync Job update for {self.name}")

        # Read the current job resource
        job = self._read_resource()
        if not job:
            logger.debug(f"No sync job found for {self.name}")
            return

        # Check job status
        job_status = job.status
        if not job_status:
            logger.debug(f"No status available for job {job.metadata.name}")
            return

        # Check if completed
        if not hasattr(job_status, "completion_time") or not job_status.completion_time:
            logger.debug(f"Job {job.metadata.name} not completed yet, doing nothing.")
            return

        # Get success and failure conditions
        succeeded = (
            job_status.succeeded == 1 if hasattr(job_status, "succeeded") else False
        )
        failed = job_status.failed == 1 if hasattr(job_status, "failed") else False

        if succeeded or failed:
            # Update OdooInstance status
            timestamp = datetime.now().isoformat()
            status_patch = {
                "syncStatus": {
                    "lastAttempt": timestamp,
                }
            }

            if succeeded:
                status_patch["syncStatus"]["lastSync"] = timestamp
                logger.info(f"Git sync job succeeded for {self.name}")
            else:
                status_patch["syncStatus"]["lastError"] = "Git sync job failed"
                logger.error(f"Git sync job failed for {self.name}")

            # Update status and spec via k8s client API
            # Update the status
            client.CustomObjectsApi().patch_namespaced_custom_object_status(
                group="bemade.org",
                version="v1",
                namespace=self.namespace,
                plural="odooinstances",
                name=self.handler.name,
                body={"status": status_patch},
            )

            # Restart the deployment
            self.handler.deployment.scale(1)

    def handle_delete(self):
        """Handle deletion of a sync job."""
        logger.debug(f"Handling Git Sync Job deletion for {self.name}")
        job = self._read_resource()
        if job:
            try:
                client.BatchV1Api().delete_namespaced_job(
                    name=job.metadata.name,
                    namespace=self.namespace,
                    body=client.V1DeleteOptions(propagation_policy="Foreground"),
                )
                logger.info(f"Deleted sync job {job.metadata.name}")
            except client.exceptions.ApiException as e:
                logger.error(f"Error deleting sync job: {e}")
        else:
            logger.debug(f"No sync job found to delete for {self.name}")

    @property
    def is_running(self):
        status = self.resource and self.resource.status
        return status and (status.succeeded or status.failed)

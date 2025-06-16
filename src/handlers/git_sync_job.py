from __future__ import annotations
from .job_handler import JobHandler
from datetime import datetime
from kubernetes import client
import logging
from typing import TYPE_CHECKING, Optional

if TYPE_CHECKING:
    from .odoo_handler import OdooHandler

logger = logging.getLogger(__name__)


class GitSyncJob(JobHandler):
    """Handler for Git sync jobs directly owned by OdooInstance resources.

    This handler creates and manages jobs to sync Git repositories specified in OdooInstance specs.
    It works directly with OdooInstance resources rather than with a separate GitSync CRD.
    """

    def __init__(self, handler: OdooHandler):
        super().__init__(
            handler=handler,
            status_key="syncJob",
            status_phase="Syncing",
            completion_patch={"spec": {"sync": None}},
        )

    def _get_resource_body(self) -> client.V1Job:
        # Get git project information from the OdooInstance spec
        git_project = self.spec.get("gitProject", {})
        repository = git_project.get("repository")
        branch = git_project.get("branch", "main")
        ssh_secret_name = git_project.get("sshSecret")

        if not repository:
            raise ValueError(f"No repository specified in gitProject for {self.name}")

        # Generate a timestamp for the job name to ensure uniqueness
        timestamp = datetime.now().strftime("%Y%m%d-%H%M%S")
        job_name = f"{self.name}-git-sync-{timestamp}"

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

MOUNT_DIR="/repo"
REPO_DIR="/repo/odoo-code"
BRANCH="{branch}"
REPOSITORY="{repository}"

# Create the subdirectory if it doesn't exist
mkdir -p "$REPO_DIR"

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
    # Normal clone into the subdirectory
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
        return client.V1Job(
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

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
        # Default uid/gid for running the container
        run_as_user = git_project.get("runAsUser", 1000)
        run_as_group = git_project.get("runAsGroup", 1000)
        # Extract resource requirements if specified
        resources = git_project.get("resources", {})

        if not repository:
            raise ValueError(f"No repository specified in gitProject for {self.name}")

        # Generate a timestamp for the job name to ensure uniqueness
        timestamp = datetime.now().strftime("%Y%m%d-%H%M%S")
        job_name = f"{self.name}-git-sync-{timestamp}"

        # Get PVC name
        pvc_name = (
            f"{self.spec.get('name') or self.name.replace('-gitsync', '')}-repo-pvc"
        )

        # Prepare volumes for the pod
        volumes = [
            client.V1Volume(
                name="repo-volume",
                persistent_volume_claim=client.V1PersistentVolumeClaimVolumeSource(
                    claim_name=pvc_name
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
mkdir -p /root/.ssh
cp /etc/git-secret/ssh-privatekey /root/.ssh/id_rsa
chmod 600 /root/.ssh/id_rsa
ssh-keyscan -t rsa github.com gitlab.com bitbucket.org >> /root/.ssh/known_hosts

# Set git to use the SSH key
git config --global core.sshCommand 'ssh -i /root/.ssh/id_rsa -o StrictHostKeyChecking=accept-new'

# Make git more verbose for debugging
git config --global --add advice.detachedHead false
"""

        git_script = f"""#!/bin/sh
set -x

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
    git reset --hard
    git stash && git stash clear

    # Fetch and reset to the remote branch
    git fetch origin "$BRANCH"
    git reset --hard "origin/$BRANCH"

    # Update submodules to their recorded commits
    git submodule foreach --recursive 'git stash && git stash clear'
    git submodule update --init --recursive --recommend-shallow
else
    echo "Cloning repository..."
    # Use full clone for main repo (no depth limit) but shallow submodules
    git clone --branch "$BRANCH" "$REPOSITORY" "$REPO_DIR" --recurse-submodules --shallow-submodules

fi

echo "Git sync completed successfully"
"""

        # Init container script to fix permissions
        init_script = """
#!/bin/sh
set -x

# Ensure proper ownership for the mounted volume
chown -R 0:0 /repo
chmod -R 775 /repo
"""

        # Prepare volume mounts
        volume_mounts = [client.V1VolumeMount(name="repo-volume", mount_path="/repo")]

        # Add SSH secret volume mount if provided
        if ssh_secret_name:
            volume_mounts.append(
                client.V1VolumeMount(name="git-secret", mount_path="/etc/git-secret")
            )

        # Create resource requirements object if specified
        container_resources = None
        if resources:
            container_resources = client.V1ResourceRequirements(
                requests=resources.get(
                    "requests",
                    {"cpu": "100m", "memory": "128Mi", "ephemeral-storage": "1Gi"},
                ),
                limits=resources.get(
                    "limits",
                    {"cpu": "500m", "memory": "512Mi", "ephemeral-storage": "2Gi"},
                ),
            )

        # Create the init container to fix permissions
        init_container = client.V1Container(
            name="init-fix-permissions",
            image="alpine:latest",
            command=["/bin/sh", "-c", init_script],
            volume_mounts=volume_mounts,
            security_context=client.V1SecurityContext(
                run_as_user=0,  # Run as root for permission changes
                run_as_group=0,
            ),
            resources=container_resources,
        )

        # Create the main container
        container = client.V1Container(
            name="git-sync",
            image="alpine/git:latest",
            command=["/bin/sh", "-c", git_script],
            volume_mounts=volume_mounts,
            security_context=client.V1SecurityContext(
                run_as_user=0,  # Run as root to ensure write permissions
                run_as_group=0,
                # Note: allowPrivilegeEscalation not supported in this k8s client version
            ),
            resources=container_resources,
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
                        init_containers=[init_container],
                        containers=[container],
                        restart_policy="Never",
                        volumes=volumes,
                        security_context=client.V1PodSecurityContext(
                            fs_group=0  # Set filesystem group to access mounted volumes
                        ),
                    )
                ),
                backoff_limit=2,
            ),
        )

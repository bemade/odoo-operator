import logging
from kubernetes import client
from .pvc_handler import PVCHandler


class GitRepoPVC(PVCHandler):
    """Manages the Git repository Persistent Volume Claim for Odoo addons.

    This handles both the PVC creation and Git sync initialization.
    """

    def __init__(self, handler):
        super().__init__(
            handler=handler,
            pvc_name_suffix="repo-pvc",
            default_size="1Gi",
            default_storage_class="longhorn",
        )

    def _get_storage_spec(self):
        """Get the storage specification from the CRD."""
        git_project = self.spec.get("gitProject", {})
        storage_spec = git_project.get("storage", {})
        return {
            "size": storage_spec.get("size") or self.default_size,
            "class": storage_spec.get("class") or self.default_storage_class,
        }

    def _get_resource_body(self):
        """Get the PVC definition."""
        storage = self._get_storage_spec()

        return client.V1PersistentVolumeClaim(
            metadata=client.V1ObjectMeta(
                name=self._get_pvc_name(),
                owner_references=[self.owner_reference],
            ),
            spec=client.V1PersistentVolumeClaimSpec(
                access_modes=["ReadWriteOnce"],
                storage_class_name=storage["class"],
                resources=client.V1ResourceRequirements(
                    requests={"storage": storage["size"]}
                ),
            ),
        )

    def handle_create(self):
        """Create the PVC and initialize Git sync."""
        super().handle_create()
        self._init_git_sync()

    def _init_git_sync(self):
        """Initialize Git sync if configured."""
        git_project = self.spec.get("gitProject")
        if not git_project:
            return

        try:
            # Import here to avoid circular import
            from .git_sync_handler import GitSyncHandler
            # Create GitSync using the parent's body - GitSyncHandler expects the full CR
            git_sync_body = {
                "apiVersion": "odoo.bemade.org/v1",
                "kind": "GitSync",
                "metadata": {
                    "name": f"{self.name}-git-sync",
                    "namespace": self.namespace,
                    "ownerReferences": [
                        {
                            "apiVersion": self.body["apiVersion"],
                            "kind": self.body["kind"],
                            "name": self.name,
                            "uid": self.owner_reference.uid,
                            "controller": True,
                            "blockOwnerDeletion": True,
                        }
                    ],
                },
                "spec": {
                    "repository": git_project.get("repository"),
                    "branch": git_project.get("branch", "main"),
                    "targetPath": "/mnt/addons",
                    "pvcName": self._get_pvc_name(),
                    "sshSecret": git_project.get("sshSecret"),
                    "odooInstance": self.name,  # Reference to the parent OdooInstance
                },
            }

            # Initialize and create the GitSync resource
            GitSyncHandler(git_sync_body).on_create()
        except Exception as e:
            logging.error(f"Failed to initialize Git sync: {e}")
            raise

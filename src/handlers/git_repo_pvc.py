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
        )

    def _get_storage_spec(self):
        """Get the storage specification from the CRD."""
        git_project = self.spec.get("gitProject", {})
        spec = {}

        # Get storage class if specified
        storage_class = git_project.get("storageClass")
        if storage_class:
            spec["storage_class_name"] = storage_class

        # Get size (with default from constructor)
        size = git_project.get("size") or self.default_size

        spec["resources"] = client.V1ResourceRequirements(requests={"storage": size})

        logging.info(
            f"Using Git repo PVC spec: size={size}, storageClass={storage_class}"
        )
        return spec

    def _get_resource_body(self):
        """Get the PVC definition."""
        storage_spec = self._get_storage_spec()

        return client.V1PersistentVolumeClaim(
            metadata=client.V1ObjectMeta(
                name=self._get_pvc_name(),
                owner_references=[self.owner_reference],
            ),
            spec=client.V1PersistentVolumeClaimSpec(
                access_modes=["ReadWriteOnce"],
                **storage_spec,
            ),
        )

    def _should_create(self):
        """Check if this PVC should be created."""
        git_project = self.spec.get("gitProject")
        if not git_project:
            logging.info(
                f"No gitProject configured for {self.name}, skipping repo PVC creation"
            )
            return False
        return True

    def handle_create(self):
        """Create the PVC and initialize Git sync."""
        # Only create if gitProject is specified
        if not self._should_create():
            return

        super().handle_create()
        self._init_git_sync()

    def _init_git_sync(self):
        """Initialize Git sync if configured."""
        git_project = self.spec.get("gitProject")
        if not git_project:
            logging.info(f"No gitProject configured for {self.name}")
            return

        try:
            # Import here to avoid circular import
            from .git_sync_handler import GitSyncHandler

            # Create GitSync using the parent's body - GitSyncHandler expects the full CR
            # Use parent's body for API version and kind references
            git_sync_body = {
                "apiVersion": "bemade.org/v1",
                "kind": "GitSync",
                "metadata": {
                    # "name": f"{self.name}-git-sync",
                    "namespace": self.namespace,
                    "generateName": "true",
                    "ownerReferences": [
                        {
                            "apiVersion": "bemade.org/v1",
                            "kind": "OdooInstance",
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

            logging.info(f"Initializing Git sync for {self.name}")

            # Use the custom objects API directly to create the GitSync resource
            api = client.CustomObjectsApi()
            try:
                api.create_namespaced_custom_object(
                    group="bemade.org",
                    version="v1",
                    namespace=self.namespace,
                    plural="gitsyncs",
                    body=git_sync_body,
                )
                logging.info(f"Successfully created GitSync resource for {self.name}")
            except client.exceptions.ApiException as e:
                if e.status == 409:  # Conflict - resource already exists
                    logging.info(
                        f"GitSync resource for {self.name} already exists, skipping creation"
                    )
                else:
                    logging.error(f"Failed to create GitSync resource: {e}")
                    raise
        except Exception as e:
            logging.error(f"Failed to initialize Git sync: {e}")
            raise

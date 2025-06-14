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
        """Initialize Git sync for the newly created PVC if gitProject is specified.

        This directly updates the OdooInstance spec to enable sync, which will trigger
        the sync job through the OdooHandler's sync handling mechanism.
        """
        spec = self.spec
        git_project = spec.get("gitProject")
        if not git_project or not git_project.get("repository"):
            logging.info(f"No git repository specified in spec, skipping Git sync")
            return

        logging.info(f"Initializing Git sync for {self.name}")

        deployment_exists = False
        while not deployment_exists:
            try:
                deployment_exists = handler.deployment.resource
            except client.exceptions.ApiException as e:
                if e.status == 404:
                    logging.info(
                        f"Deployment {self.name} not found, waiting before running sync..."
                    )
                    time.sleep(1)
                else:
                    raise

        # Get the current OdooInstance resource
        api = client.CustomObjectsApi()
        sync_spec = {
            "spec": {
                "sync": {
                    "enabled": True,
                }
            }
        }

        # Patch the OdooInstance to enable sync
        api.patch_namespaced_custom_object(
            group="bemade.org",
            version="v1",
            namespace=self.namespace,
            plural="odooinstances",
            name=self.handler.name,
            body=sync_spec,
        )
        logging.info(
            f"Enabled sync on OdooInstance {self.name} for initial git repo setup"
        )

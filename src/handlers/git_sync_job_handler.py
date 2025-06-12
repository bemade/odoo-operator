from .resource_handler import ResourceHandler
from kubernetes import client
import kopf


class GitSyncJobHandler(ResourceHandler):
    def __init__(self, body, **kwargs):
        self.body = body
        self.spec = body.get("spec", {})
        self.meta = body.get("meta", body.get("metadata"))
        self.namespace = self.meta.get("namespace")
        self.name = self.meta.get("name")
        self.uid = self.meta.get("uid")
        self.owner_references = self.meta.get("ownerReferences", [])
        self._resource = None

    def _read_resource(self):
        return client.BatchV1Api().read_namespaced_job(
            name=self.name,
            namespace=self.namespace,
        )

    def handle_update(self):
        logger.debug(f"Handling Git Sync Job update.")
        job: client.V1Job = self.resource
        status = job.status
        succeeded = status.succeeded and status.succeeded > 0
        failed = status.failed and status.failed > 0
        if succeeded or failed:
            owner_refs = job.metadata.owner_references
            for ref in owner_refs:
                if ref.kind.lower() == "gitsync":
                    logger.debug(
                        f"Found GitSync owner reference: {ref}. Patching status."
                    )
                    client.CustomObjectsApi().patch_namespaced_custom_object(
                        group="bemade.org",
                        version="v1",
                        namespace=self.namespace,
                        plural="gitsyncs",
                        name=ref.name,
                        body={
                            "status": {
                                "succeeded": bool(succeeded),
                                "failed": bool(failed),
                            }
                        },
                    )
                    return
            raise kopf.AdmissionError("GitSync not found")
